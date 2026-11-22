//! Cluster-side inputs of the ephemeral PKO preview (`SDD.md` §4.2, OPEN-9).

use keel_core::KEEL_PREVIEWS_NAMESPACE;
use keel_pko::{
    EphemeralPreviewParams, PkoGitAuth, PkoStack, PreviewSecretCopy, attach_s3_credentials_env_refs,
};
use serde_json::Value;

use crate::error::ServerError;
use crate::proposal_repo::ScopeRepo;
use crate::state::AppState;

use super::secrets::render_secret_string_data;
use super::tenant_stack::{attach_mirrored_github_token, read_tenant_stack_source};

/// Secrets the preview runner already mirrors with tenant-scoped values (state backend, forge
/// token); tenant Stack refs to them are never copied into a preview.
fn preview_runner_managed_secrets(state: &AppState) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::from([
        keel_kube::S3_CREDENTIALS_SECRET_NAME.to_string(),
        keel_kube::GITHUB_TOKEN_SECRET_NAME.to_string(),
        keel_kube::TENANT_S3_CREDENTIALS_SECRET_NAME.to_string(),
    ]);
    if let Some(name) = &state.s3_credentials_secret {
        out.insert(name.clone());
    }
    out
}

/// Plans provider-credential Secret copies for a tenant preview and rewrites its refs. The PR's
/// own `<instance>-secrets` values (not yet decrypted into the tenant namespace by Flux for a
/// brand-new stack) are attached as `proposed_data` so the preview evaluates the PR.
pub(super) fn plan_preview_credentials(
    state: &AppState,
    params: &mut EphemeralPreviewParams,
    tenant_stack: &PkoStack,
    instance_secret_name: &str,
    proposed_secret_values: &Value,
) -> Result<Vec<PreviewSecretCopy>, ServerError> {
    let mut copies =
        params.inherit_tenant_credentials(tenant_stack, &preview_runner_managed_secrets(state))?;
    let proposed = render_secret_string_data(proposed_secret_values);
    for copy in &mut copies {
        if copy.source_name == instance_secret_name && !proposed.is_empty() {
            copy.proposed_data = Some(
                proposed
                    .iter()
                    .filter(|(k, _)| copy.keys.contains(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            );
        }
    }
    Ok(copies)
}

/// Resolves cluster-side preview inputs for `tenant` (OPEN-9): the tenant's **scoped** S3 state
/// identity mirrored into `keel-previews` (never the control-plane admin keys) and the tenant's
/// cluster-reachable Flux Git source (`tenant-<id>/keel-gitops` only, never the platform
/// `flux-system` source) + read-only credentials. Returns a human-readable reason when the PKO
/// preview cannot run (rendered as `notExecuted`); a missing tenant source fails closed.
pub(super) async fn attach_preview_cluster_inputs(
    state: &AppState,
    repo: &ScopeRepo,
    tenant: &str,
    params: &mut EphemeralPreviewParams,
    secret_copies: &mut Vec<PreviewSecretCopy>,
) -> Option<String> {
    if let Some(forge) = repo.tenant_forge() {
        return attach_tenant_repo_preview_inputs(state, forge, tenant, params, secret_copies)
            .await;
    }
    let Some(cluster) = state.preview_cluster.clone() else {
        return Some("No live cluster connected: PKO preview not executed".to_string());
    };
    let prereqs = match cluster.ensure_preview_prerequisites(tenant).await {
        Ok(p) => p,
        // Fail closed with the actionable (secret-free) reason: no tenant-owned Git source.
        Err(keel_kube::KubeError::PreviewSourceUnavailable(reason)) => {
            tracing::warn!(
                tenant,
                audit = "preview-source-denied",
                pr_id = %params.pr_id,
                reason = %reason,
                "PKO preview refused: tenant has no usable Git source"
            );
            return Some(format!("{reason}: PKO preview not executed"));
        }
        Err(e) => {
            tracing::warn!(tenant, error = %e, "preview prerequisites unavailable");
            return Some(format!(
                "Preview prerequisites unavailable for tenant `{tenant}`: PKO preview not executed"
            ));
        }
    };
    // Previews read only the tenant's own Flux source; never fall back to the control-plane
    // remote (which grants whole-repository read) when the cluster reports none.
    let Some(url) = prereqs.git_url else {
        tracing::warn!(
            tenant,
            audit = "preview-source-denied",
            pr_id = %params.pr_id,
            "PKO preview refused: tenant has no Git source"
        );
        return Some(format!(
            "Tenant `{tenant}` has no Flux Git source (`tenant-{tenant}/keel-gitops`); previews never fall back to the platform repository: PKO preview not executed"
        ));
    };
    // The preview clones the same platform-written source as the tenant Stack it previews
    // (`gitSource.projectRepo` committed by keel-server) or, for a brand-new/unparsable Stack, the
    // platform-written `tenants/<id>/git-repository.yaml` on `origin/main`; never the live,
    // tenant-editable `keel-gitops` `spec.url` (the cluster source only supplies the read-only
    // credentials) and never the control-plane remote.
    let forge_public_url = state.forge_public_url.as_deref();
    let platform_url = match params.tenant_repo_url.clone() {
        // Stacks committed before PKO clone URLs were separated from Flux URLs may still carry the
        // in-cluster `http://` Forgejo Service: preview them through the HTTPS origin too.
        Some(u) => keel_pko::pko_project_repo_url(&u, forge_public_url).unwrap_or(u),
        None => match read_tenant_stack_source(state, tenant)
            .await
            .and_then(|source| source.pko_project_repo(forge_public_url))
        {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!(
                    tenant,
                    audit = "preview-source-denied",
                    pr_id = %params.pr_id,
                    "PKO preview refused: tenant has no platform-written Git source"
                );
                let reason = match e {
                    keel_kube::KubeError::TenantSourceUnavailable(reason) => reason,
                    _ => format!("Tenant `{tenant}` Git source could not be resolved"),
                };
                return Some(format!("{reason}: PKO preview not executed"));
            }
        },
    };
    if let Err(e) = keel_pko::validate_git_source_url(&platform_url) {
        tracing::warn!(tenant, pr_id = %params.pr_id, error = %e, "preview Git source URL refused");
        return Some(format!(
            "Tenant `{tenant}` Git source cannot be cloned by PKO ({e}): PKO preview not executed"
        ));
    }
    // `url` is the live Flux URL; compare the clone URL PKO would derive from it.
    if keel_pko::pko_project_repo_url(&url, forge_public_url)
        .ok()
        .as_deref()
        != Some(platform_url.as_str())
    {
        tracing::warn!(
            tenant,
            audit = "preview-source-mismatch",
            pr_id = %params.pr_id,
            "live tenant Git source URL differs from the platform-written source; the preview clones the platform-written one"
        );
    }
    // Each preview gets its own copy of only the allow-listed read-only Git keys, owned by the
    // preview Stack (GC'd with it); no long-lived per-tenant credential in `keel-previews`.
    let git_copy = match prereqs.git_secret.as_deref() {
        Some(source_secret) => {
            let target = match keel_pko::preview_secret_copy_name(&params.pr_id, source_secret) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(tenant, pr_id = %params.pr_id, error = %e, "preview Git credential copy name refused");
                    return Some(format!(
                        "Tenant `{tenant}` read-only Git credentials cannot be staged for the preview: PKO preview not executed"
                    ));
                }
            };
            // An unusable credential (e.g. an SSH source with a token-only Secret) must not fail
            // the proposal after its PR was opened: report the preview as not executed instead.
            if let Err(e) =
                PkoGitAuth::from_flux_secret(&target, &platform_url, &prereqs.git_secret_keys)
            {
                tracing::warn!(tenant, pr_id = %params.pr_id, error = %e, "preview Git credentials unusable");
                return Some(format!(
                    "Tenant `{tenant}` read-only Git credentials cannot authenticate its Git source ({e}): PKO preview not executed"
                ));
            }
            Some(PreviewSecretCopy {
                source_name: source_secret.to_string(),
                target_name: target,
                keys: prereqs.git_secret_keys.clone(),
                proposed_data: None,
            })
        }
        None => None,
    };
    params.repo_url = Some(platform_url);
    params.git_secret_name = git_copy.as_ref().map(|c| c.target_name.clone());
    params.git_secret_keys = prereqs.git_secret_keys;
    if let Some(copy) = git_copy {
        secret_copies.retain(|c| c.target_name != copy.target_name);
        secret_copies.push(copy);
    }
    attach_preview_runtime_inputs(state, tenant, params, prereqs.s3_secret).await
}

/// Preview inputs of a tenant with its own repository: the preview `Stack` clones the tenant
/// repository (URL from the platform forge configuration, never the tenant-editable
/// `keel-gitops` object) at exactly the PR head commit (`gitSource.commit`), authenticated with a
/// per-preview copy of the tenant's own read credential (`keel-gitops-credentials`: HTTPS
/// `username`/`password` or SSH `sshPrivateKey`/`known_hosts`), copied before the `Stack` is
/// applied and owned by it ([`crate::preview_lifecycle::launch_pko_preview`]). Returns the reason
/// when the preview cannot run (rendered as `notExecuted`; never a platform credential).
async fn attach_tenant_repo_preview_inputs(
    state: &AppState,
    forge: &crate::tenant_repos::TenantRepoForge,
    tenant: &str,
    params: &mut EphemeralPreviewParams,
    secret_copies: &mut Vec<PreviewSecretCopy>,
) -> Option<String> {
    let Some(cluster) = state.preview_cluster.clone() else {
        return Some("No live cluster connected: PKO preview not executed".to_string());
    };
    let source = match forge.source(tenant) {
        Ok(source) => source,
        Err(e) => {
            tracing::warn!(tenant, pr_id = %params.pr_id, error = %e, "tenant repository source refused");
            return Some(format!(
                "Tenant `{tenant}` repository cannot be resolved: PKO preview not executed"
            ));
        }
    };
    let Some(commit) = params
        .head_sha
        .clone()
        .filter(|c| keel_kube::is_full_commit_sha(c))
    else {
        return Some(
            "The pull request head commit is unknown: PKO preview not executed".to_string(),
        );
    };
    let prereqs = match cluster
        .ensure_tenant_repo_preview_prerequisites(&source)
        .await
    {
        Ok(p) => p,
        Err(keel_kube::KubeError::PreviewSourceUnavailable(reason)) => {
            tracing::warn!(
                tenant,
                audit = "preview-source-denied",
                pr_id = %params.pr_id,
                reason = %reason,
                "PKO preview refused: tenant repository credential unusable"
            );
            return Some(format!("{reason}: PKO preview not executed"));
        }
        Err(e) => {
            tracing::warn!(tenant, error = %e, "preview prerequisites unavailable");
            return Some(format!(
                "Preview prerequisites unavailable for tenant `{tenant}`: PKO preview not executed"
            ));
        }
    };
    let copy_name = match keel_pko::preview_secret_copy_name(
        &params.pr_id,
        keel_kube::TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
    ) {
        Ok(name) => name,
        Err(e) => {
            tracing::warn!(tenant, pr_id = %params.pr_id, error = %e, "preview Git credential copy name refused");
            return Some(format!(
                "Tenant `{tenant}` repository credential cannot be staged for the preview: PKO preview not executed"
            ));
        }
    };
    let git_source = match keel_kube::tenant_repo_preview_git_source(
        &source,
        &commit,
        &params.repo_dir,
        &copy_name,
    ) {
        Ok(git) => git,
        Err(e) => {
            tracing::warn!(tenant, pr_id = %params.pr_id, error = %e, "preview Git source refused");
            return Some(format!(
                "Tenant `{tenant}` repository cannot be cloned by PKO ({e}): PKO preview not executed"
            ));
        }
    };
    params.git_source = Some(git_source);
    params.repo_url = Some(source.url().to_string());
    params.git_secret_name = Some(copy_name.clone());
    params.git_secret_keys = prereqs.git_secret_keys.clone();
    secret_copies.retain(|c| c.target_name != copy_name);
    secret_copies.push(PreviewSecretCopy {
        source_name: keel_kube::TENANT_GITOPS_CREDENTIALS_SECRET_NAME.to_string(),
        target_name: copy_name,
        keys: prereqs.git_secret_keys,
        proposed_data: None,
    });
    attach_preview_runtime_inputs(state, tenant, params, prereqs.s3_secret).await
}

/// Shared final step of both preview-input paths: the tenant's scoped S3 state identity (S3
/// backends only; fails closed when the control plane runs with S3 credentials but the tenant has
/// none), the mirrored GitHub token and the Docker proxy. Returns the reason when the preview
/// cannot run.
async fn attach_preview_runtime_inputs(
    state: &AppState,
    tenant: &str,
    params: &mut EphemeralPreviewParams,
    s3_secret: Option<String>,
) -> Option<String> {
    if params.backend_url.starts_with("s3://") {
        match s3_secret {
            Some(secret) => attach_s3_credentials_env_refs(&mut params.env_refs, &secret),
            None if state.s3_credentials_secret.is_some() => {
                return Some(format!(
                    "Tenant `{tenant}` has no scoped state credentials: PKO preview not executed"
                ));
            }
            None => {}
        }
    }
    attach_mirrored_github_token(state, KEEL_PREVIEWS_NAMESPACE, &mut params.env_refs).await;
    params.docker_host = state.docker_proxy_url.clone();
    None
}

/// Adds the SSH `known_hosts` of the per-preview credential copy to a preview `Stack` that clones
/// a tenant repository over SSH (the Keel CA is already trusted).
pub(super) fn apply_preview_source_trust(stack: &mut PkoStack, params: &EphemeralPreviewParams) {
    let ssh = params
        .git_source
        .as_ref()
        .is_some_and(|g| keel_pko::is_ssh_git_url(&g.project_repo));
    if let (true, Some(secret)) = (ssh, params.git_secret_name.as_deref())
        && let Some(tpl) = stack.spec.workspace_template.take()
    {
        stack.spec.workspace_template = Some(keel_kube::tenant_repo_workspace_template(
            tpl,
            keel_kube::TenantRepoTransport::Ssh,
            secret,
        ));
    }
}
