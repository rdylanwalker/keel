//! `POST /api/v1/tenants/{id}/stack-sources/migrate` (`SDD.md` §4.2 "Tenant Stack Git source").
//!
//! Tenant PKO `Stack`s committed by earlier releases use `spec.fluxSource` →
//! `tenant-<id>/keel-gitops`, so their workspace pods download whole-repository artifacts from
//! `source-controller` (`flux-system` port 9090). This endpoint opens **one** PR per tenant that
//! rewrites every such `tenants/<id>/**/stack.yaml` to PKO `gitSource` (same platform-written
//! source and read-only `gitAuth` as newly proposed Stacks), keeping programs and config
//! unchanged. It never commits to `main` and never touches the `flux-system` management Stack.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use keel_core::validate_dns_label;
use keel_pko::{FluxSourceMigration, build_preview_branch, migrate_legacy_flux_source_stack};
use serde::{Deserialize, Serialize};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;

use super::proposals::{resolve_tenant_stack_source, tenant_stack_git_source};
use super::{
    next_unique_branch_timestamp, open_pr_for_session, read_contained_optional,
    resolve_contained_path, stage_commit_push_and_return_to_main, write_contained,
};

/// Query parameters of `POST /api/v1/tenants/{id}/stack-sources/migrate`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MigrateStackSourcesQuery {
    /// Report what would be rewritten without creating a branch or PR.
    pub dry_run: bool,
}

/// A legacy `stack.yaml` that is not rewritten automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedStackSource {
    pub path: String,
    pub reason: String,
}

/// Response body of `POST /api/v1/tenants/{id}/stack-sources/migrate`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateStackSourcesResponse {
    pub tenant: String,
    pub dry_run: bool,
    /// Repository paths rewritten from `fluxSource` to PKO `gitSource`.
    pub migrated: Vec<String>,
    /// Legacy `fluxSource` Stacks left unchanged (with a secret-free reason).
    pub skipped: Vec<SkippedStackSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<crate::proposal_repo::ScopedProposal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Recursively lists `stack.yaml` files under `dir` (repository-relative, sorted), skipping
/// symlinks and anything resolving outside `root`.
async fn find_stack_manifests(root: &Path, dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        if resolve_contained_path(root, &current).await.is_err() {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file()
                && entry.file_name() == "stack.yaml"
                && let Ok(rel) = path.strip_prefix(root)
                && let Some(rel) = rel.to_str()
            {
                out.push(rel.to_string());
            }
        }
    }
    out.sort();
    out
}

/// `POST /api/v1/tenants/{id}/stack-sources/migrate[?dryRun=true]` (platform admin only).
///
/// Opens one PR on `keel/<id>/migrate-stack-sources-<ts>` that moves every legacy tenant `Stack`
/// (`spec.fluxSource` → `tenant-<id>/keel-gitops`) to PKO `gitSource`
/// (`keel_pko::migrate_legacy_flux_source_stack`). Returns `201` with the PR, or `200` when there
/// is nothing to migrate (or `dryRun`). Fails closed with `409` when the tenant has no
/// platform-written Git source (`tenants/<id>/git-repository.yaml`).
pub async fn migrate_stack_sources(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<MigrateStackSourcesQuery>,
) -> Result<(StatusCode, Json<MigrateStackSourcesResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "tenant")?;
    let source = resolve_tenant_stack_source(&state, &id).await?;

    let guard = state.git_lock.lock().await;
    state
        .git_runner
        .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
        .await?;
    let root = state.repo_workdir.clone();
    let mut migrated: Vec<(String, String)> = Vec::new();
    let mut skipped = Vec::new();
    for rel in find_stack_manifests(&root, &root.join("tenants").join(&id)).await {
        let Some(content) = read_contained_optional(&root, &root.join(&rel)).await? else {
            continue;
        };
        match migrate_legacy_flux_source_stack(&content, &id, |dir| {
            tenant_stack_git_source(&source, dir)
                .map_err(|e| keel_pko::PkoError::Validation(e.to_string()))
        }) {
            Ok(FluxSourceMigration::Migrated(yaml)) => migrated.push((rel, yaml)),
            Ok(FluxSourceMigration::Unchanged) => {}
            Ok(FluxSourceMigration::Skipped(reason)) => {
                skipped.push(SkippedStackSource { path: rel, reason })
            }
            Err(e) => {
                tracing::warn!(tenant = %id, path = %rel, error = %e, "legacy Stack source not migrated");
                skipped.push(SkippedStackSource {
                    path: rel,
                    reason: "manifest could not be parsed or rewritten".to_string(),
                });
            }
        }
    }
    let paths: Vec<String> = migrated.iter().map(|(p, _)| p.clone()).collect();
    if migrated.is_empty() || query.dry_run {
        drop(guard);
        return Ok((
            StatusCode::OK,
            Json(MigrateStackSourcesResponse {
                tenant: id,
                dry_run: query.dry_run,
                migrated: paths,
                skipped,
                proposal: None,
                branch: None,
            }),
        ));
    }

    let branch =
        build_preview_branch(&id, "migrate-stack-sources", next_unique_branch_timestamp())?;
    state
        .git_runner
        .checkout_new_branch(&state.repo_workdir, &branch)
        .await?;
    for (rel, yaml) in &migrated {
        if let Err(e) = write_contained(&root, &root.join(rel), yaml).await {
            let _ = state
                .git_runner
                .checkout_branch(&state.repo_workdir, "main")
                .await;
            return Err(e);
        }
    }
    let title = format!("fix({id}): clone tenant PKO Stacks with gitSource");
    let commit_sha =
        stage_commit_push_and_return_to_main(&state, &auth, &branch, &title, &paths).await?;
    drop(guard);

    let mut body = format!(
        "Moves {} tenant `{id}` PKO Stack(s) from Flux `fluxSource` (whole-repository \
         `source-controller` artifacts) to PKO `gitSource` on the tenant's platform-written Git \
         source with its read-only credentials. Programs and config are unchanged.\n\n- Commit: `{commit_sha}`\n",
        paths.len()
    );
    for p in &paths {
        body.push_str(&format!("- `{p}`\n"));
    }
    if !skipped.is_empty() {
        body.push_str("\nNot migrated (review manually):\n");
        for s in &skipped {
            body.push_str(&format!("- `{}`: {}\n", s.path, s.reason));
        }
    }
    let proposal = open_pr_for_session(&state, &auth, &id, &branch, title, body).await?;
    tracing::info!(
        tenant = %id,
        audit = "stack-source-migration",
        pr_id = %proposal.id,
        stacks = paths.len(),
        "opened tenant Stack source migration PR"
    );
    Ok((
        StatusCode::CREATED,
        Json(MigrateStackSourcesResponse {
            tenant: id,
            dry_run: false,
            migrated: paths,
            skipped,
            proposal: Some(proposal),
            branch: Some(branch),
        }),
    ))
}
