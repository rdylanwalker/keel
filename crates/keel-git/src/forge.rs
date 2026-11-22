//! `ForgeProvider` trait and domain models for Pull Request / Merge Request workflows.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

use crate::git::{GitError, parse_proposal_branch_tenant};

/// Lifecycle state of a Pull Request / proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestState {
    Open,
    Merged,
    Closed,
}

impl fmt::Display for PullRequestState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Merged => write!(f, "merged"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

/// Request payload for opening a new Pull Request on a configured [`ForgeProvider`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePrRequest {
    pub title: String,
    pub body: String,
    pub source_branch: String,
    pub target_branch: String,
    pub tenant: Option<String>,
    pub author: Option<String>,
}

impl CreatePrRequest {
    /// Creates a new [`CreatePrRequest`] targeting `"main"` and inferring `tenant`
    /// when `source_branch` follows `keel/<tenant>/<action>-<timestamp>`.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        source_branch: impl Into<String>,
    ) -> Self {
        let source_branch = source_branch.into();
        let inferred_tenant = parse_proposal_branch_tenant(&source_branch).map(ToOwned::to_owned);
        Self {
            title: title.into(),
            body: body.into(),
            source_branch,
            target_branch: "main".to_string(),
            tenant: inferred_tenant,
            author: None,
        }
    }

    /// Explicitly sets the tenant identifier associated with this proposal.
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    /// Sets the authenticated author (`Name <email>`) associated with this proposal.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }
}

/// Comment attached to a Pull Request (such as a PKO v2 ephemeral preview diff report).
///
/// Serialized in `camelCase` (consistent with the rest of the REST API).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestComment {
    pub id: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// Canonical Pull Request / proposal representation returned by [`ForgeProvider`] implementations.
///
/// Serialized in `camelCase` (consistent with the rest of the REST API).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestInfo {
    pub id: String,
    pub title: String,
    pub body: String,
    pub state: PullRequestState,
    pub source_branch: String,
    pub target_branch: String,
    pub head_sha: String,
    pub tenant: Option<String>,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub comments: Vec<PullRequestComment>,
    #[serde(default)]
    pub diff_stat: Option<String>,
    /// Merge (or squash) commit created on the target branch once the PR is merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    /// Merge-base / target-branch commit the PR diff is computed against (recorded at merge time
    /// by the local forge, `diff_refs.base_sha` on GitLab).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
}

/// Errors returned by [`ForgeProvider`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("Pull request '{0}' not found")]
    NotFound(String),

    #[error("Invalid pull request input: {0}")]
    InvalidInput(String),

    #[error("Pull request '{0}' cannot be modified or merged while in state '{1}'")]
    InvalidState(String, PullRequestState),

    #[error("Fast-forward merge failed for pull request '{0}': {1}")]
    MergeConflict(String, String),

    #[error("Git operation failed: {0}")]
    Git(#[from] GitError),

    #[error("Forge HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Forge REST API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },

    /// The forge throttled the request (HTTP 429, or a 403 carrying `Retry-After` /
    /// `X-RateLimit-Remaining: 0`). `retry_after` is the forge-advertised wait, when it sent one.
    #[error("Forge rate limit exceeded (HTTP {status}); retry after {}", retry_after.map(|d| format!("{}s", d.as_secs())).unwrap_or_else(|| "a while".to_string()))]
    RateLimited {
        status: u16,
        retry_after: Option<std::time::Duration>,
    },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The provider does not implement this operation (for example per-repository credentials
    /// on the development-only [`crate::LocalGitProvider`], `SDD.md` §3.2).
    #[error("Forge operation not supported by this provider: {0}")]
    Unsupported(String),
}

impl ForgeError {
    fn unsupported(operation: &str) -> Self {
        Self::Unsupported(operation.to_string())
    }
}

/// Result of [`ForgeProvider::ensure_repository`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryEnsureOutcome {
    /// The repository already existed; nothing was changed.
    Existed,
    /// The repository was missing and has been created.
    Created,
}

/// A repository addressed by owner (user, organisation or GitLab group path) and name.
///
/// Also the target of a Forgejo repository-scoped access token (`RepoTargetOption`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryTarget {
    /// Owning user or organisation (GitLab: namespace path, may contain `/`).
    pub owner: String,
    /// Repository name.
    pub name: String,
}

impl RepositoryTarget {
    /// Validates and builds a target: every `/`-separated owner segment and the name must match
    /// `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$` (no `..`, no `%`), because they are interpolated into
    /// forge URL paths.
    pub fn new(owner: &str, name: &str) -> Result<Self, ForgeError> {
        Ok(Self {
            owner: crate::remote_repo::namespace_path(owner, "owner")?,
            name: crate::remote_repo::single_segment(name, "repo")?,
        })
    }

    /// `owner/name`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl fmt::Display for RepositoryTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Branch protected by [`ForgeProvider::ensure_main_protection`].
pub const PROTECTED_MAIN_BRANCH: &str = "main";

/// Required name prefix of tokens minted by [`ForgeProvider::create_repo_read_token`]. Stage 0
/// deletes every other token the Forgejo tenant-reader bot owns (`keel_core` keeps the same value
/// as `TENANT_REPO_TOKEN_NAME_PREFIX`).
pub const REPO_READ_TOKEN_NAME_PREFIX: &str = "keel-tenant-repo-";

/// Creation options for [`ForgeProvider::ensure_named_repository`]. Only applied when the
/// repository is created; an existing repository is never modified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryOptions {
    /// Create the repository as private (GitLab: `private`, otherwise `internal`; never public).
    pub private: bool,
    /// Seed an initial commit (README) so the default branch exists and can be protected.
    pub auto_init: bool,
    /// Default branch name for the initial commit. GitHub ignores it (account/org setting).
    pub default_branch: Option<String>,
    /// Optional repository description.
    pub description: Option<String>,
}

impl Default for RepositoryOptions {
    /// Private, auto-initialised, default branch `main`.
    fn default() -> Self {
        Self {
            private: true,
            auto_init: true,
            default_branch: Some(PROTECTED_MAIN_BRANCH.to_string()),
            description: None,
        }
    }
}

/// Repository returned by [`ForgeProvider::ensure_named_repository`].
///
/// `clone_url` is what the forge reports (it follows the forge's configured root URL, which may
/// differ from the address in-cluster clients use); callers build SSH transport URLs themselves
/// ([`crate::ssh_clone_url`]) and never trust a forge-reported `ssh_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryInfo {
    /// Owner (user, organisation or GitLab namespace path).
    pub owner: String,
    /// Repository name (GitLab: project path).
    pub name: String,
    /// Whether the repository is private (GitLab: visibility is not `public`/`internal`).
    pub private: bool,
    /// Default branch (empty when the forge reports none, e.g. an empty repository).
    pub default_branch: String,
    /// HTTPS clone URL as reported by the forge.
    pub clone_url: Option<String>,
    /// Web URL.
    pub html_url: Option<String>,
    /// Whether this call created the repository.
    pub outcome: RepositoryEnsureOutcome,
}

impl RepositoryInfo {
    /// The repository as a [`RepositoryTarget`].
    pub fn target(&self) -> RepositoryTarget {
        RepositoryTarget {
            owner: self.owner.clone(),
            name: self.name.clone(),
        }
    }
}

/// Transport of the per-repository read-only credential a forge issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepoCredentialKind {
    /// HTTPS basic auth with a repository-scoped read token
    /// ([`ForgeProvider::create_repo_read_token`]): Forgejo (15+) bot token, GitLab deploy token.
    HttpsToken,
    /// SSH with a read-only deploy key ([`ForgeProvider::ensure_deploy_key`]): GitHub.
    SshDeployKey,
}

/// A repository-scoped read-only token. `Debug` never prints the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct RepoReadToken {
    /// Forge id of the token (pass to [`ForgeProvider::revoke_repo_read_token`]).
    pub id: String,
    /// Token name.
    pub name: String,
    /// HTTPS basic-auth username to pair with the token (Forgejo: the bot login; GitLab: the
    /// generated `gitlab+deploy-token-<n>` username).
    pub username: String,
    /// Token secret, returned by the forge only once.
    pub token: String,
}

impl fmt::Debug for RepoReadToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepoReadToken")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("username", &self.username)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Metadata of a live repository read token ([`ForgeProvider::list_repo_read_tokens`]); never
/// the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoReadTokenInfo {
    /// Forge id of the token (pass to [`ForgeProvider::revoke_repo_read_token`]).
    pub id: String,
    /// Token name.
    pub name: String,
}

/// A repository deploy key (public half only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployKeyInfo {
    /// Forge id of the key.
    pub id: String,
    /// Title.
    pub title: String,
    /// `SHA256:<base64>` fingerprint computed locally from the key (`None` when the forge
    /// returned a key Keel cannot parse).
    pub fingerprint: Option<String>,
    /// Whether the key is read-only (cannot push).
    pub read_only: bool,
}

/// Outcome of [`ForgeProvider::ensure_deploy_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeployKeyOutcome {
    /// The key was added.
    Created,
    /// A key with the same fingerprint and access mode already existed.
    Existed,
    /// A key with the same fingerprint but a different access mode existed; it was deleted and
    /// re-added with the requested mode.
    Replaced,
}

/// Result of [`ForgeProvider::ensure_deploy_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredDeployKey {
    /// The key as it now exists on the forge.
    pub key: DeployKeyInfo,
    /// What the call did.
    pub outcome: DeployKeyOutcome,
}

/// Outcome of [`ForgeProvider::ensure_main_protection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BranchProtectionOutcome {
    /// `main` was unprotected and is now protected.
    Created,
    /// `main` was protected with different settings; Keel's settings were applied.
    Updated,
    /// `main` already had Keel's settings.
    Unchanged,
    /// The forge plan does not offer branch protection for this repository (GitHub Free private
    /// repositories). Nothing was changed; direct pushes to `main` are not blocked by the forge.
    UnsupportedPlan,
}

/// Outcome of [`ForgeProvider::disable_forks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "result", content = "detail")]
pub enum ForkPolicyOutcome {
    /// Forking was allowed and has been disabled for the repository.
    Disabled,
    /// Forking was already disabled (per repository, organisation or instance).
    AlreadyDisabled,
    /// Forking is governed by an instance or organisation setting Keel cannot (or may not)
    /// change through this repository; the string tells the operator what to set.
    RequiresAdminSetting(String),
}

/// Thin forge abstraction isolating PR/MR operations across Forgejo/Gitea, GitHub, GitLab,
/// and offline/air-gapped [`crate::LocalGitProvider`] repositories (`SDD.md` §3.2).
#[async_trait]
pub trait ForgeProvider: Send + Sync {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError>;

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError>;

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError>;

    /// Lists PRs, optionally filtered by tenant and state (`None` = every state).
    async fn list_pull_requests(
        &self,
        tenant: Option<&str>,
        state: Option<PullRequestState>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError>;

    /// Lists open PRs, optionally filtered by tenant.
    async fn list_open_pull_requests(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError> {
        self.list_pull_requests(tenant, Some(PullRequestState::Open))
            .await
    }

    /// Merges the PR. When `expected_head_sha` is `Some`, the forge must refuse the merge if the
    /// PR head no longer matches that commit (Gitea/Forgejo `head_commit_id`, GitHub/GitLab `sha`,
    /// local branch head comparison), pinning the merge to the reviewed revision.
    async fn merge_pull_request(
        &self,
        pr_id: &str,
        expected_head_sha: Option<&str>,
    ) -> Result<PullRequestInfo, ForgeError>;

    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError>;

    async fn reopen_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError>;

    /// Ensures the configured repository exists, creating it (with the requested visibility)
    /// when missing. Existing repositories are left untouched (visibility is not changed).
    async fn ensure_repository(&self, private: bool)
    -> Result<RepositoryEnsureOutcome, ForgeError>;

    // ------------------------------------------------------------------------------------
    // Per-repository operations (per-tenant repositories, `SDD.md` §3.2). Every method has a
    // default that returns [`ForgeError::Unsupported`]; REST wrappers only, no git transport.
    // ------------------------------------------------------------------------------------

    /// Returns a provider with the same forge, credentials and client, scoped to
    /// `owner/name` (PR operations then target that repository).
    fn for_repository(
        &self,
        _owner: &str,
        _name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        Err(ForgeError::unsupported("for_repository"))
    }

    /// Ensures repository `owner/name` exists, creating it with `opts` when missing. The owner
    /// type (organisation/group vs. the authenticated user) is resolved explicitly; creating a
    /// repository for any other user (GitLab: another user's personal namespace) is refused.
    ///
    /// An existing repository is never modified. It is adopted only when the forge reports the
    /// requested owner/name (a renamed or transferred repository answered through a redirect is
    /// refused) and, when `opts.private` is set, reports it as private; otherwise
    /// [`ForgeError::InvalidInput`].
    async fn ensure_named_repository(
        &self,
        _owner: &str,
        _name: &str,
        _opts: &RepositoryOptions,
    ) -> Result<RepositoryInfo, ForgeError> {
        Err(ForgeError::unsupported("ensure_named_repository"))
    }

    /// Mints a read-only token that works on `repo` only (Forgejo 15+: `read:repository` token
    /// of the basic-auth bot restricted to `repo`; GitLab: project deploy token with
    /// `read_repository`). Fails closed if the forge does not confirm the restriction (Forgejo:
    /// exactly the requested scope and repository; GitLab: exactly `read_repository`).
    ///
    /// `name` must start with [`REPO_READ_TOKEN_NAME_PREFIX`] (Stage 0 deletes any other token
    /// of the Forgejo bot). Forgejo token names are unique per user: minting a name that is
    /// already taken by a token restricted to exactly `repo` revokes that token and mints a new
    /// one (retry after a failed Secret write); a name taken by any other token is refused. To
    /// rotate without a gap, mint under a new name, store it, then revoke the old token by id.
    async fn create_repo_read_token(
        &self,
        _repo: &RepositoryTarget,
        _name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        Err(ForgeError::unsupported("create_repo_read_token"))
    }

    /// Lists the live repository read tokens that work on `repo` (Forgejo: the basic-auth bot's
    /// tokens restricted to exactly `repo`; GitLab: the project's active deploy tokens). Fails
    /// closed instead of truncating; never returns token secrets.
    async fn list_repo_read_tokens(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<Vec<RepoReadTokenInfo>, ForgeError> {
        Err(ForgeError::unsupported("list_repo_read_tokens"))
    }

    /// Revokes a token minted by [`Self::create_repo_read_token`]. A missing token (404) is
    /// success.
    async fn revoke_repo_read_token(
        &self,
        _repo: &RepositoryTarget,
        _token_id: &str,
    ) -> Result<(), ForgeError> {
        Err(ForgeError::unsupported("revoke_repo_read_token"))
    }

    /// Ensures the OpenSSH `public_key` is a deploy key of `repo` with the requested access.
    /// Keys are matched by SHA-256 fingerprint (titles and comments are ignored); a key with the
    /// same fingerprint but another access mode is replaced.
    async fn ensure_deploy_key(
        &self,
        _repo: &RepositoryTarget,
        _title: &str,
        _public_key: &str,
        _read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        Err(ForgeError::unsupported("ensure_deploy_key"))
    }

    /// Lists the deploy keys of `repo` (fails closed instead of truncating).
    async fn list_deploy_keys(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        Err(ForgeError::unsupported("list_deploy_keys"))
    }

    /// Removes deploy key `key_id` from `repo`. A missing key (404) is success.
    async fn revoke_deploy_key(
        &self,
        _repo: &RepositoryTarget,
        _key_id: &str,
    ) -> Result<(), ForgeError> {
        Err(ForgeError::unsupported("revoke_deploy_key"))
    }

    /// Protects [`PROTECTED_MAIN_BRANCH`] of `repo`: no direct or force pushes and no deletion
    /// (also for repository admins), changes only through merged PRs/MRs, no required approvals
    /// (Keel merges after its own checks).
    ///
    /// Must run after `main` has its initial commit (push the seed first, or create the
    /// repository with `auto_init`): a missing `main` fails with [`ForgeError::InvalidInput`]
    /// instead of protecting a branch Keel's own seed push would then be refused on.
    async fn ensure_main_protection(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        Err(ForgeError::unsupported("ensure_main_protection"))
    }

    /// Disables forking of `repo` where the forge allows it per repository, otherwise reports
    /// the instance/organisation setting that governs it.
    async fn disable_forks(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        Err(ForgeError::unsupported("disable_forks"))
    }

    /// The forge's published SSH host public keys (one OpenSSH `<type> <base64>` line each) for
    /// building a pinned `known_hosts` for [`RepoCredentialKind::SshDeployKey`] credentials
    /// without trusting on first use (GitHub: `GET /meta` `ssh_keys`, fetched over
    /// TLS-verified HTTPS).
    async fn ssh_host_keys(&self) -> Result<Vec<String>, ForgeError> {
        Err(ForgeError::unsupported("ssh_host_keys"))
    }
}

/// Validates an expected head commit SHA (hex, 7..=64 chars) before sending it to a forge.
pub fn validate_commit_sha(sha: &str) -> Result<&str, ForgeError> {
    if (7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(sha)
    } else {
        Err(ForgeError::InvalidInput(format!(
            "expected head SHA must be a 7-64 character hex commit id, got '{sha}'"
        )))
    }
}

/// Checks whether a [`PullRequestInfo`] belongs to `tenant` (via explicit metadata or branch prefix).
pub(crate) fn matches_tenant(pr: &PullRequestInfo, tenant: &str) -> bool {
    let sanitized_filter = crate::git::sanitize_branch_segment(tenant);
    if let Some(pr_tenant) = pr.tenant.as_deref()
        && (pr_tenant == tenant
            || crate::git::sanitize_branch_segment(pr_tenant) == sanitized_filter)
    {
        return true;
    }
    if let Some(branch_tenant) = parse_proposal_branch_tenant(&pr.source_branch)
        && (branch_tenant == tenant || branch_tenant == sanitized_filter)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pr_models_builders_and_tenant_matching() {
        assert_eq!(PullRequestState::Open.to_string(), "open");
        assert_eq!(PullRequestState::Merged.to_string(), "merged");
        assert_eq!(PullRequestState::Closed.to_string(), "closed");

        let req = CreatePrRequest::new("Title", "Body", "feature/custom-branch")
            .with_tenant("team-sec")
            .with_author("Ops <ops@keel.io>");
        assert_eq!(req.target_branch, "main");
        assert_eq!(req.tenant.as_deref(), Some("team-sec"));
        assert_eq!(req.author.as_deref(), Some("Ops <ops@keel.io>"));

        let now = Utc::now();
        let pr = PullRequestInfo {
            id: "10".to_string(),
            title: "Title".to_string(),
            body: "Body".to_string(),
            state: PullRequestState::Open,
            source_branch: "feature/custom-branch".to_string(),
            target_branch: "main".to_string(),
            head_sha: "deadbeef".to_string(),
            tenant: Some("team-sec".to_string()),
            url: "https://forge.local/pulls/10".to_string(),
            created_at: now,
            updated_at: now,
            comments: Vec::new(),
            diff_stat: None,
            merge_commit_sha: None,
            base_sha: None,
        };
        assert!(matches_tenant(&pr, "team-sec"));

        // camelCase on the wire.
        let json = serde_json::to_value(&pr).unwrap();
        assert_eq!(json["sourceBranch"], "feature/custom-branch");
        assert!(json.get("source_branch").is_none());
        let back: PullRequestInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back, pr);
        assert!(matches_tenant(&pr, "Team Sec"));
        assert!(!matches_tenant(&pr, "other"));
    }
}
