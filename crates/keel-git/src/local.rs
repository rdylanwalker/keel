//! Offline / air-gapped [`ForgeProvider`] implementation backed by a local bare Git repository
//! and Git-tracked `.keel/proposals/<pr_id>.json` metadata (`SDD.md` §3.2).

use async_trait::async_trait;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::forge::{
    CreatePrRequest, ForgeError, ForgeProvider, PullRequestComment, PullRequestInfo,
    PullRequestState, matches_tenant,
};
use crate::git::{
    CommitAuthor, GitError, GitRunner, parse_proposal_branch_tenant, sanitize_branch_segment,
    validate_git_ref,
};

const PROPOSALS_BRANCH: &str = "keel-proposals";
const PROPOSALS_DIR: &str = ".keel/proposals";

/// RAII temporary working directory cleaned up automatically on [`Drop`].
struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    fn create(label: &str) -> Result<Self, std::io::Error> {
        let dir_name = format!(
            "keel-local-forge-{}-{}",
            sanitize_branch_segment(label),
            uuid::Uuid::new_v4()
        );
        let path = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// [`ForgeProvider`] backed by a local bare Git repository.
///
/// Proposal metadata (`.keel/proposals/<pr_id>.json`) is committed cleanly via [`GitRunner`]
/// onto a dedicated `keel-proposals` metadata branch in the bare repository so proposal
/// comments and state transitions are versioned in Git without mutating the proposal branch's
/// `HEAD` SHA or polluting `main` prior to fast-forward merge.
#[derive(Debug, Clone)]
pub struct LocalGitProvider {
    bare_repo_path: PathBuf,
    git_runner: GitRunner,
    bot_author: CommitAuthor,
    mutex: Arc<Mutex<()>>,
}

impl LocalGitProvider {
    /// Creates a new [`LocalGitProvider`] backed by `bare_repo_path`.
    pub fn new(bare_repo_path: impl Into<PathBuf>) -> Self {
        Self::with_runner(bare_repo_path, GitRunner::new())
    }

    /// Creates a new [`LocalGitProvider`] with a custom [`GitRunner`].
    pub fn with_runner(bare_repo_path: impl Into<PathBuf>, git_runner: GitRunner) -> Self {
        Self {
            bare_repo_path: bare_repo_path.into(),
            git_runner,
            bot_author: CommitAuthor::new("Keel Control Plane", "keel-bot@keel.io"),
            mutex: Arc::new(Mutex::new(())),
        }
    }

    /// Returns the path to the backing bare Git repository.
    pub fn bare_repo_path(&self) -> &Path {
        &self.bare_repo_path
    }

    /// Closes an open pull request without merging it.
    pub async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        validate_pr_id(pr_id)?;
        let _guard = self.mutex.lock().await;

        let mut pr = self
            .read_proposal_unlocked(pr_id)
            .await?
            .ok_or_else(|| ForgeError::NotFound(pr_id.to_string()))?;

        if pr.state != PullRequestState::Open {
            return Err(ForgeError::InvalidState(pr_id.to_string(), pr.state));
        }

        pr.state = PullRequestState::Closed;
        pr.updated_at = Utc::now();
        self.write_proposal_unlocked(&pr, &format!("chore(proposals): close proposal #{pr_id}"))
            .await?;
        Ok(pr)
    }

    fn remote_url_str(&self) -> Result<&str, ForgeError> {
        self.bare_repo_path.to_str().ok_or_else(|| {
            ForgeError::InvalidInput("bare_repo_path must be valid UTF-8".to_string())
        })
    }

    async fn read_proposal_unlocked(
        &self,
        pr_id: &str,
    ) -> Result<Option<PullRequestInfo>, ForgeError> {
        let rel_path = format!("{PROPOSALS_DIR}/{pr_id}.json");
        let full_ref = format!("refs/heads/{PROPOSALS_BRANCH}");
        if !self
            .git_runner
            .ref_exists(&self.bare_repo_path, &full_ref)
            .await?
        {
            return Ok(None);
        }
        let Some(raw) = self
            .git_runner
            .show_file_at_ref(&self.bare_repo_path, &full_ref, &rel_path)
            .await?
        else {
            return Ok(None);
        };
        let pr: PullRequestInfo = serde_json::from_str(&raw)?;
        Ok(Some(pr))
    }

    async fn read_all_proposals_unlocked(&self) -> Result<Vec<PullRequestInfo>, ForgeError> {
        let full_ref = format!("refs/heads/{PROPOSALS_BRANCH}");
        if !self
            .git_runner
            .ref_exists(&self.bare_repo_path, &full_ref)
            .await?
        {
            return Ok(Vec::new());
        }

        let entries = self
            .git_runner
            .list_tree_names(&self.bare_repo_path, &full_ref, PROPOSALS_DIR)
            .await?;

        let mut proposals = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(stem) = entry.strip_suffix(".json")
                && validate_pr_id(stem).is_ok()
                && let Some(pr) = self.read_proposal_unlocked(stem).await?
            {
                proposals.push(pr);
            }
        }

        proposals.sort_by(|a, b| match (a.id.parse::<u64>(), b.id.parse::<u64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => a.id.cmp(&b.id),
        });
        Ok(proposals)
    }

    async fn write_proposal_unlocked(
        &self,
        pr: &PullRequestInfo,
        commit_message: &str,
    ) -> Result<(), ForgeError> {
        let tmp = TempWorkDir::create("meta")?;
        let remote_url = self.remote_url_str()?;
        let full_ref = format!("refs/heads/{PROPOSALS_BRANCH}");

        if self
            .git_runner
            .ref_exists(&self.bare_repo_path, &full_ref)
            .await?
        {
            self.git_runner
                .clone_branch(remote_url, PROPOSALS_BRANCH, tmp.path())
                .await?;
        } else {
            self.git_runner
                .init_worktree_with_remote(tmp.path(), PROPOSALS_BRANCH, remote_url)
                .await?;
        }

        let proposals_dir = tmp.path().join(PROPOSALS_DIR);
        tokio::fs::create_dir_all(&proposals_dir).await?;

        let rel_file = PathBuf::from(PROPOSALS_DIR).join(format!("{}.json", pr.id));
        let full_file = tmp.path().join(&rel_file);
        let serialized = serde_json::to_string_pretty(pr)?;
        tokio::fs::write(&full_file, format!("{serialized}\n")).await?;

        self.git_runner.add(tmp.path(), [&rel_file]).await?;
        self.git_runner
            .commit(tmp.path(), commit_message, &self.bot_author)
            .await?;
        self.git_runner
            .push(tmp.path(), "origin", PROPOSALS_BRANCH)
            .await?;

        Ok(())
    }

    async fn refresh_live_git_fields(&self, pr: &mut PullRequestInfo) -> Result<(), ForgeError> {
        if pr.state != PullRequestState::Open {
            return Ok(());
        }
        let source_ref = format!("refs/heads/{}", pr.source_branch);
        if self
            .git_runner
            .ref_exists(&self.bare_repo_path, &source_ref)
            .await?
        {
            pr.head_sha = self
                .git_runner
                .rev_parse(&self.bare_repo_path, &source_ref)
                .await?;

            let target_ref = format!("refs/heads/{}", pr.target_branch);
            if self
                .git_runner
                .ref_exists(&self.bare_repo_path, &target_ref)
                .await?
            {
                let stat = self
                    .git_runner
                    .diff_stat(&self.bare_repo_path, &target_ref, &source_ref)
                    .await?;
                pr.diff_stat = Some(stat);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ForgeProvider for LocalGitProvider {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        let title = req.title.trim().to_string();
        if title.is_empty() {
            return Err(ForgeError::InvalidInput(
                "pull request title must not be empty".to_string(),
            ));
        }

        let target_branch = if req.target_branch.trim().is_empty() {
            "main".to_string()
        } else {
            req.target_branch.trim().to_string()
        };
        let source_branch = req.source_branch.trim().to_string();

        validate_git_ref(&source_branch, "source_branch")?;
        validate_git_ref(&target_branch, "target_branch")?;

        if source_branch == target_branch {
            return Err(ForgeError::InvalidInput(
                "source_branch and target_branch must differ".to_string(),
            ));
        }
        if source_branch == PROPOSALS_BRANCH || target_branch == PROPOSALS_BRANCH {
            return Err(ForgeError::InvalidInput(format!(
                "branch '{PROPOSALS_BRANCH}' is reserved for proposal metadata"
            )));
        }

        let _guard = self.mutex.lock().await;

        let source_ref = format!("refs/heads/{source_branch}");
        if !self
            .git_runner
            .ref_exists(&self.bare_repo_path, &source_ref)
            .await?
        {
            return Err(ForgeError::InvalidInput(format!(
                "source branch '{source_branch}' does not exist in bare repository"
            )));
        }

        let head_sha = self
            .git_runner
            .rev_parse(&self.bare_repo_path, &source_ref)
            .await?;

        let target_ref = format!("refs/heads/{target_branch}");
        let diff_stat = if self
            .git_runner
            .ref_exists(&self.bare_repo_path, &target_ref)
            .await?
        {
            Some(
                self.git_runner
                    .diff_stat(&self.bare_repo_path, &target_ref, &source_ref)
                    .await?,
            )
        } else {
            None
        };

        let existing = self.read_all_proposals_unlocked().await?;
        if existing.iter().any(|p| {
            p.state == PullRequestState::Open
                && p.source_branch == source_branch
                && p.target_branch == target_branch
        }) {
            return Err(ForgeError::InvalidInput(format!(
                "an open pull request already exists for '{source_branch}' -> '{target_branch}'"
            )));
        }

        let next_id = existing
            .iter()
            .filter_map(|p| p.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let pr_id = next_id.to_string();

        let tenant = req
            .tenant
            .map(|t| sanitize_branch_segment(&t))
            .filter(|t| !t.is_empty())
            .or_else(|| parse_proposal_branch_tenant(&source_branch).map(ToOwned::to_owned));

        let now = Utc::now();
        let pr = PullRequestInfo {
            id: pr_id.clone(),
            title,
            body: req.body,
            state: PullRequestState::Open,
            source_branch,
            target_branch,
            head_sha,
            tenant,
            url: format!(
                "local://{}/.keel/proposals/{pr_id}.json",
                self.bare_repo_path.display()
            ),
            created_at: now,
            updated_at: now,
            comments: Vec::new(),
            diff_stat,
        };

        self.write_proposal_unlocked(&pr, &format!("chore(proposals): open proposal #{pr_id}"))
            .await?;

        Ok(pr)
    }

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        validate_pr_id(pr_id)?;
        let _guard = self.mutex.lock().await;

        let mut pr = self
            .read_proposal_unlocked(pr_id)
            .await?
            .ok_or_else(|| ForgeError::NotFound(pr_id.to_string()))?;

        self.refresh_live_git_fields(&mut pr).await?;
        Ok(pr)
    }

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError> {
        validate_pr_id(pr_id)?;
        if markdown_report.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "preview comment markdown_report must not be empty".to_string(),
            ));
        }

        let _guard = self.mutex.lock().await;

        let mut pr = self
            .read_proposal_unlocked(pr_id)
            .await?
            .ok_or_else(|| ForgeError::NotFound(pr_id.to_string()))?;

        let comment = PullRequestComment {
            id: format!("comment-{}", pr.comments.len() + 1),
            body: markdown_report.to_string(),
            created_at: Utc::now(),
        };
        pr.comments.push(comment);
        pr.updated_at = Utc::now();

        self.write_proposal_unlocked(
            &pr,
            &format!("chore(proposals): post preview comment on proposal #{pr_id}"),
        )
        .await?;

        Ok(())
    }

    async fn list_open_pull_requests(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError> {
        self.list_pull_requests(tenant, Some(PullRequestState::Open))
            .await
    }

    async fn list_pull_requests(
        &self,
        tenant: Option<&str>,
        state: Option<PullRequestState>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError> {
        let _guard = self.mutex.lock().await;
        let all = self.read_all_proposals_unlocked().await?;

        let mut filtered = Vec::new();
        for mut pr in all {
            if let Some(expected_state) = state
                && pr.state != expected_state
            {
                continue;
            }
            if let Some(tenant_filter) = tenant
                && !matches_tenant(&pr, tenant_filter)
            {
                continue;
            }
            if pr.state == PullRequestState::Open {
                self.refresh_live_git_fields(&mut pr).await?;
            }
            filtered.push(pr);
        }
        Ok(filtered)
    }

    async fn merge_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        validate_pr_id(pr_id)?;
        let _guard = self.mutex.lock().await;

        let mut pr = self
            .read_proposal_unlocked(pr_id)
            .await?
            .ok_or_else(|| ForgeError::NotFound(pr_id.to_string()))?;

        if pr.state != PullRequestState::Open {
            return Err(ForgeError::InvalidState(pr_id.to_string(), pr.state));
        }

        let tmp = TempWorkDir::create("merge")?;
        let remote_url = self.remote_url_str()?;
        self.git_runner.clone(remote_url, tmp.path()).await?;
        self.git_runner.fetch(tmp.path(), "origin").await?;

        let remote_source_ref = format!("origin/{}", pr.source_branch);
        let merged_sha = match self
            .git_runner
            .fast_forward_merge(tmp.path(), &pr.target_branch, &remote_source_ref)
            .await
        {
            Ok(sha) => sha,
            Err(GitError::CommandFailed { stderr, .. }) => {
                return Err(ForgeError::MergeConflict(pr_id.to_string(), stderr));
            }
            Err(e) => return Err(ForgeError::Git(e)),
        };

        self.git_runner
            .push(tmp.path(), "origin", &pr.target_branch)
            .await
            .map_err(|e| match e {
                GitError::CommandFailed { stderr, .. } => {
                    ForgeError::MergeConflict(pr_id.to_string(), stderr)
                }
                other => ForgeError::Git(other),
            })?;

        pr.state = PullRequestState::Merged;
        pr.head_sha = merged_sha;
        pr.updated_at = Utc::now();

        self.write_proposal_unlocked(&pr, &format!("chore(proposals): merge proposal #{pr_id}"))
            .await?;

        Ok(pr)
    }

    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        LocalGitProvider::close_pull_request(self, pr_id).await
    }
}

fn validate_pr_id(pr_id: &str) -> Result<(), ForgeError> {
    if pr_id.is_empty()
        || pr_id.len() > 64
        || !pr_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        || pr_id.starts_with('-')
    {
        return Err(ForgeError::InvalidInput(format!(
            "invalid pull request identifier '{pr_id}'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::format_proposal_branch;
    use tempfile::TempDir;

    async fn seed_bare_repo(tmp: &TempDir) -> (PathBuf, GitRunner, CommitAuthor) {
        let bare = tmp.path().join("infra-gitops.git");
        let seed_work = tmp.path().join("seed-work");
        let runner = GitRunner::new();
        let author = CommitAuthor::new("Seed Admin", "admin@keel.io");

        runner.init_bare(&bare).await.unwrap();
        runner
            .clone(bare.to_str().unwrap(), &seed_work)
            .await
            .unwrap();
        tokio::fs::create_dir_all(seed_work.join(".keel"))
            .await
            .unwrap();
        tokio::fs::write(seed_work.join(".keel/config.yaml"), "version: v1alpha1\n")
            .await
            .unwrap();
        runner.add(&seed_work, [".keel/config.yaml"]).await.unwrap();
        runner
            .commit(&seed_work, "chore: initial seed layout", &author)
            .await
            .unwrap();
        runner.push(&seed_work, "origin", "main").await.unwrap();

        (bare, runner, author)
    }

    #[tokio::test]
    async fn test_local_git_provider_full_pr_comment_filter_and_merge_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let (bare, runner, author) = seed_bare_repo(&tmp).await;
        let provider = LocalGitProvider::new(&bare);
        assert_eq!(provider.bare_repo_path(), bare.as_path());

        assert!(
            provider
                .list_open_pull_requests(None)
                .await
                .unwrap()
                .is_empty()
        );

        // Create branch for tenant alpha
        let work_alpha = tmp.path().join("work-alpha");
        runner
            .clone(bare.to_str().unwrap(), &work_alpha)
            .await
            .unwrap();
        let branch_alpha = format_proposal_branch("alpha", "install-authentik", 1_731_600_001);
        runner
            .checkout_new_branch(&work_alpha, &branch_alpha)
            .await
            .unwrap();
        let app_dir = work_alpha.join("tenants/alpha/apps/authentik");
        tokio::fs::create_dir_all(&app_dir).await.unwrap();
        tokio::fs::write(app_dir.join("values.yaml"), "replicas: 2\n")
            .await
            .unwrap();
        runner
            .add(&work_alpha, ["tenants/alpha/apps/authentik/values.yaml"])
            .await
            .unwrap();
        let alpha_commit_sha = runner
            .commit(&work_alpha, "feat(alpha): install authentik", &author)
            .await
            .unwrap();
        runner
            .push(&work_alpha, "origin", &branch_alpha)
            .await
            .unwrap();

        // Create branch for tenant beta
        let work_beta = tmp.path().join("work-beta");
        runner
            .clone(bare.to_str().unwrap(), &work_beta)
            .await
            .unwrap();
        let branch_beta = format_proposal_branch("beta", "provision-postgres", 1_731_600_002);
        runner
            .checkout_new_branch(&work_beta, &branch_beta)
            .await
            .unwrap();
        tokio::fs::write(work_beta.join(".keel/config.yaml"), "version: v1alpha2\n")
            .await
            .unwrap();
        runner.add(&work_beta, [".keel/config.yaml"]).await.unwrap();
        runner
            .commit(&work_beta, "feat(beta): update config", &author)
            .await
            .unwrap();
        runner
            .push(&work_beta, "origin", &branch_beta)
            .await
            .unwrap();

        // Open PR #1 (alpha) and PR #2 (beta)
        let pr1 = provider
            .create_pull_request(
                CreatePrRequest::new(
                    "Install Authentik for alpha",
                    "Deploys Authentik via HelmRelease",
                    &branch_alpha,
                )
                .with_author("Seed Admin <admin@keel.io>"),
            )
            .await
            .unwrap();
        assert_eq!(pr1.id, "1");
        assert_eq!(pr1.state, PullRequestState::Open);
        assert_eq!(pr1.tenant.as_deref(), Some("alpha"));
        assert_eq!(pr1.head_sha, alpha_commit_sha);
        assert!(pr1.diff_stat.as_deref().unwrap().contains("values.yaml"));

        // Duplicate open PR for the same branch must fail
        assert!(
            provider
                .create_pull_request(CreatePrRequest::new("Dup", "Body", &branch_alpha))
                .await
                .is_err()
        );

        let pr2 = provider
            .create_pull_request(CreatePrRequest::new(
                "Provision Postgres for beta",
                "Adds managed db",
                &branch_beta,
            ))
            .await
            .unwrap();
        assert_eq!(pr2.id, "2");
        assert_eq!(pr2.tenant.as_deref(), Some("beta"));

        // Verify .keel/proposals/<pr_id>.json is tracked in Git on keel-proposals branch
        let meta_check = tmp.path().join("meta-check");
        runner
            .clone_branch(bare.to_str().unwrap(), "keel-proposals", &meta_check)
            .await
            .unwrap();
        assert!(meta_check.join(".keel/proposals/1.json").exists());
        assert!(meta_check.join(".keel/proposals/2.json").exists());

        // Filter open PRs by tenant
        let all_open = provider.list_open_pull_requests(None).await.unwrap();
        assert_eq!(all_open.len(), 2);
        let alpha_open = provider
            .list_open_pull_requests(Some("alpha"))
            .await
            .unwrap();
        assert_eq!(alpha_open.len(), 1);
        assert_eq!(alpha_open[0].id, "1");
        let beta_open = provider
            .list_open_pull_requests(Some("beta"))
            .await
            .unwrap();
        assert_eq!(beta_open.len(), 1);
        assert_eq!(beta_open[0].id, "2");
        assert!(
            provider
                .list_open_pull_requests(Some("gamma"))
                .await
                .unwrap()
                .is_empty()
        );

        // Post PKO preview comment on PR #1
        let report = "### PKO v2 Preview\n- `+ kubernetes:helm.sh/v4:Release authentik`";
        provider.post_preview_comment("1", report).await.unwrap();

        let fetched_pr1 = provider.get_pull_request("1").await.unwrap();
        assert_eq!(fetched_pr1.comments.len(), 1);
        assert_eq!(fetched_pr1.comments[0].body, report);
        assert_eq!(fetched_pr1.head_sha, alpha_commit_sha);

        // Fast-forward merge PR #1 into main
        let merged_pr1 = provider.merge_pull_request("1").await.unwrap();
        assert_eq!(merged_pr1.state, PullRequestState::Merged);
        assert_eq!(merged_pr1.head_sha, alpha_commit_sha);

        // Verify main in bare repo fast-forwarded to alpha_commit_sha and contains the new file
        let verify_main = tmp.path().join("verify-main");
        runner
            .clone(bare.to_str().unwrap(), &verify_main)
            .await
            .unwrap();
        assert_eq!(
            runner.current_head_sha(&verify_main).await.unwrap(),
            alpha_commit_sha
        );
        assert!(
            verify_main
                .join("tenants/alpha/apps/authentik/values.yaml")
                .exists()
        );

        // Re-merging PR #1 fails with InvalidState
        assert!(matches!(
            provider.merge_pull_request("1").await,
            Err(ForgeError::InvalidState(_, PullRequestState::Merged))
        ));

        // PR #2 is no longer a fast-forward of main because main advanced; must return MergeConflict
        assert!(matches!(
            provider.merge_pull_request("2").await,
            Err(ForgeError::MergeConflict(_, _))
        ));

        // Close PR #2 and verify no open PRs remain
        let closed_pr2 = provider.close_pull_request("2").await.unwrap();
        assert_eq!(closed_pr2.state, PullRequestState::Closed);
        assert!(matches!(
            provider.close_pull_request("2").await,
            Err(ForgeError::InvalidState(_, PullRequestState::Closed))
        ));
        assert!(
            provider
                .list_open_pull_requests(None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_local_git_provider_validation_and_not_found_errors() {
        let tmp = TempDir::new().unwrap();
        let (bare, _, _) = seed_bare_repo(&tmp).await;
        let provider = LocalGitProvider::new(&bare);

        assert!(matches!(
            provider.get_pull_request("999").await,
            Err(ForgeError::NotFound(_))
        ));
        assert!(matches!(
            provider.get_pull_request("../etc/passwd").await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            provider.post_preview_comment("1", "   ").await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            provider
                .create_pull_request(CreatePrRequest::new("", "body", "keel/a/b-1"))
                .await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            provider
                .create_pull_request(CreatePrRequest::new("Title", "body", "main"))
                .await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            provider
                .create_pull_request(CreatePrRequest::new(
                    "Title",
                    "body",
                    "keel/missing/branch-1"
                ))
                .await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(matches!(
            provider
                .create_pull_request(CreatePrRequest::new("Title", "body", "keel-proposals"))
                .await,
            Err(ForgeError::InvalidInput(_))
        ));
    }
}
