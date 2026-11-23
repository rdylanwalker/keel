//! Repository behind a proposal scope (`SDD.md` §3.2, §6.2, §6.3).
//!
//! Every proposal route is scoped: `/api/v1/tenants/{scope}/proposals[/{pr}...]`. A scope is a
//! tenant id or the reserved [`PLATFORM_SCOPE`] (`management`, the platform repository: catalog
//! edits, management add-ons, tenant stubs). PR ids are per repository.
//!
//! * A tenant with its own repository (every remote forge) proposes, merges and previews in that
//!   repository only: a disposable clone under [`crate::TenantRepos::checkout_dir`], serialized by
//!   the per-repository lock, pushed with the server's forge credentials.
//! * The development-only Local Git provider (D8) keeps tenants in the platform repository: the
//!   tenant scope then filters the platform repository's PRs by tenant.
//! * A remote forge without usable tenant-repository configuration fails closed (`503`).
//!
//! Previews are keyed by a repository-scoped key ([`keel_pko::preview_key`]: `<tenant>-<n>` for
//! tenant repositories), labelled with [`keel_pko::LABEL_REPO`], so equal PR numbers of different
//! repositories never share a preview and GC resolves each PR in its own repository.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_git::{
    CreatePrRequest, ForgeError, ForgeProvider, PullRequestInfo, PullRequestState,
    parse_proposal_branch_tenant,
};
use serde::Serialize;
use tokio::sync::OwnedMutexGuard;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::{AppState, MANAGEMENT_TENANT};
use crate::tenant_repos::TenantRepoForge;

/// Scope of platform-repository proposals (reserved tenant id).
pub const PLATFORM_SCOPE: &str = MANAGEMENT_TENANT;

/// Branch every working tree (platform checkout, tenant repository clones) is kept on between
/// operations; proposal work always resets back to it.
pub const WORKTREE_BRANCH: &str = "main";

/// A proposal route scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProposalScope {
    /// The platform repository ([`PLATFORM_SCOPE`]); cluster administrators only.
    Platform,
    /// One tenant (its own repository, or its PRs in the platform repository with the Local Git
    /// provider).
    Tenant(String),
}

impl ProposalScope {
    /// Parses a `{tenant}` path segment.
    pub fn parse(raw: &str) -> Result<Self, ServerError> {
        keel_core::validate_dns_label(raw, "tenant")?;
        Ok(if raw == PLATFORM_SCOPE {
            Self::Platform
        } else {
            Self::Tenant(raw.to_string())
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Platform => PLATFORM_SCOPE,
            Self::Tenant(t) => t,
        }
    }

    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::Platform => None,
            Self::Tenant(t) => Some(t),
        }
    }

    /// Read access: the platform scope needs a cluster administrator, a tenant scope tenant
    /// access (member, tenant admin or cluster admin).
    pub fn require_access(&self, auth: &AuthSession) -> Result<(), ServerError> {
        match self {
            Self::Platform => auth.require_admin(),
            Self::Tenant(t) => auth.require_tenant_access(t),
        }
    }
}

/// Serializable identity of the repository a proposal lives in (never credentials).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalRepository {
    /// `platform` or `tenant-<id>` (also the preview `keel.io/repo` label).
    pub label: String,
    /// `owner/name` of a tenant's own forge repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
}

/// A PR together with the scope and repository it belongs to (`GET` proposal listings and every
/// proposal response), so clients can build the scoped routes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedProposal {
    #[serde(flatten)]
    pub pr: PullRequestInfo,
    /// Route scope (`{tenant}` of `/api/v1/tenants/{tenant}/proposals/{id}`).
    pub scope: String,
    pub repository: ProposalRepository,
}

impl std::ops::Deref for ScopedProposal {
    type Target = PullRequestInfo;
    fn deref(&self) -> &PullRequestInfo {
        &self.pr
    }
}

/// The Git repository (forge provider + local working tree) behind a proposal scope.
#[derive(Clone)]
pub struct ScopeRepo {
    pub scope: ProposalScope,
    /// PR operations of this repository.
    pub forge: Arc<dyn ForgeProvider>,
    /// Working tree: the platform checkout, or the tenant repository's disposable clone.
    pub workdir: PathBuf,
    /// The tenant repository forge configuration when this is a tenant's own repository.
    dedicated: Option<Arc<TenantRepoForge>>,
}

impl std::fmt::Debug for ScopeRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeRepo")
            .field("scope", &self.scope)
            .field("workdir", &self.workdir)
            .field("dedicated", &self.dedicated.is_some())
            .finish()
    }
}

impl ScopeRepo {
    /// The platform repository.
    pub fn platform(state: &AppState) -> Self {
        Self {
            scope: ProposalScope::Platform,
            forge: state.forge.clone(),
            workdir: state.repo_workdir.clone(),
            dedicated: None,
        }
    }

    /// The repository of `scope`; `503` for a tenant scope on a misconfigured remote forge.
    pub fn resolve(state: &AppState, scope: &ProposalScope) -> Result<Self, ServerError> {
        match scope {
            ProposalScope::Platform => Ok(Self::platform(state)),
            ProposalScope::Tenant(t) => Self::for_tenant(state, t),
        }
    }

    /// The repository holding `tenant`'s content: its own repository, or the platform repository
    /// for the development-only Local Git provider.
    pub fn for_tenant(state: &AppState, tenant: &str) -> Result<Self, ServerError> {
        keel_core::validate_dns_label(tenant, "tenant")?;
        if tenant == PLATFORM_SCOPE {
            return Err(ServerError::BadRequest(format!(
                "`{PLATFORM_SCOPE}` is the platform scope, not a tenant"
            )));
        }
        match state.tenant_repos.require_forge()? {
            Some(forge) => Ok(Self {
                scope: ProposalScope::Tenant(tenant.to_string()),
                forge: forge.repository_provider(tenant)?,
                workdir: state.tenant_repos.checkout_dir(tenant)?,
                dedicated: Some(forge.clone()),
            }),
            None => Ok(Self {
                scope: ProposalScope::Tenant(tenant.to_string()),
                forge: state.forge.clone(),
                workdir: state.repo_workdir.clone(),
                dedicated: None,
            }),
        }
    }

    pub fn tenant(&self) -> Option<&str> {
        self.scope.tenant()
    }

    /// `true` for a tenant's own repository (every tenant on a remote forge).
    pub fn is_dedicated(&self) -> bool {
        self.dedicated.is_some()
    }

    /// The tenant repository forge configuration (dedicated repositories only).
    pub fn tenant_forge(&self) -> Option<&Arc<TenantRepoForge>> {
        self.dedicated.as_ref()
    }

    /// Repository identity (`platform` or `tenant-<id>` + `owner/name`).
    pub fn repository(&self) -> ProposalRepository {
        match (&self.dedicated, self.tenant()) {
            (Some(forge), Some(tenant)) => ProposalRepository {
                label: format!("tenant-{tenant}"),
                full_name: forge.target(tenant).ok().map(|t| t.full_name()),
            },
            _ => ProposalRepository {
                label: keel_pko::PLATFORM_REPO_LABEL.to_string(),
                full_name: None,
            },
        }
    }

    /// Wraps `pr` with this scope and repository.
    pub fn scoped(&self, pr: PullRequestInfo) -> ScopedProposal {
        ScopedProposal {
            pr,
            scope: self.scope.as_str().to_string(),
            repository: self.repository(),
        }
    }

    /// Repository-scoped preview key of PR `pr_id` ([`keel_pko::preview_key`]).
    pub fn preview_key(&self, pr_id: &str) -> Result<String, ServerError> {
        match (&self.dedicated, self.tenant()) {
            (Some(_), Some(tenant)) => Ok(keel_pko::preview_key(Some(tenant), pr_id)?),
            _ => Ok(pr_id.trim().trim_start_matches('#').to_ascii_lowercase()),
        }
    }

    /// Serializes Git work on this repository: the platform `git_lock`, or the tenant
    /// repository's own lock.
    pub async fn lock(&self, state: &AppState) -> OwnedMutexGuard<()> {
        match (&self.dedicated, self.tenant()) {
            (Some(_), Some(tenant)) => state.tenant_repos.lock(tenant).await,
            _ => state.git_lock.clone().lock_owned().await,
        }
    }

    /// Brings the working tree to `origin/main` (cloning a tenant repository on first use).
    /// Callers hold [`Self::lock`].
    pub async fn sync_main(&self, state: &AppState) -> Result<(), ServerError> {
        match self.tenant().filter(|_| self.is_dedicated()) {
            Some(tenant) => {
                let platform_remote = state
                    .git_runner
                    .get_remote_url(&state.repo_workdir, "origin")
                    .await?;
                state
                    .tenant_repos
                    .sync_checkout(&state.git_runner, &platform_remote, tenant)
                    .await
                    .map_err(|err| {
                        let label = self
                            .repository()
                            .full_name
                            .unwrap_or_else(|| format!("tenant-{tenant}"));
                        tracing::warn!(tenant, repository = %label, error = %err, "tenant repository clone/sync failed");
                        ServerError::ServiceUnavailable(format!(
                            "tenant `{tenant}` repository `{label}` cannot be cloned or synced; check the forge, or re-run POST /api/v1/tenants to (re)provision it"
                        ))
                    })?;
                state.tenant_repos.mark_synced(tenant);
            }
            None => {
                state
                    .git_runner
                    .sync_branch_from_remote(&self.workdir, "origin", WORKTREE_BRANCH)
                    .await?;
            }
        }
        Ok(())
    }

    /// Read-path variant of [`Self::sync_main`] for a tenant's own repository: when the clone
    /// was synced from the forge within [`READ_SYNC_TTL`] it is only reset locally to its
    /// fetched `origin/main` (no network); otherwise it is synced. Bounds the forge traffic and
    /// the time the tenant lock is held by polling clients. Callers hold [`Self::lock`]. No-op for
    /// the platform repository (its readers use committed refs).
    pub async fn sync_main_for_read(&self, state: &AppState) -> Result<(), ServerError> {
        let Some(tenant) = self.tenant().filter(|_| self.is_dedicated()) else {
            return Ok(());
        };
        if state.tenant_repos.synced_within(tenant, READ_SYNC_TTL)
            && tokio::fs::try_exists(self.workdir.join(".git"))
                .await
                .unwrap_or(false)
            && state
                .git_runner
                .reset_branch_to_remote_ref(&self.workdir, "origin", WORKTREE_BRANCH)
                .await
                .is_ok()
        {
            return Ok(());
        }
        self.sync_main(state).await
    }

    /// Makes the working tree available with fresh `origin/*` refs for read-only Git work (diff,
    /// head resolution). Callers hold [`Self::lock`]. `false` when no working tree exists (a
    /// platform repository without a checkout).
    pub async fn refresh(&self, state: &AppState) -> Result<bool, ServerError> {
        if self.is_dedicated() {
            self.sync_main(state).await?;
            return Ok(true);
        }
        if !tokio::fs::try_exists(self.workdir.join(".git"))
            .await
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let _ = state.git_runner.fetch(&self.workdir, "origin").await;
        Ok(true)
    }

    /// Fetches `pr_id` and checks it belongs to this scope (`404` otherwise): in the platform
    /// repository a tenant scope only sees PRs of that tenant.
    pub async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ServerError> {
        let pr = self.forge.get_pull_request(pr_id).await?;
        if !self.owns(&pr) {
            return Err(ServerError::NotFound(format!(
                "Proposal '{pr_id}' not found in scope '{}'",
                self.scope.as_str()
            )));
        }
        Ok(pr)
    }

    /// `true` when `pr` belongs to this scope.
    pub fn owns(&self, pr: &PullRequestInfo) -> bool {
        match self.tenant() {
            // Every PR of a tenant's own repository belongs to the tenant.
            Some(_) if self.is_dedicated() => true,
            Some(tenant) => pr_tenant(pr) == Some(tenant),
            None => true,
        }
    }

    /// Opens a PR for `branch` in this repository.
    pub async fn open_pull_request(
        &self,
        auth: &AuthSession,
        tenant: &str,
        branch: &str,
        title: String,
        body: String,
    ) -> Result<PullRequestInfo, ServerError> {
        let req = CreatePrRequest::new(title, body, branch)
            .with_tenant(tenant)
            .with_author(crate::routes::pr_author_for_session(auth));
        Ok(self.forge.create_pull_request(req).await?)
    }
}

/// Tenant of a PR: its forge tenant tag, else its `keel/<tenant>/...` branch.
pub fn pr_tenant(pr: &PullRequestInfo) -> Option<&str> {
    pr.tenant
        .as_deref()
        .or_else(|| parse_proposal_branch_tenant(&pr.source_branch))
}

/// The tenant's resource index: the platform repository's `tenants/kustomization.yaml`
/// (Local Git provider; entries `<id>/...`, applied by `keel-tenants`) or the tenant repository's
/// `tenants/<id>/kustomization.yaml` (entries relative to `tenants/<id>/`, applied by the
/// tenant's `keel-tenant-root` as `tenant-<id>-sa`).
///
/// Callers work with entries relative to `tenants/` in both layouts (`<id>/apps/x/...`);
/// [`Self::load`] / [`Self::store`] translate.
#[derive(Debug, Clone)]
pub struct TenantIndex {
    tenant: String,
    dedicated: bool,
}

impl TenantIndex {
    pub fn new(repo: &ScopeRepo, tenant: &str) -> Self {
        Self {
            tenant: tenant.to_string(),
            dedicated: repo.is_dedicated(),
        }
    }

    /// `true` for a tenant repository's own index.
    pub fn is_dedicated(&self) -> bool {
        self.dedicated
    }

    /// Repository-relative path of the index file.
    pub fn rel_path(&self) -> String {
        if self.dedicated {
            format!("tenants/{}/kustomization.yaml", self.tenant)
        } else {
            "tenants/kustomization.yaml".to_string()
        }
    }

    /// Entries (relative to `tenants/`); empty when the file is missing or unparsable.
    pub async fn load(&self, workdir: &Path) -> Vec<String> {
        let raw = crate::routes::read_contained_optional(workdir, &workdir.join(self.rel_path()))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let entries: Vec<String> = noyalib::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if self.dedicated {
            entries
                .into_iter()
                .map(|e| format!("{}/{e}", self.tenant))
                .collect()
        } else {
            entries
        }
    }

    /// Writes `entries` (relative to `tenants/`) and returns the path to stage. A tenant
    /// repository index refuses entries outside `tenants/<id>/`, the platform stub files and
    /// remote bases ([`keel_kube::build_tenant_repo_kustomization_yaml`]).
    pub async fn store(&self, workdir: &Path, entries: &[String]) -> Result<String, ServerError> {
        let rel = self.rel_path();
        let yaml = if self.dedicated {
            let prefix = format!("{}/", self.tenant);
            let mut own = Vec::with_capacity(entries.len());
            for entry in entries {
                let Some(rest) = entry.strip_prefix(&prefix) else {
                    return Err(ServerError::Internal(format!(
                        "tenant `{}` index entry `{entry}` is outside the tenant directory",
                        self.tenant
                    )));
                };
                own.push(rest.to_string());
            }
            keel_kube::build_tenant_repo_kustomization_yaml(&self.tenant, &own)?
        } else {
            crate::routes::serialize_tenants_kustomization(entries)?
        };
        let path = workdir.join(&rel);
        if let Some(parent) = path.parent() {
            crate::routes::create_dir_all_contained(workdir, parent).await?;
        }
        crate::routes::write_contained(workdir, &path, yaml).await?;
        Ok(rel)
    }
}

/// Maps a preview key back to the forge provider of its repository and the PR id there.
/// `Err` when the repository cannot be resolved (the caller must then leave the preview alone).
pub fn forge_for_preview_key(
    state: &AppState,
    key: &str,
) -> Result<(Arc<dyn ForgeProvider>, String), ServerError> {
    match keel_pko::parse_preview_key(key) {
        (Some(tenant), number) => match state.tenant_repos.require_forge()? {
            Some(forge) => Ok((forge.repository_provider(tenant)?, number.to_string())),
            None => Ok((state.forge.clone(), key.to_string())),
        },
        (None, _) => Ok((state.forge.clone(), key.to_string())),
    }
}

/// A tenant repository clone synced from the forge more recently than this is reused by read
/// paths ([`ScopeRepo::sync_main_for_read`]).
pub const READ_SYNC_TTL: Duration = Duration::from_secs(10);

/// Freshness of a cached single PR lookup on read paths ([`ProposalListCache::get_pull_request`]).
pub const PR_LOOKUP_TTL: Duration = Duration::from_secs(5);

/// Upper bound of concurrent forge listings in a fan-out.
pub const PROPOSAL_LIST_CONCURRENCY: usize = 6;
/// Freshness of a cached repository PR listing.
pub const PROPOSAL_LIST_TTL: Duration = Duration::from_secs(15);
/// Back-off after a rate-limited listing whose response carried no `Retry-After`.
pub const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);

type ListKey = (String, String);

/// In-memory TTL cache of per-repository PR listings plus per-repository back-off after forge
/// rate limits (`Retry-After`). A disposable cache: keel-server stays stateless.
#[derive(Default)]
pub struct ProposalListCache {
    entries: std::sync::Mutex<HashMap<ListKey, (Instant, Vec<PullRequestInfo>)>>,
    backoff: std::sync::Mutex<HashMap<String, Instant>>,
    /// Single PR lookups of read paths, keyed `<repo label>#<pr id>`.
    prs: std::sync::Mutex<HashMap<String, (Instant, PullRequestInfo)>>,
}

impl std::fmt::Debug for ProposalListCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProposalListCache").finish_non_exhaustive()
    }
}

/// Outcome of a cached repository listing.
#[derive(Debug, Clone)]
pub struct CachedListing {
    pub proposals: Vec<PullRequestInfo>,
    /// Served from cache past its TTL because the forge is rate limiting (or failing).
    pub stale: bool,
}

impl ProposalListCache {
    fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drops cached listings of `repo_label` (after a create/merge/close/reopen there), for
    /// every scope listing that repository.
    pub fn invalidate(&self, repo_label: &str) {
        let prefix = format!("{repo_label}#");
        Self::lock(&self.entries).retain(|(key, _), _| !key.starts_with(&prefix));
        Self::lock(&self.prs).retain(|key, _| !key.starts_with(&prefix));
    }

    /// [`ScopeRepo::get_pull_request`] through a short TTL cache ([`PR_LOOKUP_TTL`]), for read
    /// paths polled by clients (`GET .../preview`). Mutations look PRs up directly.
    pub async fn get_pull_request(
        &self,
        repo: &ScopeRepo,
        pr_id: &str,
    ) -> Result<PullRequestInfo, ServerError> {
        let key = format!(
            "{}#{}",
            repo.repository().label,
            pr_id.trim().trim_start_matches('#')
        );
        if let Some((at, pr)) = Self::lock(&self.prs).get(&key)
            && at.elapsed() < PR_LOOKUP_TTL
            && repo.owns(pr)
        {
            return Ok(pr.clone());
        }
        let pr = repo.get_pull_request(pr_id).await?;
        let mut prs = Self::lock(&self.prs);
        prs.retain(|_, (at, _)| at.elapsed() < PR_LOOKUP_TTL);
        prs.insert(key, (Instant::now(), pr.clone()));
        Ok(pr)
    }

    /// Lists `repo`'s PRs (`state_filter`), through the cache. While the repository is backing
    /// off after a rate limit, or when the forge fails, the last listing is served as `stale`
    /// (an error only when nothing is cached).
    pub async fn list(
        &self,
        repo: &ScopeRepo,
        state_filter: Option<PullRequestState>,
    ) -> Result<CachedListing, ServerError> {
        let label = repo.repository().label;
        let key: ListKey = (
            format!("{label}#{}", repo.scope.as_str()),
            format!("{state_filter:?}"),
        );
        let cached = Self::lock(&self.entries).get(&key).cloned();
        if let Some((at, prs)) = &cached
            && at.elapsed() < PROPOSAL_LIST_TTL
        {
            return Ok(CachedListing {
                proposals: prs.clone(),
                stale: false,
            });
        }
        let backing_off = Self::lock(&self.backoff)
            .get(&label)
            .is_some_and(|until| Instant::now() < *until);
        let stale_or = |err: ServerError| match &cached {
            Some((_, prs)) => Ok(CachedListing {
                proposals: prs.clone(),
                stale: true,
            }),
            None => Err(err),
        };
        if backing_off {
            return stale_or(ServerError::ServiceUnavailable(format!(
                "forge rate limit: proposals of `{label}` are temporarily unavailable; retry later"
            )));
        }
        let tenant_filter = match (&repo.scope, repo.is_dedicated()) {
            (ProposalScope::Tenant(t), false) => Some(t.as_str()),
            _ => None,
        };
        match repo
            .forge
            .list_pull_requests(tenant_filter, state_filter)
            .await
        {
            Ok(mut prs) => {
                prs.retain(|pr| repo.owns(pr));
                Self::lock(&self.entries).insert(key, (Instant::now(), prs.clone()));
                Ok(CachedListing {
                    proposals: prs,
                    stale: false,
                })
            }
            Err(ForgeError::RateLimited { retry_after, .. }) => {
                let wait = retry_after.unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF);
                Self::lock(&self.backoff).insert(label.clone(), Instant::now() + wait);
                tracing::warn!(
                    repository = %label,
                    wait_secs = wait.as_secs(),
                    "forge rate limit while listing proposals; backing off"
                );
                stale_or(ServerError::ServiceUnavailable(format!(
                    "forge rate limit: proposals of `{label}` are temporarily unavailable; retry in {}s",
                    wait.as_secs()
                )))
            }
            Err(err) => {
                tracing::warn!(repository = %label, error = %err, "listing proposals failed");
                stale_or(err.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Lists one PR, then answers with a rate limit (`Retry-After` 30s).
    struct ThrottlingForge {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ForgeProvider for ThrottlingForge {
        async fn create_pull_request(
            &self,
            _req: CreatePrRequest,
        ) -> Result<PullRequestInfo, ForgeError> {
            unreachable!()
        }
        async fn get_pull_request(&self, _pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
            unreachable!()
        }
        async fn post_preview_comment(&self, _pr_id: &str, _md: &str) -> Result<(), ForgeError> {
            Ok(())
        }
        async fn list_pull_requests(
            &self,
            _tenant: Option<&str>,
            _state: Option<PullRequestState>,
        ) -> Result<Vec<PullRequestInfo>, ForgeError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(vec![pr("1")])
            } else {
                Err(ForgeError::RateLimited {
                    status: 429,
                    retry_after: Some(Duration::from_secs(30)),
                })
            }
        }
        async fn merge_pull_request(
            &self,
            _pr_id: &str,
            _sha: Option<&str>,
        ) -> Result<PullRequestInfo, ForgeError> {
            unreachable!()
        }
        async fn close_pull_request(&self, _pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
            unreachable!()
        }
        async fn reopen_pull_request(&self, _pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
            unreachable!()
        }
        async fn ensure_repository(
            &self,
            _private: bool,
        ) -> Result<keel_git::RepositoryEnsureOutcome, ForgeError> {
            unreachable!()
        }
    }

    fn pr(id: &str) -> PullRequestInfo {
        PullRequestInfo {
            id: id.to_string(),
            title: "t".to_string(),
            body: String::new(),
            state: PullRequestState::Open,
            source_branch: "keel/acme/app-web-1".to_string(),
            target_branch: "main".to_string(),
            head_sha: String::new(),
            tenant: Some("acme".to_string()),
            url: String::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            comments: Vec::new(),
            diff_stat: None,
            merge_commit_sha: None,
            base_sha: None,
        }
    }

    #[tokio::test]
    async fn listings_back_off_after_a_rate_limit_and_serve_the_last_result() {
        let forge = Arc::new(ThrottlingForge {
            calls: AtomicUsize::new(0),
        });
        let repo = ScopeRepo {
            scope: ProposalScope::Platform,
            forge: forge.clone(),
            workdir: PathBuf::from("/nonexistent"),
            dedicated: None,
        };
        let cache = ProposalListCache::default();
        let first = cache.list(&repo, None).await.unwrap();
        assert!(!first.stale);
        assert_eq!(first.proposals.len(), 1);
        // Within the TTL: no forge call.
        cache.list(&repo, None).await.unwrap();
        assert_eq!(forge.calls.load(Ordering::SeqCst), 1);
        // Expire the entry: the forge throttles, the last listing is served as stale, and the
        // repository backs off (no further forge calls until `Retry-After` passed).
        for entry in ProposalListCache::lock(&cache.entries).values_mut() {
            entry.0 = Instant::now() - PROPOSAL_LIST_TTL - Duration::from_secs(1);
        }
        let stale = cache.list(&repo, None).await.unwrap();
        assert!(stale.stale);
        assert_eq!(stale.proposals.len(), 1);
        assert_eq!(forge.calls.load(Ordering::SeqCst), 2);
        let again = cache.list(&repo, None).await.unwrap();
        assert!(again.stale);
        assert_eq!(forge.calls.load(Ordering::SeqCst), 2, "backing off");
        // Nothing cached at all: the rate limit surfaces as 503.
        cache.invalidate("platform");
        let err = cache.list(&repo, None).await.unwrap_err();
        assert!(matches!(err, ServerError::ServiceUnavailable(_)), "{err:?}");
    }

    #[test]
    fn platform_scope_preview_keys_are_bare_pr_numbers() {
        let repo = ScopeRepo {
            scope: ProposalScope::Tenant("acme".to_string()),
            forge: Arc::new(ThrottlingForge {
                calls: AtomicUsize::new(0),
            }),
            workdir: PathBuf::from("/nonexistent"),
            dedicated: None,
        };
        // Local Git provider: one repository, plain PR numbers, `platform` label.
        assert_eq!(repo.preview_key("#7").unwrap(), "7");
        assert_eq!(repo.repository().label, "platform");
        let mut other = pr("7");
        other.tenant = Some("beta".to_string());
        other.source_branch = "keel/beta/app-web-1".to_string();
        assert!(repo.owns(&pr("7")));
        assert!(
            !repo.owns(&other),
            "a tenant scope never sees other tenants' PRs"
        );
    }

    #[test]
    fn scopes_parse_the_reserved_platform_id() {
        assert_eq!(
            ProposalScope::parse("management").unwrap(),
            ProposalScope::Platform
        );
        assert_eq!(
            ProposalScope::parse("acme").unwrap(),
            ProposalScope::Tenant("acme".to_string())
        );
        assert!(ProposalScope::parse("../x").is_err());
        assert_eq!(ProposalScope::Platform.tenant(), None);
    }

    #[tokio::test]
    async fn tenant_repository_indexes_translate_entries() {
        let dir = tempfile::tempdir().unwrap();
        let idx = TenantIndex {
            tenant: "acme".to_string(),
            dedicated: true,
        };
        assert_eq!(idx.rel_path(), "tenants/acme/kustomization.yaml");
        assert!(idx.load(dir.path()).await.is_empty());
        let rel = idx
            .store(dir.path(), &["acme/apps/web/helmrelease.yaml".to_string()])
            .await
            .unwrap();
        let raw = std::fs::read_to_string(dir.path().join(&rel)).unwrap();
        assert!(raw.contains("- apps/web/helmrelease.yaml"), "{raw}");
        assert_eq!(
            idx.load(dir.path()).await,
            vec!["acme/apps/web/helmrelease.yaml".to_string()]
        );
        // Other tenants' entries and the platform stub never reach a tenant repository index.
        assert!(
            idx.store(dir.path(), &["beta/apps/x.yaml".to_string()])
                .await
                .is_err()
        );
        assert!(
            idx.store(dir.path(), &["acme/rbac.yaml".to_string()])
                .await
                .is_err()
        );

        let mono = TenantIndex {
            tenant: "acme".to_string(),
            dedicated: false,
        };
        assert_eq!(mono.rel_path(), "tenants/kustomization.yaml");
        mono.store(dir.path(), &["acme/rbac.yaml".to_string()])
            .await
            .unwrap();
        assert_eq!(
            mono.load(dir.path()).await,
            vec!["acme/rbac.yaml".to_string()]
        );
    }
}
