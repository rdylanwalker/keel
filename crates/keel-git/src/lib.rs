//! `keel-git`
//!
//! Direct `git` CLI runner (HTTPS credential injection) and `ForgeProvider` trait abstraction for Git forge integrations
//! (Forgejo/Gitea, GitHub, GitLab, and Local Git).

mod forge;
mod forgejo_admin;
mod git;
mod http;
mod local;
mod remote;
mod remote_repo;
mod ssh_keys;
#[cfg(test)]
mod test_http;

pub use forge::{
    BranchProtectionOutcome, CreatePrRequest, DeployKeyInfo, DeployKeyOutcome, EnsuredDeployKey,
    ForgeError, ForgeProvider, ForkPolicyOutcome, PullRequestComment, PullRequestInfo,
    PullRequestState, REPO_READ_TOKEN_NAME_PREFIX, RepoCredentialKind, RepoReadToken,
    RepoReadTokenInfo, RepositoryEnsureOutcome, RepositoryInfo, RepositoryOptions,
    RepositoryTarget, validate_commit_sha,
};
pub use forgejo_admin::{
    AccessTokenInfo, ProvisionOutcome, ServiceUserSpec, StrayCredentialReport, TeamSpec,
};
pub use git::{
    CommitAuthor, GitCredentials, GitError, GitRunner, REMOTE_CREDENTIAL_STORE_FILE,
    ensure_workdir_from_remote, parse_proposal_branch_tenant, validate_git_ref,
};
pub use local::LocalGitProvider;
pub use remote::{
    ForgejoGiteaProvider, GitHubProvider, GitLabProvider, RemoteRepoSlug, parse_remote_repo_url,
};
pub use ssh_keys::{
    generate_ed25519_key_pair, known_hosts_file, ssh_clone_url, ssh_public_key_fingerprint,
};
