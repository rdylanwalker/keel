//! Per-repository forge operations for per-tenant repositories (`SDD.md` §3.2): repository
//! creation with explicit owner resolution, read-only credentials (repository-scoped tokens,
//! deploy keys), `main` protection and fork policy.
//!
//! These are inherent `pub(crate)` helpers; the `ForgeProvider` impls in [`crate::remote`]
//! delegate to them. REST wrappers only: no git transport, no secrets in logs or errors.

use std::sync::Arc;

use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Value, json};

use crate::forge::{
    BranchProtectionOutcome, DeployKeyInfo, DeployKeyOutcome, EnsuredDeployKey, ForgeError,
    ForgeProvider, ForkPolicyOutcome, PROTECTED_MAIN_BRANCH, REPO_READ_TOKEN_NAME_PREFIX,
    RepoReadToken, RepoReadTokenInfo, RepositoryEnsureOutcome, RepositoryInfo, RepositoryOptions,
    RepositoryTarget,
};
use crate::http::{
    PageCap, delete_ignoring_missing, error_for, fetch_pages, get_optional, json_body,
};
use crate::remote::{
    ForgejoGiteaProvider, GitHubProvider, GitLabProvider, url_encode_project_path,
};
use crate::ssh_keys::{normalize_ssh_public_key, ssh_public_key_fingerprint};

// ============================================================================
// Shared helpers
// ============================================================================

/// Maximum length of one forge path segment (user, organisation, group or repository name).
const MAX_SEGMENT_LEN: usize = 100;

/// Validates one URL path segment (user, organisation, group or repository name) against a strict
/// allowlist: `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$` without `..`. Anything else (`%`, `/`, `\`,
/// whitespace, `.`/`..`) is refused: reqwest's URL parser treats `%2e%2e`, `.%2e` and `%2e.` as
/// `..` and collapses the path, so a percent-encoded dot segment could otherwise re-target a call
/// (for example `DELETE /repos/{o}/{r}/collaborators/%2e%2e` -> `DELETE /repos/{o}/{r}/`).
pub(crate) fn single_segment(value: &str, field: &str) -> Result<String, ForgeError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_SEGMENT_LEN
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !value.contains("..");
    if valid {
        Ok(value.to_string())
    } else {
        Err(ForgeError::InvalidInput(format!(
            "invalid forge {field}: '{}' (allowed: letters, digits, '.', '_', '-'; must start with a letter or digit)",
            value.escape_default()
        )))
    }
}

/// Validates a `/`-separated namespace path (GitLab groups and subgroups; a single segment on
/// Forgejo/GitHub): every segment must pass [`single_segment`].
pub(crate) fn namespace_path(value: &str, field: &str) -> Result<String, ForgeError> {
    if value.is_empty() || value.len() > 255 {
        return Err(ForgeError::InvalidInput(format!(
            "invalid forge {field}: '{}'",
            value.escape_default()
        )));
    }
    for segment in value.split('/') {
        single_segment(segment, field)?;
    }
    Ok(value.to_string())
}

/// `owner/name` for Forgejo/GitHub URLs (both single segments).
fn flat_repo_path(repo: &RepositoryTarget) -> Result<String, ForgeError> {
    Ok(format!(
        "{}/{}",
        single_segment(&repo.owner, "owner")?,
        single_segment(&repo.name, "repo")?
    ))
}

/// Fails with `InvalidInput` when [`PROTECTED_MAIN_BRANCH`] does not exist yet: Forgejo and
/// GitLab would protect the missing branch and then reject Keel's own initial push, and GitHub
/// answers the protection call with a bare 404.
async fn require_main_branch<F>(
    client: &Client,
    branch_url: &str,
    authorize: F,
    repo: &RepositoryTarget,
) -> Result<(), ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    if get_optional(client, branch_url, authorize, "branch")
        .await?
        .is_none()
    {
        return Err(ForgeError::InvalidInput(format!(
            "branch '{PROTECTED_MAIN_BRANCH}' of {repo} does not exist yet; push the initial commit (or create the repository with auto_init) before protecting it"
        )));
    }
    Ok(())
}

/// Forge ids are numbers on every supported forge; accept both JSON numbers and strings.
fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Validates a numeric forge id passed back by a caller (it is interpolated into a URL path).
fn numeric_id<'a>(id: &'a str, what: &str) -> Result<&'a str, ForgeError> {
    if id.is_empty() || id.len() > 20 || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ForgeError::InvalidInput(format!(
            "{what} id must be numeric, got '{id}'"
        )));
    }
    Ok(id)
}

/// `true` when every key of the `desired` object has the same value in `actual`.
fn fields_match(actual: &Value, desired: &Value) -> bool {
    desired
        .as_object()
        .is_some_and(|d| d.iter().all(|(k, v)| actual.get(k) == Some(v)))
}

fn deploy_key_from_json(v: &Value, read_only: bool) -> Option<DeployKeyInfo> {
    Some(DeployKeyInfo {
        id: id_string(v.get("id")?)?,
        title: str_field(v, "title").unwrap_or_default(),
        fingerprint: v
            .get("key")
            .and_then(Value::as_str)
            .and_then(|k| ssh_public_key_fingerprint(k).ok()),
        read_only,
    })
}

/// What [`ensure_deploy_key`]-style calls must do with the existing keys.
#[derive(Debug, PartialEq, Eq)]
struct DeployKeyPlan {
    keep: Option<DeployKeyInfo>,
    delete: Vec<String>,
}

fn plan_deploy_key(
    existing: &[DeployKeyInfo],
    fingerprint: &str,
    read_only: bool,
) -> DeployKeyPlan {
    let mut keep = None;
    let mut delete = Vec::new();
    for key in existing
        .iter()
        .filter(|k| k.fingerprint.as_deref() == Some(fingerprint))
    {
        if key.read_only == read_only && keep.is_none() {
            keep = Some(key.clone());
        } else {
            delete.push(key.id.clone());
        }
    }
    DeployKeyPlan { keep, delete }
}

fn validated_deploy_key(title: &str, public_key: &str) -> Result<(String, String), ForgeError> {
    let title = title.trim();
    if title.is_empty() || title.len() > 100 || title.chars().any(char::is_control) {
        return Err(ForgeError::InvalidInput(
            "deploy key title must be 1-100 printable characters".to_string(),
        ));
    }
    Ok((title.to_string(), normalize_ssh_public_key(public_key)?))
}

fn creation_body(name: &str, opts: &RepositoryOptions) -> Value {
    let mut body = json!({ "name": name, "private": opts.private, "auto_init": opts.auto_init });
    if let Some(branch) = opts.default_branch.as_deref().filter(|b| !b.is_empty()) {
        body["default_branch"] = Value::from(branch);
    }
    if let Some(description) = opts.description.as_deref() {
        body["description"] = Value::from(description);
    }
    body
}

/// Repository info from a Forgejo/Gitea/GitHub repository object, falling back to the requested
/// owner/name when the forge omits them. A missing `private` field is reported as public (fail
/// closed: never claim privacy the forge did not confirm).
fn flat_repo_info(
    v: &Value,
    owner: &str,
    name: &str,
    outcome: RepositoryEnsureOutcome,
) -> RepositoryInfo {
    RepositoryInfo {
        owner: v
            .get("owner")
            .and_then(|o| o.get("login"))
            .and_then(Value::as_str)
            .unwrap_or(owner)
            .to_string(),
        name: str_field(v, "name").unwrap_or_else(|| name.to_string()),
        private: v.get("private").and_then(Value::as_bool).unwrap_or(false),
        default_branch: str_field(v, "default_branch").unwrap_or_default(),
        clone_url: str_field(v, "clone_url"),
        html_url: str_field(v, "html_url"),
        outcome,
    }
}

/// How `ensure_named_repo` treats a repository that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingRepo {
    /// [`ForgeProvider::ensure_named_repository`]: an existing repository is adopted only if the
    /// forge reports the requested owner/name (no rename/transfer redirect) and, when
    /// `opts.private` is set, that it is private.
    Verify,
    /// Stage 0 [`ForgeProvider::ensure_repository`]: the operator's platform repository is used
    /// as is (historic behaviour, visibility is never changed).
    Accept,
}

/// Refuses to adopt an existing repository that is not the one requested (renamed/transferred
/// repositories are answered through redirects) or not private although privacy was requested.
fn verify_existing(
    info: RepositoryInfo,
    owner: &str,
    name: &str,
    opts: &RepositoryOptions,
    mode: ExistingRepo,
) -> Result<RepositoryInfo, ForgeError> {
    if mode == ExistingRepo::Accept {
        return Ok(info);
    }
    if !info.owner.eq_ignore_ascii_case(owner) || !info.name.eq_ignore_ascii_case(name) {
        return Err(ForgeError::InvalidInput(format!(
            "the forge answered '{owner}/{name}' with repository '{}/{}' (renamed or transferred); refusing to adopt it",
            info.owner, info.name
        )));
    }
    if opts.private && !info.private {
        return Err(ForgeError::InvalidInput(format!(
            "repository '{owner}/{name}' already exists and is not private; refusing to adopt it"
        )));
    }
    Ok(info)
}

/// Validates a token name minted by `create_repo_read_token`: it must carry
/// [`REPO_READ_TOKEN_NAME_PREFIX`] (Stage 0 deletes every other token of the Forgejo bot) and be
/// printable.
fn repo_read_token_name(name: &str) -> Result<&str, ForgeError> {
    let name = name.trim();
    let suffix_ok = name
        .strip_prefix(REPO_READ_TOKEN_NAME_PREFIX)
        .is_some_and(|s| !s.is_empty());
    if !suffix_ok || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(ForgeError::InvalidInput(format!(
            "repository read token names must be printable, at most 255 characters and start with '{REPO_READ_TOKEN_NAME_PREFIX}' followed by a suffix"
        )));
    }
    Ok(name)
}

// ----------------------------------------------------------------------------
// Forgejo/Gitea and GitHub share the deploy-key REST shape
// (`GET|POST /repos/{o}/{r}/keys`, `DELETE /repos/{o}/{r}/keys/{id}`, `read_only`).
// ----------------------------------------------------------------------------

async fn flat_list_deploy_keys<F>(
    client: &Client,
    repo_api: &str,
    page_size_param: &str,
    authorize: F,
) -> Result<Vec<DeployKeyInfo>, ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    let raw: Vec<Value> = fetch_pages(
        client,
        &format!("{repo_api}/keys"),
        page_size_param,
        authorize,
        "deploy keys",
        PageCap::FailClosed,
    )
    .await?;
    Ok(raw
        .iter()
        .filter_map(|k| {
            let read_only = k.get("read_only").and_then(Value::as_bool).unwrap_or(false);
            deploy_key_from_json(k, read_only)
        })
        .collect())
}

async fn flat_ensure_deploy_key<F>(
    client: &Client,
    repo_api: &str,
    page_size_param: &str,
    authorize: F,
    title: &str,
    public_key: &str,
    read_only: bool,
) -> Result<EnsuredDeployKey, ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder + Copy,
{
    let (title, key) = validated_deploy_key(title, public_key)?;
    let fingerprint = ssh_public_key_fingerprint(&key)?;
    let existing = flat_list_deploy_keys(client, repo_api, page_size_param, authorize).await?;
    let plan = plan_deploy_key(&existing, &fingerprint, read_only);
    for id in &plan.delete {
        delete_ignoring_missing(
            client,
            &format!("{repo_api}/keys/{id}"),
            authorize,
            "deploy key",
        )
        .await?;
    }
    if let Some(keep) = plan.keep {
        let outcome = if plan.delete.is_empty() {
            DeployKeyOutcome::Existed
        } else {
            DeployKeyOutcome::Replaced
        };
        return Ok(EnsuredDeployKey { key: keep, outcome });
    }
    let resp = authorize(client.post(format!("{repo_api}/keys")))
        .json(&json!({ "title": title, "key": key, "read_only": read_only }))
        .send()
        .await?;
    if resp.status() == StatusCode::UNPROCESSABLE_ENTITY {
        // Duplicate (concurrent add, or GitHub's "key is already in use"): accept only an
        // identical key on this repository.
        let err = error_for(resp, "deploy key").await;
        let again = flat_list_deploy_keys(client, repo_api, page_size_param, authorize).await?;
        return match plan_deploy_key(&again, &fingerprint, read_only).keep {
            Some(key) => Ok(EnsuredDeployKey {
                key,
                outcome: DeployKeyOutcome::Existed,
            }),
            None => Err(err),
        };
    }
    if !resp.status().is_success() {
        return Err(error_for(resp, "deploy key").await);
    }
    let body = json_body(resp).await?;
    let key = deploy_key_from_json(&body, read_only).ok_or_else(|| ForgeError::Api {
        status: 201,
        message: "deploy key response has no id".to_string(),
    })?;
    let outcome = if plan.delete.is_empty() {
        DeployKeyOutcome::Created
    } else {
        DeployKeyOutcome::Replaced
    };
    Ok(EnsuredDeployKey {
        key: DeployKeyInfo {
            fingerprint: Some(fingerprint),
            ..key
        },
        outcome,
    })
}

// ============================================================================
// Forgejo / Gitea
// ============================================================================

/// Settings Keel manages on the Forgejo `main` protection rule.
/// `unprotected_file_patterns` must be empty: files matching it may be pushed directly even with
/// `enable_push: false`.
fn forgejo_main_protection() -> Value {
    json!({
        "enable_push": false,
        "enable_push_whitelist": false,
        "enable_merge_whitelist": false,
        "required_approvals": 0,
        "apply_to_admins": true,
        "unprotected_file_patterns": "",
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerKind {
    Organization,
    AuthenticatedUser,
}

impl ForgejoGiteaProvider {
    fn repo_api_for(&self, repo: &RepositoryTarget) -> Result<String, ForgeError> {
        Ok(self.api(&format!("/repos/{}", flat_repo_path(repo)?)))
    }

    pub(crate) fn scoped_to(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        let mut scoped = self.clone();
        scoped.owner = single_segment(owner, "owner")?;
        scoped.repo = single_segment(name, "repo")?;
        Ok(Arc::new(scoped))
    }

    /// The authenticated user (`GET /user`, needs `read:user`) or an organisation
    /// (`GET /orgs/{owner}`, needs `read:organization`); anything else is refused instead of
    /// guessing from a failed create. The user is checked first so a token without
    /// organisation scopes can still create repositories in its own account.
    async fn owner_kind(&self, owner: &str) -> Result<OwnerKind, ForgeError> {
        let login = match self.authenticated_login().await {
            Ok(login) => Some(login),
            Err(
                ForgeError::NotFound(_)
                | ForgeError::Api {
                    status: 401 | 403, ..
                },
            ) => None,
            Err(err) => return Err(err),
        };
        if login
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case(owner))
        {
            return Ok(OwnerKind::AuthenticatedUser);
        }
        if get_optional(
            &self.client,
            &self.api(&format!("/orgs/{owner}")),
            |b| self.authorized(b),
            "organization",
        )
        .await?
        .is_some()
        {
            return Ok(OwnerKind::Organization);
        }
        Err(ForgeError::InvalidInput(match login {
            Some(_) => format!(
                "Forgejo/Gitea owner '{owner}' is neither an organisation nor the authenticated user"
            ),
            None => format!(
                "Forgejo/Gitea owner '{owner}' is not an organisation and the token cannot read its own user (needs read:user)"
            ),
        }))
    }

    pub(crate) async fn ensure_named_repo(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
        mode: ExistingRepo,
    ) -> Result<RepositoryInfo, ForgeError> {
        let owner = single_segment(owner, "owner")?;
        let name = single_segment(name, "repo")?;
        let authorize = |b| self.authorized(b);
        let repo_url = self.api(&format!("/repos/{owner}/{name}"));
        let existing = |v: &Value| {
            verify_existing(
                flat_repo_info(v, &owner, &name, RepositoryEnsureOutcome::Existed),
                &owner,
                &name,
                opts,
                mode,
            )
        };
        if let Some(v) = get_optional(&self.client, &repo_url, authorize, "repository").await? {
            return existing(&v);
        }
        let create_url = match self.owner_kind(&owner).await? {
            OwnerKind::Organization => self.api(&format!("/orgs/{owner}/repos")),
            OwnerKind::AuthenticatedUser => self.api("/user/repos"),
        };
        let resp = authorize(self.client.post(&create_url))
            .json(&creation_body(&name, opts))
            .send()
            .await?;
        if resp.status() == StatusCode::CONFLICT {
            let v = get_optional(&self.client, &repo_url, authorize, "repository")
                .await?
                .ok_or_else(|| ForgeError::NotFound(format!("{owner}/{name}")))?;
            return existing(&v);
        }
        if !resp.status().is_success() {
            return Err(error_for(resp, "repository").await);
        }
        let v = json_body(resp).await?;
        Ok(flat_repo_info(
            &v,
            &owner,
            &name,
            RepositoryEnsureOutcome::Created,
        ))
    }

    /// Mints the bot's `read:repository` token restricted to `repo`. Forgejo token names are
    /// unique per user: when `name` is taken by a token restricted to exactly this repository
    /// (a retry after a failed Secret write), that token is deleted and the name re-minted once;
    /// a name used for anything else is refused.
    pub(crate) async fn create_repo_token(
        &self,
        repo: &RepositoryTarget,
        name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        let username = self.basic_auth_username("minting a repository read token")?;
        let name = repo_read_token_name(name)?;
        let target = RepositoryTarget {
            owner: single_segment(&repo.owner, "owner")?,
            name: single_segment(&repo.name, "repo")?,
        };
        let targets = std::slice::from_ref(&target);
        let minted = match self
            .create_repository_scoped_token(name, &["read:repository"], targets)
            .await
        {
            Err(ForgeError::Api {
                status: 400 | 409 | 422,
                message,
            }) if message.to_ascii_lowercase().contains("already") => {
                let wanted = target.full_name().to_ascii_lowercase();
                let previous = self
                    .list_access_tokens()
                    .await?
                    .into_iter()
                    .find(|t| t.name == name)
                    .ok_or_else(|| ForgeError::Api {
                        status: 400,
                        message: format!("access token name '{name}' is taken: {message}"),
                    })?;
                let same_repo = previous
                    .repositories
                    .as_ref()
                    .is_some_and(|r| r.len() == 1 && r[0].to_ascii_lowercase() == wanted);
                if !same_repo {
                    return Err(ForgeError::InvalidInput(format!(
                        "access token name '{name}' is already used by a token that is not restricted to {target}"
                    )));
                }
                self.delete_access_token(previous.id).await?;
                self.create_repository_scoped_token(name, &["read:repository"], targets)
                    .await?
            }
            other => other?,
        };
        Ok(RepoReadToken {
            id: minted.id.to_string(),
            name: name.to_string(),
            username,
            token: minted.token.clone(),
        })
    }

    /// The bot's tokens restricted to exactly `repo` (case-insensitive `owner/name`).
    pub(crate) async fn list_repo_tokens(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<RepoReadTokenInfo>, ForgeError> {
        let wanted = format!(
            "{}/{}",
            single_segment(&repo.owner, "owner")?,
            single_segment(&repo.name, "repo")?
        )
        .to_ascii_lowercase();
        Ok(self
            .list_access_tokens()
            .await?
            .into_iter()
            .filter(|t| {
                t.repositories
                    .as_ref()
                    .is_some_and(|r| r.len() == 1 && r[0].to_ascii_lowercase() == wanted)
            })
            .map(|t| RepoReadTokenInfo {
                id: t.id.to_string(),
                name: t.name,
            })
            .collect())
    }

    pub(crate) async fn revoke_repo_token(&self, token_id: &str) -> Result<(), ForgeError> {
        let id: i64 = numeric_id(token_id, "access token")?
            .parse()
            .map_err(|_| ForgeError::InvalidInput(format!("invalid token id '{token_id}'")))?;
        self.delete_access_token(id).await
    }

    pub(crate) async fn list_repo_deploy_keys(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        flat_list_deploy_keys(&self.client, &self.repo_api_for(repo)?, "limit", |b| {
            self.authorized(b)
        })
        .await
    }

    pub(crate) async fn ensure_repo_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        flat_ensure_deploy_key(
            &self.client,
            &self.repo_api_for(repo)?,
            "limit",
            |b| self.authorized(b),
            title,
            public_key,
            read_only,
        )
        .await
    }

    pub(crate) async fn revoke_repo_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        let id = numeric_id(key_id, "deploy key")?;
        delete_ignoring_missing(
            &self.client,
            &format!("{}/keys/{id}", self.repo_api_for(repo)?),
            |b| self.authorized(b),
            "deploy key",
        )
        .await
    }

    /// `GET` the `main` rule, then `POST` (missing) or `PATCH` (drifted). A `POST` answered with
    /// 403/409/422 because a concurrent call created the rule is resolved by re-reading it.
    pub(crate) async fn ensure_repo_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        let repo_api = self.repo_api_for(repo)?;
        let authorize = |b| self.authorized(b);
        require_main_branch(
            &self.client,
            &format!("{repo_api}/branches/{PROTECTED_MAIN_BRANCH}"),
            authorize,
            repo,
        )
        .await?;
        let rule_url = format!("{repo_api}/branch_protections/{PROTECTED_MAIN_BRANCH}");
        let desired = forgejo_main_protection();
        let current = match get_optional(&self.client, &rule_url, authorize, "branch protection")
            .await?
        {
            Some(current) => current,
            None => {
                let mut body = desired.clone();
                body["rule_name"] = Value::from(PROTECTED_MAIN_BRANCH);
                body["branch_name"] = Value::from(PROTECTED_MAIN_BRANCH);
                let resp = authorize(self.client.post(format!("{repo_api}/branch_protections")))
                    .json(&body)
                    .send()
                    .await?;
                if resp.status().is_success() {
                    return Ok(BranchProtectionOutcome::Created);
                }
                let err = error_for(resp, "branch protection").await;
                let raced = matches!(
                    err,
                    ForgeError::Api {
                        status: 403 | 409 | 422,
                        ..
                    }
                );
                match get_optional(&self.client, &rule_url, authorize, "branch protection").await? {
                    Some(current) if raced => current,
                    _ => return Err(err),
                }
            }
        };
        if fields_match(&current, &desired) {
            return Ok(BranchProtectionOutcome::Unchanged);
        }
        let resp = authorize(self.client.patch(&rule_url))
            .json(&desired)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, "branch protection").await);
        }
        Ok(BranchProtectionOutcome::Updated)
    }

    /// Forgejo/Gitea have no per-repository fork switch; forking is governed by the instance
    /// setting `[repository] DISABLE_FORKS`, reported by `GET /settings/repository`.
    pub(crate) async fn repo_fork_policy(&self) -> Result<ForkPolicyOutcome, ForgeError> {
        let settings = get_optional(
            &self.client,
            &self.api("/settings/repository"),
            |b| self.authorized(b),
            "repository settings",
        )
        .await?
        .unwrap_or(Value::Null);
        if settings.get("forks_disabled").and_then(Value::as_bool) == Some(true) {
            Ok(ForkPolicyOutcome::AlreadyDisabled)
        } else {
            Ok(ForkPolicyOutcome::RequiresAdminSetting(
                "set `[repository] DISABLE_FORKS = true` in the Forgejo/Gitea app.ini (instance-wide; there is no per-repository switch)".to_string(),
            ))
        }
    }
}

// ============================================================================
// GitHub
// ============================================================================

/// GitHub's 403 for features a plan does not include ("Upgrade to GitHub Pro or make this
/// repository public to enable this feature.").
fn github_plan_limited(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("upgrade to github pro") || m.contains("to enable this feature")
}

/// Keel's `main` protection on GitHub: PRs required (zero approvals: Keel merges after its own
/// checks), enforced for admins, no force pushes or deletion. A `PUT` replaces the whole rule.
fn github_main_protection() -> Value {
    json!({
        "required_status_checks": null,
        "enforce_admins": true,
        "required_pull_request_reviews": {
            "required_approving_review_count": 0,
            "dismiss_stale_reviews": false,
            "require_code_owner_reviews": false
        },
        "restrictions": null,
        "allow_force_pushes": false,
        "allow_deletions": false,
        "lock_branch": false
    })
}

/// `true` when the rule is exactly as strict as Keel's: PRs required with no bypass actors (users,
/// teams or apps allowed to push past the PR requirement), enforced for admins, no force pushes or
/// deletion, no push restrictions and no branch lock (which would also block Keel's merges).
/// Anything else is replaced by the canonical rule.
fn github_protection_matches(v: &Value) -> bool {
    let enabled = |key: &str| {
        v.get(key)
            .and_then(|o| o.get("enabled"))
            .and_then(Value::as_bool)
    };
    let empty_list = |o: &Value, key: &str| {
        o.get(key)
            .is_none_or(|l| l.is_null() || l.as_array().is_some_and(Vec::is_empty))
    };
    let no_bypass = |r: &Value| {
        r.get("bypass_pull_request_allowances").is_none_or(|b| {
            b.is_null() || ["users", "teams", "apps"].iter().all(|k| empty_list(b, k))
        })
    };
    enabled("enforce_admins") == Some(true)
        && enabled("allow_force_pushes") == Some(false)
        && enabled("allow_deletions") == Some(false)
        && enabled("lock_branch") != Some(true)
        && v.get("restrictions").is_none_or(Value::is_null)
        && v.get("required_pull_request_reviews").is_some_and(|r| {
            r.is_object()
                && no_bypass(r)
                && r.get("required_approving_review_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    == 0
        })
}

/// A 403/422 from `PATCH allow_forking` caused by organisation fork policy (as opposed to a token
/// without admin rights on the repository, which must surface as an error).
fn github_fork_policy_refusal(body: &str) -> bool {
    // Only the human-readable messages count (`documentation_url` must not match).
    let text = match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let mut parts: Vec<String> = str_field(&v, "message").into_iter().collect();
            if let Some(errors) = v.get("errors").and_then(Value::as_array) {
                parts.extend(errors.iter().filter_map(|e| match e {
                    Value::String(s) => Some(s.clone()),
                    other => str_field(other, "message"),
                }));
            }
            parts.join(" ")
        }
        Err(_) => body.to_string(),
    };
    text.to_ascii_lowercase().contains("fork")
}

impl GitHubProvider {
    fn repo_api_for(&self, repo: &RepositoryTarget) -> Result<String, ForgeError> {
        Ok(self.api(&format!("/repos/{}", flat_repo_path(repo)?)))
    }

    pub(crate) fn scoped_to(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        let mut scoped = self.clone();
        scoped.owner = single_segment(owner, "owner")?;
        scoped.repo = single_segment(name, "repo")?;
        Ok(Arc::new(scoped))
    }

    async fn authenticated_login(&self) -> Result<String, ForgeError> {
        let user = get_optional(
            &self.client,
            &self.api("/user"),
            |b| self.authorized(b),
            "user",
        )
        .await?
        .unwrap_or(Value::Null);
        str_field(&user, "login").ok_or_else(|| ForgeError::Api {
            status: 200,
            message: "GitHub /user response has no login".to_string(),
        })
    }

    /// `GET /users/{owner}` reports `type: Organization|User`; a user must be the token owner.
    async fn owner_kind(&self, owner: &str) -> Result<OwnerKind, ForgeError> {
        let account = get_optional(
            &self.client,
            &self.api(&format!("/users/{owner}")),
            |b| self.authorized(b),
            "account",
        )
        .await?
        .ok_or_else(|| ForgeError::NotFound(owner.to_string()))?;
        if account.get("type").and_then(Value::as_str) == Some("Organization") {
            return Ok(OwnerKind::Organization);
        }
        if self
            .authenticated_login()
            .await?
            .eq_ignore_ascii_case(owner)
        {
            Ok(OwnerKind::AuthenticatedUser)
        } else {
            Err(ForgeError::InvalidInput(format!(
                "GitHub owner '{owner}' is neither an organisation nor the authenticated user"
            )))
        }
    }

    /// GitHub has no `default_branch` on create (the first branch follows the account or
    /// organisation setting), so with `auto_init` a differing initial branch is renamed
    /// (`POST /repos/{o}/{r}/branches/{current}/rename`) to `opts.default_branch`.
    pub(crate) async fn ensure_named_repo(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
        mode: ExistingRepo,
    ) -> Result<RepositoryInfo, ForgeError> {
        let owner = single_segment(owner, "owner")?;
        let name = single_segment(name, "repo")?;
        let wanted_branch = match opts.default_branch.as_deref().filter(|b| !b.is_empty()) {
            Some(branch) if opts.auto_init => Some(single_segment(branch, "default branch")?),
            _ => None,
        };
        let authorize = |b| self.authorized(b);
        let repo_url = self.api(&format!("/repos/{owner}/{name}"));
        let existing = |v: &Value| {
            verify_existing(
                flat_repo_info(v, &owner, &name, RepositoryEnsureOutcome::Existed),
                &owner,
                &name,
                opts,
                mode,
            )
        };
        if let Some(v) = get_optional(&self.client, &repo_url, authorize, "repository").await? {
            return existing(&v);
        }
        let create_url = match self.owner_kind(&owner).await? {
            OwnerKind::Organization => self.api(&format!("/orgs/{owner}/repos")),
            OwnerKind::AuthenticatedUser => self.api("/user/repos"),
        };
        let mut body = creation_body(&name, opts);
        if let Some(obj) = body.as_object_mut() {
            obj.remove("default_branch");
        }
        let resp = authorize(self.client.post(&create_url))
            .json(&body)
            .send()
            .await?;
        if matches!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY | StatusCode::CONFLICT
        ) {
            // 422 "name already exists on this account" (concurrent create) counts as existing.
            let err = error_for(resp, "repository").await;
            let already = matches!(&err, ForgeError::Api { message, .. } if message.contains("already exists"))
                || matches!(&err, ForgeError::Api { status: 409, .. });
            if already
                && let Some(v) =
                    get_optional(&self.client, &repo_url, authorize, "repository").await?
            {
                return existing(&v);
            }
            return Err(err);
        }
        if !resp.status().is_success() {
            return Err(error_for(resp, "repository").await);
        }
        let v = json_body(resp).await?;
        let mut info = flat_repo_info(&v, &owner, &name, RepositoryEnsureOutcome::Created);
        if let Some(wanted) = wanted_branch
            && !info.default_branch.is_empty()
            && info.default_branch != wanted
        {
            let current = single_segment(&info.default_branch, "default branch")?;
            let resp = authorize(
                self.client
                    .post(format!("{repo_url}/branches/{current}/rename")),
            )
            .json(&json!({ "new_name": wanted }))
            .send()
            .await?;
            if !resp.status().is_success() {
                return Err(error_for(resp, "default branch").await);
            }
            info.default_branch = wanted;
        }
        Ok(info)
    }

    pub(crate) async fn list_repo_deploy_keys(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        flat_list_deploy_keys(&self.client, &self.repo_api_for(repo)?, "per_page", |b| {
            self.authorized(b)
        })
        .await
    }

    pub(crate) async fn ensure_repo_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        flat_ensure_deploy_key(
            &self.client,
            &self.repo_api_for(repo)?,
            "per_page",
            |b| self.authorized(b),
            title,
            public_key,
            read_only,
        )
        .await
    }

    pub(crate) async fn revoke_repo_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        let id = numeric_id(key_id, "deploy key")?;
        delete_ignoring_missing(
            &self.client,
            &format!("{}/keys/{id}", self.repo_api_for(repo)?),
            |b| self.authorized(b),
            "deploy key",
        )
        .await
    }

    /// Reads the protection of `main` (403 plan limits => [`BranchProtectionOutcome::UnsupportedPlan`]).
    async fn read_main_protection(
        &self,
        url: &str,
    ) -> Result<Result<Option<Value>, BranchProtectionOutcome>, ForgeError> {
        let resp = self.authorized(self.client.get(url)).send().await?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(Ok(None));
        }
        if status.is_success() {
            return Ok(Ok(Some(json_body(resp).await?)));
        }
        let err = error_for(resp, "branch protection").await;
        match &err {
            ForgeError::Api {
                status: 403,
                message,
            } if github_plan_limited(message) => Ok(Err(BranchProtectionOutcome::UnsupportedPlan)),
            _ => Err(err),
        }
    }

    pub(crate) async fn ensure_repo_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        let repo_api = self.repo_api_for(repo)?;
        require_main_branch(
            &self.client,
            &format!("{repo_api}/branches/{PROTECTED_MAIN_BRANCH}"),
            |b| self.authorized(b),
            repo,
        )
        .await?;
        let url = format!("{repo_api}/branches/{PROTECTED_MAIN_BRANCH}/protection");
        let current = match self.read_main_protection(&url).await? {
            Ok(current) => current,
            Err(outcome) => return Ok(outcome),
        };
        if current.as_ref().is_some_and(github_protection_matches) {
            return Ok(BranchProtectionOutcome::Unchanged);
        }
        let resp = self
            .authorized(self.client.put(&url))
            .json(&github_main_protection())
            .send()
            .await?;
        if !resp.status().is_success() {
            let err = error_for(resp, "branch protection").await;
            return match &err {
                ForgeError::Api {
                    status: 403,
                    message,
                } if github_plan_limited(message) => Ok(BranchProtectionOutcome::UnsupportedPlan),
                _ => Err(err),
            };
        }
        Ok(if current.is_some() {
            BranchProtectionOutcome::Updated
        } else {
            BranchProtectionOutcome::Created
        })
    }

    /// `allow_forking` is a repository setting only for private organisation repositories, and
    /// only while the organisation allows private forks at all.
    pub(crate) async fn disable_repo_forks(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        let repo_api = self.repo_api_for(repo)?;
        let authorize = |b| self.authorized(b);
        let current = get_optional(&self.client, &repo_api, authorize, "repository")
            .await?
            .ok_or_else(|| ForgeError::NotFound(repo.full_name()))?;
        if current.get("allow_forking").and_then(Value::as_bool) == Some(false) {
            return Ok(ForkPolicyOutcome::AlreadyDisabled);
        }
        if current.get("private").and_then(Value::as_bool) != Some(true) {
            return Ok(ForkPolicyOutcome::RequiresAdminSetting(
                "public GitHub repositories can always be forked; make the repository private"
                    .to_string(),
            ));
        }
        let org_hint = "disable 'Allow forking of private repositories' for the organisation (`members_can_fork_private_repositories: false`); user-owned private repositories cannot disable forking per repository";
        let resp = authorize(self.client.patch(&repo_api))
            .json(&json!({ "allow_forking": false }))
            .send()
            .await?;
        if matches!(
            resp.status(),
            StatusCode::FORBIDDEN | StatusCode::UNPROCESSABLE_ENTITY
        ) {
            // Only an organisation fork policy is an admin setting; a token without admin rights
            // on the repository (or an invalid token) is a credential problem and surfaces.
            let err = error_for(resp, "repository").await;
            return match &err {
                ForgeError::Api { message, .. } if github_fork_policy_refusal(message) => Ok(
                    ForkPolicyOutcome::RequiresAdminSetting(org_hint.to_string()),
                ),
                _ => Err(err),
            };
        }
        if !resp.status().is_success() {
            return Err(error_for(resp, "repository").await);
        }
        let updated = json_body(resp).await?;
        if updated.get("allow_forking").and_then(Value::as_bool) == Some(false) {
            Ok(ForkPolicyOutcome::Disabled)
        } else {
            Ok(ForkPolicyOutcome::RequiresAdminSetting(
                org_hint.to_string(),
            ))
        }
    }
}

// ============================================================================
// GitLab
// ============================================================================

/// GitLab access levels.
const GITLAB_NO_ACCESS: u64 = 0;
const GITLAB_MAINTAINER: u64 = 40;

fn gitlab_access_levels<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// Only role-based entries (no per-user/group grants) with exactly the expected levels.
fn gitlab_levels_are(levels: &[&Value], expected: u64) -> bool {
    !levels.is_empty()
        && levels.iter().all(|l| {
            l.get("access_level").and_then(Value::as_u64) == Some(expected)
                && l.get("user_id").is_none_or(Value::is_null)
                && l.get("group_id").is_none_or(Value::is_null)
                && l.get("deploy_key_id").is_none_or(Value::is_null)
        })
}

fn gitlab_protection_matches(v: &Value) -> bool {
    gitlab_levels_are(
        &gitlab_access_levels(v, "push_access_levels"),
        GITLAB_NO_ACCESS,
    ) && gitlab_levels_are(
        &gitlab_access_levels(v, "merge_access_levels"),
        GITLAB_MAINTAINER,
    ) && v.get("allow_force_push").and_then(Value::as_bool) != Some(true)
}

fn gitlab_repo_info(
    v: &Value,
    project_path: &str,
    outcome: RepositoryEnsureOutcome,
) -> RepositoryInfo {
    let full = str_field(v, "path_with_namespace").unwrap_or_else(|| project_path.to_string());
    let (owner, name) = full
        .rsplit_once('/')
        .map(|(o, n)| (o.to_string(), n.to_string()))
        .unwrap_or_default();
    RepositoryInfo {
        owner,
        name,
        private: v.get("visibility").and_then(Value::as_str) == Some("private"),
        default_branch: str_field(v, "default_branch").unwrap_or_default(),
        clone_url: str_field(v, "http_url_to_repo"),
        html_url: str_field(v, "web_url"),
        outcome,
    }
}

impl GitLabProvider {
    fn project_api_for(&self, repo: &RepositoryTarget) -> Result<String, ForgeError> {
        let path = format!(
            "{}/{}",
            namespace_path(&repo.owner, "owner")?,
            single_segment(&repo.name, "repo")?
        );
        Ok(self.api(&format!("/projects/{}", url_encode_project_path(&path))))
    }

    pub(crate) fn scoped_to(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        let path = format!(
            "{}/{}",
            namespace_path(owner, "owner")?,
            single_segment(name, "repo")?
        );
        let mut scoped = self.clone();
        scoped.encoded_project = url_encode_project_path(&path);
        scoped.project_path = path;
        Ok(Arc::new(scoped))
    }

    /// Username of the token owner (`GET /api/v4/user`).
    async fn authenticated_username(&self) -> Result<String, ForgeError> {
        let user = get_optional(
            &self.client,
            &self.api("/user"),
            |b| self.authorized(b),
            "user",
        )
        .await?
        .unwrap_or(Value::Null);
        str_field(&user, "username").ok_or_else(|| ForgeError::Api {
            status: 200,
            message: "GitLab /user response has no username".to_string(),
        })
    }

    /// Resolves `namespace` (`GET /namespaces/{path}`) to its id. Groups are accepted; a user
    /// namespace only when it is the token owner's own (an admin token could otherwise create
    /// projects in another user's personal namespace, making that user the Owner).
    async fn creation_namespace_id(&self, namespace: &str) -> Result<u64, ForgeError> {
        let ns = get_optional(
            &self.client,
            &self.api(&format!(
                "/namespaces/{}",
                url_encode_project_path(namespace)
            )),
            |b| self.authorized(b),
            namespace,
        )
        .await?
        .ok_or_else(|| ForgeError::NotFound(namespace.to_string()))?;
        match ns.get("kind").and_then(Value::as_str) {
            Some("group") => {}
            Some("user") => {
                let full_path = str_field(&ns, "full_path").unwrap_or_default();
                let login = self.authenticated_username().await?;
                if !full_path.eq_ignore_ascii_case(&login) {
                    return Err(ForgeError::InvalidInput(format!(
                        "GitLab namespace '{namespace}' is another user's personal namespace; only groups and the token owner's own namespace are allowed"
                    )));
                }
            }
            other => {
                return Err(ForgeError::InvalidInput(format!(
                    "GitLab namespace '{namespace}' has unexpected kind '{}'",
                    other.unwrap_or("<missing>")
                )));
            }
        }
        ns.get("id").and_then(Value::as_u64).ok_or_else(|| {
            ForgeError::InvalidInput(format!("GitLab namespace '{namespace}' response has no id"))
        })
    }

    pub(crate) async fn ensure_named_repo(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
        mode: ExistingRepo,
    ) -> Result<RepositoryInfo, ForgeError> {
        let namespace = namespace_path(owner, "owner")?;
        let name = single_segment(name, "repo")?;
        let project_path = format!("{namespace}/{name}");
        let project_api = self.project_api_for(&RepositoryTarget {
            owner: namespace.clone(),
            name: name.clone(),
        })?;
        let authorize = |b| self.authorized(b);
        let existing = |v: &Value| {
            verify_existing(
                gitlab_repo_info(v, &project_path, RepositoryEnsureOutcome::Existed),
                &namespace,
                &name,
                opts,
                mode,
            )
        };
        if let Some(v) = get_optional(&self.client, &project_api, authorize, "repository").await? {
            return existing(&v);
        }
        let namespace_id = self.creation_namespace_id(&namespace).await?;
        let mut body = json!({
            "name": name,
            "path": name,
            "namespace_id": namespace_id,
            "visibility": if opts.private { "private" } else { "internal" },
            "initialize_with_readme": opts.auto_init,
        });
        if let Some(branch) = opts.default_branch.as_deref().filter(|b| !b.is_empty()) {
            body["default_branch"] = Value::from(branch);
        }
        if let Some(description) = opts.description.as_deref() {
            body["description"] = Value::from(description);
        }
        let resp = authorize(self.client.post(self.api("/projects")))
            .json(&body)
            .send()
            .await?;
        if resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::CONFLICT {
            // "has already been taken" (concurrent create) counts as existing.
            let err = error_for(resp, "repository").await;
            if matches!(&err, ForgeError::Api { message, .. } if message.contains("already been taken"))
                && let Some(v) =
                    get_optional(&self.client, &project_api, authorize, "repository").await?
            {
                return existing(&v);
            }
            return Err(err);
        }
        if !resp.status().is_success() {
            return Err(error_for(resp, "repository").await);
        }
        let v = json_body(resp).await?;
        Ok(gitlab_repo_info(
            &v,
            &project_path,
            RepositoryEnsureOutcome::Created,
        ))
    }

    /// Project deploy token with only `read_repository`; fails closed (and revokes it) when
    /// GitLab reports other scopes or no secret.
    pub(crate) async fn create_repo_token(
        &self,
        repo: &RepositoryTarget,
        name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        let name = repo_read_token_name(name)?;
        let project_api = self.project_api_for(repo)?;
        let resp = self
            .authorized(self.client.post(format!("{project_api}/deploy_tokens")))
            .json(&json!({ "name": name, "scopes": ["read_repository"] }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, "deploy token").await);
        }
        let body = json_body(resp).await?;
        let id = body.get("id").and_then(id_string);
        let scopes: Vec<&str> = body
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        match (id, str_field(&body, "token"), str_field(&body, "username")) {
            (Some(id), Some(token), Some(username)) if scopes == ["read_repository"] => {
                Ok(RepoReadToken {
                    id,
                    name: name.to_string(),
                    username,
                    token,
                })
            }
            (id, _, _) => {
                if let Some(id) = id
                    && let Err(err) = self.revoke_repo_token(repo, &id).await
                {
                    tracing::warn!(error = %err, token_id = %id, "Could not revoke a GitLab deploy token with unexpected scopes; revoke it manually");
                }
                Err(ForgeError::Api {
                    status: 201,
                    message: format!(
                        "GitLab deploy token was not limited to read_repository (scopes: {})",
                        scopes.join(",")
                    ),
                })
            }
        }
    }

    /// The project's active (not revoked, not expired) deploy tokens.
    pub(crate) async fn list_repo_tokens(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<RepoReadTokenInfo>, ForgeError> {
        let raw: Vec<Value> = fetch_pages(
            &self.client,
            &format!("{}/deploy_tokens?active=true", self.project_api_for(repo)?),
            "per_page",
            |b| self.authorized(b),
            "deploy tokens",
            PageCap::FailClosed,
        )
        .await?;
        Ok(raw
            .iter()
            .filter(|t| {
                !t.get("revoked").and_then(Value::as_bool).unwrap_or(false)
                    && !t.get("expired").and_then(Value::as_bool).unwrap_or(false)
            })
            .filter_map(|t| {
                Some(RepoReadTokenInfo {
                    id: t.get("id").and_then(id_string)?,
                    name: str_field(t, "name").unwrap_or_default(),
                })
            })
            .collect())
    }

    pub(crate) async fn revoke_repo_token(
        &self,
        repo: &RepositoryTarget,
        token_id: &str,
    ) -> Result<(), ForgeError> {
        let id = numeric_id(token_id, "deploy token")?;
        delete_ignoring_missing(
            &self.client,
            &format!("{}/deploy_tokens/{id}", self.project_api_for(repo)?),
            |b| self.authorized(b),
            "deploy token",
        )
        .await
    }

    pub(crate) async fn list_repo_deploy_keys(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        let raw: Vec<Value> = fetch_pages(
            &self.client,
            &format!("{}/deploy_keys", self.project_api_for(repo)?),
            "per_page",
            |b| self.authorized(b),
            "deploy keys",
            PageCap::FailClosed,
        )
        .await?;
        Ok(raw
            .iter()
            .filter_map(|k| {
                let can_push = k.get("can_push").and_then(Value::as_bool).unwrap_or(true);
                deploy_key_from_json(k, !can_push)
            })
            .collect())
    }

    pub(crate) async fn ensure_repo_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        let (title, key) = validated_deploy_key(title, public_key)?;
        let fingerprint = ssh_public_key_fingerprint(&key)?;
        let project_api = self.project_api_for(repo)?;
        let existing = self.list_repo_deploy_keys(repo).await?;
        let plan = plan_deploy_key(&existing, &fingerprint, read_only);
        for id in &plan.delete {
            self.revoke_repo_deploy_key(repo, id).await?;
        }
        if let Some(keep) = plan.keep {
            let outcome = if plan.delete.is_empty() {
                DeployKeyOutcome::Existed
            } else {
                DeployKeyOutcome::Replaced
            };
            return Ok(EnsuredDeployKey { key: keep, outcome });
        }
        let resp = self
            .authorized(self.client.post(format!("{project_api}/deploy_keys")))
            .json(&json!({ "title": title, "key": key, "can_push": !read_only }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, "deploy key").await);
        }
        let body = json_body(resp).await?;
        let created = deploy_key_from_json(&body, read_only).ok_or_else(|| ForgeError::Api {
            status: 201,
            message: "deploy key response has no id".to_string(),
        })?;
        Ok(EnsuredDeployKey {
            key: DeployKeyInfo {
                fingerprint: Some(fingerprint),
                ..created
            },
            outcome: if plan.delete.is_empty() {
                DeployKeyOutcome::Created
            } else {
                DeployKeyOutcome::Replaced
            },
        })
    }

    pub(crate) async fn revoke_repo_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        let id = numeric_id(key_id, "deploy key")?;
        delete_ignoring_missing(
            &self.client,
            &format!("{}/deploy_keys/{id}", self.project_api_for(repo)?),
            |b| self.authorized(b),
            "deploy key",
        )
        .await
    }

    /// `main` is protected by default on GitLab with other levels, and the protection API has
    /// no portable in-place update, so a drifted rule is deleted and re-created (push: no one,
    /// merge: Maintainers, no force push). `main` is briefly unprotected between the calls.
    pub(crate) async fn ensure_repo_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        let project_api = self.project_api_for(repo)?;
        let authorize = |b| self.authorized(b);
        require_main_branch(
            &self.client,
            &format!("{project_api}/repository/branches/{PROTECTED_MAIN_BRANCH}"),
            authorize,
            repo,
        )
        .await?;
        let rule_url = format!("{project_api}/protected_branches/{PROTECTED_MAIN_BRANCH}");
        let current = get_optional(&self.client, &rule_url, authorize, "branch protection").await?;
        if current.as_ref().is_some_and(gitlab_protection_matches) {
            return Ok(BranchProtectionOutcome::Unchanged);
        }
        if current.is_some() {
            delete_ignoring_missing(&self.client, &rule_url, authorize, "branch protection")
                .await?;
        }
        let resp = authorize(
            self.client
                .post(format!("{project_api}/protected_branches")),
        )
        .json(&json!({
            "name": PROTECTED_MAIN_BRANCH,
            "push_access_level": GITLAB_NO_ACCESS,
            "merge_access_level": GITLAB_MAINTAINER,
            "allow_force_push": false,
        }))
        .send()
        .await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, "branch protection").await);
        }
        Ok(if current.is_some() {
            BranchProtectionOutcome::Updated
        } else {
            BranchProtectionOutcome::Created
        })
    }

    pub(crate) async fn disable_repo_forks(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        let project_api = self.project_api_for(repo)?;
        let authorize = |b| self.authorized(b);
        let project = get_optional(&self.client, &project_api, authorize, "repository")
            .await?
            .ok_or_else(|| ForgeError::NotFound(repo.full_name()))?;
        if project.get("forking_access_level").and_then(Value::as_str) == Some("disabled") {
            return Ok(ForkPolicyOutcome::AlreadyDisabled);
        }
        let resp = authorize(self.client.put(&project_api))
            .json(&json!({ "forking_access_level": "disabled" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, "repository").await);
        }
        Ok(ForkPolicyOutcome::Disabled)
    }
}

#[cfg(test)]
#[path = "remote_repo_tests.rs"]
mod tests;
