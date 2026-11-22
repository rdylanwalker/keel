//! Stage 0 provisioning of `tenant-default`'s dedicated repository (`SDD.md` §3.2, §6.2).
//!
//! `tenant-default` is provisioned exactly like an API-created tenant
//! (`keel_server::tenant_repos`): a private repository `<owner>/<monorepo>-tenant-default`, `main`
//! protection, the fork policy, a read-only credential that works on that repository only (stored
//! in the cluster-only `tenant-default/keel-gitops-credentials` Secret) and the mandatory
//! `tenant-default/sops-age` key. Being Stage 0, it seeds the repository's `main` directly with
//! `tenants/default/kustomization.yaml` before protecting it, and writes the platform stub
//! straight into the seed commit of the platform repository. The platform repository's own
//! read token is never shared with `tenant-default`.
//!
//! Only the development-only Local Git provider keeps `tenant-default` inside the platform
//! repository (D8).

use std::sync::Arc;

use k8s_openapi::api::core::v1::Secret;
use keel_core::{
    ForgeConfig, KEEL_SYSTEM_NAMESPACE, TENANT_REPO_READER_SECRET_NAME, TENANT_REPOS_ORG,
};
use keel_git::{
    CommitAuthor, ForgeProvider, ForgejoGiteaProvider, GitCredentials, GitRunner, RepositoryOptions,
};
use keel_kube::TenantIsolationConfig;
use keel_server::tenant_repos::{
    self, TENANT_REPO_DESCRIPTION, TenantRepoForge, TenantRepoForgeKind, TenantRepoNaming,
};

use super::DEFAULT_TENANT_ID;
use super::kube::KubeTarget;
use crate::commands::CliError;

fn server_err(err: keel_server::ServerError) -> CliError {
    CliError::Execution(err.to_string())
}

/// Seed files (`(path, yaml)`) and `tenants/kustomization.yaml` entries of a tenant stub.
pub(crate) type TenantStubFiles = (Vec<(String, String)>, Vec<String>);

/// Seed files and `tenants/kustomization.yaml` entries for `tenant-default` in the platform
/// repository: the dedicated-repository stub (`rbac.yaml`, `git-repository.yaml`,
/// `sync.yaml`) for every remote forge, `None` for the Local Git provider (D8).
pub(crate) fn default_tenant_stub(
    forge: &ForgeConfig,
) -> Result<Option<TenantStubFiles>, CliError> {
    let Some(naming) = TenantRepoNaming::from_forge_config(forge).map_err(server_err)? else {
        return Ok(None);
    };
    let source = naming.source(DEFAULT_TENANT_ID).map_err(server_err)?;
    let stub = keel_kube::synthesize_tenant_repo_stub(
        &TenantIsolationConfig::new(DEFAULT_TENANT_ID),
        &source,
    )?;
    let entries = stub.kustomization_entries();
    Ok(Some((stub.files, entries)))
}

/// How Stage 0 reaches the forge for `tenant-default`'s repository.
pub(crate) struct DefaultTenantForge {
    pub(crate) forge: TenantRepoForge,
    /// URL the skeleton commit is pushed to (port-forwarded in-cluster Forgejo or the external
    /// forge origin) with `push_credentials`.
    pub(crate) push_url: String,
    pub(crate) push_credentials: GitCredentials,
}

/// Forge access for the in-cluster Forgejo through the Stage 0 port-forward: the admin basic
/// auth provider and the tenant-reader bot (credentials from
/// `keel-system/keel-forgejo-tenant-reader`) that mints the read token.
pub(crate) async fn in_cluster_forgejo_forge(
    target: KubeTarget<'_>,
    forge_cfg: &ForgeConfig,
    local_port: u16,
    admin_user: &str,
    admin_password: &str,
) -> Result<DefaultTenantForge, CliError> {
    let naming = TenantRepoNaming::from_forge_config(forge_cfg)
        .map_err(server_err)?
        .ok_or_else(|| CliError::Validation("the in-cluster Forgejo is not configured".into()))?;
    let base = format!("http://127.0.0.1:{local_port}");
    let admin: Arc<dyn ForgeProvider> = Arc::new(ForgejoGiteaProvider::with_basic_auth(
        &base,
        admin_user,
        &naming.monorepo_name,
        admin_user,
        admin_password,
    )?);
    let read = |key: &'static str| {
        target.read_secret_key(KEEL_SYSTEM_NAMESPACE, TENANT_REPO_READER_SECRET_NAME, key)
    };
    let (Some(bot_user), Some(bot_password)) = (read("username").await, read("password").await)
    else {
        return Err(CliError::Execution(format!(
            "{KEEL_SYSTEM_NAMESPACE}/{TENANT_REPO_READER_SECRET_NAME} is missing: the tenant-reader bot must exist before tenant-default's repository credential can be minted"
        )));
    };
    let bot: Arc<dyn ForgeProvider> = Arc::new(ForgejoGiteaProvider::with_basic_auth(
        &base,
        TENANT_REPOS_ORG,
        &naming.monorepo_name,
        &bot_user,
        &bot_password,
    )?);
    let ca_crt = target
        .read_secret_key(KEEL_SYSTEM_NAMESPACE, "keel-wildcard-tls", "ca.crt")
        .await;
    let push_url = format!(
        "{base}/{}/{}.git",
        naming.owner,
        naming.repo_name(DEFAULT_TENANT_ID).map_err(server_err)?
    );
    Ok(DefaultTenantForge {
        forge: TenantRepoForge {
            naming,
            ca_crt,
            admin,
            token_minter: Some(bot),
        },
        push_url,
        push_credentials: GitCredentials::HttpsBasic {
            username: admin_user.to_string(),
            password_or_token: admin_password.to_string(),
        },
    })
}

/// Forge access for an external `--git-url` forge with the operator `--git-token`. External
/// Forgejo/Gitea also needs the tenant-reader bot Secret (created by an operator).
pub(crate) async fn external_forge(
    target: KubeTarget<'_>,
    forge_cfg: &ForgeConfig,
    token: &str,
) -> Result<Option<DefaultTenantForge>, CliError> {
    let Some(naming) = TenantRepoNaming::from_forge_config(forge_cfg).map_err(server_err)? else {
        return Ok(None);
    };
    let Some(admin) = super::forge::build_remote_forge_provider(
        forge_cfg.provider,
        &forge_cfg.repo_url,
        forge_cfg.api_url.as_deref(),
        token,
    )?
    else {
        return Ok(None);
    };
    let admin: Arc<dyn ForgeProvider> = Arc::from(admin);
    let token_minter = if naming.kind == TenantRepoForgeKind::Forgejo {
        let read = |key: &'static str| {
            target.read_secret_key(KEEL_SYSTEM_NAMESPACE, TENANT_REPO_READER_SECRET_NAME, key)
        };
        match (read("username").await, read("password").await) {
            (Some(user), Some(password)) => {
                let slug = keel_git::parse_remote_repo_url(&forge_cfg.repo_url)?;
                Some(Arc::new(ForgejoGiteaProvider::with_basic_auth(
                    super::forge::forgejo_base(forge_cfg.api_url.as_deref(), &slug.origin_url),
                    TENANT_REPOS_ORG,
                    &naming.monorepo_name,
                    &user,
                    &password,
                )?) as Arc<dyn ForgeProvider>)
            }
            _ => {
                return Err(CliError::Validation(format!(
                    "an external Forgejo/Gitea needs the `{TENANT_REPOS_ORG}` organisation and a restricted tenant-reader bot whose credentials (`username`, `password`) are stored in {KEEL_SYSTEM_NAMESPACE}/{TENANT_REPO_READER_SECRET_NAME}; create them (or use --with-forge) and re-run"
                )));
            }
        }
    } else {
        None
    };
    let target_repo = naming.target(DEFAULT_TENANT_ID).map_err(server_err)?;
    let push_url =
        tenant_repos::tenant_push_url(&forge_cfg.repo_url, &target_repo).map_err(server_err)?;
    Ok(Some(DefaultTenantForge {
        forge: TenantRepoForge {
            naming,
            ca_crt: None,
            admin,
            token_minter,
        },
        push_url,
        push_credentials: GitCredentials::HttpsBasic {
            // Same HTTPS identity as the platform seed push (any username works with a token).
            username: "git".to_string(),
            password_or_token: token.to_string(),
        },
    }))
}

/// Seeds `main` of the tenant repository with `tenants/default/kustomization.yaml` (Stage 0 may
/// commit to `main`; the branch is protected right after). Returns `true` when it pushed.
async fn seed_default_tenant_repo(access: &DefaultTenantForge) -> Result<bool, CliError> {
    let tmp = tempfile::tempdir()?;
    let work = tmp.path().join("tenant-default");
    let git = GitRunner::new().with_credentials(access.push_credentials.clone());
    git.clone(&access.push_url, &work).await?;
    let rel = format!("tenants/{DEFAULT_TENANT_ID}/kustomization.yaml");
    if tokio::fs::try_exists(work.join(&rel))
        .await
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let yaml = keel_kube::build_tenant_repo_kustomization_yaml(DEFAULT_TENANT_ID, &[])?;
    keel_kube::ensure_git_safe_manifests(&rel, &yaml)?;
    tokio::fs::create_dir_all(work.join(format!("tenants/{DEFAULT_TENANT_ID}"))).await?;
    tokio::fs::write(work.join(&rel), yaml).await?;
    git.add(&work, [rel.as_str()]).await?;
    git.commit(
        &work,
        "chore(bootstrap): initialise tenant repository",
        &CommitAuthor::new("Keel Bootstrap", "bootstrap@keel.io"),
    )
    .await?;
    git.push(&work, "origin", "HEAD:refs/heads/main")
        .await
        .map_err(|e| {
            CliError::Execution(format!(
                "could not seed `main` of the tenant-default repository ({e}); if `main` is already protected, add `tenants/default/kustomization.yaml` through a pull request and re-run"
            ))
        })?;
    Ok(true)
}

/// Identity Stage 0 writes the platform-owned credentials Secret as: the operator before the
/// Kyverno `protect-platform-owned-tenant-objects` policy exists, `keel-tenant-reconciler`
/// (which may write Secrets in `tenant-*` only) afterwards, so managed-cluster admins outside
/// `system:masters` are not denied.
async fn platform_owned_writer(target: KubeTarget<'_>) -> Result<Option<String>, CliError> {
    let installed = !target
        .output(&[
            "get",
            "validatingpolicies.policies.kyverno.io",
            keel_kube::kyverno::POLICY_PROTECT_PLATFORM_OWNED_TENANT_OBJECTS,
            "--ignore-not-found",
            "-o",
            "name",
        ])
        .await
        .unwrap_or_default()
        .trim()
        .is_empty();
    Ok(installed.then(|| {
        format!(
            "system:serviceaccount:{}:{}",
            keel_core::FLUX_SYSTEM_NAMESPACE,
            keel_kube::tenant::TENANT_RECONCILER_SERVICE_ACCOUNT
        )
    }))
}

async fn read_secret(
    target: KubeTarget<'_>,
    namespace: &str,
    name: &str,
) -> Result<Option<Secret>, CliError> {
    let raw = target
        .output(&[
            "-n",
            namespace,
            "get",
            "secret",
            name,
            "--ignore-not-found",
            "-o",
            "json",
        ])
        .await?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&raw)?))
}

/// Ensures the mandatory `tenant-default/sops-age` key (create-if-absent; fails closed when
/// `age-keygen` is unavailable). Returns `true` when it created the key.
pub(crate) async fn ensure_default_tenant_age_key(
    target: KubeTarget<'_>,
) -> Result<bool, CliError> {
    let namespace = keel_kube::tenant_namespace(DEFAULT_TENANT_ID)?;
    if read_secret(target, &namespace, keel_kube::TENANT_SOPS_AGE_SECRET_NAME)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let key = tenant_repos::generate_tenant_age_key(DEFAULT_TENANT_ID)
        .await
        .map_err(server_err)?;
    let secret = keel_kube::build_tenant_age_key_secret(DEFAULT_TENANT_ID, &key)?;
    // `kubectl create`, never `apply`: no copy of the private key in the last-applied annotation,
    // and an existing key (Git content may be encrypted to it) is never replaced.
    target
        .write_secret_manifest(&serde_yml::to_string(&secret)?, false, None)
        .await
}

/// `tenant-default/keel-gitops-credentials` through `kubectl` (`get`, `create`, `replace`; never
/// `apply`, so the credential is not copied into the last-applied annotation), written as
/// [`platform_owned_writer`].
struct KubectlCredentialSecrets<'a> {
    target: KubeTarget<'a>,
    writer: Option<String>,
}

fn cli_to_server(err: CliError) -> keel_server::ServerError {
    keel_server::ServerError::ServiceUnavailable(err.to_string())
}

#[async_trait::async_trait]
impl tenant_repos::TenantCredentialSecrets for KubectlCredentialSecrets<'_> {
    async fn read(&self, tenant: &str) -> Result<Option<Secret>, keel_server::ServerError> {
        let namespace = keel_kube::tenant_namespace(tenant)?;
        read_secret(
            self.target,
            &namespace,
            keel_kube::TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
        )
        .await
        .map_err(cli_to_server)
    }
    async fn create_if_absent(
        &self,
        _tenant: &str,
        secret: Secret,
    ) -> Result<bool, keel_server::ServerError> {
        self.write(&secret, false).await
    }
    async fn replace(&self, _tenant: &str, secret: Secret) -> Result<(), keel_server::ServerError> {
        self.write(&secret, true).await.map(|_| ())
    }
}

impl KubectlCredentialSecrets<'_> {
    async fn write(
        &self,
        secret: &Secret,
        replace: bool,
    ) -> Result<bool, keel_server::ServerError> {
        let yaml = serde_yml::to_string(secret)
            .map_err(|e| keel_server::ServerError::Internal(e.to_string()))?;
        self.target
            .write_secret_manifest(&yaml, replace, self.writer.as_deref())
            .await
            .map_err(cli_to_server)
    }
}

/// Provisions `tenant-default`'s repository, credential Secret and `sops-age` key (idempotent;
/// a valid credential Secret is kept, an unusable one rotated and the previous credential
/// revoked). Returns an operator note (never secret values).
pub(crate) async fn provision_default_tenant_repo(
    target: KubeTarget<'_>,
    access: &DefaultTenantForge,
) -> Result<String, CliError> {
    let forge = &access.forge;
    let tenant = DEFAULT_TENANT_ID;
    let repo = forge.target(tenant).map_err(server_err)?;
    forge
        .admin
        .ensure_named_repository(
            &repo.owner,
            &repo.name,
            &RepositoryOptions {
                description: Some(TENANT_REPO_DESCRIPTION.to_string()),
                ..RepositoryOptions::default()
            },
        )
        .await
        .map_err(|e| {
            server_err(tenant_repos::provisioning_error(
                tenant,
                &repo.full_name(),
                "repository creation",
                e,
            ))
        })?;
    let seeded = seed_default_tenant_repo(access).await?;
    let state = tenant_repos::provision_forge(forge, tenant)
        .await
        .map_err(server_err)?;
    let age_created = ensure_default_tenant_age_key(target).await?;

    let namespace = keel_kube::tenant_namespace(tenant)?;
    let source = forge.source(tenant).map_err(server_err)?;
    let secrets = KubectlCredentialSecrets {
        target,
        writer: platform_owned_writer(target).await?,
    };
    let credential = match tenant_repos::reconcile_tenant_credential(forge, tenant, &secrets)
        .await
        .map_err(server_err)?
    {
        tenant_repos::StepOutcome::Created => "issued",
        tenant_repos::StepOutcome::Rotated => "rotated",
        _ => "kept",
    };
    Ok(format!(
        "tenant-default repository `{}` ready ({}, {}; `main` protection: {:?}; forks: {:?}); read credential {credential} in {namespace}/{} ({} transport); SOPS age key {}",
        repo.full_name(),
        if state.repository.outcome == keel_git::RepositoryEnsureOutcome::Created {
            "created"
        } else {
            "existing"
        },
        if seeded { "seeded" } else { "already seeded" },
        state.main_protection,
        state.forks,
        keel_kube::TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
        source.transport(),
        if age_created { "created" } else { "kept" },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::ForgeKind;

    fn cfg(provider: ForgeKind, repo_url: &str, public_url: Option<&str>) -> ForgeConfig {
        ForgeConfig {
            provider,
            repo_url: repo_url.to_string(),
            public_url: public_url.map(str::to_string),
            ..ForgeConfig::default()
        }
    }

    #[test]
    fn default_tenant_gets_the_dedicated_stub_on_every_remote_forge() {
        let forgejo = cfg(
            ForgeKind::Forgejo,
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
            Some("https://forgejo.keel.local:8888"),
        );
        let (files, entries) = default_tenant_stub(&forgejo).unwrap().unwrap();
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "tenants/default/rbac.yaml",
                "tenants/default/git-repository.yaml",
                "tenants/default/sync.yaml"
            ]
        );
        assert_eq!(
            entries,
            [
                "default/rbac.yaml",
                "default/git-repository.yaml",
                "default/sync.yaml"
            ]
        );
        let git_repo = &files[1].1;
        assert!(git_repo.contains(
            "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-default.git"
        ));
        assert!(git_repo.contains("keel-gitops-credentials"));
        // The platform repository's read credential is never referenced by tenant-default.
        assert!(!git_repo.contains("name: git-credentials"));
        assert!(!git_repo.contains("keel/infra-gitops.git"));

        let github = cfg(ForgeKind::GitHub, "https://github.com/acme/infra.git", None);
        let (files, _) = default_tenant_stub(&github).unwrap().unwrap();
        assert!(
            files[1]
                .1
                .contains("ssh://git@github.com/acme/infra-tenant-default.git")
        );

        // Local Git provider (D8): tenant-default stays in the platform repository.
        let local = cfg(ForgeKind::Local, "/tmp/keel-gitops.git", None);
        assert!(default_tenant_stub(&local).unwrap().is_none());
    }
}
