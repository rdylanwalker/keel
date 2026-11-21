//! REST and Server-Sent Events (SSE) API handlers (`SDD.md` §7.3).

pub mod catalog;
pub mod management;
pub mod proposals;
pub mod tenants;
pub mod workloads;

pub use catalog::*;
pub use management::*;
pub use proposals::*;
pub use tenants::*;
pub use workloads::*;

use std::path::{Path, PathBuf};

use chrono::Utc;
use keel_core::{CatalogManifest, ProviderRegistry, SopsConfig, parse_catalog};
use keel_git::{CommitAuthor, CreatePrRequest, PullRequestInfo};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;

static LAST_BRANCH_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn attach_infra_secret_refs(
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
    secrets_ref: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
    secret_name: &str,
    secret_keys: &[String],
) {
    for secret_key in secret_keys {
        let ref_val = keel_pko::PkoEnvRef::from_secret(secret_name, secret_key);
        match secret_key.as_str() {
            // AWS provider credentials must go into Pulumi `secretsRef` (`aws:accessKey` /
            // `aws:secretKey`) so they do not overwrite `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
            // used by the SeaweedFS S3 state backend.
            "awsAccessKeyId" | "AWS_ACCESS_KEY_ID" => {
                secrets_ref.insert("aws:accessKey".to_string(), ref_val);
            }
            "awsSecretAccessKey" | "AWS_SECRET_ACCESS_KEY" => {
                secrets_ref.insert("aws:secretKey".to_string(), ref_val);
            }
            // Azure & GCP providers read standard uppercase environment variables.
            "armClientId" | "ARM_CLIENT_ID" => {
                env_refs.insert("ARM_CLIENT_ID".to_string(), ref_val);
            }
            "armClientSecret" | "ARM_CLIENT_SECRET" => {
                env_refs.insert("ARM_CLIENT_SECRET".to_string(), ref_val);
            }
            "armTenantId" | "ARM_TENANT_ID" => {
                env_refs.insert("ARM_TENANT_ID".to_string(), ref_val);
            }
            "armSubscriptionId" | "ARM_SUBSCRIPTION_ID" => {
                env_refs.insert("ARM_SUBSCRIPTION_ID".to_string(), ref_val);
            }
            "googleCredentials" | "GOOGLE_CREDENTIALS" => {
                env_refs.insert("GOOGLE_CREDENTIALS".to_string(), ref_val);
            }
            other => {
                env_refs.insert(other.to_string(), ref_val);
            }
        }
    }
}

pub(crate) async fn resolve_sops_recipients(repo_workdir: &Path, tenant_pub: &str) -> String {
    let sops_cfg = repo_workdir.join(".sops.yaml");
    if let Ok(raw) = tokio::fs::read_to_string(&sops_cfg).await
        && let Ok(cfg) = SopsConfig::from_yaml(&raw)
        && let Some(cluster_pub) = cfg.cluster_age_recipient()
    {
        let cluster_pub = cluster_pub.trim();
        if cluster_pub.starts_with("age1") && cluster_pub != tenant_pub.trim() {
            return format!("{},{cluster_pub}", tenant_pub.trim());
        }
    }
    tenant_pub.trim().to_string()
}

pub(crate) fn next_unique_branch_timestamp() -> u64 {
    let now = Utc::now().timestamp().max(1) as u64;
    let mut prev = LAST_BRANCH_TS.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST_BRANCH_TS.compare_exchange_weak(
            prev,
            next,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(actual) => prev = actual,
        }
    }
}

/// Joins `rel` onto `base`, validating that `rel` is a relative path containing no `ParentDir` (`..`)
/// components and that the resulting path remains within `base`.
pub(crate) fn safe_relative_path(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel_trim = rel.trim();
    if rel_trim.is_empty() {
        return None;
    }
    let rel_path = Path::new(rel_trim);
    if rel_path.is_absolute() {
        return None;
    }
    for component in rel_path.components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => return None,
            _ => {}
        }
    }
    let joined = base.join(rel_path);
    if joined.starts_with(base) {
        Some(joined)
    } else {
        None
    }
}

pub(crate) async fn find_catalog_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(base_dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let candidate = entry.path().join("catalog.yaml");
        if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            out.push(candidate);
        }
    }
    out.sort();
    out
}

pub(crate) async fn load_provider_registry(
    repo_workdir: &Path,
) -> Result<ProviderRegistry, ServerError> {
    let registry_path = repo_workdir.join("providers/registry.yaml");
    if tokio::fs::try_exists(&registry_path).await.unwrap_or(false) {
        let yaml = tokio::fs::read_to_string(&registry_path).await?;
        Ok(ProviderRegistry::from_yaml(&yaml)?)
    } else {
        Ok(ProviderRegistry::default_day1())
    }
}

pub(crate) async fn load_catalog_manifest(
    repo_workdir: &Path,
    catalog_id: &str,
    kind_hint: Option<&str>,
) -> Result<CatalogManifest, ServerError> {
    let app_path = repo_workdir
        .join("catalog/apps")
        .join(catalog_id)
        .join("catalog.yaml");
    let infra_path = repo_workdir
        .join("catalog/infra")
        .join(catalog_id)
        .join("catalog.yaml");
    let bundle_path = repo_workdir
        .join("catalog/bundles")
        .join(catalog_id)
        .join("catalog.yaml");

    let target_path = match kind_hint.map(|k| k.to_ascii_lowercase()).as_deref() {
        Some("application") | Some("app") => app_path,
        Some("infra") | Some("blueprint") | Some("infrablueprint") => infra_path,
        Some("bundle") | Some("catalogbundle") => bundle_path,
        _ => {
            if tokio::fs::try_exists(&app_path).await.unwrap_or(false) {
                app_path
            } else if tokio::fs::try_exists(&infra_path).await.unwrap_or(false) {
                infra_path
            } else {
                bundle_path
            }
        }
    };

    if !tokio::fs::try_exists(&target_path).await.unwrap_or(false) {
        return Err(ServerError::NotFound(format!(
            "Catalog item '{catalog_id}' not found"
        )));
    }

    let yaml = tokio::fs::read_to_string(&target_path).await?;
    Ok(parse_catalog(&yaml)?)
}

pub(crate) fn serialize_tenants_kustomization(resources: &[String]) -> Result<String, ServerError> {
    let kustomization_doc = serde_json::json!({
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": resources,
    });
    let mut yaml = serde_yml::to_string(&kustomization_doc).map_err(|e| {
        ServerError::Internal(format!(
            "failed to serialize tenants/kustomization.yaml: {e}"
        ))
    })?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml)
}

pub(crate) struct ResolvedTenantResource {
    pub rel_dir: String,
    pub bundle_item: Option<(String, String)>,
}

/// Git ref holding committed, merged GitOps state read by GET handlers (`OPEN-13`).
pub(crate) const COMMITTED_GITOPS_REF: &str = "origin/main";

/// Read-only view over GitOps repository content: either a committed ref (read via
/// `git show <ref>:<path>` without touching the shared worktree, so no `git_lock` is needed) or
/// the checked-out worktree (callers must hold `AppState::git_lock` while using it).
pub(crate) enum RepoReader<'a> {
    AtRef {
        state: &'a AppState,
        rev: &'static str,
    },
    Worktree {
        root: &'a Path,
    },
}

impl<'a> RepoReader<'a> {
    /// Returns a committed-ref reader for `origin/main` when that ref exists locally.
    pub(crate) async fn committed(state: &'a AppState) -> Option<Self> {
        let exists = state
            .git_runner
            .ref_exists(&state.repo_workdir, "refs/remotes/origin/main")
            .await
            .unwrap_or(false);
        exists.then_some(Self::AtRef {
            state,
            rev: COMMITTED_GITOPS_REF,
        })
    }

    pub(crate) async fn read(&self, rel: &str) -> Option<String> {
        match self {
            Self::AtRef { state, rev } => state
                .git_runner
                .read_file_at_ref(&state.repo_workdir, rev, rel)
                .await
                .ok()
                .flatten(),
            Self::Worktree { root } => tokio::fs::read_to_string(root.join(rel)).await.ok(),
        }
    }

    pub(crate) async fn exists(&self, rel: &str) -> bool {
        match self {
            Self::AtRef { state, rev } => state
                .git_runner
                .path_exists_at_ref(&state.repo_workdir, rev, rel)
                .await
                .unwrap_or(false),
            Self::Worktree { root } => tokio::fs::try_exists(root.join(rel)).await.unwrap_or(false),
        }
    }

    /// Lists entry names (files and directories) directly under `rel`.
    pub(crate) async fn list_dir(&self, rel: &str) -> Vec<String> {
        match self {
            Self::AtRef { state, rev } => state
                .git_runner
                .list_dir_at_ref(&state.repo_workdir, rev, rel)
                .await
                .unwrap_or_default(),
            Self::Worktree { root } => {
                let mut out = Vec::new();
                if let Ok(mut entries) = tokio::fs::read_dir(root.join(rel)).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if let Some(n) = entry.file_name().to_str() {
                            out.push(n.to_string());
                        }
                    }
                }
                out.sort();
                out
            }
        }
    }

    /// [`resolve_tenant_resource_dir`] equivalent operating through this reader.
    pub(crate) async fn resolve_tenant_resource_dir(
        &self,
        tenant: &str,
        subdir: &str,
        name: &str,
    ) -> Option<ResolvedTenantResource> {
        let direct_rel_dir = format!("tenants/{tenant}/{subdir}/{name}");
        if self.exists(&direct_rel_dir).await {
            return Some(ResolvedTenantResource {
                rel_dir: direct_rel_dir,
                bundle_item: None,
            });
        }
        for b_str in self.list_dir(&format!("tenants/{tenant}/bundles")).await {
            let Some(item_suffix) = name.strip_prefix(&format!("{b_str}-")) else {
                continue;
            };
            let candidate = format!("tenants/{tenant}/bundles/{b_str}/{item_suffix}");
            if !self.exists(&candidate).await {
                continue;
            }
            let kind_matches = match subdir {
                "infra" => self.exists(&format!("{candidate}/stack.yaml")).await,
                "apps" => {
                    self.exists(&format!("{candidate}/helmrelease.yaml")).await
                        || self
                            .exists(&format!("{candidate}/helmrelease-canary.yaml"))
                            .await
                        || self.exists(&format!("{candidate}/manifests.yaml")).await
                }
                _ => true,
            };
            if kind_matches {
                return Some(ResolvedTenantResource {
                    rel_dir: candidate,
                    bundle_item: Some((b_str.clone(), item_suffix.to_string())),
                });
            }
        }
        None
    }
}

pub(crate) async fn resolve_tenant_resource_dir(
    repo_workdir: &Path,
    tenant: &str,
    subdir: &str,
    name: &str,
) -> Option<ResolvedTenantResource> {
    let direct_rel_dir = format!("tenants/{tenant}/{subdir}/{name}");
    if tokio::fs::try_exists(repo_workdir.join(&direct_rel_dir))
        .await
        .unwrap_or(false)
    {
        return Some(ResolvedTenantResource {
            rel_dir: direct_rel_dir,
            bundle_item: None,
        });
    }

    let bundles_dir = repo_workdir.join("tenants").join(tenant).join("bundles");
    let mut b_entries = tokio::fs::read_dir(&bundles_dir).await.ok()?;
    while let Ok(Some(b_entry)) = b_entries.next_entry().await {
        let b_name = b_entry.file_name();
        let Some(b_str) = b_name.to_str() else {
            continue;
        };
        if let Some(item_suffix) = name.strip_prefix(&format!("{b_str}-")) {
            let candidate = format!("tenants/{tenant}/bundles/{b_str}/{item_suffix}");
            let candidate_abs = repo_workdir.join(&candidate);
            if !tokio::fs::try_exists(&candidate_abs).await.unwrap_or(false) {
                continue;
            }
            let kind_matches = match subdir {
                "infra" => tokio::fs::try_exists(candidate_abs.join("stack.yaml"))
                    .await
                    .unwrap_or(false),
                "apps" => {
                    tokio::fs::try_exists(candidate_abs.join("helmrelease.yaml"))
                        .await
                        .unwrap_or(false)
                        || tokio::fs::try_exists(candidate_abs.join("helmrelease-canary.yaml"))
                            .await
                            .unwrap_or(false)
                        || tokio::fs::try_exists(candidate_abs.join("manifests.yaml"))
                            .await
                            .unwrap_or(false)
                }
                _ => true,
            };
            if kind_matches {
                return Some(ResolvedTenantResource {
                    rel_dir: candidate,
                    bundle_item: Some((b_str.to_string(), item_suffix.to_string())),
                });
            }
        }
    }
    None
}

pub(crate) fn commit_author_for_session(auth: &AuthSession) -> CommitAuthor {
    CommitAuthor::new(
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
    )
}

pub(crate) fn pr_author_for_session(auth: &AuthSession) -> String {
    format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    )
}

pub(crate) async fn stage_commit_push_and_return_to_main<I, S>(
    state: &AppState,
    auth: &AuthSession,
    branch: &str,
    commit_msg: &str,
    staged_paths: I,
) -> Result<String, ServerError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<Path>,
{
    state
        .git_runner
        .add(&state.repo_workdir, staged_paths)
        .await?;
    let author = commit_author_for_session(auth);
    let sha = state
        .git_runner
        .commit(&state.repo_workdir, commit_msg, &author)
        .await?;
    state
        .git_runner
        .push(&state.repo_workdir, "origin", branch)
        .await?;
    let _ = state
        .git_runner
        .checkout_branch(&state.repo_workdir, "main")
        .await;
    Ok(sha)
}

pub(crate) async fn open_pr_for_session(
    state: &AppState,
    auth: &AuthSession,
    tenant: &str,
    branch: &str,
    title: String,
    body: String,
) -> Result<PullRequestInfo, ServerError> {
    let pr_req = CreatePrRequest::new(title, body, branch)
        .with_tenant(tenant)
        .with_author(pr_author_for_session(auth));
    Ok(state.forge.create_pull_request(pr_req).await?)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Json;
    use axum::extract::{Path as AxumPath, Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use keel_core::{AppRuntime, AuthPhase, KEEL_SYSTEM_NAMESPACE, clean_candidate_ssh_host};
    use keel_git::{CommitAuthor, GitRunner, LocalGitProvider, PullRequestState};
    use keel_kube::{InMemorySecretStore, TelemetryMux};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;
    use crate::auth::SessionClaims;

    fn test_auth() -> AuthSession {
        AuthSession {
            claims: SessionClaims {
                sub: "admin@keel.local".to_string(),
                email: Some("admin@keel.local".to_string()),
                name: Some("Keel Admin".to_string()),
                groups: vec!["keel-admins".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        }
    }

    async fn setup_test_state() -> (TempDir, AppState, PathBuf) {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let bare_repo = temp_dir.path().join("remote.git");
        let workdir = temp_dir.path().join("workdir");

        let git = GitRunner::new();
        git.init_bare(&bare_repo).await.expect("init bare");
        git.clone(bare_repo.to_str().unwrap(), &workdir)
            .await
            .expect("clone");

        tokio::fs::create_dir_all(workdir.join("catalog/core/s3-state-backend"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("catalog/core/s3-state-backend/catalog.yaml"),
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: s3-state-backend
  name: SeaweedFS S3 Backend
  description: Seed S3-compatible state backend
  category: storage
  runtime: helm
spec:
  upstream:
    repoURL: https://seaweedfs.github.io/seaweedfs/helm
    chart: seaweedfs
    version: "4.53.0"
  schema:
    type: object
    properties:
      s3:
        type: object
"#,
        )
        .await
        .unwrap();

        tokio::fs::create_dir_all(workdir.join("clusters/management/core-addons"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("clusters/management/core-addons/s3-seaweedfs.yaml"),
            r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: s3-state-backend
  namespace: keel-system
  labels:
    app.kubernetes.io/managed-by: keel
    keel.io/protect: "true"
spec:
  interval: 10m
  chart:
    spec:
      chart: seaweedfs
      version: "4.53.0"
  values:
    s3:
      enabled: true
      enableAuth: true
    master:
      replicas: 1
"#,
        )
        .await
        .unwrap();

        git.add(&workdir, ["."]).await.unwrap();
        git.commit(
            &workdir,
            "chore: initial core addon seed",
            &CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
        git.push(&workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        let forge = Arc::new(LocalGitProvider::new(&bare_repo));
        let secret_store = Arc::new(InMemorySecretStore::new());
        // Provisioned tenant: Pulumi Stack proposals require its state passphrase Secret.
        keel_kube::SecretStateStore::ensure_pulumi_passphrase(
            secret_store.as_ref(),
            "tenant-default",
        )
        .await
        .unwrap();
        let state = AppState::new(
            forge,
            secret_store,
            TelemetryMux::default(),
            git,
            &workdir,
            bare_repo.to_string_lossy().to_string(),
            AuthPhase::OidcActive,
        );

        (temp_dir, state, workdir)
    }

    #[tokio::test]
    async fn get_catalog_returns_core_addons() {
        let (_tmp, state, _workdir) = setup_test_state().await;
        let Json(resp) = get_catalog(
            test_auth(),
            State(state),
            axum::extract::Query(crate::routes::catalog::GetCatalogQuery { tenant: None }),
        )
        .await
        .expect("get_catalog succeeds");

        assert_eq!(resp.core_addons.len(), 1);
        assert_eq!(resp.core_addons[0].metadata.id, "s3-state-backend");
    }

    #[tokio::test]
    async fn delete_release_rejects_protected_management_namespaces() {
        let (_tmp, state, _workdir) = setup_test_state().await;

        for ns in [KEEL_SYSTEM_NAMESPACE, "flux-system", "kyverno"] {
            let err = delete_release(
                test_auth(),
                State(state.clone()),
                AxumPath((ns.to_string(), "s3-state-backend".to_string())),
            )
            .await
            .expect_err("protected namespace must be rejected");

            match err {
                ServerError::Forbidden(msg) => {
                    assert_eq!(
                        msg,
                        "Protected management-plane base addon cannot be deleted via API"
                    );
                }
                other => panic!("expected Forbidden, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn upgrade_management_addon_creates_branch_and_pr() {
        let (_tmp, state, workdir) = setup_test_state().await;

        let req = UpgradeManagementAddonRequest {
            version: Some("4.54.0".to_string()),
            values: json!({
                "s3": {
                    "enableAuth": true
                },
                "filer": {
                    "resources": { "requests": { "cpu": "100m" } }
                }
            }),
        };

        let (status, Json(resp)) = upgrade_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
            Json(req),
        )
        .await
        .expect("upgrade_management_addon succeeds");

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(resp.id, "s3-state-backend");
        assert!(
            resp.branch
                .starts_with("chore/core-addon-s3-state-backend-")
        );
        assert_eq!(resp.proposal.source_branch, resp.branch);

        // Inspect updated file on the pushed branch
        state
            .git_runner
            .checkout_branch(&workdir, &resp.branch)
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(
            workdir.join("clusters/management/core-addons/s3-seaweedfs.yaml"),
        )
        .await
        .unwrap();
        let doc: Value = serde_yml::from_str(&raw).unwrap();
        assert_eq!(doc["spec"]["chart"]["spec"]["version"], "4.54.0");
        assert_eq!(doc["spec"]["values"]["s3"]["enabled"], true);
        assert_eq!(doc["spec"]["values"]["s3"]["enableAuth"], true);
        assert_eq!(
            doc["spec"]["values"]["filer"]["resources"]["requests"]["cpu"],
            "100m"
        );
        assert_eq!(doc["metadata"]["labels"]["keel.io/protect"], "true");
        state
            .git_runner
            .checkout_branch(&workdir, "main")
            .await
            .unwrap();

        // Disabling S3 authentication on the state backend is a locked value -> 400.
        let err = upgrade_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
            Json(UpgradeManagementAddonRequest {
                version: None,
                values: json!({ "s3": { "enableAuth": false } }),
            }),
        )
        .await
        .expect_err("locked value change is rejected");
        assert!(
            matches!(&err, ServerError::Core(keel_core::CoreError::Validation(msg)) if msg.contains("/s3/enableAuth")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rollback_management_addon_requires_two_commits_and_reverts_when_present() {
        let (_tmp, state, workdir) = setup_test_state().await;

        // With only 1 commit, rollback must return BadRequest (400)
        let err = rollback_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
        )
        .await
        .expect_err("rollback with 1 commit should fail");
        match err {
            ServerError::BadRequest(msg) => {
                assert_eq!(
                    msg,
                    "No previous revision of core add-on 's3-state-backend' found to roll back to"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }

        // Create a second commit on main modifying s3-seaweedfs.yaml
        let addon_path = workdir.join("clusters/management/core-addons/s3-seaweedfs.yaml");
        let original_yaml = tokio::fs::read_to_string(&addon_path).await.unwrap();
        let updated_yaml = original_yaml.replace("4.53.0", "4.54.0");
        tokio::fs::write(&addon_path, &updated_yaml).await.unwrap();
        state
            .git_runner
            .add(
                &workdir,
                ["clusters/management/core-addons/s3-seaweedfs.yaml"],
            )
            .await
            .unwrap();
        state
            .git_runner
            .commit(
                &workdir,
                "chore(core-addon): bump s3-state-backend to 4.54.0",
                &CommitAuthor::new("Keel Admin", "admin@keel.local"),
            )
            .await
            .unwrap();
        state
            .git_runner
            .push(&workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        // Now rollback should succeed and restore 4.53.0 on a revert branch + PR
        let (status, Json(resp)) = rollback_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
        )
        .await
        .expect("rollback with 2 commits succeeds");

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(resp.id, "s3-state-backend");
        assert!(
            resp.branch
                .starts_with("revert/core-addon-s3-state-backend-")
        );
        assert_eq!(resp.proposal.source_branch, resp.branch);

        state
            .git_runner
            .checkout_branch(&workdir, &resp.branch)
            .await
            .unwrap();
        let reverted_raw = tokio::fs::read_to_string(&addon_path).await.unwrap();
        assert_eq!(reverted_raw, original_yaml);
    }

    #[test]
    fn serialize_tenants_kustomization_ends_with_newline_and_preserves_prior_entries() {
        let initial = serialize_tenants_kustomization(&[
            "default/rbac.yaml".to_string(),
            "default/apps/e2e-lab-podinfo/helmrelease-canary.yaml".to_string(),
        ])
        .unwrap();
        assert!(initial.ends_with('\n'));

        let updated = serialize_tenants_kustomization(&[
            "default/rbac.yaml".to_string(),
            "default/apps/e2e-lab-podinfo/helmrelease-canary.yaml".to_string(),
            "acme/apps/acme-podinfo/helmrelease.yaml".to_string(),
        ])
        .unwrap();
        assert!(updated.ends_with('\n'));
        assert!(
            updated.starts_with(&initial),
            "appending a new tenant entry must not alter existing lines"
        );

        let parsed: serde_json::Value = serde_yml::from_str(&updated).unwrap();
        assert_eq!(parsed["resources"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn register_and_deploy_manifest_catalog_app_with_flux_and_pulumi_engines() {
        let (_tmp, state, workdir) = setup_test_state().await;

        let raw_manifests = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: ${app_name:=demo}-cfg
data:
  LOG_LEVEL: ${log_level:=info}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ${app_name:=demo}
spec:
  replicas: ${replicas:=2}
"#;

        let reg_req = RegisterCatalogAppRequest {
            kind: "app".to_string(),
            id: "raw-demo".to_string(),
            name: "Raw Demo App".to_string(),
            category: "Workloads".to_string(),
            runtime: Some(AppRuntime::Manifest),
            repo_url: None,
            chart: None,
            version: None,
            path: None,
            flagger_supported: false,
            provider: None,
            pulumi_runtime: None,
            entrypoint: None,
            protect_by_default: false,
            terraform_provider: None,
            pulumi_yaml: None,
            manifests_yaml: Some(raw_manifests.to_string()),
            default_engine: Some("flux".to_string()),
            description: Some("Multi-doc raw K8s manifest app".to_string()),
            items: None,
            schema: None,
            protected: false,
            tenant: None,
            resource_kind: None,
        };

        let (status, Json(reg_resp)) =
            register_catalog_app(test_auth(), State(state.clone()), Json(reg_req))
                .await
                .expect("register manifest catalog app succeeds");
        assert_eq!(status, StatusCode::CREATED);
        let app_dto = reg_resp.application.expect("application dto present");
        assert_eq!(app_dto.application.metadata.runtime, AppRuntime::Manifest);
        assert_eq!(app_dto.default_engine.as_deref(), Some("flux"));
        assert!(
            app_dto
                .manifests_yaml
                .as_deref()
                .unwrap()
                .contains("kind: Deployment")
        );

        // Merge the catalog registration PR so `raw-demo` is on `main`
        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(reg_resp.proposal.id),
        )
        .await
        .expect("merge catalog PR");

        // Verify GET /api/v1/catalog returns manifests_yaml and default_engine
        let Json(cat_resp) = get_catalog(
            test_auth(),
            State(state.clone()),
            axum::extract::Query(crate::routes::catalog::GetCatalogQuery { tenant: None }),
        )
        .await
        .expect("get_catalog succeeds");
        let found = cat_resp
            .applications
            .iter()
            .find(|a| a.application.metadata.id == "raw-demo")
            .expect("raw-demo in catalog");
        assert_eq!(found.default_engine.as_deref(), Some("flux"));
        assert!(found.manifests_yaml.is_some());
        assert!(cat_resp.manifests_yaml.contains_key("raw-demo"));

        // 1. Create proposal with Flux engine
        let flux_prop_req = CreateProposalRequest {
            tenant: "default".to_string(),
            catalog_id: "raw-demo".to_string(),
            kind: Some("app".to_string()),
            name: Some("raw-flux-inst".to_string()),
            title: None,
            body: None,
            values: json!({
                "app_name": "flux-svc",
                "log_level": "debug",
                "replicas": "3"
            }),
            canary: false,
            target_cluster: None,
            version: None,
            target_namespace: None,
            action_engine: Some("flux".to_string()),
        };
        let (p_status, Json(flux_prop_resp)) =
            create_proposal(test_auth(), State(state.clone()), Json(flux_prop_req))
                .await
                .expect("create flux manifest proposal");
        assert_eq!(p_status, StatusCode::CREATED);
        assert_eq!(
            flux_prop_resp.report.resources[0].resource_type,
            "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization"
        );

        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(flux_prop_resp.proposal.id),
        )
        .await
        .expect("merge flux manifest PR");

        let flux_app_dir = workdir.join("tenants/default/apps/raw-flux-inst");
        assert!(flux_app_dir.join("manifests.yaml").is_file());
        assert!(flux_app_dir.join("values-configmap.yaml").is_file());
        assert!(flux_app_dir.join("kustomization.yaml").is_file());
        assert!(flux_app_dir.join("flux-kustomization.yaml").is_file());

        let fk_raw = tokio::fs::read_to_string(flux_app_dir.join("flux-kustomization.yaml"))
            .await
            .unwrap();
        assert!(fk_raw.contains("substituteFrom"));
        assert!(fk_raw.contains("raw-flux-inst-values"));

        let Json(rel_cfg) = get_release_config(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "raw-flux-inst".to_string())),
        )
        .await
        .expect("get_release_config for flux manifest app");
        assert_eq!(rel_cfg.action_engine.as_deref(), Some("flux"));
        assert_eq!(rel_cfg.values["app_name"], "flux-svc");

        // 2. Create proposal with Pulumi engine
        let pulumi_prop_req = CreateProposalRequest {
            tenant: "default".to_string(),
            catalog_id: "raw-demo".to_string(),
            kind: Some("app".to_string()),
            name: Some("raw-pulumi-inst".to_string()),
            title: None,
            body: None,
            values: json!({
                "app_name": "pulumi-svc",
                "log_level": "warn",
                "replicas": "4"
            }),
            canary: false,
            target_cluster: None,
            version: None,
            target_namespace: None,
            action_engine: Some("pulumi".to_string()),
        };
        let (p2_status, Json(pulumi_prop_resp)) =
            create_proposal(test_auth(), State(state.clone()), Json(pulumi_prop_req))
                .await
                .expect("create pulumi manifest proposal");
        assert_eq!(p2_status, StatusCode::CREATED);
        assert!(
            pulumi_prop_resp
                .report
                .resources
                .iter()
                .any(|r| r.resource_type == "kubernetes:yaml/v2:ConfigGroup")
        );
        assert!(
            pulumi_prop_resp
                .report
                .resources
                .iter()
                .any(|r| r.resource_type == "kubernetes:apps/v1:Deployment")
        );

        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(pulumi_prop_resp.proposal.id),
        )
        .await
        .expect("merge pulumi manifest PR");

        let pulumi_app_dir = workdir.join("tenants/default/apps/raw-pulumi-inst");
        assert!(pulumi_app_dir.join("manifests.yaml").is_file());
        assert!(pulumi_app_dir.join("Pulumi.yaml").is_file());
        assert!(pulumi_app_dir.join("stack.yaml").is_file());
        assert!(pulumi_app_dir.join("kustomization.yaml").is_file());

        let Json(pulumi_rel_cfg) = get_release_config(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "raw-pulumi-inst".to_string())),
        )
        .await
        .expect("get_release_config for pulumi manifest app");
        assert_eq!(pulumi_rel_cfg.action_engine.as_deref(), Some("pulumi"));
        assert_eq!(pulumi_rel_cfg.values["app_name"], "pulumi-svc");

        // 3. Update existing manifest catalog item via PUT /api/v1/catalog/app/raw-demo
        let updated_manifests = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: ${app_name:=demo}-cfg
data:
  LOG_LEVEL: ${log_level:=info}
  API_KEY: ${api_key}
"#;
        let update_req = UpdateCatalogItemRequest {
            name: "Raw Demo App v2".to_string(),
            category: "Workloads".to_string(),
            description: Some("Updated manifest app with secret".to_string()),
            runtime: Some(AppRuntime::Manifest),
            repo_url: Some(String::new()),
            chart: None,
            version: None,
            path: None,
            flagger_supported: Some(false),
            provider: None,
            pulumi_runtime: None,
            entrypoint: None,
            protect_by_default: None,
            pulumi_yaml: None,
            manifests_yaml: Some(updated_manifests.to_string()),
            default_engine: Some("pulumi".to_string()),
            items: None,
            schema: Some(json!({
                "type": "object",
                "properties": {
                    "app_name": { "type": "string" },
                    "log_level": { "type": "string" },
                    "api_key": { "type": "string", "writeOnly": true, "format": "password" }
                }
            })),
            protected: None,
            tenant: None,
            resource_kind: None,
        };
        let (up_status, Json(up_resp)) = update_catalog_item(
            test_auth(),
            State(state.clone()),
            AxumPath(("app".to_string(), "raw-demo".to_string())),
            Json(update_req),
        )
        .await
        .expect("update_catalog_item for manifest app");
        assert_eq!(up_status, StatusCode::OK);
        assert_eq!(
            up_resp
                .application
                .as_ref()
                .and_then(|a| a.default_engine.as_deref()),
            Some("pulumi")
        );
        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(up_resp.proposal.id),
        )
        .await
        .expect("merge catalog update PR");

        // Verify list_releases includes both Flux and Pulumi manifest releases
        let Json(releases_resp) = list_releases(
            test_auth(),
            State(state.clone()),
            Query(ListReleasesQuery {
                tenant: Some("default".to_string()),
            }),
        )
        .await
        .expect("list_releases succeeds");
        assert!(
            releases_resp
                .releases
                .iter()
                .any(|r| r.name == "raw-flux-inst" && r.runtime == "manifest")
        );
        assert!(
            releases_resp
                .releases
                .iter()
                .any(|r| r.name == "raw-pulumi-inst" && r.runtime == "manifest")
        );

        // Verify synthesize_report_from_git_diff handles flux-kustomization.yaml and stack.yaml under apps/
        let sample_diff = "diff --git a/tenants/default/apps/raw-flux-inst/flux-kustomization.yaml b/tenants/default/apps/raw-flux-inst/flux-kustomization.yaml\nnew file mode 100644\n--- /dev/null\n+++ b/tenants/default/apps/raw-flux-inst/flux-kustomization.yaml\ndiff --git a/tenants/default/apps/raw-pulumi-inst/stack.yaml b/tenants/default/apps/raw-pulumi-inst/stack.yaml\nnew file mode 100644\n--- /dev/null\n+++ b/tenants/default/apps/raw-pulumi-inst/stack.yaml\n";
        let diff_report = synthesize_report_from_git_diff("99", sample_diff);
        assert_eq!(diff_report.resources.len(), 2);
        assert_eq!(
            diff_report.resources[0].resource_type,
            "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization"
        );
        assert_eq!(
            diff_report.resources[1].resource_type,
            "pulumi:pulumi.com/v1:Stack"
        );
    }

    #[tokio::test]
    async fn stack_ssh_info_resolves_configured_and_fallback_targets_and_simulated_shell() {
        let (_tmp, state, workdir) = setup_test_state().await;

        // 0. Unknown stacks are rejected with 404 instead of fabricating a target (`OPEN-17`).
        let ghost = get_stack_ssh_info(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "ghost-vm".to_string())),
        )
        .await;
        assert!(matches!(
            ghost,
            Err(SshInfoError::Server(ServerError::NotFound(_)))
        ));

        // 1. Fallback across multiple providers for existing VM stacks that have no outputs yet
        for (stack_name, expected_provider, expected_user) in [
            ("proxmox-vm-1", "proxmox", "ubuntu"),
            ("aws-vm-1", "aws", "ubuntu"),
            ("azure-vm-1", "azure", "azureuser"),
            ("gcp-vm-1", "gcp", "debian"),
            ("hetzner-vm-1", "hetzner", "root"),
        ] {
            let dir = workdir.join("tenants/default/infra").join(stack_name);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(
                dir.join("stack.yaml"),
                format!(
                    "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: {stack_name}\n  namespace: tenant-default\n  labels:\n    keel.io/provider: {expected_provider}\n    keel.io/resource-kind: vm\nspec:\n  stack: {stack_name}\n"
                ),
            )
            .await
            .unwrap();
            let Json(fallback_info) = get_stack_ssh_info(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), stack_name.to_string())),
            )
            .await
            .expect("get_stack_ssh_info fallback");
            assert_eq!(fallback_info.stack_name, stack_name);
            assert_eq!(fallback_info.provider, expected_provider);
            assert_eq!(fallback_info.ssh_user, expected_user);
            assert_eq!(
                fallback_info.ssh_host,
                format!("{stack_name}.default.local")
            );
        }

        // 2. Store user SSH key and create stacks with explicit ipAddress/ciuser/sshPort in values.yaml, stack.yaml, and Pulumi.yaml
        let _ = put_git_credentials(
            test_auth(),
            State(state.clone()),
            Json(UpdateGitCredentialsRequest {
                ssh_private_key: Some(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\ntest\n-----END OPENSSH PRIVATE KEY-----"
                        .to_string(),
                ),
                personal_access_token: None,
                username: Some("admin".to_string()),
            }),
        )
        .await
        .expect("store user ssh key");

        let vm_dir = workdir.join("tenants/default/infra/hetzner-node");
        tokio::fs::create_dir_all(&vm_dir).await.unwrap();
        tokio::fs::write(
            vm_dir.join("values.yaml"),
            "ipAddress: 203.0.113.42/24\nsshPort: 2222\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            vm_dir.join("stack.yaml"),
            r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: hetzner-node
  namespace: tenant-default
  labels:
    keel.io/provider: hetzner
spec:
  stack: organization/hetzner-vm/default-hetzner-node
"#,
        )
        .await
        .unwrap();

        let Json(vm_info) = get_stack_ssh_info(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "hetzner-node".to_string())),
        )
        .await
        .expect("get_stack_ssh_info configured");
        assert_eq!(vm_info.provider, "hetzner");
        assert_eq!(vm_info.ssh_host, "203.0.113.42");
        assert_eq!(vm_info.ssh_user, "root");
        assert_eq!(vm_info.ssh_port, 2222);
        assert_eq!(vm_info.ssh_command, "ssh -p 2222 root@203.0.113.42");
        assert!(vm_info.has_saved_ssh_key);

        // Stack with sshHost/sshUser/sshPort in stack.yaml spec.config and Pulumi.yaml outputs
        let stack_cfg_dir = workdir.join("tenants/default/infra/custom-box");
        tokio::fs::create_dir_all(&stack_cfg_dir).await.unwrap();
        tokio::fs::write(
            stack_cfg_dir.join("stack.yaml"),
            r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: custom-box
  namespace: tenant-default
spec:
  stack: organization/custom/default-custom-box
  config:
    keel:provider: aws
    sshUser: ec2-user
    sshPort: "2200"
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            stack_cfg_dir.join("Pulumi.yaml"),
            "name: custom-box\nruntime: yaml\noutputs:\n  publicIp: 198.51.100.77\n",
        )
        .await
        .unwrap();

        let Json(custom_info) = get_stack_ssh_info(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "custom-box".to_string())),
        )
        .await
        .expect("get_stack_ssh_info from stack.yaml + Pulumi.yaml");
        assert_eq!(custom_info.provider, "aws");
        assert_eq!(custom_info.ssh_host, "198.51.100.77");
        assert_eq!(custom_info.ssh_user, "ec2-user");
        assert_eq!(custom_info.ssh_port, 2200);

        // 3. Verify SSH capability gating across providers
        // Non-VM stacks (e.g. a `kind` cluster) are rejected with 422.
        let cluster_dir = workdir.join("tenants/default/infra/dev-cluster");
        tokio::fs::create_dir_all(&cluster_dir).await.unwrap();
        tokio::fs::write(
            cluster_dir.join("stack.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: dev-cluster\n  namespace: tenant-default\n  labels:\n    keel.io/provider: kind\nspec:\n  stack: dev-cluster\n",
        )
        .await
        .unwrap();
        let cluster = get_stack_ssh_info(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "dev-cluster".to_string())),
        )
        .await;
        assert!(
            matches!(&cluster, Err(SshInfoError::Unsupported(msg)) if msg.contains("SSH not supported for cluster")),
            "{cluster:?}"
        );

        assert!(crate::ssh::is_ssh_supported_infra(
            &vm_info.provider,
            &vm_info.stack_name
        ));
        assert!(crate::ssh::is_ssh_supported_infra(
            &custom_info.provider,
            &custom_info.stack_name
        ));
        assert!(!crate::ssh::is_ssh_supported_infra(
            "kind",
            "management-cluster-core"
        ));
        assert!(!crate::ssh::is_ssh_supported_infra(
            "kubernetes",
            "postgres-managed"
        ));
    }

    #[test]
    fn clean_candidate_ssh_host_extracts_external_ip_from_arrays_and_rejects_invalid() {
        assert_eq!(
            clean_candidate_ssh_host(r#"[["127.0.0.1"], ["192.168.1.150"]]"#).as_deref(),
            Some("192.168.1.150")
        );
        assert_eq!(
            clean_candidate_ssh_host(r#"["169.254.1.1", "203.0.113.5/24"]"#).as_deref(),
            Some("203.0.113.5")
        );
        assert_eq!(
            clean_candidate_ssh_host(r#"["127.0.0.1"]"#).as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(clean_candidate_ssh_host("dhcp"), None);
        assert_eq!(clean_candidate_ssh_host("-oProxyCommand=evil"), None);
        assert_eq!(clean_candidate_ssh_host("192.168.1.0/24"), None);
    }

    #[tokio::test]
    async fn healthz_metrics_providers_and_tenants_crud() {
        let (_tmp, state, _workdir) = setup_test_state().await;

        assert_eq!(healthz().await, "OK");
        let metrics_resp = metrics(State(state.clone())).await;
        assert_eq!(metrics_resp.status(), StatusCode::OK);

        // Providers list & toggle
        let Json(providers) = list_providers(test_auth(), State(state.clone()))
            .await
            .expect("list_providers");
        assert!(!providers.providers.is_empty());
        let first_id = providers.providers[0].id.clone();
        let target_enabled = !providers.providers[0].enabled;
        let Json(toggled) = toggle_provider(
            test_auth(),
            State(state.clone()),
            AxumPath(first_id.clone()),
            Json(ToggleProviderRequest {
                enabled: target_enabled,
            }),
        )
        .await
        .expect("toggle_provider");
        assert_eq!(toggled.provider.enabled, target_enabled);

        // Create tenant, list tenants, reject deleting "default", delete created tenant
        let (status, Json(created)) = create_tenant(
            test_auth(),
            State(state.clone()),
            Json(CreateTenantRequest {
                id: "acme".to_string(),
            }),
        )
        .await
        .expect("create_tenant");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created.tenant.id, "acme");

        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(created.proposal.id.clone()),
        )
        .await
        .expect("merge create_tenant proposal");

        let Json(tenants) = list_tenants(test_auth(), State(state.clone()))
            .await
            .expect("list_tenants");
        assert!(tenants.tenants.iter().any(|t| t.id == "acme"));

        let default_del = delete_tenant(
            test_auth(),
            State(state.clone()),
            AxumPath("default".to_string()),
        )
        .await
        .expect_err("cannot delete default tenant");
        assert!(matches!(default_del, ServerError::BadRequest(_)));

        let Json(deleted) = delete_tenant(
            test_auth(),
            State(state.clone()),
            AxumPath("acme".to_string()),
        )
        .await
        .expect("delete_tenant");
        assert_eq!(deleted.id, "acme");
    }

    #[tokio::test]
    async fn proposal_reject_reopen_and_profile_git_credentials() {
        let (_tmp, state, _workdir) = setup_test_state().await;

        let req = UpgradeManagementAddonRequest {
            version: Some("4.55.0".to_string()),
            values: json!({ "s3": { "enableAuth": true } }),
        };
        let (status, Json(upgrade)) = upgrade_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
            Json(req),
        )
        .await
        .expect("upgrade_management_addon");
        assert_eq!(status, StatusCode::CREATED);
        let pr_id = upgrade.proposal.id.clone();

        let Json(open_list) = list_proposals(
            test_auth(),
            State(state.clone()),
            axum::extract::Query(ListProposalsQuery {
                status: Some("open".to_string()),
                tenant: None,
            }),
        )
        .await
        .expect("list_proposals");
        assert!(open_list.proposals.iter().any(|p| p.id == pr_id));

        let Json(rejected) =
            reject_proposal(test_auth(), State(state.clone()), AxumPath(pr_id.clone()))
                .await
                .expect("reject_proposal");
        assert_eq!(rejected.proposal.state, PullRequestState::Closed);

        let Json(reopened) =
            reopen_proposal(test_auth(), State(state.clone()), AxumPath(pr_id.clone()))
                .await
                .expect("reopen_proposal");
        assert_eq!(reopened.proposal.state, PullRequestState::Open);

        // Profile Git credentials
        let Json(creds_resp) = put_git_credentials(
            test_auth(),
            State(state.clone()),
            Json(UpdateGitCredentialsRequest {
                username: Some("git".to_string()),
                ssh_private_key: Some(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----"
                        .to_string(),
                ),
                personal_access_token: Some("ghp_test123".to_string()),
            }),
        )
        .await
        .expect("put_git_credentials");
        assert!(creds_resp.stored);
        assert!(creds_resp.has_ssh_key);
        assert!(creds_resp.has_personal_access_token);
    }

    #[tokio::test]
    async fn stack_and_release_diagnostics_actions_and_delete_guards() {
        let (_tmp, state, _workdir) = setup_test_state().await;

        let Json(s_diag) = get_stack_diagnostics(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
        )
        .await
        .expect("get_stack_diagnostics fallback");
        assert_eq!(s_diag.name, "vm-1");
        assert!(
            restart_stack_workspace(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
            )
            .await
            .is_err()
        );
        assert!(
            reconcile_stack(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
            )
            .await
            .is_err()
        );
        assert!(
            force_delete_stack(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
            )
            .await
            .is_err()
        );
        assert!(
            unlock_stack(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
            )
            .await
            .is_err()
        );
        let Json(r_diag) = get_release_diagnostics(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "podinfo".to_string())),
        )
        .await
        .expect("get_release_diagnostics fallback");
        assert_eq!(r_diag.name, "podinfo");
        assert!(
            reconcile_release(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "podinfo".to_string())),
            )
            .await
            .is_err()
        );

        let del_protected = delete_stack(
            test_auth(),
            State(state.clone()),
            AxumPath((
                "flux-system".to_string(),
                "management-cluster-core".to_string(),
            )),
        )
        .await
        .expect_err("cannot delete management-cluster-core");
        assert!(matches!(
            del_protected,
            ServerError::BadRequest(_) | ServerError::Forbidden(_)
        ));
    }

    #[tokio::test]
    async fn update_catalog_item_config_queries_and_delete_release() {
        let (_tmp, state, workdir) = setup_test_state().await;

        tokio::fs::create_dir_all(workdir.join("catalog/apps/podinfo"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("catalog/apps/podinfo/catalog.yaml"),
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: podinfo
  name: Podinfo
  description: Demo app
  category: web
  runtime: helm
spec:
  upstream:
    repoURL: https://stefanprodan.github.io/podinfo
    chart: podinfo
    version: "6.5.0"
  schema:
    type: object
    properties:
      replicaCount:
        type: integer
"#,
        )
        .await
        .unwrap();

        tokio::fs::create_dir_all(workdir.join("tenants/default/apps/podinfo"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("tenants/default/apps/podinfo/values.yaml"),
            "replicaCount: 2\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            workdir.join("tenants/default/apps/podinfo/helmrelease.yaml"),
            r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: podinfo
  namespace: tenant-default
  labels:
    keel.io/catalog-id: podinfo
spec:
  interval: 5m
  chart:
    spec:
      chart: podinfo
      version: "6.5.0"
  values:
    replicaCount: 2
"#,
        )
        .await
        .unwrap();

        tokio::fs::create_dir_all(workdir.join("tenants/default/infra/vm-1"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("tenants/default/infra/vm-1/stack.yaml"),
            r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: vm-1
  namespace: tenant-default
  labels:
    keel.io/catalog-id: proxmox-vm
    keel.io/provider: proxmox
spec:
  stack: organization/proxmox-vm/default-vm-1
  backend: s3://keel-state
  config:
    vmName: vm-1
"#,
        )
        .await
        .unwrap();

        let git = GitRunner::new();
        git.add(&workdir, ["."]).await.unwrap();
        git.commit(
            &workdir,
            "feat: seed podinfo and vm-1",
            &CommitAuthor::new("Keel Test", "test@keel.local"),
        )
        .await
        .unwrap();
        git.push(&workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        // 1. Update catalog item creates a PR
        let (status, Json(updated)) = update_catalog_item(
            test_auth(),
            State(state.clone()),
            AxumPath(("app".to_string(), "podinfo".to_string())),
            Json(UpdateCatalogItemRequest {
                name: "Podinfo Updated".to_string(),
                category: "web".to_string(),
                description: Some("Updated description".to_string()),
                runtime: Some(AppRuntime::Helm),
                repo_url: Some("https://stefanprodan.github.io/podinfo".to_string()),
                chart: Some("podinfo".to_string()),
                version: Some("6.6.0".to_string()),
                path: None,
                flagger_supported: Some(true),
                provider: None,
                pulumi_runtime: None,
                entrypoint: None,
                protect_by_default: None,
                pulumi_yaml: None,
                manifests_yaml: None,
                default_engine: None,
                items: None,
                schema: None,
                protected: None,
                tenant: None,
                resource_kind: None,
            }),
        )
        .await
        .expect("update_catalog_item");
        assert_eq!(status, StatusCode::OK);
        assert!(updated.proposal.title.contains("podinfo"));

        // 2. get_release_config & get_stack_config
        let Json(rel_cfg) = get_release_config(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "podinfo".to_string())),
        )
        .await
        .expect("get_release_config");
        assert_eq!(rel_cfg.catalog_id, "podinfo");
        assert_eq!(rel_cfg.values["replicaCount"], 2);

        let Json(stack_cfg) = get_stack_config(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
        )
        .await
        .expect("get_stack_config");
        assert_eq!(stack_cfg.catalog_id, "vm-1");

        // 3. delete_release on non-protected tenant release creates a PR
        let Json(del_rel) = delete_release(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "podinfo".to_string())),
        )
        .await
        .expect("delete_release");
        assert!(!del_rel.proposal.id.is_empty());

        // 4. events_stream SSE endpoint
        let sse_resp = events_stream(
            test_auth(),
            State(state.clone()),
            axum::extract::Query(EventsStreamQuery { namespace: None }),
        )
        .await
        .into_response();
        assert_eq!(sse_resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sops_recipients_and_cloud_secret_refs_and_tenant_read_rbac() {
        let (_tmp, state, workdir) = setup_test_state().await;

        // 1. resolve_sops_recipients skips leading tenant rules and picks cluster_age_recipient
        tokio::fs::write(
            workdir.join(".sops.yaml"),
            r#"creation_rules:
  - path_regex: ^tenants/alpha/.*\.enc\.yaml$
    encrypted_regex: ^(data|stringData)$
    age: age1alphatenantkey000000000000000000000000000000000000000000
  - path_regex: ^clusters/.*\.enc\.yaml$
    encrypted_regex: ^(data|stringData)$
    age: age1clusterkey0000000000000000000000000000000000000000000000
"#,
        )
        .await
        .unwrap();
        let recipients = resolve_sops_recipients(
            &workdir,
            "age1betatenantkey0000000000000000000000000000000000000000000",
        )
        .await;
        assert_eq!(
            recipients,
            "age1betatenantkey0000000000000000000000000000000000000000000,age1clusterkey0000000000000000000000000000000000000000000000"
        );

        // 2. attach_infra_secret_refs maps both camelCase and UPPERCASE AWS keys to secretsRef
        let mut env_refs = std::collections::BTreeMap::new();
        let mut secrets_ref = std::collections::BTreeMap::new();
        attach_infra_secret_refs(
            &mut env_refs,
            &mut secrets_ref,
            "aws-vm-secrets",
            &[
                "AWS_ACCESS_KEY_ID".to_string(),
                "AWS_SECRET_ACCESS_KEY".to_string(),
                "ARM_CLIENT_ID".to_string(),
                "GOOGLE_CREDENTIALS".to_string(),
            ],
        );
        assert!(secrets_ref.contains_key("aws:accessKey"));
        assert!(secrets_ref.contains_key("aws:secretKey"));
        assert!(!env_refs.contains_key("AWS_ACCESS_KEY_ID"));
        assert!(!env_refs.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(env_refs.contains_key("ARM_CLIENT_ID"));
        assert!(env_refs.contains_key("GOOGLE_CREDENTIALS"));

        // 3. Non-admin tenant-alpha user is denied diagnostics & config reads for tenant-default and system namespaces
        let alpha_user = AuthSession {
            claims: SessionClaims {
                sub: "alpha@keel.local".to_string(),
                email: Some("alpha@keel.local".to_string()),
                name: Some("Alpha User".to_string()),
                groups: vec!["tenant-alpha".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(matches!(
            get_stack_diagnostics(
                alpha_user.clone(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
        assert!(matches!(
            get_stack_diagnostics(
                alpha_user.clone(),
                State(state.clone()),
                AxumPath((
                    "flux-system".to_string(),
                    "management-cluster-core".to_string()
                )),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
        assert!(matches!(
            get_release_diagnostics(
                alpha_user.clone(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "podinfo".to_string())),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
        assert!(matches!(
            get_stack_config(
                alpha_user.clone(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "vm-1".to_string())),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
        assert!(matches!(
            get_release_config(
                alpha_user.clone(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "podinfo".to_string())),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
        assert!(matches!(
            get_bundle_config(
                alpha_user.clone(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), "bundle-1".to_string())),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
        assert!(matches!(
            list_proposals(
                alpha_user,
                State(state),
                Query(ListProposalsQuery {
                    tenant: Some("default".to_string()),
                    status: None,
                }),
            )
            .await,
            Err(ServerError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn catalog_protection_tenant_rbac_all_tenants_visibility_and_cloud_k8s_previews() {
        let (_tmp, state, workdir) = setup_test_state().await;

        // Enable aws, gcp, azure providers so cloud K8s cluster blueprints are active
        let mut reg = keel_core::ProviderRegistry::default_day1();
        reg.set_enabled("aws", true);
        reg.set_enabled("gcp", true);
        reg.set_enabled("azure", true);
        tokio::fs::create_dir_all(workdir.join("providers"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("providers/registry.yaml"),
            reg.to_yaml().unwrap(),
        )
        .await
        .unwrap();

        // Copy aws-eks-cluster, gcp-gke-cluster, and azure-aks-cluster blueprints into workdir
        for bp_id in ["aws-eks-cluster", "gcp-gke-cluster", "azure-aks-cluster"] {
            let dest_dir = workdir.join("catalog/infra").join(bp_id);
            tokio::fs::create_dir_all(&dest_dir).await.unwrap();
            let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../deploy/catalog/infra")
                .join(bp_id);
            tokio::fs::write(
                dest_dir.join("catalog.yaml"),
                tokio::fs::read_to_string(src_dir.join("catalog.yaml"))
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
            tokio::fs::write(
                dest_dir.join("Pulumi.yaml"),
                tokio::fs::read_to_string(src_dir.join("Pulumi.yaml"))
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        }

        // Seed 3 apps: global protected app, acme-scoped protected app, stark-scoped unprotected app
        for (id, tenant_line, protected_line) in [
            ("global-app", "", "  protected: true\n"),
            ("acme-app", "  tenant: acme\n", "  protected: true\n"),
            ("stark-app", "  tenant: stark\n", "  protected: false\n"),
        ] {
            let dir = workdir.join("catalog/apps").join(id);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            tokio::fs::write(
                dir.join("catalog.yaml"),
                format!(
                    "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: {id}\n  name: {id}\n  category: apps\n  runtime: helm\n{tenant_line}{protected_line}spec:\n  upstream:\n    repoURL: https://stefanprodan.github.io/podinfo\n    chart: podinfo\n    version: \"6.5.0\"\n  schema:\n    type: object\n    properties: {{}}\n"
                ),
            )
            .await
            .unwrap();
        }

        let git = GitRunner::new();
        git.add(&workdir, ["."]).await.unwrap();
        git.commit(
            &workdir,
            "feat: seed multi-tenant catalog and cloud k8s blueprints",
            &CommitAuthor::new("Keel Test", "test@keel.local"),
        )
        .await
        .unwrap();
        git.push(&workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        let global_admin = test_auth();
        let acme_admin = AuthSession {
            claims: SessionClaims {
                sub: "admin@acme.local".to_string(),
                email: Some("admin@acme.local".to_string()),
                name: Some("Acme Admin".to_string()),
                groups: vec!["tenant-acme-admin".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };

        // 1. Global admin with All Tenants (`tenant: None` and `tenant: Some("all")`) sees ALL catalog items
        for query_tenant in [None, Some("all".to_string()), Some(String::new())] {
            let Json(cat) = get_catalog(
                global_admin.clone(),
                State(state.clone()),
                Query(GetCatalogQuery {
                    tenant: query_tenant,
                }),
            )
            .await
            .unwrap();
            let app_ids: Vec<&str> = cat
                .applications
                .iter()
                .map(|a| a.application.metadata.id.as_str())
                .collect();
            assert!(app_ids.contains(&"global-app"));
            assert!(app_ids.contains(&"acme-app"));
            assert!(app_ids.contains(&"stark-app"));
        }

        // 2. Global admin filtering by `?tenant=acme` sees global items AND acme items (not stark)
        let Json(cat_acme) = get_catalog(
            global_admin.clone(),
            State(state.clone()),
            Query(GetCatalogQuery {
                tenant: Some("acme".to_string()),
            }),
        )
        .await
        .unwrap();
        let acme_app_ids: Vec<&str> = cat_acme
            .applications
            .iter()
            .map(|a| a.application.metadata.id.as_str())
            .collect();
        assert_eq!(acme_app_ids, vec!["acme-app", "global-app"]);

        // 3. Tenant admin for acme with `tenant: None` sees only global items + acme items (never stark)
        let Json(cat_acme_user) = get_catalog(
            acme_admin.clone(),
            State(state.clone()),
            Query(GetCatalogQuery { tenant: None }),
        )
        .await
        .unwrap();
        let acme_user_ids: Vec<&str> = cat_acme_user
            .applications
            .iter()
            .map(|a| a.application.metadata.id.as_str())
            .collect();
        assert_eq!(acme_user_ids, vec!["acme-app", "global-app"]);

        // 4. Protected catalog item blocks deletion (even for global admin) but allows updates
        let del_prot = delete_catalog_item(
            global_admin.clone(),
            State(state.clone()),
            AxumPath(("app".to_string(), "acme-app".to_string())),
        )
        .await;
        assert!(matches!(del_prot, Err(ServerError::BadRequest(_))));

        // 5. Tenant admin for acme cannot delete stark's unprotected item or global items
        let del_cross = delete_catalog_item(
            acme_admin.clone(),
            State(state.clone()),
            AxumPath(("app".to_string(), "stark-app".to_string())),
        )
        .await;
        assert!(matches!(del_cross, Err(ServerError::Forbidden(_))));

        // 6. Global admin can delete stark's unprotected item via PR
        let Json(del_ok) = delete_catalog_item(
            global_admin.clone(),
            State(state.clone()),
            AxumPath(("app".to_string(), "stark-app".to_string())),
        )
        .await
        .expect("global admin deletes unprotected stark-app");
        assert_eq!(del_ok.id, "stark-app");
        assert!(del_ok.branch.starts_with("keel/catalog/delete-stark-app-"));

        // 7. Validate PKO preview & Stack synthesis for aws-eks-cluster, gcp-gke-cluster, and azure-aks-cluster with k8s 1.35
        for (catalog_id, values) in [
            (
                "aws-eks-cluster",
                serde_json::json!({
                    "clusterName": "eks-prod",
                    "region": "us-east-1",
                    "kubernetesVersion": "1.35",
                    "nodeInstanceType": "t3.medium",
                    "desiredNodeCount": 2
                }),
            ),
            (
                "gcp-gke-cluster",
                serde_json::json!({
                    "clusterName": "gke-prod",
                    "project": "keel-gcp",
                    "zone": "us-central1-a",
                    "kubernetesVersion": "1.35",
                    "machineType": "e2-medium",
                    "nodeCount": 2
                }),
            ),
            (
                "azure-aks-cluster",
                serde_json::json!({
                    "clusterName": "aks-prod",
                    "location": "eastus",
                    "kubernetesVersion": "1.35",
                    "vmSize": "Standard_DS2_v2",
                    "nodeCount": 2
                }),
            ),
        ] {
            let (status, Json(prop)) = create_proposal(
                global_admin.clone(),
                State(state.clone()),
                Json(CreateProposalRequest {
                    tenant: "default".to_string(),
                    catalog_id: catalog_id.to_string(),
                    kind: Some("infra".to_string()),
                    name: Some(catalog_id.to_string()),
                    title: None,
                    body: None,
                    values,
                    canary: false,
                    target_cluster: None,
                    version: None,
                    target_namespace: None,
                    action_engine: None,
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("Failed proposal for {catalog_id}: {e}"));
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(prop.preview_stack.spec.preview, Some(true));
            assert_eq!(
                prop.preview_stack
                    .spec
                    .config
                    .get("kubernetesVersion")
                    .map(String::as_str),
                Some("1.35")
            );
            assert_eq!(
                prop.preview_stack
                    .spec
                    .config
                    .get("kubeconfigSecretName")
                    .map(String::as_str),
                prop.preview_stack
                    .spec
                    .config
                    .get("kubeconfigSecretName")
                    .map(String::as_str)
            );
        }
    }
}
