//! `keel-git`
//!
//! Direct `git` CLI runner (SSH/HTTPS) and `ForgeProvider` trait abstraction for Git forge integrations
//! (Forgejo/Gitea, GitHub, GitLab, and Local Git).

pub mod forge;
pub mod git;
pub mod local;
pub mod remote;

pub use forge::{
    CreatePrRequest, ForgeError, ForgeProvider, PullRequestComment, PullRequestInfo,
    PullRequestState, RepositoryEnsureOutcome, validate_commit_sha,
};
pub use git::{
    CommitAuthor, EphemeralSecretFile, GitCredentials, GitError, GitRunner, WorkdirSync,
    ensure_workdir_from_remote, format_proposal_branch, parse_proposal_branch_tenant,
    sanitize_branch_segment, validate_git_ref, validate_remote_url_without_credentials,
};
pub use local::LocalGitProvider;
pub use remote::{
    ForgejoGiteaProvider, GitHubProvider, GitLabProvider, RemoteRepoSlug, parse_remote_repo_url,
};
