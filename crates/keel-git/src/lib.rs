//! `keel-git`
//!
//! Direct `git` CLI runner (SSH/HTTPS) and `ForgeProvider` trait abstraction for Git forge integrations
//! (Forgejo/Gitea, GitHub, GitLab, and Local Git).

pub mod forge;
pub mod forgejo_admin;
pub mod git;
pub mod local;
pub mod remote;
mod remote_repo;
pub mod ssh_keys;
#[cfg(test)]
mod test_http;

pub use forge::{
    BranchProtectionOutcome, CollaboratorOutcome, CreatePrRequest, DeployKeyInfo, DeployKeyOutcome,
    EnsuredDeployKey, ForgeError, ForgeProvider, ForkInfo, ForkPolicyOutcome,
    PROTECTED_MAIN_BRANCH, PullRequestComment, PullRequestInfo, PullRequestState,
    REPO_READ_TOKEN_NAME_PREFIX, RepoCredentialKind, RepoReadToken, RepoReadTokenInfo,
    RepositoryEnsureOutcome, RepositoryInfo, RepositoryOptions, RepositoryTarget,
    validate_commit_sha,
};
pub use forgejo_admin::{
    AccessTokenInfo, MintedAccessToken, ORG_OWNERS_TEAM, ProvisionOutcome, ServiceUser,
    ServiceUserSpec, StrayCredentialReport, TeamInfo, TeamSpec,
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
pub use ssh_keys::{
    GITHUB_SSH_FALLBACK_HOST, GITHUB_SSH_FALLBACK_PORT, GeneratedSshKeyPair,
    generate_ed25519_key_pair, known_hosts_file, known_hosts_line, normalize_ssh_public_key,
    ssh_clone_url, ssh_public_key_fingerprint,
};
