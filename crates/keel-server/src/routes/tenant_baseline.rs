//! `POST /api/v1/tenants/{id}/baseline/migrate` (`SDD.md` §6.2 "Platform-owned tenant objects").
//!
//! Kyverno `protect-platform-owned-tenant-objects` only protects objects labelled
//! `keel.io/platform-owned`. Tenants created before the label existed (through the API, or by an
//! earlier `keel bootstrap init`) have unlabelled `tenants/<id>/rbac.yaml` (and, for
//! bootstrap-created tenants, `tenants/<id>/git-repository.yaml`) objects, so their tenant admins
//! can still edit or delete them (e.g. `default-deny-all`). This endpoint opens **one** PR that adds
//! the label to those platform-written files only, changing nothing else. It never commits to
//! `main`; `keel-tenants` applies the label once the PR is merged.

use std::path::Path;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use keel_core::validate_dns_label;
use keel_git::PullRequestInfo;
use keel_kube::{
    PLATFORM_OWNED_TENANT_FILES, ensure_tenant_pko_workspace_network_policy,
    label_platform_owned_manifests, platform_owned_labels_missing,
};
use keel_pko::build_preview_branch;
use serde::{Deserialize, Serialize};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;

use super::{
    next_unique_branch_timestamp, open_pr_for_session, read_contained_optional,
    stage_commit_push_and_return_to_main, write_contained,
};

/// Query parameters of `POST /api/v1/tenants/{id}/baseline/migrate`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MigrateBaselineQuery {
    /// Report what would be labelled without creating a branch or PR.
    pub dry_run: bool,
}

/// Response body of `POST /api/v1/tenants/{id}/baseline/migrate`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateBaselineResponse {
    pub tenant: String,
    pub dry_run: bool,
    /// Repository paths whose platform-written objects gain `keel.io/platform-owned`.
    pub labelled: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<PullRequestInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Start-up audit: tenant ids (sorted) under `<root>/tenants/` whose platform-written baseline
/// files ([`PLATFORM_OWNED_TENANT_FILES`]) have objects without `keel.io/platform-owned`, logged
/// with `audit="platform-owned-baseline"`. Their baseline is not protected by Kyverno until
/// `keel tenant migrate-baseline --tenant <id>` is merged. Only tenant ids are logged.
pub async fn report_unlabelled_tenant_baselines(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(root.join("tenants")).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = tokio::fs::symlink_metadata(entry.path()).await else {
            continue;
        };
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !meta.is_dir() || validate_dns_label(&id, "tenant").is_err() {
            continue;
        }
        let mut missing = false;
        for file in PLATFORM_OWNED_TENANT_FILES {
            let path = entry.path().join(file);
            if let Ok(Some(content)) = read_contained_optional(root, &path).await
                && platform_owned_labels_missing(&content)
            {
                missing = true;
            }
        }
        if missing {
            out.push(id);
        }
    }
    out.sort();
    for id in &out {
        tracing::warn!(
            tenant = %id,
            audit = "platform-owned-baseline",
            "tenant baseline objects lack keel.io/platform-owned, so Kyverno does not protect them from tenant edits; run `keel tenant migrate-baseline --tenant {id}` and merge the PR"
        );
    }
    out
}

/// `POST /api/v1/tenants/{id}/baseline/migrate[?dryRun=true]` (platform admin only).
///
/// Opens one PR on `keel/<id>/migrate-baseline-<ts>` that adds `keel.io/platform-owned: "true"` to
/// the platform-written objects of `tenants/<id>/rbac.yaml` and `tenants/<id>/git-repository.yaml`
/// (`keel_kube::label_platform_owned_manifests`; other documents stay byte-for-byte) and appends
/// the `allow-pko-workspace` NetworkPolicy to `rbac.yaml` when no baseline file defines it
/// (`keel_kube::ensure_tenant_pko_workspace_network_policy`). Returns
/// `201` with the PR, `200` when every object is already labelled (or `dryRun`), and `404` when
/// the tenant has no `tenants/<id>/rbac.yaml`.
pub async fn migrate_tenant_baseline(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<MigrateBaselineQuery>,
) -> Result<(StatusCode, Json<MigrateBaselineResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "tenant")?;

    let guard = state.git_lock.lock().await;
    state
        .git_runner
        .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
        .await?;
    let root = state.repo_workdir.clone();
    // (path, current content, changed)
    let mut files: Vec<(String, String, bool)> = Vec::new();
    for file in PLATFORM_OWNED_TENANT_FILES {
        let rel = format!("tenants/{id}/{file}");
        let Some(content) = read_contained_optional(&root, &root.join(&rel)).await? else {
            continue;
        };
        match label_platform_owned_manifests(&content)? {
            Some(updated) => files.push((rel, updated, true)),
            None => files.push((rel, content, false)),
        }
    }
    let rbac_rel = format!("tenants/{id}/rbac.yaml");
    let Some(rbac_index) = files.iter().position(|(rel, _, _)| *rel == rbac_rel) else {
        drop(guard);
        return Err(ServerError::NotFound(format!(
            "tenant `{id}` has no tenants/{id}/rbac.yaml"
        )));
    };
    // Tenants created by earlier releases lack `allow-pko-workspace`, so `default-deny-all` blocks
    // their PKO workspace pods; add it to the baseline in the same PR.
    let others: Vec<&str> = files
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != rbac_index)
        .map(|(_, (_, content, _))| content.as_str())
        .collect();
    if let Some(updated) =
        ensure_tenant_pko_workspace_network_policy(&id, &files[rbac_index].1, &others)?
    {
        files[rbac_index].1 = updated;
        files[rbac_index].2 = true;
    }
    let labelled: Vec<(String, String)> = files
        .into_iter()
        .filter(|(_, _, changed)| *changed)
        .map(|(rel, content, _)| (rel, content))
        .collect();
    let paths: Vec<String> = labelled.iter().map(|(p, _)| p.clone()).collect();
    if labelled.is_empty() || query.dry_run {
        drop(guard);
        return Ok((
            StatusCode::OK,
            Json(MigrateBaselineResponse {
                tenant: id,
                dry_run: query.dry_run,
                labelled: paths,
                proposal: None,
                branch: None,
            }),
        ));
    }

    let branch = build_preview_branch(&id, "migrate-baseline", next_unique_branch_timestamp())?;
    state
        .git_runner
        .checkout_new_branch(&state.repo_workdir, &branch)
        .await?;
    for (rel, yaml) in &labelled {
        if let Err(e) = write_contained(&root, &root.join(rel), yaml).await {
            let _ = state
                .git_runner
                .checkout_branch(&state.repo_workdir, "main")
                .await;
            return Err(e);
        }
    }
    let title = format!("fix({id}): protect the platform-owned tenant baseline");
    let commit_sha =
        stage_commit_push_and_return_to_main(&state, &auth, &branch, &title, &paths).await?;
    drop(guard);

    let mut body = format!(
        "Labels the platform-written objects of tenant `{id}` with `keel.io/platform-owned: \"true\"` \
         so Kyverno `protect-platform-owned-tenant-objects` reserves changes to them for the \
         platform (keel-tenant-reconciler / keel-server), and adds the tenant's \
         `allow-pko-workspace` NetworkPolicy to `rbac.yaml` when it is missing (PKO workspace \
         pods need it through `default-deny-all`). Nothing else changes.\n\n- Commit: `{commit_sha}`\n"
    );
    for p in &paths {
        body.push_str(&format!("- `{p}`\n"));
    }
    let proposal = open_pr_for_session(&state, &auth, &id, &branch, title, body).await?;
    tracing::info!(
        tenant = %id,
        audit = "platform-owned-baseline-migration",
        pr_id = %proposal.id,
        files = paths.len(),
        "opened tenant baseline labelling PR"
    );
    Ok((
        StatusCode::CREATED,
        Json(MigrateBaselineResponse {
            tenant: id,
            dry_run: false,
            labelled: paths,
            proposal: Some(proposal),
            branch: Some(branch),
        }),
    ))
}
