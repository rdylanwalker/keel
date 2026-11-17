//! Pure `git` CLI runner executing operations via `tokio::process::Command`
//! with explicit argument vectors (zero shell invocation per `NFR-SEC-06`)
//! and ephemeral SSH/HTTPS credential injection (`NFR-SEC-05`).

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Errors returned by [`GitRunner`] operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("Invalid git argument ({field}): {reason}")]
    InvalidArgument { field: &'static str, reason: String },

    #[error("Git process I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Git command `git {subcommand}` failed (exit {exit_code:?}): {stderr}")]
    CommandFailed {
        subcommand: String,
        exit_code: Option<i32>,
        stderr: String,
    },
}

/// Commit author metadata (`Name <email>`) recorded in the Git audit trail (`NFR-OBS-01`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
}

impl CommitAuthor {
    /// Creates a new [`CommitAuthor`].
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    /// Validates and formats the author as `"Name <email>"` for `git commit --author`.
    pub fn to_git_author_flag(&self) -> Result<String, GitError> {
        let name = self.name.trim();
        let email = self.email.trim();
        if name.is_empty() {
            return Err(GitError::InvalidArgument {
                field: "author.name",
                reason: "author name must not be empty".to_string(),
            });
        }
        if email.is_empty() || !email.contains('@') {
            return Err(GitError::InvalidArgument {
                field: "author.email",
                reason: "author email must be a valid non-empty email".to_string(),
            });
        }
        for ch in ['\0', '\n', '\r', '<', '>'] {
            if name.contains(ch) || email.contains(ch) {
                return Err(GitError::InvalidArgument {
                    field: "author",
                    reason: "author fields must not contain control characters or angle brackets"
                        .to_string(),
                });
            }
        }
        if name.starts_with('-') || email.starts_with('-') {
            return Err(GitError::InvalidArgument {
                field: "author",
                reason: "author name and email must not start with '-'".to_string(),
            });
        }
        Ok(format!("{name} <{email}>"))
    }
}

/// Ephemeral credentials injected into `git` subprocesses without persisting or logging secrets.
#[derive(Clone, Default)]
pub enum GitCredentials {
    #[default]
    None,
    /// Ephemeral SSH private key written with strict `0600` permissions and deleted on drop.
    SshKey {
        private_key_pem: String,
        known_hosts: Option<String>,
    },
    /// Ephemeral HTTPS Basic / Personal Access Token credentials injected via Git environment config.
    HttpsBasic {
        username: String,
        password_or_token: String,
    },
}

impl fmt::Debug for GitCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "GitCredentials::None"),
            Self::SshKey { known_hosts, .. } => f
                .debug_struct("GitCredentials::SshKey")
                .field("private_key_pem", &"[REDACTED]")
                .field("known_hosts", &known_hosts.as_ref().map(|_| "[PRESENT]"))
                .finish(),
            Self::HttpsBasic { username, .. } => f
                .debug_struct("GitCredentials::HttpsBasic")
                .field("username", username)
                .field("password_or_token", &"[REDACTED]")
                .finish(),
        }
    }
}

/// RAII temporary secret file created atomically with `0600` permissions and scrubbed on [`Drop`].
pub struct EphemeralSecretFile {
    path: PathBuf,
    byte_len: usize,
}

impl EphemeralSecretFile {
    /// Writes secret material to a freshly created `0600` file in the OS temporary directory.
    pub fn create(prefix: &str, contents: &[u8]) -> Result<Self, GitError> {
        let safe_prefix = sanitize_branch_segment(prefix);
        let file_name = format!(
            "keel-{}-{}.tmp",
            if safe_prefix.is_empty() {
                "secret"
            } else {
                &safe_prefix
            },
            uuid::Uuid::new_v4()
        );
        let path = std::env::temp_dir().join(file_name);

        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let mut file = opts.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        Ok(Self {
            path,
            byte_len: contents.len(),
        })
    }

    /// Path to the ephemeral secret file while this guard remains alive.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for EphemeralSecretFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EphemeralSecretFile")
            .field("path", &self.path)
            .finish()
    }
}

impl Drop for EphemeralSecretFile {
    fn drop(&mut self) {
        if self.byte_len > 0
            && let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&self.path)
        {
            let zeros = vec![0u8; self.byte_len];
            let _ = f.write_all(&zeros);
            let _ = f.sync_all();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Prepared environment variables and RAII guards for a single `git` invocation.
pub(crate) struct PreparedAuthEnv {
    pub envs: Vec<(String, String)>,
    pub redact_tokens: Vec<String>,
    pub _files: Vec<EphemeralSecretFile>,
}

impl PreparedAuthEnv {
    pub fn from_credentials(creds: &GitCredentials) -> Result<Self, GitError> {
        match creds {
            GitCredentials::None => Ok(Self {
                envs: Vec::new(),
                redact_tokens: Vec::new(),
                _files: Vec::new(),
            }),
            GitCredentials::SshKey {
                private_key_pem,
                known_hosts,
            } => {
                if private_key_pem.trim().is_empty() || private_key_pem.contains('\0') {
                    return Err(GitError::InvalidArgument {
                        field: "private_key_pem",
                        reason: "SSH private key must be non-empty and free of NUL bytes"
                            .to_string(),
                    });
                }

                let mut normalized_key = private_key_pem.clone();
                if !normalized_key.ends_with('\n') {
                    normalized_key.push('\n');
                }
                let key_file = EphemeralSecretFile::create("ssh-key", normalized_key.as_bytes())?;
                let quoted_key_path = shell_single_quote_path(key_file.path())?;

                let mut files = vec![key_file];
                let host_check_opts = if let Some(hosts) = known_hosts {
                    if hosts.contains('\0') || hosts.contains('\n') || hosts.contains('\r') {
                        return Err(GitError::InvalidArgument {
                            field: "known_hosts",
                            reason:
                                "known_hosts must not contain NUL or newline control characters"
                                    .to_string(),
                        });
                    }
                    let hosts_file = EphemeralSecretFile::create("known-hosts", hosts.as_bytes())?;
                    let quoted_hosts_path = shell_single_quote_path(hosts_file.path())?;
                    files.push(hosts_file);
                    format!(
                        "-o StrictHostKeyChecking=yes -o UserKnownHostsFile={quoted_hosts_path}"
                    )
                } else {
                    tracing::warn!(
                        "SSH known_hosts not provided; using StrictHostKeyChecking=accept-new (TOFU)"
                    );
                    "-o StrictHostKeyChecking=accept-new".to_string()
                };

                let ssh_cmd = format!(
                    "ssh -i {quoted_key_path} -o IdentitiesOnly=yes -o BatchMode=yes {host_check_opts}"
                );

                Ok(Self {
                    envs: vec![("GIT_SSH_COMMAND".to_string(), ssh_cmd)],
                    redact_tokens: vec![private_key_pem.trim().to_string()],
                    _files: files,
                })
            }
            GitCredentials::HttpsBasic {
                username,
                password_or_token,
            } => {
                if username.is_empty()
                    || password_or_token.is_empty()
                    || username.contains('\0')
                    || username.contains('\n')
                    || username.contains('\r')
                    || password_or_token.contains('\0')
                    || password_or_token.contains('\n')
                    || password_or_token.contains('\r')
                {
                    return Err(GitError::InvalidArgument {
                        field: "https_credentials",
                        reason:
                            "HTTPS credentials must be non-empty and free of control characters"
                                .to_string(),
                    });
                }

                use base64::Engine;
                let raw = format!("{username}:{password_or_token}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                let header_value = format!("Authorization: Basic {encoded}");

                Ok(Self {
                    envs: vec![
                        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                        (
                            "GIT_CONFIG_KEY_0".to_string(),
                            "http.extraHeader".to_string(),
                        ),
                        ("GIT_CONFIG_VALUE_0".to_string(), header_value),
                    ],
                    redact_tokens: vec![password_or_token.clone(), encoded],
                    _files: Vec::new(),
                })
            }
        }
    }
}

fn shell_single_quote_path(path: &Path) -> Result<String, GitError> {
    let s = path.to_str().ok_or_else(|| GitError::InvalidArgument {
        field: "temp_path",
        reason: "temporary path is not valid UTF-8".to_string(),
    })?;
    if s.starts_with('-') || s.contains('\0') || s.contains('\n') || s.contains('\r') {
        return Err(GitError::InvalidArgument {
            field: "temp_path",
            reason: "temporary path contains control characters or leading '-'".to_string(),
        });
    }
    let escaped = s.replace('\'', "'\\''");
    Ok(format!("'{escaped}'"))
}

/// Sanitizes an arbitrary string segment (such as a tenant ID or action name)
/// into lowercase ASCII alphanumeric characters separated by single hyphens.
pub fn sanitize_branch_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = true; // suppresses leading dashes

    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }

    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 63 {
        out.truncate(63);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Deterministically formats a proposal branch name as `keel/<tenant>/<action>-<timestamp>`,
/// sanitizing `tenant` and `action` to safe Git branch characters.
pub fn format_proposal_branch(tenant: &str, action: &str, timestamp: i64) -> String {
    let clean_tenant = {
        let s = sanitize_branch_segment(tenant);
        if s.is_empty() {
            "default".to_string()
        } else {
            s
        }
    };
    let clean_action = {
        let s = sanitize_branch_segment(action);
        if s.is_empty() {
            "proposal".to_string()
        } else {
            s
        }
    };
    format!("keel/{clean_tenant}/{clean_action}-{timestamp}")
}

/// Extracts the `<tenant>` segment from a canonical `keel/<tenant>/<action>-<timestamp>` branch name.
pub fn parse_proposal_branch_tenant(branch: &str) -> Option<&str> {
    let rest = branch.strip_prefix("keel/")?;
    let (tenant, remainder) = rest.split_once('/')?;
    if tenant.is_empty() || remainder.is_empty() {
        return None;
    }
    Some(tenant)
}

/// Validates a non-empty Git argument, blocking NUL/newline injection and leading `-` option injection.
pub(crate) fn validate_non_option_arg(value: &str, field: &'static str) -> Result<(), GitError> {
    if value.is_empty() {
        return Err(GitError::InvalidArgument {
            field,
            reason: "value must not be empty".to_string(),
        });
    }
    if value.starts_with('-') {
        return Err(GitError::InvalidArgument {
            field,
            reason: "value must not start with '-' (option injection defense)".to_string(),
        });
    }
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(GitError::InvalidArgument {
            field,
            reason: "value must not contain NUL or newline characters".to_string(),
        });
    }
    Ok(())
}

/// Validates a Git branch or ref name against Git's ref-format safety rules and option injection.
pub fn validate_git_ref(ref_name: &str, field: &'static str) -> Result<(), GitError> {
    validate_non_option_arg(ref_name, field)?;

    if ref_name.starts_with('/')
        || ref_name.ends_with('/')
        || ref_name.starts_with('.')
        || ref_name.ends_with('.')
        || ref_name.ends_with(".lock")
        || ref_name.contains("..")
        || ref_name.contains("//")
        || ref_name.contains("@{")
    {
        return Err(GitError::InvalidArgument {
            field,
            reason: format!("invalid git ref syntax: '{ref_name}'"),
        });
    }

    for ch in ref_name.chars() {
        if ch.is_control() || matches!(ch, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Err(GitError::InvalidArgument {
                field,
                reason: format!("disallowed character '{ch}' in git ref"),
            });
        }
    }

    for component in ref_name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(GitError::InvalidArgument {
                field,
                reason: format!("invalid git ref path component in '{ref_name}'"),
            });
        }
    }

    Ok(())
}

/// Validates a Git refspec (e.g. `main`, `keel/t/a-1`, or `HEAD:refs/heads/main`).
fn validate_refspec(refspec: &str) -> Result<(), GitError> {
    validate_non_option_arg(refspec, "refspec")?;
    if let Some((src, dst)) = refspec.split_once(':') {
        if !src.is_empty() {
            validate_git_ref(src, "refspec.src")?;
        }
        validate_git_ref(dst, "refspec.dst")?;
        Ok(())
    } else {
        validate_git_ref(refspec, "refspec")
    }
}

/// Validates a remote URL or local repository path, blocking `ext::` transport command execution.
fn validate_remote_url(remote_url: &str) -> Result<(), GitError> {
    validate_non_option_arg(remote_url, "remote_url")?;
    if remote_url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("ext::")
    {
        return Err(GitError::InvalidArgument {
            field: "remote_url",
            reason: "git ext:: remote transport is prohibited".to_string(),
        });
    }
    Ok(())
}

/// Direct `git` CLI runner executing commands via `tokio::process::Command::new("git")`
/// with explicit argument vectors (never invoking a shell).
#[derive(Debug, Clone)]
pub struct GitRunner {
    git_binary: PathBuf,
    credentials: GitCredentials,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GitRunner {
    /// Creates a new [`GitRunner`] using `"git"` on `PATH` and no credentials.
    pub fn new() -> Self {
        Self {
            git_binary: PathBuf::from("git"),
            credentials: GitCredentials::None,
        }
    }

    /// Returns a clone of this runner configured with ephemeral [`GitCredentials`].
    pub fn with_credentials(mut self, credentials: GitCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// Returns a reference to the configured [`GitCredentials`].
    pub fn credentials(&self) -> &GitCredentials {
        &self.credentials
    }

    /// Initializes a bare Git repository with `main` as the initial `HEAD` branch.
    pub async fn init_bare(&self, repo_path: &Path) -> Result<(), GitError> {
        let repo_str = path_to_non_empty_str(repo_path, "repo_path")?;
        if let Some(parent) = repo_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        self.exec(
            None,
            "init",
            &["init", "--bare", "--initial-branch=main", "--", repo_str],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Clones `remote_url` into `dest_path`.
    pub async fn clone(&self, remote_url: &str, dest_path: &Path) -> Result<(), GitError> {
        validate_remote_url(remote_url)?;
        let dest_str = path_to_non_empty_str(dest_path, "dest_path")?;
        self.exec(None, "clone", &["clone", "--", remote_url, dest_str], &[])
            .await?;
        Ok(())
    }

    /// Clones a single `branch` from `remote_url` into `dest_path`.
    pub async fn clone_branch(
        &self,
        remote_url: &str,
        branch: &str,
        dest_path: &Path,
    ) -> Result<(), GitError> {
        validate_remote_url(remote_url)?;
        validate_git_ref(branch, "branch")?;
        let dest_str = path_to_non_empty_str(dest_path, "dest_path")?;
        self.exec(
            None,
            "clone",
            &[
                "clone",
                "--branch",
                branch,
                "--single-branch",
                "--",
                remote_url,
                dest_str,
            ],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Fetches updates from `remote` (e.g. `"origin"`).
    pub async fn fetch(&self, work_dir: &Path, remote: &str) -> Result<(), GitError> {
        validate_git_ref(remote, "remote")?;
        self.exec(
            Some(work_dir),
            "fetch",
            &["fetch", "--prune", "--", remote],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Checks out `branch` and fast-forwards/resets it to `<remote>/<branch>` when the remote ref exists.
    pub async fn sync_branch_from_remote(
        &self,
        work_dir: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<(), GitError> {
        validate_git_ref(remote, "remote")?;
        validate_git_ref(branch, "branch")?;
        self.checkout_branch(work_dir, branch).await?;
        if self.fetch(work_dir, remote).await.is_ok() {
            let remote_ref = format!("refs/remotes/{remote}/{branch}");
            if self.ref_exists(work_dir, &remote_ref).await? {
                let target = format!("{remote}/{branch}");
                self.exec(Some(work_dir), "reset", &["reset", "--hard", &target], &[])
                    .await?;
            }
        }
        Ok(())
    }

    /// Creates and checks out a new branch (`git checkout -b <branch>`).
    pub async fn checkout_new_branch(&self, work_dir: &Path, branch: &str) -> Result<(), GitError> {
        validate_git_ref(branch, "branch")?;
        self.exec(
            Some(work_dir),
            "checkout",
            &["checkout", "-b", branch, "--"],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Checks out an existing local or remote-tracking branch.
    pub async fn checkout_branch(&self, work_dir: &Path, branch: &str) -> Result<(), GitError> {
        validate_git_ref(branch, "branch")?;
        let local_ref = format!("refs/heads/{branch}");
        if self.ref_exists(work_dir, &local_ref).await? {
            self.exec(Some(work_dir), "checkout", &["checkout", branch, "--"], &[])
                .await?;
        } else {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            if self.ref_exists(work_dir, &remote_ref).await? {
                self.exec(
                    Some(work_dir),
                    "checkout",
                    &["checkout", "--track", &format!("origin/{branch}")],
                    &[],
                )
                .await?;
            } else {
                return Err(GitError::CommandFailed {
                    subcommand: "checkout".to_string(),
                    exit_code: Some(1),
                    stderr: format!("branch '{branch}' does not exist locally or on origin"),
                });
            }
        }
        Ok(())
    }

    /// Stages one or more file paths via `git add -- <paths...>`.
    pub async fn add<I, P>(&self, work_dir: &Path, paths: I) -> Result<(), GitError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut args: Vec<String> = vec!["add".to_string(), "--".to_string()];
        let mut count = 0usize;
        for p in paths {
            let s = path_to_non_empty_str(p.as_ref(), "path")?;
            args.push(s.to_string());
            count += 1;
        }
        if count == 0 {
            return Err(GitError::InvalidArgument {
                field: "paths",
                reason: "at least one path must be provided to git add".to_string(),
            });
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.exec(Some(work_dir), "add", &arg_refs, &[]).await?;
        Ok(())
    }

    /// Commits staged changes with explicit authorship and returns the new commit SHA.
    pub async fn commit(
        &self,
        work_dir: &Path,
        message: &str,
        author: &CommitAuthor,
    ) -> Result<String, GitError> {
        let trimmed = message.trim();
        if trimmed.is_empty() || trimmed.contains('\0') {
            return Err(GitError::InvalidArgument {
                field: "message",
                reason: "commit message must be non-empty and free of NUL bytes".to_string(),
            });
        }
        let author_flag = format!("--author={}", author.to_git_author_flag()?);
        let extra_envs = [
            ("GIT_AUTHOR_NAME", author.name.trim()),
            ("GIT_AUTHOR_EMAIL", author.email.trim()),
            ("GIT_COMMITTER_NAME", author.name.trim()),
            ("GIT_COMMITTER_EMAIL", author.email.trim()),
        ];

        self.exec(
            Some(work_dir),
            "commit",
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                &author_flag,
                "-m",
                trimmed,
            ],
            &extra_envs,
        )
        .await?;

        self.current_head_sha(work_dir).await
    }

    /// Pushes `refspec` to `remote` (`git push -- <remote> <refspec>`).
    pub async fn push(&self, work_dir: &Path, remote: &str, refspec: &str) -> Result<(), GitError> {
        validate_git_ref(remote, "remote")?;
        validate_refspec(refspec)?;
        self.exec(
            Some(work_dir),
            "push",
            &["push", "--", remote, refspec],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Performs a fast-forward-only merge of `source_ref` into `target_branch` and returns the new HEAD SHA.
    pub async fn fast_forward_merge(
        &self,
        work_dir: &Path,
        target_branch: &str,
        source_ref: &str,
    ) -> Result<String, GitError> {
        validate_git_ref(target_branch, "target_branch")?;
        validate_git_ref(source_ref, "source_ref")?;

        let local_target = format!("refs/heads/{target_branch}");
        let remote_target = format!("refs/remotes/origin/{target_branch}");

        if self.ref_exists(work_dir, &local_target).await?
            || self.ref_exists(work_dir, &remote_target).await?
        {
            self.checkout_branch(work_dir, target_branch).await?;
            self.exec(
                Some(work_dir),
                "merge",
                &["merge", "--ff-only", "--", source_ref],
                &[],
            )
            .await?;
        } else {
            // Target branch does not exist yet (e.g. empty repository); create it directly at source_ref.
            self.exec(
                Some(work_dir),
                "checkout",
                &["checkout", "-b", target_branch, source_ref],
                &[],
            )
            .await?;
        }

        self.current_head_sha(work_dir).await
    }

    /// Returns the full commit SHA of `HEAD` in `work_dir`.
    pub async fn current_head_sha(&self, work_dir: &Path) -> Result<String, GitError> {
        self.rev_parse(work_dir, "HEAD").await
    }

    /// Resolves any valid `rev` (e.g. `HEAD`, `refs/heads/main`, `origin/main`) to a commit SHA.
    pub async fn rev_parse(&self, work_dir: &Path, rev: &str) -> Result<String, GitError> {
        validate_git_ref(rev, "rev")?;
        let out = self
            .exec(
                Some(work_dir),
                "rev-parse",
                &["rev-parse", "--verify", rev],
                &[],
            )
            .await?;
        Ok(out.trim().to_string())
    }

    /// Checks whether a full Git ref (e.g. `refs/heads/main`) exists in `work_dir`.
    pub async fn ref_exists(&self, work_dir: &Path, full_ref: &str) -> Result<bool, GitError> {
        validate_git_ref(full_ref, "full_ref")?;
        let res = self
            .exec(
                Some(work_dir),
                "show-ref",
                &["show-ref", "--verify", "--quiet", "--", full_ref],
                &[],
            )
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(GitError::CommandFailed { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Returns the human-readable `git diff --stat <base_ref> <head_ref>` output.
    pub async fn diff_stat(
        &self,
        work_dir: &Path,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<String, GitError> {
        validate_git_ref(base_ref, "base_ref")?;
        validate_git_ref(head_ref, "head_ref")?;
        let out = self
            .exec(
                Some(work_dir),
                "diff",
                &["diff", "--stat", base_ref, head_ref, "--"],
                &[],
            )
            .await?;
        Ok(out.trim().to_string())
    }

    /// Returns the unified `git diff <base_ref> <head_ref>` patch output.
    pub async fn diff_refs(
        &self,
        work_dir: &Path,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<String, GitError> {
        validate_git_ref(base_ref, "base_ref")?;
        validate_git_ref(head_ref, "head_ref")?;
        let out = self
            .exec(
                Some(work_dir),
                "diff",
                &["diff", "--no-color", base_ref, head_ref, "--"],
                &[],
            )
            .await?;
        Ok(out.trim().to_string())
    }

    /// Lists up to `limit` commit SHAs touching `repo_rel_path` in `repo_dir`.
    pub async fn log_file_commits(
        &self,
        repo_dir: &Path,
        repo_rel_path: &str,
        limit: usize,
    ) -> Result<Vec<String>, GitError> {
        validate_non_option_arg(repo_rel_path, "repo_rel_path")?;
        let limit_str = limit.max(1).to_string();
        let out = self
            .exec(
                Some(repo_dir),
                "log",
                &["log", "-n", &limit_str, "--format=%H", "--", repo_rel_path],
                &[],
            )
            .await?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// Reads the UTF-8 contents of an object `<rev>:<repo_rel_path>` from a bare or non-bare repo.
    pub async fn show_file_at_ref(
        &self,
        repo_dir: &Path,
        rev: &str,
        repo_rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        validate_git_ref(rev, "rev")?;
        validate_non_option_arg(repo_rel_path, "repo_rel_path")?;
        let spec = format!("{rev}:{repo_rel_path}");
        match self
            .exec(Some(repo_dir), "show", &["show", &spec], &[])
            .await
        {
            Ok(content) => Ok(Some(content)),
            Err(GitError::CommandFailed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Lists entry names inside `<rev>:<repo_rel_dir>` in a bare or non-bare repo.
    pub(crate) async fn list_tree_names(
        &self,
        repo_dir: &Path,
        rev: &str,
        repo_rel_dir: &str,
    ) -> Result<Vec<String>, GitError> {
        validate_git_ref(rev, "rev")?;
        validate_non_option_arg(repo_rel_dir, "repo_rel_dir")?;
        let spec = format!("{rev}:{repo_rel_dir}");
        match self
            .exec(
                Some(repo_dir),
                "ls-tree",
                &["ls-tree", "--name-only", &spec],
                &[],
            )
            .await
        {
            Ok(out) => Ok(out
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(ToOwned::to_owned)
                .collect()),
            Err(GitError::CommandFailed { .. }) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Initializes a local working directory repository with a specified initial branch and remote.
    pub(crate) async fn init_worktree_with_remote(
        &self,
        work_dir: &Path,
        initial_branch: &str,
        remote_url: &str,
    ) -> Result<(), GitError> {
        validate_git_ref(initial_branch, "initial_branch")?;
        validate_remote_url(remote_url)?;
        let dir_str = path_to_non_empty_str(work_dir, "work_dir")?;
        self.exec(
            None,
            "init",
            &[
                "init",
                &format!("--initial-branch={initial_branch}"),
                "--",
                dir_str,
            ],
            &[],
        )
        .await?;
        self.exec(
            Some(work_dir),
            "remote",
            &["remote", "add", "origin", "--", remote_url],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Reads the configured URL for `remote` (`git remote get-url -- <remote>`).
    pub async fn get_remote_url(&self, work_dir: &Path, remote: &str) -> Result<String, GitError> {
        validate_git_ref(remote, "remote")?;
        let out = self
            .exec(
                Some(work_dir),
                "remote",
                &["remote", "get-url", "--", remote],
                &[],
            )
            .await?;
        Ok(out.trim().to_string())
    }

    /// Updates the configured URL for `remote` (`git remote set-url -- <remote> <remote_url>`).
    pub async fn set_remote_url(
        &self,
        work_dir: &Path,
        remote: &str,
        remote_url: &str,
    ) -> Result<(), GitError> {
        validate_git_ref(remote, "remote")?;
        validate_remote_url(remote_url)?;
        self.exec(
            Some(work_dir),
            "remote",
            &["remote", "set-url", "--", remote, remote_url],
            &[],
        )
        .await?;
        Ok(())
    }

    /// Executes `git` directly via `tokio::process::Command` with argument vectors and ephemeral auth.
    async fn exec(
        &self,
        work_dir: Option<&Path>,
        subcommand: &str,
        args: &[&str],
        extra_envs: &[(&str, &str)],
    ) -> Result<String, GitError> {
        for arg in args {
            if arg.contains('\0') || arg.contains('\n') || arg.contains('\r') {
                return Err(GitError::InvalidArgument {
                    field: "git_arg",
                    reason: "git arguments must not contain NUL or newline control characters"
                        .to_string(),
                });
            }
        }
        let auth_guard = PreparedAuthEnv::from_credentials(&self.credentials)?;

        let mut cmd = Command::new(&self.git_binary);
        // Block git's `ext::` arbitrary command execution protocol and interactive prompts.
        cmd.arg("-c")
            .arg("protocol.ext.allow=never")
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null());

        if let Some(dir) = work_dir {
            cmd.current_dir(dir);
        }

        for (k, v) in &auth_guard.envs {
            cmd.env(k, v);
        }
        for (k, v) in extra_envs {
            cmd.env(k, v);
        }

        tracing::debug!(subcommand, ?work_dir, "Executing git command");

        let output = cmd.output().await?;
        // Ensure temporary SSH key files stay alive until subprocess exits, then drop immediately.
        let redact_tokens = auth_guard.redact_tokens.clone();
        drop(auth_guard);

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            Ok(redact_sensitive(stdout, &redact_tokens))
        } else {
            let stderr_raw = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let combined = if stderr_raw.is_empty() {
                stdout_raw
            } else {
                stderr_raw
            };
            Err(GitError::CommandFailed {
                subcommand: subcommand.to_string(),
                exit_code: output.status.code(),
                stderr: redact_sensitive(combined, &redact_tokens),
            })
        }
    }
}

fn path_to_non_empty_str<'a>(path: &'a Path, field: &'static str) -> Result<&'a str, GitError> {
    let s = path.to_str().ok_or_else(|| GitError::InvalidArgument {
        field,
        reason: "path must be valid UTF-8".to_string(),
    })?;
    validate_non_option_arg(s, field)?;
    Ok(s)
}

fn redact_sensitive(mut text: String, secrets: &[String]) -> String {
    for secret in secrets {
        if !secret.is_empty() && text.contains(secret) {
            text = text.replace(secret, "[REDACTED]");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_format_proposal_branch_and_sanitization() {
        assert_eq!(
            format_proposal_branch("acme", "deploy-postgres", 1_731_600_000),
            "keel/acme/deploy-postgres-1731600000"
        );
        assert_eq!(
            format_proposal_branch("Tenant_Alpha / Prod!", "Install Authentik!", 42),
            "keel/tenant-alpha-prod/install-authentik-42"
        );
        assert_eq!(
            format_proposal_branch("../etc/passwd", "$(whoami); rm -rf /", 99),
            "keel/etc-passwd/whoami-rm-rf-99"
        );
        assert_eq!(
            format_proposal_branch("---", "!!!", 100),
            "keel/default/proposal-100"
        );

        let long_seg = "a".repeat(100);
        let branch = format_proposal_branch(&long_seg, "update", 1);
        let parsed_tenant = parse_proposal_branch_tenant(&branch).unwrap();
        assert_eq!(parsed_tenant.len(), 63);
        assert!(validate_git_ref(&branch, "branch").is_ok());

        assert_eq!(
            parse_proposal_branch_tenant("keel/acme/deploy-123"),
            Some("acme")
        );
        assert_eq!(parse_proposal_branch_tenant("main"), None);
        assert_eq!(parse_proposal_branch_tenant("keel/onlytenant"), None);
        assert_eq!(parse_proposal_branch_tenant("keel//deploy-1"), None);
    }

    #[test]
    fn test_argument_and_ref_injection_prevention() {
        // Leading dash option injection attempts must be blocked
        assert!(validate_remote_url("--upload-pack=evil").is_err());
        assert!(validate_remote_url("ext::sh -c touch% /tmp/pwn").is_err());
        assert!(validate_remote_url("EXT::sh -c id").is_err());
        assert!(validate_git_ref("--orphan", "branch").is_err());
        assert!(validate_git_ref("-b", "branch").is_err());
        assert!(validate_git_ref("keel/../evil", "branch").is_err());
        assert!(validate_git_ref("keel/bad@{0}", "branch").is_err());
        assert!(validate_git_ref("keel/bad.lock", "branch").is_err());
        assert!(validate_git_ref("keel/space branch", "branch").is_err());
        assert!(validate_git_ref("keel/null\0byte", "branch").is_err());
        assert!(validate_refspec("--mirror").is_err());
        assert!(validate_refspec("HEAD:refs/heads/main").is_ok());
    }

    #[test]
    fn test_commit_author_validation() {
        let valid = CommitAuthor::new("Alice Operator", "alice@example.com");
        assert_eq!(
            valid.to_git_author_flag().unwrap(),
            "Alice Operator <alice@example.com>"
        );

        assert!(
            CommitAuthor::new("", "alice@example.com")
                .to_git_author_flag()
                .is_err()
        );
        assert!(
            CommitAuthor::new("Alice", "not-an-email")
                .to_git_author_flag()
                .is_err()
        );
        assert!(
            CommitAuthor::new("Alice <injected>", "alice@example.com")
                .to_git_author_flag()
                .is_err()
        );
        assert!(
            CommitAuthor::new("Alice\nBob", "alice@example.com")
                .to_git_author_flag()
                .is_err()
        );
    }

    #[test]
    fn test_ephemeral_ssh_key_permissions_and_drop_cleanup() {
        let fake_pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nsuper-secret-key-material\n-----END OPENSSH PRIVATE KEY-----";
        let creds = GitCredentials::SshKey {
            private_key_pem: fake_pem.to_string(),
            known_hosts: Some("forge.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5".to_string()),
        };

        // Ensure Debug never leaks private key material
        let debug_repr = format!("{creds:?}");
        assert!(!debug_repr.contains("super-secret-key-material"));
        assert!(debug_repr.contains("[REDACTED]"));

        let guard = PreparedAuthEnv::from_credentials(&creds).unwrap();
        assert_eq!(guard._files.len(), 2);
        let key_path = guard._files[0].path().to_path_buf();
        let hosts_path = guard._files[1].path().to_path_buf();
        assert!(key_path.exists());
        assert!(hosts_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            let hosts_mode = std::fs::metadata(&hosts_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(key_mode, 0o600);
            assert_eq!(hosts_mode, 0o600);
        }

        let ssh_cmd = &guard.envs[0].1;
        assert_eq!(guard.envs[0].0, "GIT_SSH_COMMAND");
        assert!(ssh_cmd.contains("IdentitiesOnly=yes"));
        assert!(ssh_cmd.contains("BatchMode=yes"));
        assert!(ssh_cmd.contains("StrictHostKeyChecking=yes"));

        drop(guard);
        assert!(
            !key_path.exists(),
            "ephemeral SSH key must be deleted on drop"
        );
        assert!(
            !hosts_path.exists(),
            "ephemeral known_hosts must be deleted on drop"
        );

        let creds_no_hosts = GitCredentials::SshKey {
            private_key_pem: fake_pem.to_string(),
            known_hosts: None,
        };
        let guard_no_hosts = PreparedAuthEnv::from_credentials(&creds_no_hosts).unwrap();
        assert!(format!("{:?}", guard_no_hosts._files[0]).contains("EphemeralSecretFile"));
        assert!(
            guard_no_hosts.envs[0]
                .1
                .contains("StrictHostKeyChecking=accept-new")
        );
        drop(guard_no_hosts);

        assert!(
            PreparedAuthEnv::from_credentials(&GitCredentials::SshKey {
                private_key_pem: "  ".to_string(),
                known_hosts: None,
            })
            .is_err()
        );
        assert!(
            PreparedAuthEnv::from_credentials(&GitCredentials::SshKey {
                private_key_pem: fake_pem.to_string(),
                known_hosts: Some("bad\0hosts".to_string()),
            })
            .is_err()
        );
        assert!(
            PreparedAuthEnv::from_credentials(&GitCredentials::HttpsBasic {
                username: "user\ninjected".to_string(),
                password_or_token: "tok".to_string(),
            })
            .is_err()
        );
    }

    #[test]
    fn test_https_credentials_redaction_and_env() {
        let creds = GitCredentials::HttpsBasic {
            username: "oauth2".to_string(),
            password_or_token: "glpat-topsecret12345".to_string(),
        };
        let debug_repr = format!("{creds:?}");
        assert!(!debug_repr.contains("glpat-topsecret12345"));
        assert!(debug_repr.contains("[REDACTED]"));

        let guard = PreparedAuthEnv::from_credentials(&creds).unwrap();
        assert_eq!(guard.envs.len(), 3);
        assert_eq!(
            guard.envs[0],
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string())
        );
        assert_eq!(
            guard.envs[1],
            (
                "GIT_CONFIG_KEY_0".to_string(),
                "http.extraHeader".to_string()
            )
        );
        assert!(guard.envs[2].1.starts_with("Authorization: Basic "));

        let leaked = format!(
            "fatal: header {} failed for glpat-topsecret12345",
            guard.envs[2].1
        );
        let scrubbed = redact_sensitive(leaked, &guard.redact_tokens);
        assert!(!scrubbed.contains("glpat-topsecret12345"));
        assert!(scrubbed.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn test_git_runner_end_to_end_and_shell_metacharacter_safety() {
        let tmp = TempDir::new().unwrap();
        let bare_path = tmp.path().join("origin.git");
        let work_a = tmp.path().join("work-a");
        let work_b = tmp.path().join("work-b");
        let marker_file = tmp.path().join("must_not_be_created");

        let runner = GitRunner::new();
        runner.init_bare(&bare_path).await.unwrap();
        runner
            .clone(bare_path.to_str().unwrap(), &work_a)
            .await
            .unwrap();

        let author = CommitAuthor::new("Dylan Dev", "dylan@keel.io");
        tokio::fs::write(work_a.join("README.md"), "# Infra GitOps\n")
            .await
            .unwrap();
        runner.add(&work_a, ["README.md"]).await.unwrap();

        // Commit message containing shell metacharacters must NEVER be interpreted by a shell
        let msg = format!(
            "feat: bootstrap $(touch {}) `touch {}` ; echo nope",
            marker_file.display(),
            marker_file.display()
        );
        let initial_sha = runner.commit(&work_a, &msg, &author).await.unwrap();
        assert_eq!(initial_sha.len(), 40);
        assert!(
            !marker_file.exists(),
            "shell metacharacters in commit message must never execute"
        );

        runner.push(&work_a, "origin", "main").await.unwrap();

        // Create proposal branch, add manifest, push, inspect diff_stat, and fast-forward merge
        let branch = format_proposal_branch("tenant-1", "add-stack", 1_700_000_000);
        runner.checkout_new_branch(&work_a, &branch).await.unwrap();
        let stack_dir = work_a.join("tenants/tenant-1/infra/db");
        tokio::fs::create_dir_all(&stack_dir).await.unwrap();
        tokio::fs::write(stack_dir.join("stack.yaml"), "kind: Stack\n")
            .await
            .unwrap();
        runner
            .add(&work_a, ["tenants/tenant-1/infra/db/stack.yaml"])
            .await
            .unwrap();
        let branch_sha = runner
            .commit(&work_a, "feat(tenant-1): add db stack", &author)
            .await
            .unwrap();
        runner.push(&work_a, "origin", &branch).await.unwrap();

        let stat = runner.diff_stat(&work_a, "main", &branch).await.unwrap();
        assert!(stat.contains("stack.yaml"));

        // Clone into second workdir, fetch, and fast-forward merge into main
        runner
            .clone(bare_path.to_str().unwrap(), &work_b)
            .await
            .unwrap();
        runner.fetch(&work_b, "origin").await.unwrap();
        let merged_sha = runner
            .fast_forward_merge(&work_b, "main", &format!("origin/{branch}"))
            .await
            .unwrap();
        assert_eq!(merged_sha, branch_sha);
        runner.push(&work_b, "origin", "main").await.unwrap();

        runner.checkout_branch(&work_b, &branch).await.unwrap();
        assert!(
            runner
                .checkout_branch(&work_b, "nonexistent-branch")
                .await
                .is_err()
        );
        let empty_paths: [&str; 0] = [];
        assert!(runner.add(&work_b, empty_paths).await.is_err());
        assert!(runner.commit(&work_b, "   ", &author).await.is_err());

        let release_sha = runner
            .fast_forward_merge(&work_b, "release-v1", &branch)
            .await
            .unwrap();
        assert_eq!(release_sha, branch_sha);

        let authed = GitRunner::default().with_credentials(GitCredentials::HttpsBasic {
            username: "git".to_string(),
            password_or_token: "secret".to_string(),
        });
        assert!(format!("{:?}", authed.credentials()).contains("[REDACTED]"));
    }
}
