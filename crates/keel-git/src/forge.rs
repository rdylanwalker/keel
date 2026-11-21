//! `ForgeProvider` trait and domain models for Pull Request / Merge Request workflows.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

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

    /// Overrides the target base branch (defaults to `"main"`).
    pub fn with_target_branch(mut self, target_branch: impl Into<String>) -> Self {
        self.target_branch = target_branch.into();
        self
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
/// Serialized in `camelCase` (consistent with the rest of the REST API); `snake_case` keys are
/// still accepted on deserialization for previously persisted local proposal metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestComment {
    pub id: String,
    pub body: String,
    #[serde(alias = "created_at")]
    pub created_at: DateTime<Utc>,
}

/// Canonical Pull Request / proposal representation returned by [`ForgeProvider`] implementations.
///
/// Serialized in `camelCase` (consistent with the rest of the REST API); `snake_case` keys are
/// still accepted on deserialization for previously persisted local proposal metadata and older
/// servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestInfo {
    pub id: String,
    pub title: String,
    pub body: String,
    pub state: PullRequestState,
    #[serde(alias = "source_branch")]
    pub source_branch: String,
    #[serde(alias = "target_branch")]
    pub target_branch: String,
    #[serde(alias = "head_sha")]
    pub head_sha: String,
    pub tenant: Option<String>,
    pub url: String,
    #[serde(alias = "created_at")]
    pub created_at: DateTime<Utc>,
    #[serde(alias = "updated_at")]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub comments: Vec<PullRequestComment>,
    #[serde(default, alias = "diff_stat")]
    pub diff_stat: Option<String>,
    /// Merge (or squash) commit created on the target branch once the PR is merged.
    #[serde(
        default,
        alias = "merge_commit_sha",
        skip_serializing_if = "Option::is_none"
    )]
    pub merge_commit_sha: Option<String>,
    /// Merge-base / target-branch commit the PR diff is computed against (recorded at merge time
    /// by the local forge, `diff_refs.base_sha` on GitLab).
    #[serde(default, alias = "base_sha", skip_serializing_if = "Option::is_none")]
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

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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

    async fn list_open_pull_requests(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError>;

    async fn list_pull_requests(
        &self,
        tenant: Option<&str>,
        state: Option<PullRequestState>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError> {
        let open = self.list_open_pull_requests(tenant).await?;
        match state {
            None | Some(PullRequestState::Open) => Ok(open),
            Some(_) => Ok(Vec::new()),
        }
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
            .with_target_branch("release")
            .with_tenant("team-sec")
            .with_author("Ops <ops@keel.io>");
        assert_eq!(req.target_branch, "release");
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

        // camelCase on the wire, snake_case still accepted for persisted metadata.
        let json = serde_json::to_value(&pr).unwrap();
        assert_eq!(json["sourceBranch"], "feature/custom-branch");
        assert!(json.get("source_branch").is_none());
        let mut legacy = json.clone();
        let obj = legacy.as_object_mut().unwrap();
        let v = obj.remove("sourceBranch").unwrap();
        obj.insert("source_branch".to_string(), v);
        let back: PullRequestInfo = serde_json::from_value(legacy).unwrap();
        assert_eq!(back, pr);
        assert!(matches_tenant(&pr, "Team Sec"));
        assert!(!matches_tenant(&pr, "other"));
    }
}
