//! Merge / reject / reopen of proposals, PR path validation and Stack deletions on merge.

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use keel_core::{tenant_namespace, validate_dns_label};
use keel_git::{
    PullRequestInfo, PullRequestState, parse_proposal_branch_tenant, validate_commit_sha,
};
use keel_pko::GcPlan;
use serde::Serialize;
use serde_json::Value;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::proposal_repo::{ProposalScope, ScopeRepo, ScopedProposal};
use crate::routes::load_provider_registry;
use crate::state::AppState;

use super::pr::audit_proposal_event;

/// Response body of the merge, reject and reopen routes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeProposalResponse {
    pub proposal: ScopedProposal,
    pub gc_plan: GcPlan,
}

/// Rejects path traversal (`..` segments), absolute paths, backslashes, and NUL/control characters
/// so that prefix checks such as `tenants/<id>/` cannot be bypassed.
fn is_safe_repo_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('~')
        && !path.contains('\\')
        && !path.contains("..")
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|seg| seg == ".")
}

/// Validates that a platform-repository PR's changed files are strictly scoped to
/// `tenants/<tenant>/` and only tenant-scoped additions/removals in `tenants/kustomization.yaml`.
///
/// Only reachable with the development-only Local Git provider (D8), whose tenants live in the
/// platform repository (with per-tenant repositories tenant administrators never manage
/// platform-repository PRs: that scope is cluster-admin only). The tenant baseline
/// (`tenants/<tenant>/rbac.yaml` and its `tenants/kustomization.yaml` entry) is always refused:
/// `keel-tenants` applies it with platform privileges.
fn validate_tenant_admin_pr_paths(
    tenant: &str,
    changed_files: &[String],
    kustomization_diff: Option<&str>,
) -> Result<(), ServerError> {
    if changed_files.is_empty() {
        return Err(ServerError::Forbidden(
            "Tenant administrators can only manage PRs with verified tenant-scoped file changes"
                .to_string(),
        ));
    }
    let tenant_prefix = format!("tenants/{tenant}/");
    let rbac_path = format!("{tenant_prefix}rbac.yaml");
    let rbac_entry = format!("{tenant}/rbac.yaml");
    for path in changed_files {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_safe_repo_relative_path(trimmed) {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot manage PRs touching unsafe path '{trimmed}'"
            )));
        }
        if trimmed == rbac_path {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot modify '{rbac_path}'"
            )));
        }
        if trimmed.starts_with(&tenant_prefix) {
            continue;
        }
        if trimmed == "tenants/kustomization.yaml" {
            let diff = kustomization_diff.ok_or_else(|| {
                ServerError::Forbidden(
                    "Unable to verify 'tenants/kustomization.yaml' diff for tenant admin merge"
                        .to_string(),
                )
            })?;
            let kustomize_prefix = format!("{tenant}/");
            for line in diff.lines() {
                if line.starts_with("+++")
                    || line.starts_with("---")
                    || line.starts_with("@@")
                    || line.starts_with("diff ")
                    || line.starts_with("index ")
                {
                    continue;
                }
                if let Some(rest) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
                    let item = rest.trim();
                    if matches!(
                        item,
                        "apiVersion: kustomize.config.k8s.io/v1beta1"
                            | "kind: Kustomization"
                            | "resources:"
                            | ""
                    ) {
                        continue;
                    }
                    let Some(entry) = item.strip_prefix('-').map(str::trim) else {
                        return Err(ServerError::Forbidden(
                            "Tenant administrators may only add/remove resource entries in 'tenants/kustomization.yaml'"
                                .to_string(),
                        ));
                    };
                    if !is_safe_repo_relative_path(entry)
                        || !entry.starts_with(&kustomize_prefix)
                        || entry == rbac_entry
                    {
                        return Err(ServerError::Forbidden(format!(
                            "Tenant administrators cannot modify '{entry}' in 'tenants/kustomization.yaml'"
                        )));
                    }
                }
            }
            continue;
        }
        return Err(ServerError::Forbidden(format!(
            "Tenant administrators cannot manage PRs modifying '{trimmed}'"
        )));
    }
    Ok(())
}

/// Validates that a tenant repository PR only touches the tenant's own directory: every changed
/// path is a safe relative path under `tenants/<tenant>/` and none is a platform stub file
/// (`rbac.yaml`, `git-repository.yaml`, `sync.yaml` directly under it), which only the platform
/// repository may hold.
fn validate_tenant_repo_pr_paths(
    tenant: &str,
    changed_files: &[String],
) -> Result<(), ServerError> {
    if changed_files.is_empty() {
        return Err(ServerError::Forbidden(
            "Tenant administrators can only manage PRs with verified tenant-scoped file changes"
                .to_string(),
        ));
    }
    let tenant_prefix = format!("tenants/{tenant}/");
    for path in changed_files {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_safe_repo_relative_path(trimmed) {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot manage PRs touching unsafe path '{trimmed}'"
            )));
        }
        let Some(rest) = trimmed.strip_prefix(&tenant_prefix) else {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot manage PRs modifying '{trimmed}' (outside '{tenant_prefix}')"
            )));
        };
        if keel_kube::TENANT_REPO_STUB_FILES.contains(&rest) {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot add the platform stub file '{trimmed}' to the tenant repository"
            )));
        }
    }
    Ok(())
}

/// Authorizes `merge_proposal`, `reject_proposal`, and `reopen_proposal`: cluster admins
/// (`keel-admins`) for every scope; tenant admins (`tenant-<id>-admin`) only in their tenant's
/// scope and only for PRs whose verified file changes stay inside the tenant (its own
/// repository: `tenants/<id>/` only; the Local Git provider's platform repository: see
/// [`validate_tenant_admin_pr_paths`]).
///
/// Returns the PR and, for tenant administrators, the exact source-branch head SHA whose file
/// scope was verified, so callers can pin the subsequent forge operation to that commit.
async fn require_pr_tenant_admin_access(
    auth: &AuthSession,
    state: &AppState,
    repo: &ScopeRepo,
    pr_id: &str,
    expected_state: Option<PullRequestState>,
) -> Result<(PullRequestInfo, Option<String>), ServerError> {
    match &repo.scope {
        ProposalScope::Platform => auth.require_admin()?,
        ProposalScope::Tenant(t) => auth.require_tenant_admin(t)?,
    }
    // Direct lookup: paginated list scans could miss older PRs (OPEN-18).
    let pr = repo.get_pull_request(pr_id).await?;
    if auth.is_admin() {
        return Ok((pr, None));
    }
    if let Some(expected) = expected_state
        && pr.state != expected
    {
        return Err(ServerError::NotFound(format!(
            "Pull request '{pr_id}' is not {expected}"
        )));
    }
    let Some(tenant) = repo.tenant() else {
        return Err(ServerError::Forbidden(
            "Administrator privileges required for this operation".to_string(),
        ));
    };
    if pr.target_branch != "main" {
        return Err(ServerError::Forbidden(format!(
            "Tenant administrators can only manage PRs targeting 'main' (PR #{pr_id} targets '{}')",
            pr.target_branch
        )));
    }
    let (head_sha, changed_files, kustomization_diff) = {
        let _guard = repo.lock(state).await;
        if !repo.refresh(state).await.unwrap_or(false) {
            return Err(ServerError::Forbidden(
                "Git workdir unavailable to verify PR file scope for tenant administrator"
                    .to_string(),
            ));
        }
        let dir = repo.workdir.as_path();
        // Resolve the branch head once; the scope check and the merge are both pinned to it so a
        // push between verification and merge cannot smuggle in unverified changes.
        let head_sha = state
            .git_runner
            .rev_parse(dir, &format!("origin/{}", pr.source_branch))
            .await
            .ok()
            .filter(|sha| validate_commit_sha(sha).is_ok())
            .ok_or_else(|| {
                ServerError::Forbidden(format!(
                    "Unable to resolve head commit of PR #{pr_id} for tenant administrator"
                ))
            })?;
        let range = format!("origin/{}...{head_sha}", pr.target_branch);
        let files = state
            .git_runner
            .diff_name_only(dir, &range)
            .await
            .map_err(|_| {
                ServerError::Forbidden(format!(
                    "Unable to verify changed files on PR #{pr_id} for tenant administrator"
                ))
            })?;
        let k_diff =
            if !repo.is_dedicated() && files.iter().any(|f| f == "tenants/kustomization.yaml") {
                state
                    .git_runner
                    .diff_file_zero_context(dir, &range, "tenants/kustomization.yaml")
                    .await
                    .ok()
            } else {
                None
            };
        (head_sha, files, k_diff)
    };

    if repo.is_dedicated() {
        validate_tenant_repo_pr_paths(tenant, &changed_files)?;
    } else {
        // The PR must belong to the tenant (`keel/<id>/...`) in the shared platform repository.
        if parse_proposal_branch_tenant(&pr.source_branch) != Some(tenant) {
            return Err(ServerError::Forbidden(format!(
                "PR #{pr_id} branch '{}' is not scoped to tenant '{tenant}'",
                pr.source_branch
            )));
        }
        validate_tenant_admin_pr_paths(tenant, &changed_files, kustomization_diff.as_deref())?;
    }
    Ok((pr, Some(head_sha)))
}

/// Resolves the head SHA a cluster-admin merge is pinned to: the fetched `origin/<branch>` commit
/// when the repository's working tree is available, else the forge-reported PR head. Returns
/// `None` only when no valid commit id can be determined (the forge then merges its current head).
/// Callers hold [`ScopeRepo::lock`]; also returns whether the working tree was refreshed.
async fn resolve_merge_head_sha(
    state: &AppState,
    repo: &ScopeRepo,
    pr: &PullRequestInfo,
) -> Result<(Option<String>, bool), ServerError> {
    let ready = repo.refresh(state).await.unwrap_or(false);
    if ready
        && let Ok(sha) = state
            .git_runner
            .rev_parse(&repo.workdir, &format!("origin/{}", pr.source_branch))
            .await
        && validate_commit_sha(&sha).is_ok()
    {
        return Ok((Some(sha), ready));
    }
    Ok((
        validate_commit_sha(&pr.head_sha).ok().map(str::to_string),
        ready,
    ))
}

/// `POST /api/v1/tenants/{tenant}/proposals/{pr_id}/merge`
///
/// Merges the PR in the scope's repository, pinned to the verified head commit. A tenant
/// repository merge is picked up by the tenant's `keel-gitops` source (1 minute poll); a
/// platform repository merge additionally applies the provider policy and completes tenant
/// creation/deletion side effects.
pub async fn merge_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((scope, pr_id)): AxumPath<(String, String)>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    let scope = ProposalScope::parse(&scope)?;
    let repo = ScopeRepo::resolve(&state, &scope)?;
    let (pr, verified_sha) =
        require_pr_tenant_admin_access(&auth, &state, &repo, &pr_id, Some(PullRequestState::Open))
            .await?;
    // One refresh per merge: a tenant administrator's verification just fetched the repository
    // (its objects hold the verified head); otherwise refresh once here and reuse that tree for
    // the head commit and the Stack deletion plan.
    let (head_sha, stack_deletions) = {
        let _guard = repo.lock(&state).await;
        let (head_sha, ready) = match verified_sha {
            Some(sha) => (
                Some(sha),
                tokio::fs::try_exists(repo.workdir.join(".git"))
                    .await
                    .unwrap_or(false),
            ),
            None => resolve_merge_head_sha(&state, &repo, &pr).await?,
        };
        // Derive live Stack deletions from the PR's actual diff (removed
        // `tenants/<id>/**/stack.yaml` at the pinned head), never from free-form title text
        // (OPEN-11).
        let deletions = match head_sha.as_deref() {
            Some(sha) if ready => plan_stack_deletions_for_pr(&state, &repo, &pr, sha).await,
            _ => Vec::new(),
        };
        (head_sha, deletions)
    };

    // The forge refuses the merge if the PR head moved after verification.
    let merged = repo
        .forge
        .merge_pull_request(&pr.id, head_sha.as_deref())
        .await?;
    state.proposal_cache.invalidate(&repo.repository().label);
    audit_proposal_event("merge", &auth, &repo, &merged);

    for del in &stack_deletions {
        match state
            .state_provider
            .delete_tenant_stack(&del.namespace, &del.name)
            .await
        {
            Ok(_) => tracing::info!(
                target: "keel::audit",
                action = "stack.delete",
                user = %auth.claims.sub,
                tenant = %del.tenant,
                stack = %del.name,
                pr_id = %merged.id,
                repository = %repo.repository().label,
                "deleted Stack removed by merged proposal"
            ),
            Err(e) => tracing::warn!(
                tenant = %del.tenant,
                stack = %del.name,
                pr_id = %merged.id,
                error = %e,
                "failed to delete Stack removed by merged proposal"
            ),
        }
    }

    if let Some(tenant) = repo.tenant().filter(|_| repo.is_dedicated()) {
        // The next read or write of the tenant repository syncs its clone from the forge.
        state.tenant_repos.mark_stale(tenant);
    } else {
        let _guard = repo.lock(&state).await;
        if tokio::fs::try_exists(repo.workdir.join(".git"))
            .await
            .unwrap_or(false)
        {
            let _ = state.git_runner.fetch(&repo.workdir, "origin").await;
            let _ = state
                .git_runner
                .fast_forward_merge(&repo.workdir, "main", "origin/main")
                .await;
        }
    }

    if !repo.is_dedicated() {
        apply_platform_merge_side_effects(&state, &merged).await;
    }

    let preview_key = repo.preview_key(&merged.id)?;
    let gc_plan = crate::preview_lifecycle::gc_preview_for_pr(&state, &preview_key, "merged").await;

    Ok(Json(MergeProposalResponse {
        proposal: repo.scoped(merged),
        gc_plan,
    }))
}

/// Effects of a merged platform repository PR: the Kyverno provider allowlist follows the
/// provider registry; a merged tenant stub completes the tenant's runtime Secrets; a merged
/// tenant removal revokes the tenant repository's read credentials.
async fn apply_platform_merge_side_effects(state: &AppState, merged: &PullRequestInfo) {
    if let Ok(registry) = load_provider_registry(&state.repo_workdir).await {
        let enabled_ids: Vec<String> = registry
            .providers
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.id)
            .collect();
        if let Ok(policy_val) =
            keel_kube::build_restrict_pulumi_providers_policy_for_registry(&enabled_ids)
        {
            let _ = state
                .state_provider
                .apply_cluster_policy(keel_kube::POLICY_RESTRICT_PULUMI_PROVIDERS, &policy_val)
                .await;
        }
    }

    // A merged tenant stub makes `keel-tenants` create `tenant-<id>`; write its runtime Secrets
    // (`sops-age`, `keel-gitops-credentials`) once it exists.
    if let Some(tenant) = crate::tenant_repos::create_tenant_branch_tenant(&merged.source_branch)
        && keel_core::validate_dns_label(tenant, "tenant").is_ok()
    {
        crate::tenant_repos::spawn_post_merge_completion(
            state.secret_store.clone(),
            state.tenant_repos.clone(),
            tenant.to_string(),
        );
    }

    // A merged tenant removal: no tenant holds a credential for its repository any more.
    if let Some(tenant) = crate::tenant_repos::delete_tenant_branch_tenant(&merged.source_branch)
        && keel_core::validate_dns_label(tenant, "tenant").is_ok()
        && let Some(forge) = state.tenant_repos.forge()
    {
        // Serialized with `repair_tenant_credentials`: a repair waiting on this lock then sees
        // the stub gone from `main` and refuses to mint a credential for the deleted tenant.
        let _cluster_guard = state.tenant_repos.lock(&format!("{tenant}#cluster")).await;
        match crate::tenant_repos::revoke_all_credentials(forge, tenant).await {
            Ok(revoked) => tracing::info!(
                target: "keel::audit",
                action = "tenant.credential.revoke_all",
                tenant,
                revoked,
                "Revoked the deleted tenant's repository read credentials"
            ),
            Err(err) => tracing::warn!(
                tenant,
                error = %err,
                "Could not revoke the deleted tenant's repository read credentials; revoke them on the forge"
            ),
        }
    }
}

/// `POST /api/v1/tenants/{tenant}/proposals/{pr_id}/reject`
///
/// Closes an open PR without merging and garbage-collects any ephemeral PKO preview resources.
pub async fn reject_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((scope, pr_id)): AxumPath<(String, String)>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    let scope = ProposalScope::parse(&scope)?;
    let repo = ScopeRepo::resolve(&state, &scope)?;
    let (pr, _) =
        require_pr_tenant_admin_access(&auth, &state, &repo, &pr_id, Some(PullRequestState::Open))
            .await?;
    let closed = repo.forge.close_pull_request(&pr.id).await?;
    state.proposal_cache.invalidate(&repo.repository().label);
    audit_proposal_event("close", &auth, &repo, &closed);

    let preview_key = repo.preview_key(&closed.id)?;
    let gc_plan =
        crate::preview_lifecycle::gc_preview_for_pr(&state, &preview_key, "rejected").await;

    Ok(Json(MergeProposalResponse {
        proposal: repo.scoped(closed),
        gc_plan,
    }))
}

/// `POST /api/v1/tenants/{tenant}/proposals/{pr_id}/reopen`
///
/// Reopens a previously closed PR proposal in the scope's repository.
pub async fn reopen_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((scope, pr_id)): AxumPath<(String, String)>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    let scope = ProposalScope::parse(&scope)?;
    let repo = ScopeRepo::resolve(&state, &scope)?;
    let (pr, _) = require_pr_tenant_admin_access(
        &auth,
        &state,
        &repo,
        &pr_id,
        Some(PullRequestState::Closed),
    )
    .await?;
    let reopened = repo.forge.reopen_pull_request(&pr.id).await?;
    state.proposal_cache.invalidate(&repo.repository().label);
    audit_proposal_event("reopen", &auth, &repo, &reopened);

    Ok(Json(MergeProposalResponse {
        proposal: repo.scoped(reopened),
        gc_plan: GcPlan::default(),
    }))
}

// ============================================================================
// Git diff, deletion planning, titles & audit helpers (OPEN-2, OPEN-11, OPEN-13, UI-15, CLI-15)
// ============================================================================

/// A PKO `Stack` removed by a PR, derived from the PR's actual diff (never from its title).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StackDeletion {
    pub tenant: String,
    pub namespace: String,
    pub name: String,
}

/// Extracts `(tenant, path)` for deleted `stack.yaml` manifests under `tenants/<tenant>/...`.
fn deleted_stack_manifest_paths(deleted_files: &[String]) -> Vec<(String, String)> {
    deleted_files
        .iter()
        .map(|p| p.trim())
        .filter(|p| is_safe_repo_relative_path(p))
        .filter_map(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            if parts.len() >= 4
                && parts[0] == "tenants"
                && parts.last() == Some(&"stack.yaml")
                && validate_dns_label(parts[1], "tenant").is_ok()
            {
                Some((parts[1].to_string(), p.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Parses a deleted `stack.yaml` (as it existed at the merge-base) into a [`StackDeletion`],
/// requiring `kind: Stack` and a namespace equal to the owning tenant's namespace.
fn parse_deleted_stack_manifest(tenant: &str, raw_yaml: &str) -> Option<StackDeletion> {
    let expected_ns = tenant_namespace(tenant).ok()?;
    let doc: Value = noyalib::from_str(raw_yaml).ok()?;
    if doc.get("kind").and_then(Value::as_str) != Some("Stack") {
        return None;
    }
    let meta = doc.get("metadata")?;
    let name = meta.get("name").and_then(Value::as_str)?.trim();
    validate_dns_label(name, "stack").ok()?;
    let namespace = meta
        .get("namespace")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or(expected_ns.as_str());
    if namespace != expected_ns {
        tracing::warn!(
            tenant,
            namespace,
            stack = name,
            "Ignoring deleted Stack manifest whose namespace does not match its tenant directory"
        );
        return None;
    }
    Some(StackDeletion {
        tenant: tenant.to_string(),
        namespace: expected_ns,
        name: name.to_string(),
    })
}

/// Plans live `Stack` deletions for a PR from its real diff (OPEN-11): every `stack.yaml` removed
/// under `tenants/<id>/` between the merge-base and `head_sha` (in a tenant's own repository only
/// under that tenant's directory). Must be called with the repository lock held after `origin`
/// was fetched.
async fn plan_stack_deletions_for_pr(
    state: &AppState,
    repo: &ScopeRepo,
    pr: &PullRequestInfo,
    head_sha: &str,
) -> Vec<StackDeletion> {
    let target_ref = format!("origin/{}", pr.target_branch);
    let runner = &state.git_runner;
    let dir = &repo.workdir;
    let Ok(deleted) = runner.diff_deleted_files(dir, &target_ref, head_sha).await else {
        return Vec::new();
    };
    let mut candidates = deleted_stack_manifest_paths(&deleted);
    if let Some(tenant) = repo.tenant().filter(|_| repo.is_dedicated()) {
        candidates.retain(|(t, _)| t == tenant);
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let Ok(base) = runner.merge_base(dir, &target_ref, head_sha).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (tenant, path) in candidates {
        if let Ok(Some(raw)) = runner.read_file_at_ref(dir, &base, &path).await
            && let Some(del) = parse_deleted_stack_manifest(&tenant, &raw)
        {
            out.push(del);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_repository_prs_stay_inside_the_tenant_directory() {
        let ok = |files: &[&str]| {
            validate_tenant_repo_pr_paths(
                "acme",
                &files.iter().map(|f| f.to_string()).collect::<Vec<_>>(),
            )
        };
        assert!(
            ok(&[
                "tenants/acme/apps/web/helmrelease.yaml",
                "tenants/acme/kustomization.yaml"
            ])
            .is_ok()
        );
        for bad in [
            &["tenants/acme/rbac.yaml"][..],
            &["tenants/acme/git-repository.yaml"][..],
            &["tenants/acme/sync.yaml"][..],
            &["tenants/beta/apps/x.yaml"][..],
            &["clusters/management/x.yaml"][..],
            &["tenants/acme/../beta/x.yaml"][..],
            &[][..],
        ] {
            assert!(matches!(ok(bad), Err(ServerError::Forbidden(_))), "{bad:?}");
        }
        // A nested file named like a stub is tenant content.
        assert!(ok(&["tenants/acme/apps/x/sync.yaml"]).is_ok());
    }

    #[test]
    fn stack_deletions_derive_from_deleted_manifests_not_titles() {
        let deleted = vec![
            "tenants/acme/infra/db/stack.yaml".to_string(),
            "tenants/acme/infra/db/values.yaml".to_string(),
            "tenants/acme/bundles/web/vm/stack.yaml".to_string(),
            "tenants/../etc/stack.yaml".to_string(),
            "deploy/stack.yaml".to_string(),
            "tenants/stack.yaml".to_string(),
        ];
        assert_eq!(
            deleted_stack_manifest_paths(&deleted),
            vec![
                (
                    "acme".to_string(),
                    "tenants/acme/infra/db/stack.yaml".to_string()
                ),
                (
                    "acme".to_string(),
                    "tenants/acme/bundles/web/vm/stack.yaml".to_string()
                ),
            ]
        );

        let ok = "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: db\n  namespace: tenant-acme\n";
        assert_eq!(
            parse_deleted_stack_manifest("acme", ok),
            Some(StackDeletion {
                tenant: "acme".to_string(),
                namespace: "tenant-acme".to_string(),
                name: "db".to_string(),
            })
        );
        let no_ns = "kind: Stack\nmetadata:\n  name: web-vm\n";
        assert_eq!(
            parse_deleted_stack_manifest("acme", no_ns).map(|d| d.namespace),
            Some("tenant-acme".to_string())
        );
        // Cross-tenant namespace, wrong kind, or invalid names are ignored.
        let foreign = "kind: Stack\nmetadata:\n  name: db\n  namespace: tenant-globex\n";
        assert!(parse_deleted_stack_manifest("acme", foreign).is_none());
        assert!(
            parse_deleted_stack_manifest("acme", "kind: ConfigMap\nmetadata:\n  name: db\n")
                .is_none()
        );
        assert!(
            parse_deleted_stack_manifest("acme", "kind: Stack\nmetadata:\n  name: Bad_Name\n")
                .is_none()
        );
        assert!(parse_deleted_stack_manifest("acme", ": not yaml :").is_none());
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn safe_repo_relative_path_rejects_traversal_absolute_and_backslashes() {
        assert!(is_safe_repo_relative_path(
            "tenants/acme/apps/x/values.yaml"
        ));
        assert!(is_safe_repo_relative_path(
            "acme/apps/x/flux-kustomization.yaml"
        ));
        for bad in [
            "",
            "/etc/passwd",
            "~/x",
            "tenants/acme/../other/rbac.yaml",
            "acme/../other",
            "..",
            "tenants\\acme\\x",
            "tenants/acme/./x",
            "tenants/acme/\u{0}x",
        ] {
            assert!(!is_safe_repo_relative_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn tenant_admin_pr_paths_reject_traversal_and_foreign_entries() {
        assert!(
            validate_tenant_admin_pr_paths(
                "acme",
                &files(&["tenants/acme/apps/redis/values.yaml"]),
                None
            )
            .is_ok()
        );
        assert!(
            validate_tenant_admin_pr_paths(
                "acme",
                &files(&["tenants/acme/../globex/apps/redis/values.yaml"]),
                None
            )
            .is_err()
        );
        assert!(
            validate_tenant_admin_pr_paths("acme", &files(&["tenants/acme/rbac.yaml"]), None)
                .is_err()
        );
        assert!(validate_tenant_admin_pr_paths("acme", &files(&["deploy/x.yaml"]), None).is_err());
        assert!(validate_tenant_admin_pr_paths("acme", &[], None).is_err());

        let k = files(&["tenants/kustomization.yaml"]);
        let ok_diff = "--- a/tenants/kustomization.yaml\n+++ b/tenants/kustomization.yaml\n@@ -1 +1 @@\n+  - acme/apps/redis/flux-kustomization.yaml\n";
        assert!(validate_tenant_admin_pr_paths("acme", &k, Some(ok_diff)).is_ok());
        for bad_entry in [
            "acme/../globex/rbac.yaml",
            "/acme/x.yaml",
            "acme\\..\\globex",
            "globex/apps/x.yaml",
        ] {
            let diff = format!("+  - {bad_entry}\n");
            assert!(
                validate_tenant_admin_pr_paths("acme", &k, Some(&diff)).is_err(),
                "{bad_entry:?}"
            );
        }
        assert!(validate_tenant_admin_pr_paths("acme", &k, None).is_err());
    }

    #[test]
    fn tenant_admin_pr_paths_refuse_the_platform_owned_stub() {
        // The tenant baseline and its kustomization entry are refused.
        let k = files(&["tenants/kustomization.yaml"]);
        assert!(
            validate_tenant_admin_pr_paths("acme", &files(&["tenants/acme/rbac.yaml"]), None)
                .is_err()
        );
        for sign in ['+', '-'] {
            let diff = format!("{sign}  - acme/rbac.yaml\n");
            assert!(
                validate_tenant_admin_pr_paths("acme", &k, Some(&diff)).is_err(),
                "{sign}"
            );
        }
    }
}
