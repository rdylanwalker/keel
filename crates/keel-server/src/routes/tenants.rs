//! Provider registry and tenant onboarding/decommissioning endpoints (`SDD.md` §7.3).

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use chrono::Utc;
use keel_core::{tenant_namespace, validate_dns_label};
use keel_git::{BranchProtectionOutcome, CreatePrRequest, ForkPolicyOutcome, PullRequestInfo};
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, TenantIsolationConfig, TenantManifestBundle, WatchedCrdKind,
};
use keel_pko::build_preview_branch;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;
use crate::tenant_repos::{self, ClusterProvisioning, TenantRepoForge, TenantStubState};

use super::{
    commit_author_for_session, exists_contained, load_provider_registry, open_pr_for_session,
    pr_author_for_session, read_contained_optional, remove_dir_all_contained,
    serialize_tenants_kustomization, stage_commit_push_and_return_to_main, write_contained,
};

/// Summary of an infrastructure provider returned by `GET /api/v1/providers` and `PUT /api/v1/providers/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatusEntry {
    pub id: String,
    pub enabled: bool,
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Response body for `GET /api/v1/providers`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderStatusEntry>,
}

/// Request body for `PUT /api/v1/providers/{id}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleProviderRequest {
    pub enabled: bool,
}

/// Response body for `PUT /api/v1/providers/{id}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleProviderResponse {
    pub provider: ProviderStatusEntry,
    pub proposal: crate::proposal_repo::ScopedProposal,
    pub branch: String,
}

/// `GET /api/v1/providers`
///
/// Reads `providers/registry.yaml` (falling back to `ProviderRegistry::default_day1()`).
pub async fn list_providers(
    _auth: AuthSession,
    State(state): State<AppState>,
) -> Result<Json<ProvidersResponse>, ServerError> {
    let registry = load_provider_registry(&state.repo_workdir).await?;
    let providers = registry
        .providers
        .into_iter()
        .map(|p| ProviderStatusEntry {
            id: p.id,
            enabled: p.enabled,
            package: p.plugin,
            description: p.description,
        })
        .collect();
    Ok(Json(ProvidersResponse { providers }))
}

/// `PUT /api/v1/providers/{id}`
///
/// Validates DNS-1123 `id`, toggles `enabled` in `providers/registry.yaml` (`404` if missing),
/// commits on `keel/providers/toggle-<id>-<ts>`, pushes, and opens a PR via `ForgeProvider`.
pub async fn toggle_provider(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<ToggleProviderRequest>,
) -> Result<Json<ToggleProviderResponse>, ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;

    let mut registry = load_provider_registry(&state.repo_workdir).await?;
    let existing = registry
        .providers
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ServerError::NotFound(format!("Provider '{id}' not found in registry")))?;
    if existing.enabled == req.enabled {
        return Err(ServerError::Conflict(format!(
            "Provider '{id}' is already {} on main",
            if req.enabled { "enabled" } else { "disabled" }
        )));
    }
    registry.set_enabled(&id, req.enabled);
    let updated_yaml = registry.to_yaml()?;

    let updated_entry = registry
        .providers
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ServerError::NotFound(format!("Provider '{id}' not found in registry")))?;
    let provider = ProviderStatusEntry {
        id: updated_entry.id,
        enabled: updated_entry.enabled,
        package: updated_entry.plugin,
        description: updated_entry.description,
    };

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch("providers", &format!("toggle-{id}"), timestamp)?;
    let rel_file = "providers/registry.yaml";
    let action_verb = if req.enabled { "enable" } else { "disable" };

    let commit_sha = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        let providers_dir = state.repo_workdir.join("providers");
        tokio::fs::create_dir_all(&providers_dir).await?;
        tokio::fs::write(providers_dir.join("registry.yaml"), &updated_yaml).await?;

        let commit_msg = format!("feat(providers): {action_verb} provider {id}");
        stage_commit_push_and_return_to_main(&state, &auth, &branch, &commit_msg, [rel_file])
            .await?
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        "providers",
        &branch,
        format!("feat(providers): {action_verb} provider {id}"),
        format!(
            "Automated provider registry update for `{id}` (`enabled={}`)\n\n- Commit: `{commit_sha}`\n- Path: `{rel_file}`",
            req.enabled
        ),
    )
    .await?;

    Ok(Json(ToggleProviderResponse {
        provider,
        proposal,
        branch,
    }))
}

/// Summary of a tenant directory returned by `GET /api/v1/tenants`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantSummary {
    pub id: String,
    pub namespace: String,
}

/// Response body for `GET /api/v1/tenants`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantsResponse {
    pub tenants: Vec<TenantSummary>,
}

/// `GET /api/v1/tenants`
///
/// Scans `tenants/*` directories in `state.repo_workdir`, returning deterministic sorted order.
pub async fn list_tenants(
    auth: AuthSession,
    State(state): State<AppState>,
) -> Result<Json<TenantsResponse>, ServerError> {
    let mut tenants = Vec::new();
    let tenants_dir = state.repo_workdir.join("tenants");
    if let Ok(mut entries) = tokio::fs::read_dir(&tenants_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !auth.is_admin() && auth.require_tenant_access(&id).is_err() {
                continue;
            }
            let namespace = tenant_namespace(&id).unwrap_or_else(|_| format!("tenant-{id}"));
            tenants.push(TenantSummary { id, namespace });
        }
    }
    tenants.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(TenantsResponse { tenants }))
}

/// Request body for `POST /api/v1/tenants`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTenantRequest {
    pub id: String,
    /// Adopt an existing tenant repository that already holds content other than Keel's
    /// skeleton (for example the repository of a deleted tenant with the same id). Without it
    /// such a repository is refused with `409`, so a reused id never silently inherits old
    /// content.
    #[serde(default)]
    pub adopt_existing: bool,
}

/// Where a tenant's GitOps content lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TenantRepositoryLayout {
    /// The tenant's own forge repository (every remote forge).
    Dedicated,
    /// Inside the platform repository: development-only Local Git provider, no isolation (D8).
    Monorepo,
}

/// Provisioning state of a tenant's repository and runtime Secrets (`POST /api/v1/tenants`).
/// Never contains credentials.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantRepositoryStatus {
    pub layout: TenantRepositoryLayout,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Clone URL Flux (`keel-gitops`) and PKO use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// `https` (repository-scoped read token) or `ssh` (read-only deploy key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// `true` when this request created the repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_protection: Option<BranchProtectionOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forks: Option<ForkPolicyOutcome>,
    /// PR on the tenant repository adding `tenants/<id>/kustomization.yaml` (`None` once on
    /// `main`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skeleton_proposal: Option<crate::proposal_repo::ScopedProposal>,
    /// Runtime Secrets (`sops-age`, `keel-gitops-credentials`), written once `tenant-<id>` exists.
    pub cluster: ClusterProvisioning,
    pub notes: Vec<String>,
}

/// Response body for `POST /api/v1/tenants`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTenantResponse {
    pub tenant: TenantSummary,
    /// Platform-repository PR adding the tenant stub; `null` when the stub is already on `main`
    /// (re-run of a provisioned tenant).
    pub proposal: Option<crate::proposal_repo::ScopedProposal>,
    pub branch: Option<String>,
    pub repository: TenantRepositoryStatus,
}

/// Validates a new tenant id (DNS-1123, not reserved).
fn validate_new_tenant_id(id: &str) -> Result<String, ServerError> {
    validate_dns_label(id, "id")?;
    // `tenant-<id>-admin` is the tenant-admin IdP group; an id ending in `-admin` would make the
    // member group of `<x>-admin` collide with the admin group of `<x>` (OPEN-12).
    if id.ends_with("-admin") {
        return Err(ServerError::BadRequest(
            "Tenant id must not end with '-admin' (reserved for the tenant-<id>-admin group)"
                .to_string(),
        ));
    }
    // `management` is the pseudo-tenant of the management control plane (`GET /api/v1/stacks`,
    // `GET /api/v1/releases?tenant=management`) and must never name a real tenant.
    if id == crate::state::MANAGEMENT_TENANT {
        return Err(ServerError::BadRequest(format!(
            "Tenant id '{id}' is reserved for the management control plane"
        )));
    }
    Ok(tenant_namespace(id)?)
}

/// Best-effort tenant Pulumi state passphrase and scoped S3 identity (deferred until the
/// namespace exists; proposal paths also provision them lazily).
async fn ensure_tenant_state_identities(state: &AppState, tenant: &str, namespace: &str) {
    if let Err(err) = state.secret_store.ensure_pulumi_passphrase(namespace).await {
        tracing::debug!(
            tenant,
            error = %err,
            "Deferred tenant Pulumi state passphrase until tenant namespace exists in cluster"
        );
    }
    if let Some(provisioner) = state.tenant_s3_provisioner.as_ref() {
        match provisioner.provision_tenant_s3_identity(tenant).await {
            Ok(outcome) => tracing::info!(
                tenant,
                ?outcome,
                "Tenant scoped S3 state identity provisioning"
            ),
            Err(err) => tracing::warn!(
                tenant,
                error = %err,
                "Deferred tenant scoped S3 state identity provisioning"
            ),
        }
    }
}

/// Reads `tenants/<id>/git-repository.yaml` from the freshly synced platform `main` and whether
/// `tenants/<id>/` exists. Callers hold `git_lock`.
async fn read_platform_tenant_stub(
    state: &AppState,
    tenant: &str,
) -> Result<(Option<String>, bool), ServerError> {
    let dir = state.repo_workdir.join("tenants").join(tenant);
    let dir_exists = exists_contained(&state.repo_workdir, &dir).await?;
    let manifest =
        read_contained_optional(&state.repo_workdir, &dir.join("git-repository.yaml")).await?;
    Ok((manifest, dir_exists))
}

/// Open PRs of an earlier, unmerged `action` for `tenant` (`keel/<id>/<action>-<ts>`), oldest
/// first. Callers verify a candidate's content before resuming it: anyone who can push a branch
/// could open a PR with a matching name.
async fn open_proposal_candidates(
    forge: &dyn keel_git::ForgeProvider,
    tenant: &str,
    action: &str,
) -> Result<Vec<PullRequestInfo>, ServerError> {
    let prefix = format!("keel/{tenant}/{action}-");
    let mut prs: Vec<PullRequestInfo> = forge
        .list_open_pull_requests(Some(tenant))
        .await?
        .into_iter()
        .filter(|pr| {
            pr.source_branch.starts_with(&prefix)
                && pr.target_branch == "main"
                && keel_git::validate_git_ref(&pr.source_branch, "branch").is_ok()
        })
        .collect();
    prs.sort_by_key(|pr| pr.id.parse::<u64>().unwrap_or(u64::MAX));
    Ok(prs)
}

/// `true` when `origin/<branch>` of the clone at `dir` changes exactly the paths in `expected`
/// relative to `origin/main`, each to exactly the given content (`None`: checked by the caller).
async fn proposal_matches(
    git: &keel_git::GitRunner,
    dir: &std::path::Path,
    branch: &str,
    expected: &[(String, Option<String>)],
) -> Result<Option<String>, ServerError> {
    let Ok(head) = git.rev_parse(dir, &format!("origin/{branch}")).await else {
        return Ok(None);
    };
    let Ok(mut changed) = git
        .diff_name_only(dir, &format!("origin/main...{head}"))
        .await
    else {
        return Ok(None);
    };
    changed.sort();
    let mut wanted: Vec<String> = expected.iter().map(|(p, _)| p.clone()).collect();
    wanted.sort();
    if changed != wanted {
        return Ok(None);
    }
    for (path, content) in expected {
        if let Some(content) = content
            && git.read_file_at_ref(dir, &head, path).await?.as_deref() != Some(content.as_str())
        {
            return Ok(None);
        }
    }
    Ok(Some(head))
}

/// `resources` of a `kustomization.yaml` (empty when missing or unparsable).
fn kustomization_resources(yaml: Option<&str>) -> Vec<String> {
    yaml.and_then(|y| noyalib::from_str::<Value>(y).ok())
        .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The first open platform PR that is exactly the stub Keel would propose for `tenant` now (the
/// synthesized stub files plus the `tenants/kustomization.yaml` entries, nothing else). Other
/// candidates are reported in `notes` and never resumed. Callers hold `git_lock` with `main`
/// synced.
async fn find_resumable_stub_proposal(
    state: &AppState,
    tenant: &str,
    stub: &keel_kube::TenantRepoStub,
    notes: &mut Vec<String>,
) -> Result<Option<PullRequestInfo>, ServerError> {
    let candidates = open_proposal_candidates(
        state.forge.as_ref(),
        tenant,
        tenant_repos::CREATE_TENANT_ACTION,
    )
    .await?;
    if candidates.is_empty() {
        return Ok(None);
    }
    let _ = state.git_runner.fetch(&state.repo_workdir, "origin").await;
    let main_resources = kustomization_resources(
        read_contained_optional(
            &state.repo_workdir,
            &state.repo_workdir.join("tenants/kustomization.yaml"),
        )
        .await?
        .as_deref(),
    );
    let mut expected: Vec<(String, Option<String>)> = stub
        .files
        .iter()
        .map(|(p, y)| (p.clone(), Some(y.clone())))
        .collect();
    expected.push(("tenants/kustomization.yaml".to_string(), None));
    let mut wanted_resources = main_resources;
    for entry in stub.kustomization_entries() {
        if !wanted_resources.contains(&entry) {
            wanted_resources.push(entry);
        }
    }
    wanted_resources.sort();
    for pr in candidates {
        if let Some(head) = proposal_matches(
            &state.git_runner,
            &state.repo_workdir,
            &pr.source_branch,
            &expected,
        )
        .await?
        {
            let mut resources = kustomization_resources(
                state
                    .git_runner
                    .read_file_at_ref(&state.repo_workdir, &head, "tenants/kustomization.yaml")
                    .await?
                    .as_deref(),
            );
            resources.sort();
            if resources == wanted_resources {
                return Ok(Some(pr));
            }
        }
        notes.push(format!(
            "Ignored open pull request #{} (`{}`): its changes are not the tenant stub Keel generates; close it",
            pr.id, pr.source_branch
        ));
    }
    Ok(None)
}

/// Paths on the tenant repository's `main` other than the forge's auto-init files and Keel's
/// empty skeleton (`tenants/<id>/kustomization.yaml` without resources).
async fn foreign_tenant_repo_content(
    dir: &std::path::Path,
    tenant: &str,
) -> Result<Vec<String>, ServerError> {
    const AUTO_INIT: [&str; 4] = ["README.md", "README", "LICENSE", ".gitignore"];
    let skeleton = format!("tenants/{tenant}/kustomization.yaml");
    let mut foreign = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, rel)) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&path).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if rel.is_empty() && name == ".git" {
                continue;
            }
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if entry.file_type().await?.is_dir() {
                stack.push((entry.path(), child));
            } else if rel.is_empty() && AUTO_INIT.contains(&name.as_str()) {
                continue;
            } else if child == skeleton {
                let yaml = tokio::fs::read_to_string(entry.path()).await.ok();
                if !kustomization_resources(yaml.as_deref()).is_empty() {
                    foreign.push(child);
                }
            } else {
                foreign.push(child);
            }
        }
    }
    foreign.sort();
    Ok(foreign)
}

/// Adds `tenants/<id>/kustomization.yaml` (no resources yet) to the tenant repository through a
/// PR, so the tenant's `keel-tenant-root` Kustomization has a path to apply before the first
/// app arrives. Reuses an open skeleton PR; `None` once the file is on `main`.
///
/// With `refuse_foreign_content` (a fresh creation that found an existing repository), `main`
/// may only hold the forge's auto-init files and Keel's empty skeleton; anything else is `409`
/// so a reused tenant id never silently inherits a previous tenant's apps and Stacks.
async fn ensure_tenant_repo_skeleton(
    state: &AppState,
    auth: &AuthSession,
    forge: &TenantRepoForge,
    tenant: &str,
    refuse_foreign_content: bool,
    notes: &mut Vec<String>,
) -> Result<Option<PullRequestInfo>, ServerError> {
    let _repo_guard = state.tenant_repos.lock(tenant).await;
    let platform_remote = state
        .git_runner
        .get_remote_url(&state.repo_workdir, "origin")
        .await?;
    let dir = state
        .tenant_repos
        .sync_checkout(&state.git_runner, &platform_remote, tenant)
        .await?;
    if refuse_foreign_content {
        let foreign = foreign_tenant_repo_content(&dir, tenant).await?;
        if !foreign.is_empty() {
            let shown: Vec<&str> = foreign.iter().take(5).map(String::as_str).collect();
            return Err(ServerError::Conflict(format!(
                "tenant `{tenant}` repository `{}` already exists with content ({}{}), for example from a deleted tenant with the same id; archive or empty it, or re-run with `adoptExisting: true` to adopt that content",
                tenant_repos::target_label(forge, tenant),
                shown.join(", "),
                if foreign.len() > shown.len() {
                    ", ..."
                } else {
                    ""
                }
            )));
        }
    }
    let rel = format!("tenants/{tenant}/kustomization.yaml");
    if exists_contained(&dir, &dir.join(&rel)).await? {
        return Ok(None);
    }
    let yaml = keel_kube::build_tenant_repo_kustomization_yaml(tenant, &[])?;
    keel_kube::ensure_git_safe_manifests(&rel, &yaml)?;
    let provider = forge.repository_provider(tenant)?;
    let candidates = open_proposal_candidates(
        provider.as_ref(),
        tenant,
        tenant_repos::INIT_TENANT_REPO_ACTION,
    )
    .await?;
    if !candidates.is_empty() {
        let _ = state.git_runner.fetch(&dir, "origin").await;
    }
    let expected = [(rel.clone(), Some(yaml.clone()))];
    for pr in candidates {
        if proposal_matches(&state.git_runner, &dir, &pr.source_branch, &expected)
            .await?
            .is_some()
        {
            return Ok(Some(pr));
        }
        notes.push(format!(
            "Ignored open tenant repository pull request #{} (`{}`): its changes are not the skeleton Keel generates; close it",
            pr.id, pr.source_branch
        ));
    }
    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch(tenant, tenant_repos::INIT_TENANT_REPO_ACTION, timestamp)?;
    let commit = async {
        state.git_runner.checkout_new_branch(&dir, &branch).await?;
        tokio::fs::create_dir_all(dir.join(format!("tenants/{tenant}"))).await?;
        tokio::fs::write(dir.join(&rel), &yaml).await?;
        state.git_runner.add(&dir, [rel.as_str()]).await?;
        let sha = state
            .git_runner
            .commit(
                &dir,
                &format!("feat({tenant}): initialise tenant repository"),
                &commit_author_for_session(auth),
            )
            .await?;
        state.git_runner.push(&dir, "origin", &branch).await?;
        Ok::<String, ServerError>(sha)
    }
    .await;
    let _ = state.git_runner.checkout_branch(&dir, "main").await;
    let sha = commit?;
    let req = CreatePrRequest::new(
        format!("feat({tenant}): initialise tenant repository"),
        format!(
            "Automated tenant repository skeleton for `{tenant}`\n\n- Commit: `{sha}`\n- Path: `{rel}`\n\nMerge it before the tenant stub pull request in the platform repository, so the tenant's `keel-tenant-root` Kustomization has a path to apply."
        ),
        &branch,
    )
    .with_tenant(tenant)
    .with_author(pr_author_for_session(auth));
    let pr = provider.create_pull_request(req).await?;
    state.proposal_cache.invalidate(&format!("tenant-{tenant}"));
    Ok(Some(pr))
}

/// Writes the platform-repository tenant files on a new proposal branch, commits, pushes and
/// opens the PR. Callers hold `git_lock`, with `main` synced.
async fn open_platform_tenant_proposal(
    state: &AppState,
    auth: &AuthSession,
    tenant: &str,
    namespace: &str,
    files: Vec<(String, String)>,
    kustomization_entries: Vec<String>,
    body_note: Option<String>,
) -> Result<(crate::proposal_repo::ScopedProposal, String, String), ServerError> {
    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch(tenant, tenant_repos::CREATE_TENANT_ACTION, timestamp)?;
    for (path, yaml) in &files {
        keel_kube::ensure_git_safe_manifests(path, yaml)?;
    }
    state
        .git_runner
        .checkout_new_branch(&state.repo_workdir, &branch)
        .await?;
    let written = async {
        for (path, yaml) in &files {
            let target = state.repo_workdir.join(path);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&target, yaml).await?;
        }
        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        let existing = tokio::fs::read_to_string(&tenants_kustomization_path)
            .await
            .unwrap_or_default();
        let mut resources: Vec<String> = noyalib::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        for entry in kustomization_entries {
            if !resources.contains(&entry) {
                resources.push(entry);
            }
        }
        tokio::fs::write(
            &tenants_kustomization_path,
            serialize_tenants_kustomization(&resources)?,
        )
        .await?;
        Ok::<(), ServerError>(())
    }
    .await;
    if let Err(err) = written {
        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        return Err(err);
    }
    let mut staged: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
    staged.push("tenants/kustomization.yaml".to_string());
    let commit_msg = format!("feat({tenant}): create tenant {tenant}");
    let commit_sha =
        stage_commit_push_and_return_to_main(state, auth, &branch, &commit_msg, &staged).await?;
    let proposal = open_pr_for_session(
        state,
        auth,
        tenant,
        &branch,
        commit_msg,
        format!(
            "Automated tenant onboarding for `{tenant}` (namespace `{namespace}`)\n\n- Commit: `{commit_sha}`\n- Paths: {}{}",
            staged
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", "),
            body_note.map(|n| format!("\n\n{n}")).unwrap_or_default()
        ),
    )
    .await?;
    Ok((proposal, branch, commit_sha))
}

/// `POST /api/v1/tenants`
///
/// Creates (or resumes creating) a tenant. Idempotent: re-running it for a half-provisioned or
/// provisioned tenant reuses the repository, credential and open PRs.
///
/// Dedicated repositories (every remote forge; `503` when the forge is configured but unusable):
/// 1. checks that `age-keygen` works (the tenant `sops-age` key is mandatory, fail closed);
/// 2. ensures the private tenant repository `<owner>/<monorepo>-tenant-<id>` (`auto_init`,
///    generic description), `main` protection and the fork policy, removing foreign deploy keys;
///    an existing repository with content beyond Keel's skeleton is refused (`409`) on a fresh
///    creation unless `adoptExisting` is set, and every read credential left on a repository
///    whose tenant does not exist (a deleted tenant with the same id) is revoked;
/// 3. opens (or reuses) a tenant-repository PR adding `tenants/<id>/kustomization.yaml`;
/// 4. opens (or reuses, when its content is exactly the generated stub) the platform-repository
///    PR adding the platform-owned stub (`rbac.yaml`, `git-repository.yaml`, `sync.yaml`;
///    `201`), unless it is on `main` (`200`);
/// 5. writes the runtime Secrets (`sops-age`, `keel-gitops-credentials`) when `tenant-<id>`
///    already exists; otherwise they are written after the stub PR merges (merge hook) or by a
///    re-run.
///
/// The tenant repository PR must merge before the platform stub PR (otherwise
/// `keel-tenant-root` reports a missing path until it does). A rejected stub PR leaves an empty
/// tenant repository behind; a later create reuses it.
///
/// Local Git provider (D8, development only): the tenant lives in the platform repository
/// (`tenants/<id>/rbac.yaml` only, no isolation); duplicates are `409`.
pub async fn create_tenant(
    auth: AuthSession,
    State(state): State<AppState>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<(StatusCode, Json<CreateTenantResponse>), ServerError> {
    auth.require_admin()?;
    let namespace = validate_new_tenant_id(&req.id)?;
    let tenant = req.id;

    let Some(forge) = state.tenant_repos.require_forge()?.cloned() else {
        return create_monorepo_tenant(auth, state, tenant, namespace).await;
    };

    let source = forge.source(&tenant)?;
    let stub =
        keel_kube::synthesize_tenant_repo_stub(&TenantIsolationConfig::new(&tenant), &source)?;
    let mut notes = Vec::new();
    // One creation of a given tenant at a time (the repository clone has its own lock).
    let _create_guard = state.tenant_repos.lock(&format!("{tenant}#create")).await;
    // Mandatory SOPS key: fail closed before touching the forge when it cannot be generated.
    tenant_repos::generate_tenant_age_key(&tenant).await?;

    let (stub_state, open_proposal) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        let (manifest, dir_exists) = read_platform_tenant_stub(&state, &tenant).await?;
        let stub_state =
            tenant_repos::classify_tenant_stub(manifest.as_deref(), dir_exists, &source);
        let open = match stub_state {
            TenantStubState::Absent => {
                find_resumable_stub_proposal(&state, &tenant, &stub, &mut notes).await?
            }
            _ => None,
        };
        (stub_state, open)
    };
    if let TenantStubState::Foreign(why) = &stub_state {
        return Err(ServerError::Conflict(format!(
            "Tenant '{tenant}' already exists and is not a dedicated-repository tenant ({why})"
        )));
    }

    let forge_state = tenant_repos::provision_forge(&forge, &tenant).await?;
    let fresh_creation = stub_state == TenantStubState::Absent && open_proposal.is_none();
    let adopted_existing = fresh_creation
        && forge_state.repository.outcome != keel_git::RepositoryEnsureOutcome::Created;
    if stub_state == TenantStubState::Absent
        && !state.secret_store.namespace_exists(&namespace).await?
    {
        // No tenant can hold a credential for this repository yet: anything left over (for
        // example from a deleted tenant with the same id) is revoked.
        let revoked = tenant_repos::revoke_all_credentials(&forge, &tenant).await?;
        if revoked > 0 {
            notes.push(format!(
                "Revoked {revoked} read credential(s) left on the existing tenant repository"
            ));
        }
    }
    // The skeleton PR lives in the tenant repository (route scope: the tenant).
    let skeleton_proposal = ensure_tenant_repo_skeleton(
        &state,
        &auth,
        &forge,
        &tenant,
        adopted_existing && !req.adopt_existing,
        &mut notes,
    )
    .await?
    .map(|pr| crate::proposal_repo::ScopeRepo::for_tenant(&state, &tenant).map(|r| r.scoped(pr)))
    .transpose()?;
    if adopted_existing {
        notes.push(format!(
            "The tenant repository `{}` already existed and was adopted{}",
            forge_state.repository.target(),
            if req.adopt_existing {
                " with its existing content (`adoptExisting`)"
            } else {
                " (it held no content beyond Keel's skeleton)"
            }
        ));
    }

    if forge_state.main_protection == BranchProtectionOutcome::UnsupportedPlan {
        notes.push(
            "The forge plan does not offer branch protection for this private repository: direct pushes to `main` are not blocked by the forge".to_string(),
        );
    }
    if let ForkPolicyOutcome::RequiresAdminSetting(hint) = &forge_state.forks {
        notes.push(format!("Forks are governed by a forge setting: {hint}"));
    }
    if forge_state.removed_deploy_keys > 0 {
        notes.push(format!(
            "Removed {} deploy key(s) Keel did not add from the tenant repository",
            forge_state.removed_deploy_keys
        ));
    }
    let skeleton_order_hint = |skeleton: &crate::proposal_repo::ScopedProposal| {
        format!(
            "Merge tenant repository pull request #{} in `{}`{} first (`POST /api/v1/tenants/{tenant}/proposals/{}/merge`; its number belongs to the tenant repository, not to the platform repository), then the platform stub pull request (scope `management`); until then `keel-tenant-root` reports a missing path",
            skeleton.id,
            forge_state.repository.target(),
            if skeleton.url.is_empty() {
                String::new()
            } else {
                format!(" ({})", skeleton.url)
            },
            skeleton.id
        )
    };
    if let Some(skeleton) = &skeleton_proposal {
        notes.push(skeleton_order_hint(skeleton));
    }

    let (status, proposal, branch) = match (&stub_state, open_proposal) {
        (TenantStubState::Dedicated, _) => (StatusCode::OK, None, None),
        (_, Some(pr)) => {
            notes.push("Resumed the open tenant creation proposal".to_string());
            let branch = pr.source_branch.clone();
            (
                StatusCode::OK,
                Some(crate::proposal_repo::ScopeRepo::platform(&state).scoped(pr)),
                Some(branch),
            )
        }
        _ => {
            let entries = stub.kustomization_entries();
            let body_note = skeleton_proposal.as_ref().map(skeleton_order_hint);
            let (proposal, branch, commit_sha) = {
                let _guard = state.git_lock.lock().await;
                state
                    .git_runner
                    .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
                    .await?;
                open_platform_tenant_proposal(
                    &state,
                    &auth,
                    &tenant,
                    &namespace,
                    stub.files.clone(),
                    entries,
                    body_note,
                )
                .await?
            };
            state
                .proposal_cache
                .invalidate(keel_pko::PLATFORM_REPO_LABEL);
            state.telemetry.publish(CrdWatchEvent::new(
                WatchedCrdKind::Kustomization,
                &namespace,
                format!("tenant-{tenant}"),
                CrdReconcileStatus::Progressing,
                format!(
                    "Proposed tenant '{tenant}' creation in PR #{} ({branch})",
                    proposal.id
                ),
                Some(commit_sha),
            ));
            (StatusCode::CREATED, Some(proposal), Some(branch))
        }
    };

    let cluster = tenant_repos::complete_cluster_provisioning(
        state.secret_store.as_ref(),
        &state.tenant_repos,
        &tenant,
    )
    .await?;
    if !cluster.namespace_ready {
        notes.push(format!(
            "`{namespace}` does not exist yet: its `sops-age` and `keel-gitops-credentials` Secrets are written once the tenant stub PR is merged (re-run this request if they are still missing)"
        ));
    }
    ensure_tenant_state_identities(&state, &tenant, &namespace).await;
    // A Secret this request (re)wrote may unblock Stacks PKO stalled on it (ADDON-15).
    crate::stack_recovery::recover_tenant_stalled_stacks(&state, &tenant).await;

    tracing::info!(
        target: "keel::audit",
        action = "tenant.create",
        user = %auth.claims.sub,
        tenant = %tenant,
        repository = %forge_state.repository.target(),
        adopted_existing,
        "Tenant repository provisioned"
    );

    Ok((
        status,
        Json(CreateTenantResponse {
            tenant: TenantSummary {
                id: tenant,
                namespace,
            },
            proposal,
            branch,
            repository: TenantRepositoryStatus {
                layout: TenantRepositoryLayout::Dedicated,
                owner: Some(forge_state.repository.owner.clone()),
                name: Some(forge_state.repository.name.clone()),
                url: Some(source.url().to_string()),
                web_url: forge_state.repository.html_url.clone(),
                transport: Some(source.transport().as_str().to_string()),
                created: Some(
                    forge_state.repository.outcome == keel_git::RepositoryEnsureOutcome::Created,
                ),
                main_protection: Some(forge_state.main_protection),
                forks: Some(forge_state.forks),
                skeleton_proposal,
                cluster,
                notes,
            },
        }),
    ))
}

/// Local Git provider (D8): the tenant lives inside the platform repository.
async fn create_monorepo_tenant(
    auth: AuthSession,
    state: AppState,
    tenant: String,
    namespace: String,
) -> Result<(StatusCode, Json<CreateTenantResponse>), ServerError> {
    tenant_repos::generate_tenant_age_key(&tenant).await?;
    let rel_file = format!("tenants/{tenant}/rbac.yaml");
    let rbac_yaml = TenantManifestBundle::synthesize(&TenantIsolationConfig::new(&tenant))?
        .to_multi_doc_yaml()?;
    let (proposal, branch, commit_sha) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        if exists_contained(
            &state.repo_workdir,
            &state.repo_workdir.join("tenants").join(&tenant),
        )
        .await?
        {
            return Err(ServerError::Conflict(format!(
                "Tenant '{tenant}' already exists"
            )));
        }
        open_platform_tenant_proposal(
            &state,
            &auth,
            &tenant,
            &namespace,
            vec![(rel_file, rbac_yaml)],
            vec![format!("{tenant}/rbac.yaml")],
            None,
        )
        .await?
    };
    state.telemetry.publish(CrdWatchEvent::new(
        WatchedCrdKind::Kustomization,
        &namespace,
        format!("tenant-{tenant}"),
        CrdReconcileStatus::Progressing,
        format!(
            "Proposed tenant '{tenant}' creation in PR #{} ({branch})",
            proposal.id
        ),
        Some(commit_sha),
    ));
    let cluster = tenant_repos::complete_cluster_provisioning(
        state.secret_store.as_ref(),
        &state.tenant_repos,
        &tenant,
    )
    .await?;
    ensure_tenant_state_identities(&state, &tenant, &namespace).await;
    // A Secret this request (re)wrote may unblock Stacks PKO stalled on it (ADDON-15).
    crate::stack_recovery::recover_tenant_stalled_stacks(&state, &tenant).await;
    Ok((
        StatusCode::CREATED,
        Json(CreateTenantResponse {
            tenant: TenantSummary {
                id: tenant,
                namespace,
            },
            proposal: Some(proposal),
            branch: Some(branch),
            repository: TenantRepositoryStatus {
                layout: TenantRepositoryLayout::Monorepo,
                owner: None,
                name: None,
                url: None,
                web_url: None,
                transport: None,
                created: None,
                main_protection: None,
                forks: None,
                skeleton_proposal: None,
                cluster,
                notes: vec![
                    "Local Git provider (development only): the tenant lives in the platform repository and has no repository isolation".to_string(),
                ],
            },
        }),
    ))
}

/// Response body for `DELETE /api/v1/tenants/{id}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTenantResponse {
    pub id: String,
    pub proposal: crate::proposal_repo::ScopedProposal,
    pub branch: String,
    /// Operator follow-ups (dedicated repositories: credential revocation, archiving).
    pub notes: Vec<String>,
}

/// `DELETE /api/v1/tenants/{id}`
///
/// Rejects deleting `"default"`, validates `id`, ensures `tenants/<id>` exists,
/// removes `tenants/<id>` on proposal branch `keel/<id>/delete-tenant-<ts>`,
/// commits, pushes, and opens a PR via `ForgeProvider`. For a tenant with a dedicated
/// repository this removes the platform stub; merging the PR through Keel then revokes every
/// read credential of the tenant repository (merge hook). The repository itself is kept (Keel
/// never deletes Git history): an operator archives or deletes it on the forge, and re-creating
/// the same id refuses to adopt its content unless asked to.
pub async fn delete_tenant(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DeleteTenantResponse>, ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;
    if id == "default" {
        return Err(ServerError::BadRequest(
            "Cannot delete the 'default' tenant".to_string(),
        ));
    }
    let mut notes = Vec::new();
    if let Some(forge) = state.tenant_repos.require_forge()? {
        notes.push(format!(
            "Merging this proposal through Keel revokes every read credential of the tenant repository `{}`; if it is merged on the forge instead, revoke them there",
            tenant_repos::target_label(forge, &id)
        ));
        notes.push(format!(
            "The tenant repository `{}` is kept: archive or delete it on the forge once the tenant is gone",
            tenant_repos::target_label(forge, &id)
        ));
    }

    let rel_dir = format!("tenants/{id}");
    let tenant_dir = state.repo_workdir.join(&rel_dir);

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch(&id, tenant_repos::DELETE_TENANT_ACTION, timestamp)?;

    let commit_sha = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        if !exists_contained(&state.repo_workdir, &tenant_dir).await? {
            return Err(ServerError::NotFound(format!("Tenant '{id}' not found")));
        }
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        remove_dir_all_contained(&state.repo_workdir, &tenant_dir).await?;
        let mut staged_paths = vec![rel_dir.clone()];

        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        if let Some(existing) =
            read_contained_optional(&state.repo_workdir, &tenants_kustomization_path).await?
        {
            let resources: Vec<String> = noyalib::from_str::<Value>(&existing)
                .ok()
                .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .filter(|r| !r.starts_with(&format!("{id}/")))
                .collect();
            write_contained(
                &state.repo_workdir,
                &tenants_kustomization_path,
                serialize_tenants_kustomization(&resources)?,
            )
            .await?;
            staged_paths.push("tenants/kustomization.yaml".to_string());
        }

        let commit_msg = format!("chore({id}): delete tenant {id}");
        stage_commit_push_and_return_to_main(&state, &auth, &branch, &commit_msg, &staged_paths)
            .await?
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        &id,
        &branch,
        format!("chore({id}): delete tenant {id}"),
        format!(
            "Automated tenant decommission proposal for `{id}`\n\n- Commit: `{commit_sha}`\n- Removed: `{rel_dir}`"
        ),
    )
    .await?;

    Ok(Json(DeleteTenantResponse {
        id,
        proposal,
        branch,
        notes,
    }))
}

/// Response body for `POST /api/v1/tenants/{id}/credentials/repair`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairTenantCredentialsResponse {
    pub id: String,
    /// Tenant repository (`<owner>/<name>`); the credential works on it only.
    pub repository: String,
    #[serde(flatten)]
    pub cluster: ClusterProvisioning,
}

/// The tenant's platform stub on the platform repository's freshly fetched `main`.
async fn committed_platform_stub_state(
    state: &AppState,
    tenant: &str,
) -> Result<TenantStubState, ServerError> {
    let Some(forge) = state.tenant_repos.forge() else {
        return Ok(TenantStubState::Absent);
    };
    let source = forge.source(tenant)?;
    let _guard = state.git_lock.lock().await;
    state
        .git_runner
        .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
        .await?;
    let (manifest, dir_exists) = read_platform_tenant_stub(state, tenant).await?;
    Ok(tenant_repos::classify_tenant_stub(
        manifest.as_deref(),
        dir_exists,
        &source,
    ))
}

/// `POST /api/v1/tenants/{id}/credentials/repair` (platform admin only).
///
/// Re-mints the tenant's repository read credential and rewrites the cluster-only
/// `tenant-<id>/keel-gitops-credentials` Secret, revoking the previous credential and every other
/// read credential of the repository; a missing `sops-age` Secret is created (never replaced).
/// Flux and PKO pick the new credential up on their next pull. Normal operations (lost, leaked or
/// expired credentials), not a migration. Secret values are never logged or returned.
pub async fn repair_tenant_credentials(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RepairTenantCredentialsResponse>, ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;
    let repository = match state.tenant_repos.require_forge()? {
        Some(forge) => tenant_repos::target_label(forge, &id),
        None => String::new(),
    };
    let cluster = tenant_repos::repair_tenant_credentials(
        state.secret_store.as_ref(),
        &state.tenant_repos,
        &id,
        || committed_platform_stub_state(&state, &id),
    )
    .await?;
    // PKO never retries a Stack stalled on the missing Secret on its own (ADDON-15).
    let stalled_stacks_nudged =
        crate::stack_recovery::recover_tenant_stalled_stacks(&state, &id).await;
    tracing::info!(
        target: "keel::audit",
        action = "tenant.credential.repair",
        user = %auth.claims.sub,
        tenant = %id,
        credential = ?cluster.credential,
        sops_age = ?cluster.sops_age,
        stalled_stacks_nudged,
        "Repaired tenant runtime credentials"
    );
    Ok(Json(RepairTenantCredentialsResponse {
        id,
        repository,
        cluster,
    }))
}
