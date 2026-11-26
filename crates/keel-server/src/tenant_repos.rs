//! Per-tenant Git repositories: registry, forge provisioning and runtime credentials
//! (`SDD.md` §3.2, §6.2, §6.3).
//!
//! Every tenant owns a private forge repository `<owner>/<monorepo>-tenant-<id>` (naming
//! convention, no database): the in-cluster Forgejo/Gitea keeps them in the
//! [`keel_core::TENANT_REPOS_ORG`] organisation, GitHub and GitLab next to the platform
//! repository (same owner / group). The platform monorepo only holds the platform-owned stub
//! (`tenants/<id>/{rbac,git-repository,sync}.yaml`), and that stub's `git-repository.yaml` is the
//! platform-side record of the tenant's repository coordinates.
//!
//! The development-only Local Git provider (D8) keeps every tenant inside the monorepo
//! ([`TenantRepoLayout::Monorepo`]); it offers no isolation.
//!
//! keel-server stays stateless: tenant repository clones under [`TenantRepos::checkout_dir`] are
//! disposable caches (one async lock per repository), credentials live only in the forge and in
//! the cluster-only `tenant-<id>/keel-gitops-credentials` Secret.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use keel_core::SopsManager;
use keel_git::{
    BranchProtectionOutcome, ForgeError, ForgeProvider, ForkPolicyOutcome, GitRunner,
    REPO_READ_TOKEN_NAME_PREFIX, RepoCredentialKind, RepositoryInfo, RepositoryOptions,
    RepositoryTarget, generate_ed25519_key_pair, known_hosts_file, ssh_clone_url,
    ssh_public_key_fingerprint,
};
use keel_kube::{
    SecretStateStore, TenantAgeKey, TenantGitopsCredentialStatus, TenantRepoCredential,
    TenantRepoSource, build_pko_operator_known_hosts_configmap, pko_operator_needs_ssh_known_hosts,
    tenant_gitops_credentials_secret, tenant_gitops_credentials_status,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::ServerError;

/// Infix of tenant repository names: `<monorepo>-tenant-<id>` (D10).
pub const TENANT_REPO_NAME_INFIX: &str = "-tenant-";
/// Generic description of every tenant repository (never tenant data). Keep it generic: Forgejo
/// repository-scoped tokens can still read the name and description of other repositories in the
/// organization, so a tenant-specific description would leak to every other tenant.
pub const TENANT_REPO_DESCRIPTION: &str = "Keel tenant GitOps repository (managed by Keel; changes arrive through reviewed pull requests)";
/// Title / comment of the read-only deploy key of an SSH tenant repository.
pub const TENANT_DEPLOY_KEY_TITLE_PREFIX: &str = "keel-tenant-";
/// Proposal action of the monorepo tenant stub PR (`keel/<id>/create-tenant-<ts>`).
pub const CREATE_TENANT_ACTION: &str = "create-tenant";
/// Proposal action of the monorepo tenant removal PR (`keel/<id>/delete-tenant-<ts>`).
pub const DELETE_TENANT_ACTION: &str = "delete-tenant";
/// Proposal action of the tenant repository skeleton PR (`keel/<id>/init-tenant-repo-<ts>`).
pub const INIT_TENANT_REPO_ACTION: &str = "init-tenant-repo";
/// Maximum forge repository name length (GitHub/Forgejo: 100).
const MAX_REPO_NAME_LEN: usize = 100;

/// Forge family hosting tenant repositories; decides the credential transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TenantRepoForgeKind {
    /// Forgejo/Gitea: HTTPS, repository-scoped read token minted by the tenant-reader bot.
    Forgejo,
    /// GitLab: HTTPS, project deploy token (`read_repository`).
    GitLab,
    /// GitHub: SSH, read-only deploy key with a pinned `known_hosts`.
    GitHub,
}

/// Base of the clone URL Flux and PKO use for a tenant repository (platform configuration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantRepoCloneBase {
    /// `<origin>/<owner>/<name>.git` (`origin` is `https://host[:port]`).
    Https { origin: String },
    /// `ssh://git@<host>[:port]/<owner>/<name>.git`.
    Ssh { host: String, port: u16 },
}

/// Naming convention and transport of tenant repositories, derived from the platform forge
/// configuration only (`.keel/config.yaml` `forge`; no database, no tenant input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRepoNaming {
    pub kind: TenantRepoForgeKind,
    /// Owner of every tenant repository (organisation / user / GitLab group path).
    pub owner: String,
    /// Name of the platform repository (prefix of every tenant repository name).
    pub monorepo_name: String,
    pub clone_base: TenantRepoCloneBase,
}

impl TenantRepoNaming {
    /// Naming for the configured forge (D10): Forgejo/Gitea repositories live in the
    /// [`keel_core::TENANT_REPOS_ORG`] organisation and are cloned over HTTPS through
    /// `forge.publicUrl` (the in-cluster Forgejo ingress) or the repository origin; GitLab
    /// repositories live in the platform repository's group (HTTPS); GitHub repositories next to
    /// the platform repository (SSH on port 22). `None` for the Local Git provider (D8).
    pub fn from_forge_config(cfg: &keel_core::ForgeConfig) -> Result<Option<Self>, ServerError> {
        use keel_core::ForgeKind;
        if cfg.provider == ForgeKind::Local {
            return Ok(None);
        }
        let slug = keel_git::parse_remote_repo_url(&cfg.repo_url)?;
        let naming = match cfg.provider {
            ForgeKind::Forgejo | ForgeKind::Gitea => Self {
                kind: TenantRepoForgeKind::Forgejo,
                owner: keel_core::TENANT_REPOS_ORG.to_string(),
                monorepo_name: slug.repo,
                clone_base: TenantRepoCloneBase::Https {
                    origin: cfg
                        .public_url
                        .clone()
                        .unwrap_or_else(|| slug.origin_url.clone()),
                },
            },
            ForgeKind::GitLab => Self {
                kind: TenantRepoForgeKind::GitLab,
                owner: slug.owner,
                monorepo_name: slug.repo,
                clone_base: TenantRepoCloneBase::Https {
                    origin: slug.origin_url,
                },
            },
            ForgeKind::GitHub => Self {
                kind: TenantRepoForgeKind::GitHub,
                owner: slug.owner,
                monorepo_name: slug.repo,
                clone_base: TenantRepoCloneBase::Ssh {
                    host: slug.host,
                    port: 22,
                },
            },
            ForgeKind::Local => return Ok(None),
        };
        Ok(Some(naming))
    }

    /// `<monorepo>-tenant-<id>`.
    pub fn repo_name(&self, tenant: &str) -> Result<String, ServerError> {
        keel_core::validate_dns_label(tenant, "tenant")?;
        let name = format!("{}{TENANT_REPO_NAME_INFIX}{tenant}", self.monorepo_name);
        if name.len() > MAX_REPO_NAME_LEN {
            return Err(ServerError::BadRequest(format!(
                "tenant repository name `{name}` is longer than {MAX_REPO_NAME_LEN} characters; use a shorter tenant id"
            )));
        }
        Ok(name)
    }

    /// The tenant repository as a forge target.
    pub fn target(&self, tenant: &str) -> Result<RepositoryTarget, ServerError> {
        Ok(RepositoryTarget::new(
            &self.owner,
            &self.repo_name(tenant)?,
        )?)
    }

    /// The clone URL Flux (`keel-gitops`) and PKO (`spec.projectRepo`) use.
    pub fn clone_url(&self, tenant: &str) -> Result<String, ServerError> {
        let name = self.repo_name(tenant)?;
        Ok(match &self.clone_base {
            TenantRepoCloneBase::Https { origin } => {
                format!("{}/{}/{name}.git", origin.trim_end_matches('/'), self.owner)
            }
            TenantRepoCloneBase::Ssh { host, port } => {
                ssh_clone_url(host, *port, &self.owner, &name)?
            }
        })
    }

    /// The validated tenant source (URL + transport) the stub and credentials are built for.
    pub fn source(&self, tenant: &str) -> Result<TenantRepoSource, ServerError> {
        Ok(TenantRepoSource::new(tenant, &self.clone_url(tenant)?)?)
    }

    /// Credential transport of this forge (must match the clone URL).
    pub fn credential_kind(&self) -> RepoCredentialKind {
        match self.kind {
            TenantRepoForgeKind::Forgejo | TenantRepoForgeKind::GitLab => {
                RepoCredentialKind::HttpsToken
            }
            TenantRepoForgeKind::GitHub => RepoCredentialKind::SshDeployKey,
        }
    }

    /// Name of the Forgejo/GitLab repository read token (`keel-tenant-repo-<repo>`).
    pub fn token_name(&self, tenant: &str) -> Result<String, ServerError> {
        Ok(format!(
            "{REPO_READ_TOKEN_NAME_PREFIX}{}",
            self.repo_name(tenant)?
        ))
    }
}

/// Forge configuration for per-tenant repositories: naming plus the providers that provision
/// them.
#[derive(Clone)]
pub struct TenantRepoForge {
    pub naming: TenantRepoNaming,
    /// Public Keel CA (`ca.crt`) for HTTPS repositories behind the Keel CA (in-cluster Forgejo).
    pub ca_crt: Option<String>,
    /// Platform-token provider: repository creation, protection, forks, deploy keys, PRs.
    pub admin: Arc<dyn ForgeProvider>,
    /// Forgejo/Gitea: basic-auth provider of the tenant-reader bot that mints the
    /// repository-scoped read tokens (`None`: provisioning fails closed).
    pub token_minter: Option<Arc<dyn ForgeProvider>>,
}

impl std::fmt::Debug for TenantRepoForge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantRepoForge")
            .field("naming", &self.naming)
            .field("ca_crt", &self.ca_crt.is_some())
            .field("token_minter", &self.token_minter.is_some())
            .finish()
    }
}

impl std::ops::Deref for TenantRepoForge {
    type Target = TenantRepoNaming;
    fn deref(&self) -> &TenantRepoNaming {
        &self.naming
    }
}

impl TenantRepoForge {
    /// Provider scoped to the tenant repository (PR operations).
    pub fn repository_provider(&self, tenant: &str) -> Result<Arc<dyn ForgeProvider>, ServerError> {
        let target = self.target(tenant)?;
        Ok(self.admin.for_repository(&target.owner, &target.name)?)
    }
}

/// Where tenant content lives.
#[derive(Debug, Clone)]
pub enum TenantRepoLayout {
    /// Development-only Local Git provider (D8): tenants live in the platform repository.
    Monorepo,
    /// Every tenant has its own forge repository.
    Dedicated(Arc<TenantRepoForge>),
    /// A remote forge is configured but cannot be used for tenant repositories (for example no
    /// forge token): tenant provisioning fails closed with this reason instead of falling back
    /// to the monorepo layout.
    Unavailable(String),
}

/// Registry of tenant repositories (derived from forge configuration and the naming convention;
/// no database) plus the disposable clone cache with one async lock per repository.
#[derive(Clone)]
pub struct TenantRepos {
    layout: TenantRepoLayout,
    cache_root: PathBuf,
    locks: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Last network sync of each tenant clone ([`Self::mark_synced`]), so read paths can reuse a
    /// recent fetch instead of contacting the forge on every request.
    synced: Arc<std::sync::Mutex<HashMap<String, std::time::Instant>>>,
}

impl std::fmt::Debug for TenantRepos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantRepos")
            .field("layout", &self.layout)
            .field("cache_root", &self.cache_root)
            .finish()
    }
}

impl TenantRepos {
    /// Monorepo layout (Local provider, D8).
    pub fn monorepo(cache_root: impl Into<PathBuf>) -> Self {
        Self::new(TenantRepoLayout::Monorepo, cache_root)
    }

    /// Dedicated per-tenant repositories on `forge`.
    pub fn dedicated(forge: TenantRepoForge, cache_root: impl Into<PathBuf>) -> Self {
        Self::new(TenantRepoLayout::Dedicated(Arc::new(forge)), cache_root)
    }

    /// A remote forge that cannot provision tenant repositories (fails closed with `reason`).
    pub fn unavailable(reason: impl Into<String>, cache_root: impl Into<PathBuf>) -> Self {
        Self::new(TenantRepoLayout::Unavailable(reason.into()), cache_root)
    }

    fn new(layout: TenantRepoLayout, cache_root: impl Into<PathBuf>) -> Self {
        Self {
            layout,
            cache_root: cache_root.into(),
            locks: Arc::default(),
            synced: Arc::default(),
        }
    }

    fn synced_map(&self) -> std::sync::MutexGuard<'_, HashMap<String, std::time::Instant>> {
        self.synced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a successful network sync of `tenant`'s clone.
    pub fn mark_synced(&self, tenant: &str) {
        self.synced_map()
            .insert(tenant.to_string(), std::time::Instant::now());
    }

    /// Forgets the last sync of `tenant`'s clone (after a merge), so the next read fetches.
    pub fn mark_stale(&self, tenant: &str) {
        self.synced_map().remove(tenant);
    }

    /// `true` when `tenant`'s clone was synced from the forge within `ttl`.
    pub fn synced_within(&self, tenant: &str, ttl: std::time::Duration) -> bool {
        self.synced_map()
            .get(tenant)
            .is_some_and(|at| at.elapsed() < ttl)
    }

    /// Default clone cache root for a platform working tree: the sibling directory
    /// `<workdir>-tenant-repos` (inside the same writable volume, outside the platform checkout).
    pub fn default_cache_root(repo_workdir: &Path) -> PathBuf {
        let name = repo_workdir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "keel-workdir".to_string());
        repo_workdir.with_file_name(format!("{name}-tenant-repos"))
    }

    pub fn layout(&self) -> &TenantRepoLayout {
        &self.layout
    }

    /// The forge configuration when tenants have their own repositories.
    pub fn forge(&self) -> Option<&Arc<TenantRepoForge>> {
        match &self.layout {
            TenantRepoLayout::Dedicated(forge) => Some(forge),
            TenantRepoLayout::Monorepo | TenantRepoLayout::Unavailable(_) => None,
        }
    }

    /// `true` when tenants have (or must have) their own repositories: every remote forge,
    /// including a misconfigured one (`TenantRepoLayout::Unavailable`). Only the Local Git
    /// provider keeps tenants in the platform repository.
    pub fn is_dedicated(&self) -> bool {
        !matches!(self.layout, TenantRepoLayout::Monorepo)
    }

    /// The forge for tenant repository work, or `503` with the configuration problem when the
    /// layout is `TenantRepoLayout::Unavailable`. `None` only for the monorepo layout.
    pub fn require_forge(&self) -> Result<Option<&Arc<TenantRepoForge>>, ServerError> {
        match &self.layout {
            TenantRepoLayout::Dedicated(forge) => Ok(Some(forge)),
            TenantRepoLayout::Monorepo => Ok(None),
            TenantRepoLayout::Unavailable(reason) => Err(ServerError::ServiceUnavailable(format!(
                "tenant repositories are unavailable: {reason}"
            ))),
        }
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Disposable clone of the tenant repository (`<cache_root>/<id>`).
    pub fn checkout_dir(&self, tenant: &str) -> Result<PathBuf, ServerError> {
        keel_core::validate_dns_label(tenant, "tenant")?;
        Ok(self.cache_root.join(tenant))
    }

    /// Serializes work on one tenant repository clone (keyed by tenant id; other keys, such as
    /// `<id>#create`, serialize other per-tenant work).
    pub async fn lock(&self, tenant: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks.entry(tenant.to_string()).or_default().clone()
        };
        lock.lock_owned().await
    }

    /// Clones (or fetches and resets to `origin/main`) the tenant repository into
    /// [`Self::checkout_dir`]. Callers hold [`Self::lock`]. `platform_remote` is the platform
    /// checkout's `origin` URL; the tenant repository is pushed through the same forge origin
    /// (the server's forge credentials, never the tenant read credential).
    pub async fn sync_checkout(
        &self,
        git: &GitRunner,
        platform_remote: &str,
        tenant: &str,
    ) -> Result<PathBuf, ServerError> {
        let forge = self.forge().ok_or_else(|| {
            ServerError::Conflict(format!(
                "tenant `{tenant}` lives in the platform repository (Local Git provider)"
            ))
        })?;
        let url = tenant_push_url(platform_remote, &forge.target(tenant)?)?;
        let dir = self.checkout_dir(tenant)?;
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        keel_git::ensure_workdir_from_remote(git, &url, &dir).await?;
        Ok(dir)
    }

    /// Read-only mirror of a tenant repository's `main` (`<cache_root>/.mirrors/<id>`), used to
    /// check other tenants' committed content (route hostnames) while a proposal holds its own
    /// tenant's lock. It has its own lock (`<id>#mirror`, held only for the fetch), so reading
    /// another tenant never waits for that tenant's write lock (no lock-order deadlock between
    /// two tenants' proposals). Disposable like every clone in the cache.
    pub fn mirror_dir(&self, tenant: &str) -> Result<PathBuf, ServerError> {
        keel_core::validate_dns_label(tenant, "tenant")?;
        Ok(self.cache_root.join(".mirrors").join(tenant))
    }

    /// Clones or fetches [`Self::mirror_dir`] (bounded by `timeout`) and returns it; read
    /// `refs/remotes/origin/main` through object reads only. Errors are returned (callers fail
    /// closed); a clone that failed half-way is removed so the next attempt starts over.
    pub async fn sync_mirror(
        &self,
        git: &GitRunner,
        platform_remote: &str,
        tenant: &str,
        timeout: std::time::Duration,
    ) -> Result<PathBuf, ServerError> {
        let forge = self.forge().ok_or_else(|| {
            ServerError::Conflict(format!(
                "tenant `{tenant}` lives in the platform repository (Local Git provider)"
            ))
        })?;
        let url = tenant_push_url(platform_remote, &forge.target(tenant)?)?;
        let dir = self.mirror_dir(tenant)?;
        let _guard = self.lock(&format!("{tenant}#mirror")).await;
        let existing = tokio::fs::try_exists(dir.join(".git"))
            .await
            .unwrap_or(false);
        let work = async {
            if existing {
                if git.get_remote_url(&dir, "origin").await.ok().as_deref() != Some(url.as_str()) {
                    git.set_remote_url(&dir, "origin", &url).await?;
                }
                git.fetch(&dir, "origin").await
            } else {
                if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                    tokio::fs::remove_dir_all(&dir).await?;
                }
                if let Some(parent) = dir.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                git.clone_branch(&url, "main", &dir).await
            }
        };
        let outcome = match tokio::time::timeout(timeout, work).await {
            Ok(res) => res.map_err(ServerError::from),
            Err(_) => Err(ServerError::ServiceUnavailable(format!(
                "tenant `{tenant}` repository fetch timed out after {}s",
                timeout.as_secs()
            ))),
        };
        if outcome.is_err() && !existing {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        outcome.map(|()| dir)
    }
}

/// URL the server pushes a tenant repository through: the platform remote's origin with the
/// tenant repository path (`<origin>/<owner>/<name>.git`). A local bare-repository remote
/// (development, tests) maps to the sibling `<parent>/<owner>/<name>.git`, the layout of
/// [`keel_git::LocalGitProvider`]'s per-repository mode.
pub fn tenant_push_url(
    platform_remote: &str,
    target: &RepositoryTarget,
) -> Result<String, ServerError> {
    let trimmed = platform_remote.trim();
    if !trimmed.contains("://") && !trimmed.contains('@') && Path::new(trimmed).is_absolute() {
        let parent = Path::new(trimmed).parent().ok_or_else(|| {
            ServerError::Internal("platform repository path has no parent".to_string())
        })?;
        return Ok(parent
            .join(&target.owner)
            .join(format!("{}.git", target.name))
            .to_string_lossy()
            .into_owned());
    }
    let slug = keel_git::parse_remote_repo_url(platform_remote)?;
    if !(slug.origin_url.starts_with("https://") || slug.origin_url.starts_with("http://")) {
        return Err(ServerError::Internal(format!(
            "cannot derive a tenant repository URL from the platform remote origin `{}`",
            slug.origin_url
        )));
    }
    Ok(format!(
        "{}/{}/{}.git",
        slug.origin_url.trim_end_matches('/'),
        target.owner,
        target.name
    ))
}

/// What the monorepo `tenants/<id>/git-repository.yaml` stub says about a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantStubState {
    /// No stub on `main`.
    Absent,
    /// The stub points at the expected tenant repository.
    Dedicated,
    /// `tenants/<id>/` exists but is not (or not this) dedicated-repository stub.
    Foreign(String),
}

/// Classifies the committed stub `git-repository.yaml` against `expected` (the URL is the only
/// platform-side record of the tenant repository).
pub fn classify_tenant_stub(
    manifest: Option<&str>,
    dir_exists: bool,
    expected: &TenantRepoSource,
) -> TenantStubState {
    let Some(manifest) = manifest else {
        return if dir_exists {
            TenantStubState::Foreign("tenant directory exists without a Git source stub".into())
        } else {
            TenantStubState::Absent
        };
    };
    let url = keel_core::split_yaml_documents(manifest)
        .into_iter()
        .filter_map(|d| noyalib::from_str::<Value>(&d).ok())
        .find(|v| v.get("kind").and_then(Value::as_str) == Some("GitRepository"))
        .and_then(|v| {
            v.pointer("/spec/url")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let secret = keel_core::split_yaml_documents(manifest)
        .into_iter()
        .filter_map(|d| noyalib::from_str::<Value>(&d).ok())
        .find_map(|v| {
            v.pointer("/spec/secretRef/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    match url {
        Some(url)
            if url == expected.url()
                && secret.as_deref() == Some(keel_kube::TENANT_GITOPS_CREDENTIALS_SECRET_NAME) =>
        {
            TenantStubState::Dedicated
        }
        Some(url) => TenantStubState::Foreign(format!("its Git source points at `{url}`")),
        None => TenantStubState::Foreign("its Git source stub has no URL".into()),
    }
}

/// Forge-side state of a tenant repository after [`provision_forge`].
#[derive(Debug, Clone)]
pub struct ForgeProvisioning {
    pub repository: RepositoryInfo,
    pub main_protection: BranchProtectionOutcome,
    pub forks: ForkPolicyOutcome,
    /// Deploy keys removed because Keel did not add them (HTTPS forges never use deploy keys).
    pub removed_deploy_keys: usize,
}

/// Idempotently creates (or verifies and adopts) the private, auto-initialised tenant
/// repository, protects `main`, disables forks where the forge allows it per repository and, on
/// HTTPS forges, removes every deploy key (Keel never adds one there, so any key is foreign).
pub async fn provision_forge(
    forge: &TenantRepoForge,
    tenant: &str,
) -> Result<ForgeProvisioning, ServerError> {
    let target = forge.target(tenant)?;
    let opts = RepositoryOptions {
        description: Some(TENANT_REPO_DESCRIPTION.to_string()),
        ..RepositoryOptions::default()
    };
    let step = |what: &'static str| {
        move |e: ForgeError| provisioning_error(tenant, &target_label(forge, tenant), what, e)
    };
    let repository = forge
        .admin
        .ensure_named_repository(&target.owner, &target.name, &opts)
        .await
        .map_err(step("repository creation"))?;
    let main_protection = forge
        .admin
        .ensure_main_protection(&target)
        .await
        .map_err(step("`main` branch protection"))?;
    let forks = forge
        .admin
        .disable_forks(&target)
        .await
        .map_err(step("fork policy"))?;
    let mut removed_deploy_keys = 0;
    if forge.credential_kind() == RepoCredentialKind::HttpsToken {
        for key in forge
            .admin
            .list_deploy_keys(&target)
            .await
            .map_err(step("deploy key audit"))?
        {
            forge
                .admin
                .revoke_deploy_key(&target, &key.id)
                .await
                .map_err(step("deploy key removal"))?;
            removed_deploy_keys += 1;
        }
    }
    Ok(ForgeProvisioning {
        repository,
        main_protection,
        forks,
        removed_deploy_keys,
    })
}

/// `owner/name` of the tenant repository (the tenant id when the name is invalid).
pub fn target_label(forge: &TenantRepoForge, tenant: &str) -> String {
    forge
        .target(tenant)
        .map(|t| t.full_name())
        .unwrap_or_else(|_| tenant.to_string())
}

/// Maps a forge failure during provisioning to an actionable API error (never a secret).
pub fn provisioning_error(tenant: &str, repo: &str, step: &str, err: ForgeError) -> ServerError {
    let msg =
        format!("tenant `{tenant}` repository `{repo}`: {step} failed: {err}; retry the request");
    match err {
        ForgeError::InvalidInput(_) => ServerError::Conflict(msg),
        _ => ServerError::ServiceUnavailable(msg),
    }
}

/// A freshly issued read-only credential for a tenant repository.
#[derive(Debug)]
pub struct IssuedTenantCredential {
    pub credential: TenantRepoCredential,
    /// Forge id (token id / deploy key id), recorded in the Secret for later revocation.
    pub id: String,
    /// SHA-256 fingerprint of the deploy key (SSH only).
    pub fingerprint: Option<String>,
}

/// Issues a new read-only credential that works on the tenant repository only: a Forgejo bot
/// token restricted to the repository, a GitLab project deploy token, or a GitHub read-only
/// deploy key with `known_hosts` from the forge's published host keys.
pub async fn issue_credential(
    forge: &TenantRepoForge,
    tenant: &str,
) -> Result<IssuedTenantCredential, ServerError> {
    let target = forge.target(tenant)?;
    let label = target.full_name();
    let err = |what: &'static str| {
        let label = label.clone();
        move |e: ForgeError| provisioning_error(tenant, &label, what, e)
    };
    match forge.kind {
        TenantRepoForgeKind::Forgejo | TenantRepoForgeKind::GitLab => {
            let minter = match forge.kind {
                TenantRepoForgeKind::Forgejo => token_minter(forge, tenant)?,
                _ => forge.admin.clone(),
            };
            let token = minter
                .create_repo_read_token(&target, &forge.token_name(tenant)?)
                .await
                .map_err(err("read token minting"))?;
            Ok(IssuedTenantCredential {
                id: token.id.clone(),
                fingerprint: None,
                credential: TenantRepoCredential::HttpsToken {
                    username: token.username.clone(),
                    token: token.token.clone(),
                    ca_crt: forge.ca_crt.clone(),
                },
            })
        }
        TenantRepoForgeKind::GitHub => {
            let TenantRepoCloneBase::Ssh { host, port } = &forge.clone_base else {
                return Err(ServerError::Internal(
                    "GitHub tenant repositories need an SSH clone base".to_string(),
                ));
            };
            let known_hosts = forge_known_hosts(forge, host, *port)
                .await
                .map_err(err("SSH host key lookup"))?;
            let title = format!("{TENANT_DEPLOY_KEY_TITLE_PREFIX}{tenant}");
            let pair = generate_ed25519_key_pair(&title)?;
            let ensured = forge
                .admin
                .ensure_deploy_key(&target, &title, &pair.public_key_openssh, true)
                .await
                .map_err(err("deploy key registration"))?;
            Ok(IssuedTenantCredential {
                id: ensured.key.id.clone(),
                fingerprint: Some(pair.fingerprint_sha256.clone()),
                credential: TenantRepoCredential::SshDeployKey {
                    private_key: pair.private_key_openssh.to_string(),
                    public_key: pair.public_key_openssh.clone(),
                    known_hosts,
                },
            })
        }
    }
}

/// `known_hosts` file for the platform forge's SSH endpoint `host:port`, from the host keys the
/// forge publishes (GitHub `GET /meta`; never trust-on-first-use). Public data.
async fn forge_known_hosts(
    forge: &TenantRepoForge,
    host: &str,
    port: u16,
) -> Result<String, ForgeError> {
    let host_keys = forge.admin.ssh_host_keys().await?;
    let refs: Vec<&str> = host_keys.iter().map(String::as_str).collect();
    known_hosts_file(host, port, &refs)
}

/// Writes `keel-system/keel-pko-ssh-known-hosts`, the public SSH host keys the PKO operator
/// trusts when it tracks the `main` branch of SSH (GitHub) tenant repositories (mounted
/// statically through `SSH_KNOWN_HOSTS`, `deploy/seed/core-addons/pko.yaml`). The keys come from
/// the **platform** forge configuration (`known_hosts`, or the forge's published host keys),
/// never from a Secret in a tenant namespace. Every tenant repository lives on the one platform
/// forge, so the ConfigMap always holds exactly that host's current keys (rewritten on every
/// credential reconcile, so `keel tenant repair-credentials` also refreshes rotated keys).
/// No-op for HTTPS forges.
pub async fn ensure_pko_operator_known_hosts(
    forge: &TenantRepoForge,
    tenant: &str,
    known_hosts: Option<&str>,
    secrets: &dyn TenantCredentialSecrets,
) -> Result<(), ServerError> {
    let source = forge.source(tenant)?;
    if !pko_operator_needs_ssh_known_hosts(std::slice::from_ref(&source)) {
        return Ok(());
    }
    let known_hosts = match (known_hosts, &forge.clone_base) {
        (Some(known_hosts), _) => known_hosts.to_string(),
        (None, TenantRepoCloneBase::Ssh { host, port }) => {
            forge_known_hosts(forge, host, *port).await.map_err(|e| {
                provisioning_error(
                    tenant,
                    &forge
                        .target(tenant)
                        .map(|t| t.full_name())
                        .unwrap_or_default(),
                    "SSH host key lookup",
                    e,
                )
            })?
        }
        (None, _) => {
            return Err(ServerError::Internal(
                "SSH tenant repositories need an SSH clone base".to_string(),
            ));
        }
    };
    let doc = build_pko_operator_known_hosts_configmap(&[(&source, known_hosts.as_str())])?;
    let config_map: ConfigMap = serde_json::from_value(doc)
        .map_err(|e| ServerError::Internal(format!("PKO known_hosts ConfigMap: {e}")))?;
    secrets.put_pko_operator_known_hosts(config_map).await
}

/// Revokes a credential issued by [`issue_credential`] (missing is success).
pub async fn revoke_credential(
    forge: &TenantRepoForge,
    tenant: &str,
    credential_id: &str,
) -> Result<(), ServerError> {
    let target = forge.target(tenant)?;
    let label = target.full_name();
    let result = match forge.kind {
        TenantRepoForgeKind::Forgejo => match forge.token_minter.as_ref() {
            Some(minter) => minter.revoke_repo_read_token(&target, credential_id).await,
            None => return Ok(()),
        },
        TenantRepoForgeKind::GitLab => {
            forge
                .admin
                .revoke_repo_read_token(&target, credential_id)
                .await
        }
        TenantRepoForgeKind::GitHub => forge.admin.revoke_deploy_key(&target, credential_id).await,
    };
    result.map_err(|e| provisioning_error(tenant, &label, "credential revocation", e))
}

/// Revokes every read credential of the tenant repository except the one in use: HTTPS forges
/// list the live repository read tokens (Forgejo: the tenant-reader bot's tokens restricted to
/// this repository; GitLab: every active project deploy token) and revoke all but `keep_id`;
/// GitHub removes every deploy key except `keep_fingerprint`. With `None` everything is
/// revoked (tenant deletion, re-creation of a deleted id). Returns the number revoked.
pub async fn retire_stray_credentials(
    forge: &TenantRepoForge,
    tenant: &str,
    keep_id: Option<&str>,
    keep_fingerprint: Option<&str>,
) -> Result<usize, ServerError> {
    let target = forge.target(tenant)?;
    let label = target.full_name();
    let audit = |e| provisioning_error(tenant, &label, "credential audit", e);
    let removal = |e| provisioning_error(tenant, &label, "credential revocation", e);
    let mut removed = 0;
    match forge.kind {
        TenantRepoForgeKind::GitHub => {
            for key in forge.admin.list_deploy_keys(&target).await.map_err(audit)? {
                if keep_fingerprint.is_some() && key.fingerprint.as_deref() == keep_fingerprint {
                    continue;
                }
                forge
                    .admin
                    .revoke_deploy_key(&target, &key.id)
                    .await
                    .map_err(removal)?;
                removed += 1;
            }
        }
        TenantRepoForgeKind::Forgejo | TenantRepoForgeKind::GitLab => {
            let provider = match forge.kind {
                TenantRepoForgeKind::Forgejo => token_minter(forge, tenant)?,
                _ => forge.admin.clone(),
            };
            for token in provider
                .list_repo_read_tokens(&target)
                .await
                .map_err(audit)?
            {
                if Some(token.id.as_str()) == keep_id {
                    continue;
                }
                provider
                    .revoke_repo_read_token(&target, &token.id)
                    .await
                    .map_err(removal)?;
                removed += 1;
            }
        }
    }
    if removed > 0 {
        tracing::info!(
            target: "keel::audit",
            action = "tenant.credential.retire",
            tenant,
            removed,
            "Revoked tenant repository read credentials that are not in use"
        );
    }
    Ok(removed)
}

/// Revokes every read credential of the tenant repository ([`retire_stray_credentials`] with
/// nothing kept). Used when a tenant is deleted and before a deleted id is created again.
pub async fn revoke_all_credentials(
    forge: &TenantRepoForge,
    tenant: &str,
) -> Result<usize, ServerError> {
    retire_stray_credentials(forge, tenant, None, None).await
}

fn token_minter(
    forge: &TenantRepoForge,
    tenant: &str,
) -> Result<Arc<dyn ForgeProvider>, ServerError> {
    forge.token_minter.clone().ok_or_else(|| {
        ServerError::ServiceUnavailable(format!(
            "tenant `{tenant}` repository `{}`: the Forgejo tenant-reader bot credentials (`{}/{}`) are missing; re-run `keel bootstrap init`",
            target_label(forge, tenant),
            keel_core::KEEL_SYSTEM_NAMESPACE,
            keel_core::TENANT_REPO_READER_SECRET_NAME
        ))
    })
}

/// Storage of a tenant's cluster-only `keel-gitops-credentials` Secret: keel-server writes it
/// through the kube-rs [`SecretStateStore`], Stage 0 through `kubectl`.
#[async_trait::async_trait]
pub trait TenantCredentialSecrets: Send + Sync {
    /// The live Secret, if any. Callers never log or return its data.
    async fn read(&self, tenant: &str) -> Result<Option<Secret>, ServerError>;
    /// Creates the Secret only when it does not exist; `true` when this call created it.
    async fn create_if_absent(&self, tenant: &str, secret: Secret) -> Result<bool, ServerError>;
    /// Replaces an unusable Secret.
    async fn replace(&self, tenant: &str, secret: Secret) -> Result<(), ServerError>;
    /// Creates or replaces `keel-system/keel-pko-ssh-known-hosts` (public host keys only,
    /// [`ensure_pko_operator_known_hosts`]).
    async fn put_pko_operator_known_hosts(&self, config_map: ConfigMap) -> Result<(), ServerError>;
}

/// [`TenantCredentialSecrets`] backed by a [`SecretStateStore`] (keel-server).
pub struct StoreCredentialSecrets<'a>(pub &'a dyn SecretStateStore);

#[async_trait::async_trait]
impl TenantCredentialSecrets for StoreCredentialSecrets<'_> {
    async fn read(&self, tenant: &str) -> Result<Option<Secret>, ServerError> {
        Ok(self.0.get_tenant_gitops_credentials(tenant).await?)
    }
    async fn create_if_absent(&self, tenant: &str, secret: Secret) -> Result<bool, ServerError> {
        Ok(self
            .0
            .create_tenant_gitops_credentials_if_absent(tenant, secret)
            .await?)
    }
    async fn replace(&self, tenant: &str, secret: Secret) -> Result<(), ServerError> {
        Ok(self.0.put_tenant_gitops_credentials(tenant, secret).await?)
    }
    async fn put_pko_operator_known_hosts(&self, config_map: ConfigMap) -> Result<(), ServerError> {
        Ok(self.0.put_pko_operator_known_hosts(config_map).await?)
    }
}

/// SHA-256 fingerprint of the deploy key a valid SSH credentials Secret uses (`identity.pub`).
fn stored_deploy_key_fingerprint(secret: Option<&Secret>) -> Option<String> {
    secret
        .and_then(|s| s.data.as_ref())
        .and_then(|d| d.get(keel_kube::SSH_IDENTITY_PUB_KEY))
        .and_then(|v| std::str::from_utf8(&v.0).ok())
        .and_then(|k| ssh_public_key_fingerprint(k).ok())
}

/// Reconciles the tenant's repository read credential (shared by keel-server and Stage 0):
///
/// * a Secret that is valid for exactly this repository, records the forge credential id and,
///   for SSH, a parsable `identity.pub`, is kept ([`StepOutcome::Existed`]);
/// * otherwise a new credential is issued and stored: create-if-absent when there was no Secret
///   (a concurrent writer that won keeps its credential and ours is revoked again), replace when
///   the Secret was unusable ([`StepOutcome::Rotated`]); a credential whose Secret write fails is
///   revoked again;
/// * finally every other read credential of the repository is revoked
///   (`retire_stray_credentials`), including ones whose Secret was lost.
///
/// Never logs or returns secret values.
pub async fn reconcile_tenant_credential(
    forge: &TenantRepoForge,
    tenant: &str,
    secrets: &dyn TenantCredentialSecrets,
) -> Result<StepOutcome, ServerError> {
    store_tenant_credential(forge, tenant, secrets, false).await
}

/// Re-mints the tenant's repository read credential even when the stored one is valid
/// (`keel tenant repair-credentials`): a fresh credential replaces the Secret (or creates it),
/// then the previous one and every other read credential of the repository are revoked. Returns
/// [`StepOutcome::Rotated`] (or [`StepOutcome::Created`] when there was no Secret). Never logs or
/// returns secret values.
pub async fn rotate_tenant_credential(
    forge: &TenantRepoForge,
    tenant: &str,
    secrets: &dyn TenantCredentialSecrets,
) -> Result<StepOutcome, ServerError> {
    store_tenant_credential(forge, tenant, secrets, true).await
}

/// [`store_credential_secret`], then (SSH forges) the PKO operator host keys
/// ([`ensure_pko_operator_known_hosts`]), reusing the host keys of a freshly issued credential.
async fn store_tenant_credential(
    forge: &TenantRepoForge,
    tenant: &str,
    secrets: &dyn TenantCredentialSecrets,
    force: bool,
) -> Result<StepOutcome, ServerError> {
    let (outcome, known_hosts) = store_credential_secret(forge, tenant, secrets, force).await?;
    ensure_pko_operator_known_hosts(forge, tenant, known_hosts.as_deref(), secrets).await?;
    Ok(outcome)
}

/// Keeps or (re-)issues the credential Secret; returns the outcome and, when this call issued an
/// SSH credential that was stored, its `known_hosts` (platform forge host keys).
async fn store_credential_secret(
    forge: &TenantRepoForge,
    tenant: &str,
    secrets: &dyn TenantCredentialSecrets,
    force: bool,
) -> Result<(StepOutcome, Option<String>), ServerError> {
    let source = forge.source(tenant)?;
    let existing = secrets.read(tenant).await?;
    let ssh = forge.credential_kind() == RepoCredentialKind::SshDeployKey;
    let (previous_id, outcome) = match tenant_gitops_credentials_status(&source, existing.as_ref())
    {
        TenantGitopsCredentialStatus::Valid { credential_id } if force => {
            (credential_id, StepOutcome::Rotated)
        }
        TenantGitopsCredentialStatus::Valid { credential_id } => {
            let fingerprint = stored_deploy_key_fingerprint(existing.as_ref());
            match (credential_id, ssh, fingerprint) {
                (Some(id), true, Some(fp)) => {
                    retire_stray_credentials(forge, tenant, Some(&id), Some(&fp)).await?;
                    return Ok((StepOutcome::Existed, None));
                }
                (Some(id), false, _) => {
                    retire_stray_credentials(forge, tenant, Some(&id), None).await?;
                    return Ok((StepOutcome::Existed, None));
                }
                (id, _, _) => {
                    tracing::warn!(
                        tenant,
                        "Rotating tenant repository credentials Secret without a usable credential id or deploy key"
                    );
                    (id, StepOutcome::Rotated)
                }
            }
        }
        TenantGitopsCredentialStatus::Missing => (None, StepOutcome::Created),
        TenantGitopsCredentialStatus::Invalid {
            credential_id,
            reason,
        } => {
            tracing::warn!(
                tenant,
                reason = %reason,
                "Rotating unusable tenant repository credentials Secret"
            );
            (credential_id, StepOutcome::Rotated)
        }
    };

    let issued = issue_credential(forge, tenant).await?;
    let stored = async {
        let secret = tenant_gitops_credentials_secret(&source, &issued.credential, &issued.id)?;
        if outcome == StepOutcome::Created {
            secrets.create_if_absent(tenant, secret).await
        } else {
            secrets.replace(tenant, secret).await.map(|()| true)
        }
    }
    .await;
    match stored {
        Ok(true) => {}
        Ok(false) => {
            // Another writer stored its credential first: keep it, revoke ours.
            revoke_credential(forge, tenant, &issued.id).await?;
            return Ok((StepOutcome::Existed, None));
        }
        Err(err) => {
            if let Err(revoke_err) = revoke_credential(forge, tenant, &issued.id).await {
                tracing::warn!(
                    tenant,
                    error = %revoke_err,
                    "Could not revoke a tenant repository credential whose Secret write failed"
                );
            }
            return Err(err);
        }
    }
    if let Some(previous) = previous_id.filter(|p| *p != issued.id)
        && let Err(err) = revoke_credential(forge, tenant, &previous).await
    {
        tracing::warn!(tenant, error = %err, "Could not revoke the previous tenant repository credential");
    }
    retire_stray_credentials(
        forge,
        tenant,
        Some(&issued.id),
        issued.fingerprint.as_deref(),
    )
    .await?;
    tracing::info!(
        target: "keel::audit",
        action = "tenant.credential.issue",
        tenant,
        outcome = ?outcome,
        "Stored tenant repository read credential"
    );
    let known_hosts = match issued.credential {
        TenantRepoCredential::SshDeployKey { known_hosts, .. } => Some(known_hosts),
        _ => None,
    };
    Ok((outcome, known_hosts))
}

/// Outcome of a runtime provisioning step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StepOutcome {
    /// Waiting for the tenant namespace (created when `keel-tenants` applies the merged stub).
    PendingNamespace,
    Created,
    Existed,
    /// An unusable credential Secret was replaced with a fresh credential.
    Rotated,
    /// Not used by this layout (Local Git provider: no tenant repository credential).
    NotApplicable,
}

/// Runtime (cluster) state of a tenant after [`complete_cluster_provisioning`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterProvisioning {
    pub namespace_ready: bool,
    pub sops_age: StepOutcome,
    pub credential: StepOutcome,
}

/// Mandatory `age` keypair generation (fails closed when `age-keygen` is unavailable).
pub async fn generate_tenant_age_key(tenant: &str) -> Result<TenantAgeKey, ServerError> {
    let keypair = SopsManager::new()
        .generate_age_keypair()
        .await
        .map_err(|e| {
            ServerError::ServiceUnavailable(format!(
                "cannot generate the SOPS age key of tenant `{tenant}` ({e}); `age-keygen` must be installed on the keel-server host"
            ))
        })?;
    Ok(TenantAgeKey::new(
        keypair.public_key.clone(),
        keypair.secret_key(),
    ))
}

/// Completes the cluster side of a tenant once `tenant-<id>` exists: the mandatory
/// `sops-age` Secret (create-if-absent, never replaced) and, for dedicated repositories, the
/// `keel-gitops-credentials` Secret ([`reconcile_tenant_credential`]). Before the namespace
/// exists every step is [`StepOutcome::PendingNamespace`]. Serialized per tenant (the creation
/// request and the post-merge task may run at the same time). Idempotent; never logs or returns
/// secret values.
pub async fn complete_cluster_provisioning(
    store: &dyn SecretStateStore,
    repos: &TenantRepos,
    tenant: &str,
) -> Result<ClusterProvisioning, ServerError> {
    let namespace = keel_kube::tenant_namespace(tenant)?;
    let _cluster_guard = repos.lock(&format!("{tenant}#cluster")).await;
    if !store.namespace_exists(&namespace).await? {
        return Ok(ClusterProvisioning {
            namespace_ready: false,
            sops_age: StepOutcome::PendingNamespace,
            credential: if repos.is_dedicated() {
                StepOutcome::PendingNamespace
            } else {
                StepOutcome::NotApplicable
            },
        });
    }

    let sops_age = match store.get_tenant_age_key(tenant).await? {
        Some(_) => StepOutcome::Existed,
        None => {
            let key = generate_tenant_age_key(tenant).await?;
            if store.create_tenant_age_key_if_absent(tenant, &key).await? {
                StepOutcome::Created
            } else {
                StepOutcome::Existed
            }
        }
    };

    let credential = match repos.require_forge()? {
        Some(forge) => {
            reconcile_tenant_credential(forge, tenant, &StoreCredentialSecrets(store)).await?
        }
        None => StepOutcome::NotApplicable,
    };
    Ok(ClusterProvisioning {
        namespace_ready: true,
        sops_age,
        credential,
    })
}

/// Repairs a provisioned tenant's runtime credentials (`POST /api/v1/tenants/{id}/credentials/repair`,
/// `keel tenant repair-credentials`): re-mints the repository read credential and its
/// `tenant-<id>/keel-gitops-credentials` Secret ([`rotate_tenant_credential`]) and creates a missing
/// `sops-age` Secret (an existing key is never replaced: it decrypts the tenant's committed
/// secrets). Fails with `400` when tenants have no dedicated repository (Local Git provider), with
/// `409` while `tenant-<id>` does not exist, and with `409` when `platform_stub` (the committed
/// platform `main`, read under the lock) no longer holds this tenant's dedicated-repository stub: a
/// merged tenant removal leaves `tenant-<id>` behind until `keel-tenants` prunes it, and no
/// credential may be minted for a deleted tenant. Serialized (`<id>#cluster`) with
/// [`complete_cluster_provisioning`] and the post-merge [`revoke_all_credentials`] of a removal.
pub async fn repair_tenant_credentials<F, Fut>(
    store: &dyn SecretStateStore,
    repos: &TenantRepos,
    tenant: &str,
    platform_stub: F,
) -> Result<ClusterProvisioning, ServerError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<TenantStubState, ServerError>>,
{
    let namespace = keel_kube::tenant_namespace(tenant)?;
    let Some(forge) = repos.require_forge()? else {
        return Err(ServerError::BadRequest(
            "tenants have no dedicated repository or read credential with the Local Git provider"
                .to_string(),
        ));
    };
    let _cluster_guard = repos.lock(&format!("{tenant}#cluster")).await;
    match platform_stub().await? {
        TenantStubState::Dedicated => {}
        TenantStubState::Absent => {
            return Err(ServerError::Conflict(format!(
                "tenant `{tenant}` does not exist or is being deleted (no tenant stub on the platform repository `main`)"
            )));
        }
        TenantStubState::Foreign(why) => {
            return Err(ServerError::Conflict(format!(
                "tenant `{tenant}` is not a dedicated-repository tenant ({why})"
            )));
        }
    }
    if !store.namespace_exists(&namespace).await? {
        return Err(ServerError::Conflict(format!(
            "`{namespace}` does not exist yet; merge the tenant's stub proposal first"
        )));
    }
    let sops_age = match store.get_tenant_age_key(tenant).await? {
        Some(_) => StepOutcome::Existed,
        None => {
            let key = generate_tenant_age_key(tenant).await?;
            if store.create_tenant_age_key_if_absent(tenant, &key).await? {
                StepOutcome::Created
            } else {
                StepOutcome::Existed
            }
        }
    };
    let credential =
        rotate_tenant_credential(forge, tenant, &StoreCredentialSecrets(store)).await?;
    Ok(ClusterProvisioning {
        namespace_ready: true,
        sops_age,
        credential,
    })
}

/// Tenant id of a monorepo tenant-creation proposal branch (`keel/<id>/create-tenant-<ts>`).
pub fn create_tenant_branch_tenant(branch: &str) -> Option<&str> {
    branch_tenant_for_action(branch, CREATE_TENANT_ACTION)
}

/// Tenant id of a tenant-deletion proposal branch (`keel/<id>/delete-tenant-<ts>`).
pub fn delete_tenant_branch_tenant(branch: &str) -> Option<&str> {
    branch_tenant_for_action(branch, DELETE_TENANT_ACTION)
}

fn branch_tenant_for_action<'a>(branch: &'a str, action: &str) -> Option<&'a str> {
    let rest = branch.strip_prefix("keel/")?;
    let (tenant, rest) = rest.split_once('/')?;
    let ts = rest.strip_prefix(action)?.strip_prefix('-')?;
    (!ts.is_empty() && ts.chars().all(|c| c.is_ascii_digit())).then_some(tenant)
}

/// Attempts / interval of [`spawn_post_merge_completion`] (about ten minutes).
const POST_MERGE_ATTEMPTS: u32 = 120;
const POST_MERGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// After a tenant-creation proposal merges, `keel-tenants` creates `tenant-<id>` (and the
/// keel-server RoleBinding) within about a minute. This one-shot task (not a controller: it stops
/// after success or ~10 minutes) then writes the tenant's runtime Secrets through
/// [`complete_cluster_provisioning`]. Re-running `POST /api/v1/tenants` with the same id
/// completes the same steps when the task could not (restart, slow reconcile, forge UI merge).
pub fn spawn_post_merge_completion(
    store: Arc<dyn SecretStateStore>,
    repos: TenantRepos,
    tenant: String,
) {
    tokio::spawn(async move {
        let mut last_error = None;
        for attempt in 0..POST_MERGE_ATTEMPTS {
            match complete_cluster_provisioning(store.as_ref(), &repos, &tenant).await {
                Ok(report) if report.namespace_ready => {
                    tracing::info!(
                        tenant = %tenant,
                        sops_age = ?report.sops_age,
                        credential = ?report.credential,
                        "Completed tenant runtime provisioning after the tenant stub merged"
                    );
                    return;
                }
                Ok(_) => {}
                Err(err) => last_error = Some(err.to_string()),
            }
            if attempt + 1 < POST_MERGE_ATTEMPTS {
                tokio::time::sleep(POST_MERGE_INTERVAL).await;
            }
        }
        tracing::warn!(
            tenant = %tenant,
            error = last_error.as_deref().unwrap_or("tenant namespace never appeared"),
            "Tenant runtime provisioning did not complete; re-run POST /api/v1/tenants with the same id"
        );
    });
}

/// Bound of one tenant repository mirror fetch on the workload listing paths.
pub const MANIFEST_MIRROR_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// A tenant repository mirror fetched more recently than this is not fetched again by the
/// workload listings (`GET /api/v1/stacks`, `GET /api/v1/releases`), which clients poll.
pub const MANIFEST_MIRROR_TTL: std::time::Duration = std::time::Duration::from_secs(15);

/// `crate::state::TenantManifestSource` over the read-only tenant repository mirrors
/// ([`TenantRepos::sync_mirror`]): each tenant repository's committed `main` is fetched at most
/// once per `MANIFEST_MIRROR_TTL` (bounded concurrency; a failed fetch is retried only after
/// an exponential backoff, the last snapshot being served meanwhile) and its `tenants/<id>/` YAML is read
/// through object reads only, cached per commit. Only files under `tenants/<id>/` of tenant
/// `<id>`'s own repository are returned. A disposable in-memory cache: keel-server stays
/// stateless.
#[derive(Clone)]
pub struct TenantRepoManifests {
    repos: TenantRepos,
    git: GitRunner,
    platform_workdir: PathBuf,
    /// Successful fetches are not repeated within this window.
    ttl: std::time::Duration,
    /// Per tenant: the last mirror fetch attempt, successful or not.
    fetched: Arc<std::sync::Mutex<HashMap<String, FetchAttempt>>>,
    /// Per tenant: the `main` commit and its `tenants/<id>/` YAML.
    snapshots: Arc<std::sync::Mutex<HashMap<String, TenantSnapshot>>>,
}

/// A tenant repository's `main` commit and its `tenants/<id>/` YAML.
type TenantSnapshot = (String, Arc<crate::state::ManifestTree>);

/// Longest wait before a tenant repository mirror that keeps failing is fetched again.
pub const MANIFEST_MIRROR_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

/// The last mirror fetch attempt of one tenant repository.
#[derive(Debug, Clone, Copy)]
struct FetchAttempt {
    at: std::time::Instant,
    /// Consecutive failed attempts (`0` after a successful fetch).
    failures: u32,
}

impl FetchAttempt {
    /// Window in which the mirror is not fetched again: `ttl` after a success, else
    /// `ttl × 2^(failures-1)` capped at [`MANIFEST_MIRROR_MAX_BACKOFF`] (never shorter than `ttl`),
    /// so an unreachable repository is not retried by every listing poll.
    fn window(&self, ttl: std::time::Duration) -> std::time::Duration {
        if self.failures == 0 {
            return ttl;
        }
        let factor = 1u32 << (self.failures - 1).min(16);
        ttl.saturating_mul(factor)
            .min(MANIFEST_MIRROR_MAX_BACKOFF)
            .max(ttl)
    }

    fn current(&self, ttl: std::time::Duration) -> bool {
        self.at.elapsed() < self.window(ttl)
    }
}

impl std::fmt::Debug for TenantRepoManifests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantRepoManifests")
            .field("repos", &self.repos)
            .field("platform_workdir", &self.platform_workdir)
            .finish_non_exhaustive()
    }
}

impl TenantRepoManifests {
    pub fn new(repos: TenantRepos, git: GitRunner, platform_workdir: impl Into<PathBuf>) -> Self {
        Self {
            repos,
            git,
            platform_workdir: platform_workdir.into(),
            ttl: MANIFEST_MIRROR_TTL,
            fetched: Arc::default(),
            snapshots: Arc::default(),
        }
    }

    /// Overrides `MANIFEST_MIRROR_TTL` (tests).
    pub fn with_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.ttl = ttl;
        self
    }

    fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The committed `tenants/<tenant>/` YAML of `tenant`'s repository.
    async fn snapshot(
        &self,
        platform_remote: &str,
        tenant: &str,
    ) -> Result<Arc<crate::state::ManifestTree>, ServerError> {
        let dir = self.repos.mirror_dir(tenant)?;
        {
            // Concurrent listings wait for one fetch and then see it as current.
            let _fetch_guard = self.repos.lock(&format!("{tenant}#manifests")).await;
            let last = Self::lock(&self.fetched).get(tenant).copied();
            match last {
                Some(attempt) if attempt.current(self.ttl) && attempt.failures > 0 => {
                    return Err(ServerError::ServiceUnavailable(format!(
                        "tenant `{tenant}` repository fetch failed recently; retrying after backoff"
                    )));
                }
                Some(attempt)
                    if attempt.current(self.ttl)
                        && tokio::fs::try_exists(dir.join(".git"))
                            .await
                            .unwrap_or(false) => {}
                _ => {
                    let outcome = self
                        .repos
                        .sync_mirror(
                            &self.git,
                            platform_remote,
                            tenant,
                            MANIFEST_MIRROR_FETCH_TIMEOUT,
                        )
                        .await;
                    let failures = match (&outcome, last) {
                        (Ok(_), _) => 0,
                        (Err(_), Some(prev)) => prev.failures.saturating_add(1),
                        (Err(_), None) => 1,
                    };
                    Self::lock(&self.fetched).insert(
                        tenant.to_string(),
                        FetchAttempt {
                            at: std::time::Instant::now(),
                            failures,
                        },
                    );
                    outcome?;
                }
            }
        }
        let sha = self.git.rev_parse(&dir, "refs/remotes/origin/main").await?;
        keel_git::validate_commit_sha(&sha)?;
        if let Some((cached_sha, tree)) = Self::lock(&self.snapshots).get(tenant)
            && *cached_sha == sha
        {
            return Ok(tree.clone());
        }
        let prefix = format!("tenants/{tenant}/");
        let mut tree = crate::state::ManifestTree::new();
        for path in self
            .git
            .list_files_at_ref(&dir, &sha, &format!("tenants/{tenant}"))
            .await?
        {
            let is_yaml = path.ends_with(".yaml") || path.ends_with(".yml");
            if !is_yaml || !path.starts_with(&prefix) {
                continue;
            }
            if let Some(content) = self.git.read_file_at_ref(&dir, &sha, &path).await? {
                tree.insert(path, content);
            }
        }
        let tree = Arc::new(tree);
        Self::lock(&self.snapshots).insert(tenant.to_string(), (sha, tree.clone()));
        Ok(tree)
    }

    /// The last snapshot of `tenant` (whatever its age).
    fn last_snapshot(&self, tenant: &str) -> Option<Arc<crate::state::ManifestTree>> {
        Self::lock(&self.snapshots)
            .get(tenant)
            .map(|(_, tree)| tree.clone())
    }
}

#[async_trait::async_trait]
impl crate::state::TenantManifestSource for TenantRepoManifests {
    async fn tenant_manifests(
        &self,
        tenants: &[String],
    ) -> Vec<(String, Arc<crate::state::ManifestTree>)> {
        use futures::StreamExt as _;
        if self.repos.forge().is_none() || tenants.is_empty() {
            return Vec::new();
        }
        let platform_remote = match self
            .git
            .get_remote_url(&self.platform_workdir, "origin")
            .await
        {
            Ok(url) => url,
            Err(err) => {
                tracing::warn!(error = %err, "platform remote unavailable; tenant repository workloads are not listed");
                return tenants
                    .iter()
                    .filter_map(|t| self.last_snapshot(t).map(|tree| (t.clone(), tree)))
                    .collect();
            }
        };
        let mut out: Vec<(String, Arc<crate::state::ManifestTree>)> =
            futures::stream::iter(tenants.iter().cloned())
                .map(|tenant| {
                    let platform_remote = platform_remote.as_str();
                    async move {
                        match self.snapshot(platform_remote, &tenant).await {
                            Ok(tree) => Some((tenant, tree)),
                            Err(err) => {
                                tracing::warn!(tenant = %tenant, error = %err, "tenant repository unavailable for the workload listing; serving its last snapshot");
                                self.last_snapshot(&tenant).map(|tree| (tenant, tree))
                            }
                        }
                    }
                })
                .buffer_unordered(crate::proposal_repo::PROPOSAL_LIST_CONCURRENCY)
                .filter_map(futures::future::ready)
                .collect()
                .await;
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_git::LocalGitProvider;

    fn forge(kind: TenantRepoForgeKind, clone_base: TenantRepoCloneBase) -> TenantRepoForge {
        TenantRepoForge {
            naming: TenantRepoNaming {
                kind,
                owner: "keel-tenants".to_string(),
                monorepo_name: "infra-gitops".to_string(),
                clone_base,
            },
            ca_crt: None,
            admin: Arc::new(LocalGitProvider::new(std::env::temp_dir())),
            token_minter: None,
        }
    }

    #[test]
    fn tenant_repository_description_is_identical_for_every_tenant() {
        for marker in ["{", "}", "<", ">", "%", "$", "tenant-", "acme"] {
            assert!(
                !TENANT_REPO_DESCRIPTION.contains(marker),
                "{marker:?} in {TENANT_REPO_DESCRIPTION}"
            );
        }
    }

    #[test]
    fn repository_coordinates_follow_the_naming_convention() {
        let f = forge(
            TenantRepoForgeKind::Forgejo,
            TenantRepoCloneBase::Https {
                origin: "https://forgejo.keel.local:8888/".to_string(),
            },
        );
        assert_eq!(f.repo_name("acme").unwrap(), "infra-gitops-tenant-acme");
        assert_eq!(
            f.clone_url("acme").unwrap(),
            "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-acme.git"
        );
        assert_eq!(
            f.token_name("acme").unwrap(),
            "keel-tenant-repo-infra-gitops-tenant-acme"
        );
        assert_eq!(f.credential_kind(), RepoCredentialKind::HttpsToken);
        assert!(f.repo_name("Bad_Id").is_err());
        let long = "a".repeat(63);
        let mut f2 = f.clone();
        f2.naming.monorepo_name = "m".repeat(40);
        assert!(f2.repo_name(&long).is_err());

        let gh = forge(
            TenantRepoForgeKind::GitHub,
            TenantRepoCloneBase::Ssh {
                host: "github.com".to_string(),
                port: 22,
            },
        );
        assert_eq!(
            gh.clone_url("acme").unwrap(),
            "ssh://git@github.com/keel-tenants/infra-gitops-tenant-acme.git"
        );
        assert_eq!(
            gh.source("acme").unwrap().transport(),
            keel_kube::TenantRepoTransport::Ssh
        );
        assert_eq!(gh.credential_kind(), RepoCredentialKind::SshDeployKey);
    }

    #[test]
    fn push_url_reuses_the_platform_remote_origin() {
        let target = RepositoryTarget::new("keel-tenants", "infra-gitops-tenant-acme").unwrap();
        assert_eq!(
            tenant_push_url(
                "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
                &target
            )
            .unwrap(),
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel-tenants/infra-gitops-tenant-acme.git"
        );
        assert_eq!(
            tenant_push_url("/srv/git/infra.git", &target).unwrap(),
            "/srv/git/keel-tenants/infra-gitops-tenant-acme.git"
        );
        assert!(tenant_push_url("relative/infra.git", &target).is_err());
    }

    #[test]
    fn create_tenant_branches_are_recognised() {
        assert_eq!(
            create_tenant_branch_tenant("keel/acme/create-tenant-1700000000"),
            Some("acme")
        );
        assert_eq!(
            create_tenant_branch_tenant("keel/acme/create-tenant-"),
            None
        );
        assert_eq!(
            create_tenant_branch_tenant("keel/acme/delete-tenant-1"),
            None
        );
        assert_eq!(
            create_tenant_branch_tenant("keel/acme/create-tenant-x1"),
            None
        );
        assert_eq!(create_tenant_branch_tenant("feature/acme"), None);
    }

    #[test]
    fn default_cache_root_is_a_sibling_of_the_platform_checkout() {
        assert_eq!(
            TenantRepos::default_cache_root(Path::new("/var/lib/keel/work")),
            PathBuf::from("/var/lib/keel/work-tenant-repos")
        );
        let repos = TenantRepos::monorepo("/tmp/cache");
        assert!(!repos.is_dedicated());
        assert_eq!(
            repos.checkout_dir("acme").unwrap(),
            PathBuf::from("/tmp/cache/acme")
        );
        assert!(repos.checkout_dir("../x").is_err());
    }

    #[tokio::test]
    async fn one_lock_per_repository() {
        let repos = TenantRepos::monorepo("/tmp/cache");
        let a = repos.lock("acme").await;
        // Another repository is not blocked.
        let _b = repos.lock("beta").await;
        let repos2 = repos.clone();
        let waiter = tokio::spawn(async move {
            let _g = repos2.lock("acme").await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(a);
        waiter.await.unwrap();
    }

    #[test]
    fn failed_mirror_fetches_back_off_exponentially() {
        let ttl = std::time::Duration::from_secs(15);
        let attempt = |failures| FetchAttempt {
            at: std::time::Instant::now(),
            failures,
        };
        assert_eq!(attempt(0).window(ttl), ttl);
        assert_eq!(attempt(1).window(ttl), ttl);
        assert_eq!(attempt(2).window(ttl), ttl * 2);
        assert_eq!(attempt(3).window(ttl), ttl * 4);
        assert_eq!(attempt(40).window(ttl), MANIFEST_MIRROR_MAX_BACKOFF);
        assert!(attempt(1).current(ttl));
        assert!(!attempt(1).current(std::time::Duration::ZERO));
    }

    #[test]
    fn sync_freshness_is_tracked_per_tenant() {
        let repos = TenantRepos::monorepo("/tmp/cache");
        let ttl = std::time::Duration::from_secs(10);
        assert!(!repos.synced_within("acme", ttl));
        repos.mark_synced("acme");
        assert!(repos.synced_within("acme", ttl));
        assert!(!repos.synced_within("beta", ttl));
        assert!(!repos.synced_within("acme", std::time::Duration::ZERO));
        repos.mark_stale("acme");
        assert!(!repos.synced_within("acme", ttl));
    }

    #[test]
    fn mirrors_live_outside_the_tenant_clones() {
        let repos = TenantRepos::monorepo("/tmp/cache");
        assert_eq!(
            repos.mirror_dir("acme").unwrap(),
            PathBuf::from("/tmp/cache/.mirrors/acme")
        );
        assert!(repos.mirror_dir("../x").is_err());
    }

    #[test]
    fn stub_classification_uses_the_platform_written_url() {
        let f = forge(
            TenantRepoForgeKind::Forgejo,
            TenantRepoCloneBase::Https {
                origin: "https://forgejo.keel.local:8888".to_string(),
            },
        );
        let source = f.source("acme").unwrap();
        let stub = keel_core::yaml::to_string(
            &keel_kube::build_tenant_repo_git_repository(&source).unwrap(),
        )
        .unwrap();
        assert_eq!(
            classify_tenant_stub(Some(&stub), true, &source),
            TenantStubState::Dedicated
        );
        assert_eq!(
            classify_tenant_stub(None, false, &source),
            TenantStubState::Absent
        );
        assert!(matches!(
            classify_tenant_stub(None, true, &source),
            TenantStubState::Foreign(_)
        ));
        let platform_repo =
            stub.replace("keel-tenants/infra-gitops-tenant-acme", "keel/infra-gitops");
        assert!(matches!(
            classify_tenant_stub(Some(&platform_repo), true, &source),
            TenantStubState::Foreign(_)
        ));
    }
}
