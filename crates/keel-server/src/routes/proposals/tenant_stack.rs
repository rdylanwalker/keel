//! Tenant PKO `Stack` inputs: Pulumi project/backend, platform-written Git source and the
//! tenant's state credentials.

use std::path::Path;

use keel_kube::GITHUB_TOKEN_SECRET_NAME;
use keel_pko::{
    PkoGitAuth, PkoGitSource, PkoStack, attach_github_token_env_ref, attach_s3_credentials_env_refs,
};
use serde_json::Value;

use crate::error::ServerError;
use crate::routes::{RepoReader, read_contained_optional, read_contained_to_string};
use crate::state::AppState;

/// Resolves the Pulumi project name a tenant Stack must use: the `name` of the Pulumi.yaml that
/// is actually committed next to the Stack. That is the blueprint's `Pulumi.yaml`, or, when the
/// blueprint ships none, the generated one named after the instance (`fallback`). The catalog
/// display name is never used. The result is sanitized to Pulumi's project-name charset.
///
/// The read is contained to `repo_workdir`; an escaping symlink is treated as absent.
pub(super) async fn read_pulumi_project_name(
    repo_workdir: &Path,
    pulumi_yaml_path: &Path,
    fallback: &str,
) -> String {
    let raw = read_contained_to_string(repo_workdir, pulumi_yaml_path)
        .await
        .ok();
    pulumi_project_name_from_yaml(raw.as_deref(), fallback)
}

fn pulumi_project_name_from_yaml(raw: Option<&str>, fallback: &str) -> String {
    let declared = raw
        .and_then(|r| noyalib::from_str::<serde_json::Value>(r).ok())
        .and_then(|doc| {
            doc.get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    sanitize_pulumi_project_name(declared.as_deref().unwrap_or(fallback))
}

/// Maps a name to Pulumi's project-name charset (`[A-Za-z0-9_.-]`), collapsing other runs to `-`.
fn sanitize_pulumi_project_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
            out.push(c);
            last_dash = c == '-';
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Returns the tenant-scoped Pulumi DIY backend (`s3://<bucket>/tenants/<id>?...`) so tenant
/// Stack state lands under the prefix the tenant's scoped SeaweedFS identity can access.
pub(super) fn tenant_stack_backend_url(
    state: &AppState,
    tenant: &str,
) -> Result<String, ServerError> {
    keel_kube::tenant_s3_backend_url(&state.backend_url, tenant).map_err(|e| {
        ServerError::Internal(format!(
            "cannot derive tenant state backend for `{tenant}`: {e}"
        ))
    })
}

/// The Git source a tenant's PKO `Stack`s clone (`gitSource`).
#[derive(Debug, Clone)]
pub(super) enum TenantStackSource {
    /// Development-only Local Git provider (D8): the platform-written Flux source in the platform
    /// repository (branch, read-only credentials) plus the URL PKO clones (`spec.projectRepo`, see
    /// [`keel_kube::TenantGitSource::pko_project_repo`]).
    Platform {
        source: keel_kube::TenantGitSource,
        project_repo: String,
    },
    /// The tenant's own repository (URL and transport derived from the platform forge
    /// configuration and confirmed by the merged platform stub), cloned with
    /// `tenant-<id>/keel-gitops-credentials`.
    Dedicated(keel_kube::TenantRepoSource),
}

/// Resolves the Git source a tenant's PKO `Stack`s clone:
///
/// * a tenant with its own repository: the repository the platform forge configuration names
///   ([`crate::tenant_repos::TenantRepoNaming::source`]), accepted only once the platform-written
///   stub `tenants/<id>/git-repository.yaml` on the committed platform `main` points at exactly
///   that repository with `keel-gitops-credentials` (the tenant stub PR is merged);
/// * Local Git provider tenants: the platform-written `tenants/<id>/git-repository.yaml` (never
///   the live, tenant-editable `keel-gitops` object, never the platform `flux-system` source),
///   with the PKO clone URL derived through `AppState::forge_public_url`.
///
/// Fails closed with `409 Conflict` and an actionable message, so no Stack that could only work
/// through whole-repository access (or could never clone) is ever committed.
pub(super) async fn resolve_tenant_stack_source(
    state: &AppState,
    tenant: &str,
) -> Result<TenantStackSource, ServerError> {
    if let Some(forge) = state.tenant_repos.require_forge()? {
        let expected = forge.source(tenant)?;
        let rel = keel_kube::tenant_git_repository_manifest_path(tenant)?;
        let manifest = read_committed_platform_file(state, &rel).await;
        return match crate::tenant_repos::classify_tenant_stub(
            manifest.as_deref(),
            manifest.is_some(),
            &expected,
        ) {
            crate::tenant_repos::TenantStubState::Dedicated => {
                Ok(TenantStackSource::Dedicated(expected))
            }
            other => {
                let detail = match other {
                    crate::tenant_repos::TenantStubState::Foreign(why) => why,
                    _ => "has no merged platform stub".to_string(),
                };
                tracing::warn!(
                    tenant,
                    audit = "stack-source-denied",
                    reason = %detail,
                    "Pulumi Stack proposal refused: tenant repository stub is not merged"
                );
                Err(ServerError::Conflict(format!(
                    "tenant `{tenant}` repository is not ready for Pulumi Stacks (`{rel}` {detail}); merge the tenant stub pull request (POST /api/v1/tenants) first"
                )))
            }
        };
    }
    let resolved = read_tenant_stack_source(state, tenant)
        .await
        .and_then(|source| {
            let project_repo = source.pko_project_repo(state.forge_public_url.as_deref())?;
            Ok(TenantStackSource::Platform {
                source,
                project_repo,
            })
        });
    match resolved {
        Ok(source) => Ok(source),
        Err(keel_kube::KubeError::TenantSourceUnavailable(reason)) => {
            tracing::warn!(
                tenant,
                audit = "stack-source-denied",
                reason = %reason,
                "Pulumi Stack proposal refused: tenant has no usable Git source"
            );
            Err(ServerError::Conflict(reason))
        }
        Err(e) => Err(e.into()),
    }
}

/// Reads `rel` from the committed platform `main` (`origin/main`, no worktree access; falls back
/// to the worktree when the ref is missing). The fallback never waits for `git_lock`: a Local Git
/// provider proposal already holds it (with the worktree synced to `main`) while it resolves the
/// tenant source, and waiting would deadlock.
async fn read_committed_platform_file(state: &AppState, rel: &str) -> Option<String> {
    match RepoReader::committed(state).await {
        Some(reader) => reader.read(rel).await,
        None => {
            let _guard = state.git_lock.try_lock().ok();
            read_contained_optional(&state.repo_workdir, &state.repo_workdir.join(rel))
                .await
                .ok()
                .flatten()
        }
    }
}

/// Reads and validates the platform-written tenant Git source of a Local Git provider tenant
/// (see [`resolve_tenant_stack_source`]) without logging.
pub(super) async fn read_tenant_stack_source(
    state: &AppState,
    tenant: &str,
) -> Result<keel_kube::TenantGitSource, keel_kube::KubeError> {
    let rel = keel_kube::tenant_git_repository_manifest_path(tenant)?;
    let manifest = read_committed_platform_file(state, &rel).await;
    keel_kube::tenant_git_source_from_manifest(tenant, manifest.as_deref())
}

/// PKO `gitSource` for a tenant Stack in `repo_dir`: the tenant repository (tracking `main`,
/// `gitAuth` from `keel-gitops-credentials`), or for Local Git provider tenants the PKO clone URL
/// and branch of the platform-written source with the tenant's read-only `git-credentials`.
pub(super) fn tenant_stack_git_source(
    resolved: &TenantStackSource,
    repo_dir: &str,
) -> Result<PkoGitSource, ServerError> {
    match resolved {
        TenantStackSource::Dedicated(source) => {
            Ok(keel_kube::tenant_repo_stack_git_source(source, repo_dir)?)
        }
        TenantStackSource::Platform {
            source,
            project_repo,
        } => {
            let auth = source
                .secret_name
                .as_deref()
                .map(|secret| {
                    PkoGitAuth::from_flux_secret(
                        secret,
                        project_repo,
                        &std::collections::BTreeSet::new(),
                    )
                })
                .transpose()?;
            Ok(
                PkoGitSource::branch(project_repo, source.branch_or_default(), repo_dir)
                    .with_auth(auth),
            )
        }
    }
}

/// Adds the clone trust of the tenant's own repository to a synthesized tenant `Stack`: the SSH
/// `known_hosts` of `keel-gitops-credentials` for the `fetch` init container (the Keel CA is
/// already trusted). Nothing to add for HTTPS or Local Git provider sources.
pub(super) fn apply_tenant_source_trust(stack: &mut PkoStack, resolved: &TenantStackSource) {
    if let TenantStackSource::Dedicated(source) = resolved
        && source.transport() == keel_kube::TenantRepoTransport::Ssh
        && let Some(tpl) = stack.spec.workspace_template.take()
    {
        stack.spec.workspace_template = Some(keel_kube::tenant_repo_workspace_template(
            tpl,
            keel_kube::TenantRepoTransport::Ssh,
            keel_kube::TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
        ));
    }
}

/// Ensures `tenant_ns` holds the tenant's state credentials before a Pulumi Stack is committed:
///
/// * a mirror of the tenant's own scoped S3 identity, attached to the Stack's `envRefs`;
/// * the tenant state encryption passphrase (`keel-pulumi-passphrase`) the Stack workspace
///   sources `PULUMI_CONFIG_PASSPHRASE` from.
///
/// Fails closed with `409 Conflict` when either is missing (after one lazy provisioning attempt),
/// rather than committing a Stack that can never reach or decrypt its state (and never falling
/// back to the control-plane admin keys or an empty passphrase).
///
/// The shared GitHub token is attached too when it could be mirrored into `tenant_ns`.
pub(super) async fn attach_tenant_stack_credentials(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
) -> Result<(), ServerError> {
    attach_tenant_s3_credentials(state, tenant, tenant_ns, env_refs).await?;
    require_tenant_pulumi_passphrase(state, tenant, tenant_ns).await?;
    attach_mirrored_github_token(state, tenant_ns, env_refs).await;
    Ok(())
}

/// Attaches the shared GitHub token to `env_refs` when it is (or could be) mirrored into
/// `namespace`; a missing token is not an error.
pub(super) async fn attach_mirrored_github_token(
    state: &AppState,
    namespace: &str,
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
) {
    if matches!(
        state
            .secret_store
            .ensure_github_token_in_namespace(namespace)
            .await,
        Ok(true)
    ) {
        attach_github_token_env_ref(env_refs, GITHUB_TOKEN_SECRET_NAME);
    }
}

/// Fails closed (`409`) unless `tenant_ns` holds a usable Pulumi state passphrase. A missing
/// passphrase is provisioned lazily only through the tenant provisioner (which creates it
/// alongside the scoped S3 identity); the server never invents one on the proposal path.
async fn require_tenant_pulumi_passphrase(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
) -> Result<(), ServerError> {
    if !state.require_pulumi_passphrase {
        return Ok(());
    }
    let present = |r: Result<bool, keel_kube::KubeError>| match r {
        Ok(p) => Ok(p),
        Err(e) => {
            tracing::error!(tenant, error = %e, "failed to read tenant Pulumi state passphrase");
            Err(ServerError::Conflict(format!(
                "tenant `{tenant}` Pulumi state passphrase is unreadable; re-run tenant provisioning"
            )))
        }
    };
    if present(state.secret_store.has_pulumi_passphrase(tenant_ns).await)? {
        return Ok(());
    }
    if provision_missing_tenant_identity(state, tenant).await?
        && present(state.secret_store.has_pulumi_passphrase(tenant_ns).await)?
    {
        return Ok(());
    }
    Err(ServerError::Conflict(format!(
        "tenant `{tenant}` has no Pulumi state passphrase (`{tenant_ns}/{}`, key `{}`); \
         run tenant provisioning before creating Pulumi stacks",
        keel_pko::PULUMI_PASSPHRASE_SECRET_NAME,
        keel_pko::PULUMI_PASSPHRASE_SECRET_KEY
    )))
}

/// Ensures `tenant_ns` holds a mirror of the tenant's own scoped S3 identity and attaches it to
/// the tenant Stack's `envRefs` (`409` when the tenant has no scoped state credentials).
async fn attach_tenant_s3_credentials(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
) -> Result<(), ServerError> {
    let Some(ref s3_secret) = state.s3_credentials_secret else {
        return Ok(());
    };
    match state
        .secret_store
        .ensure_s3_credentials_in_namespace(tenant_ns)
        .await
    {
        Ok(true) => {
            attach_s3_credentials_env_refs(env_refs, s3_secret);
            Ok(())
        }
        Ok(false) => {
            if provision_missing_tenant_identity(state, tenant).await?
                && let Ok(true) = state
                    .secret_store
                    .ensure_s3_credentials_in_namespace(tenant_ns)
                    .await
            {
                attach_s3_credentials_env_refs(env_refs, s3_secret);
                return Ok(());
            }
            Err(ServerError::Conflict(format!(
                "tenant `{tenant}` has no scoped state credentials \
                 (`{tenant_ns}/{}`); run tenant provisioning before creating Pulumi stacks",
                keel_kube::TENANT_S3_CREDENTIALS_SECRET_NAME
            )))
        }
        Err(e) => {
            tracing::error!(tenant, error = %e, "failed to mirror tenant scoped state credentials");
            Err(ServerError::Conflict(format!(
                "tenant `{tenant}` scoped state credentials are unusable; re-run tenant provisioning"
            )))
        }
    }
}

/// Lazily provisions a tenant's scoped S3 identity when a provisioner is configured (e.g. the
/// tenant was onboarded through the API before its namespace existed). Returns `Ok(true)` when an
/// identity is now in place.
async fn provision_missing_tenant_identity(
    state: &AppState,
    tenant: &str,
) -> Result<bool, ServerError> {
    let Some(provisioner) = state.tenant_s3_provisioner.as_ref() else {
        return Ok(false);
    };
    match provisioner.provision_tenant_s3_identity(tenant).await {
        Ok(keel_kube::TenantS3ProvisionOutcome::NamespaceMissing) => Ok(false),
        Ok(outcome) => {
            tracing::info!(
                tenant,
                ?outcome,
                "provisioned tenant scoped S3 state identity"
            );
            Ok(true)
        }
        Err(e) => {
            tracing::error!(tenant, error = %e, "tenant scoped S3 identity provisioning failed");
            Err(ServerError::ServiceUnavailable(format!(
                "could not provision scoped state credentials for tenant `{tenant}`; retry later"
            )))
        }
    }
}

pub(super) fn populate_stack_config_from_values(
    config: &mut std::collections::BTreeMap<String, String>,
    values: &Value,
) {
    let Some(obj) = values.as_object() else {
        return;
    };
    for (key, val) in obj {
        let rendered = match val {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(_) | Value::Object(_) => val.to_string(),
        };
        config.insert(key.clone(), rendered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulumi_project_name_matches_committed_pulumi_yaml_never_display_name() {
        // Blueprint ships a Pulumi.yaml: its `name` wins.
        assert_eq!(
            pulumi_project_name_from_yaml(Some("name: kind-cluster\nruntime: yaml\n"), "kc1"),
            "kind-cluster"
        );
        // No blueprint Pulumi.yaml: the generated one is named after the instance.
        assert_eq!(pulumi_project_name_from_yaml(None, "kc1"), "kc1");
        assert_eq!(
            pulumi_project_name_from_yaml(Some("runtime: yaml\n"), "kc1"),
            "kc1"
        );
        assert_eq!(
            pulumi_project_name_from_yaml(Some(": not yaml ["), "kc1"),
            "kc1"
        );
        // Names outside Pulumi's charset are sanitized (no spaces in `organization/<p>/<s>`).
        assert_eq!(
            pulumi_project_name_from_yaml(Some("name: Local kind Cluster\n"), "kc1"),
            "Local-kind-Cluster"
        );
        assert_eq!(sanitize_pulumi_project_name("  a//b__c.d "), "a-b__c.d");
        assert_eq!(sanitize_pulumi_project_name("***"), "project");
    }
}
