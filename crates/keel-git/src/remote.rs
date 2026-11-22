//! Remote Git Forge REST adapters (`ForgejoGiteaProvider`, `GitHubProvider`, `GitLabProvider`)
//! implementing [`ForgeProvider`] via `reqwest::Client` (`SDD.md` §3.2).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

use std::sync::Arc;

use crate::remote_repo::ExistingRepo;

use crate::forge::{
    BranchProtectionOutcome, CollaboratorOutcome, CreatePrRequest, DeployKeyInfo, EnsuredDeployKey,
    ForgeError, ForgeProvider, ForkInfo, ForkPolicyOutcome, PullRequestInfo, PullRequestState,
    RepoCredentialKind, RepoReadToken, RepositoryEnsureOutcome, RepositoryInfo, RepositoryOptions,
    RepositoryTarget, matches_tenant, validate_commit_sha,
};
use crate::git::parse_proposal_branch_tenant;

const DEFAULT_USER_AGENT: &str = "keel-git/0.1.0";

fn build_http_client() -> Result<Client, ForgeError> {
    Ok(Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?)
}

fn normalize_base_url(base_url: &str) -> Result<String, ForgeError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() || !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err(ForgeError::InvalidInput(
            "base_url must start with http:// or https://".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn validate_slug(value: &str, field: &str) -> Result<String, ForgeError> {
    let trimmed = value.trim().trim_matches('/');
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed
            .chars()
            .any(|c| c.is_control() || c == '?' || c == '#')
    {
        return Err(ForgeError::InvalidInput(format!(
            "invalid forge {field}: '{value}'"
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_pr_number(pr_id: &str) -> Result<&str, ForgeError> {
    if pr_id.is_empty() || !pr_id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ForgeError::InvalidInput(format!(
            "pull request id must be numeric, got '{pr_id}'"
        )));
    }
    Ok(pr_id)
}

pub(crate) fn url_encode_project_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len() + 8);
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char);
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    encoded
}

/// Parsed components of a remote Git repository URL (`https://...` or `git@host:owner/repo.git`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepoSlug {
    /// HTTP(S) origin base URL (e.g., `https://github.com`, `https://git.example.com`).
    pub origin_url: String,
    /// Hostname (e.g., `github.com`).
    pub host: String,
    /// Repository owner or GitLab group path (e.g., `myorg` or `group/subgroup`).
    pub owner: String,
    /// Repository name without `.git` suffix.
    pub repo: String,
    /// Full project path (`{owner}/{repo}`).
    pub project_path: String,
}

/// Parses an HTTPS, HTTP, SSH (`ssh://git@host/owner/repo.git`), or SCP-style (`git@host:owner/repo.git`)
/// Git URL into a [`RemoteRepoSlug`].
pub fn parse_remote_repo_url(repo_url: &str) -> Result<RemoteRepoSlug, ForgeError> {
    let trimmed = repo_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ForgeError::InvalidInput(
            "repository URL must not be empty".to_string(),
        ));
    }

    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else if let Some(r) = trimmed.strip_prefix("ssh://") {
        ("https", r)
    } else if let Some((user_host, path)) = trimmed.split_once(':')
        && !user_host.contains('/')
        && !path.is_empty()
    {
        let host = user_host
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(user_host);
        return build_slug("https", host, path);
    } else {
        return Err(ForgeError::InvalidInput(format!(
            "unsupported remote Git URL '{repo_url}'"
        )));
    };

    let (authority, path) = rest.split_once('/').ok_or_else(|| {
        ForgeError::InvalidInput(format!("missing repository path in URL '{repo_url}'"))
    })?;
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    build_slug(scheme, host_port, path)
}

fn build_slug(scheme: &str, host_port: &str, raw_path: &str) -> Result<RemoteRepoSlug, ForgeError> {
    if host_port.trim().is_empty() {
        return Err(ForgeError::InvalidInput(
            "missing host in remote Git URL".to_string(),
        ));
    }
    let clean_path = raw_path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(raw_path.trim_matches('/'));
    let (owner, repo) = clean_path.rsplit_once('/').ok_or_else(|| {
        ForgeError::InvalidInput(format!(
            "remote Git URL path '{raw_path}' must contain '<owner>/<repo>'"
        ))
    })?;
    let owner = validate_slug(owner, "owner")?;
    let repo = validate_slug(repo, "repo")?;
    let host = host_port
        .split_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port)
        .to_string();

    Ok(RemoteRepoSlug {
        origin_url: format!("{scheme}://{host_port}"),
        host,
        project_path: format!("{owner}/{repo}"),
        owner,
        repo,
    })
}

pub(crate) async fn map_error_response(
    resp: reqwest::Response,
    pr_id_hint: Option<&str>,
    is_merge: bool,
) -> ForgeError {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable response body>".to_string());
    let truncated: String = body.chars().take(512).collect();

    if status == StatusCode::NOT_FOUND {
        return ForgeError::NotFound(pr_id_hint.unwrap_or("resource").to_string());
    }
    if is_merge
        && matches!(
            status,
            StatusCode::CONFLICT
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::UNPROCESSABLE_ENTITY
        )
    {
        return ForgeError::MergeConflict(pr_id_hint.unwrap_or("unknown").to_string(), truncated);
    }
    ForgeError::Api {
        status: status.as_u16(),
        message: truncated,
    }
}

// ============================================================================
// Forgejo / Gitea Provider (REST API v1)
// ============================================================================

/// Credentials used by [`ForgejoGiteaProvider`].
#[derive(Clone)]
pub(crate) enum ForgejoAuth {
    /// Personal access token (`Authorization: token <pat>`).
    Token(String),
    /// HTTP basic auth (used by Stage 0 before any access token has been minted).
    Basic { username: String, password: String },
}

/// Forgejo / Gitea REST v1 [`ForgeProvider`] adapter (`/api/v1/repos/{owner}/{repo}/...`).
#[derive(Clone)]
pub struct ForgejoGiteaProvider {
    pub(crate) client: Client,
    pub(crate) base_url: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) auth: ForgejoAuth,
}

impl fmt::Debug for ForgejoGiteaProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgejoGiteaProvider")
            .field("base_url", &self.base_url)
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .field("auth", &"[REDACTED]")
            .finish()
    }
}

impl ForgejoGiteaProvider {
    /// Creates a new [`ForgejoGiteaProvider`] configured for `{base_url}/api/v1/repos/{owner}/{repo}`.
    pub fn new(
        base_url: impl AsRef<str>,
        owner: impl AsRef<str>,
        repo: impl AsRef<str>,
        token: impl Into<String>,
    ) -> Result<Self, ForgeError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "Forgejo/Gitea token must not be empty".to_string(),
            ));
        }
        Self::with_auth(base_url, owner, repo, ForgejoAuth::Token(token))
    }

    /// Creates a [`ForgejoGiteaProvider`] authenticating with HTTP basic auth (username +
    /// password), e.g. the Stage 0 Forgejo admin before an access token exists.
    pub fn with_basic_auth(
        base_url: impl AsRef<str>,
        owner: impl AsRef<str>,
        repo: impl AsRef<str>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, ForgeError> {
        let username = username.into();
        let password = password.into();
        if username.trim().is_empty() || password.is_empty() {
            return Err(ForgeError::InvalidInput(
                "Forgejo/Gitea basic auth username and password must not be empty".to_string(),
            ));
        }
        Self::with_auth(
            base_url,
            owner,
            repo,
            ForgejoAuth::Basic { username, password },
        )
    }

    fn with_auth(
        base_url: impl AsRef<str>,
        owner: impl AsRef<str>,
        repo: impl AsRef<str>,
        auth: ForgejoAuth,
    ) -> Result<Self, ForgeError> {
        Ok(Self {
            client: build_http_client()?,
            base_url: normalize_base_url(base_url.as_ref())?,
            owner: validate_slug(owner.as_ref(), "owner")?,
            repo: validate_slug(repo.as_ref(), "repo")?,
            auth,
        })
    }

    /// Returns the base repository API endpoint URL.
    pub fn repo_api_url(&self) -> String {
        format!(
            "{}/api/v1/repos/{}/{}",
            self.base_url, self.owner, self.repo
        )
    }

    /// Mints a personal access token for the basic-auth user (`POST /api/v1/users/{user}/tokens`).
    /// Forgejo/Gitea only accept basic auth on this endpoint, so token-authenticated providers
    /// return [`ForgeError::InvalidInput`].
    pub async fn create_access_token(
        &self,
        name: &str,
        scopes: &[&str],
    ) -> Result<String, ForgeError> {
        let ForgejoAuth::Basic { username, .. } = &self.auth else {
            return Err(ForgeError::InvalidInput(
                "minting a Forgejo/Gitea access token requires basic auth".to_string(),
            ));
        };
        if name.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "access token name must not be empty".to_string(),
            ));
        }
        let user = validate_slug(username, "username")?;
        let resp = self
            .authorized(
                self.client
                    .post(format!("{}/api/v1/users/{user}/tokens", self.base_url)),
            )
            .json(&serde_json::json!({ "name": name, "scopes": scopes }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("access token"), false).await);
        }
        let body: serde_json::Value = resp.json().await?;
        body.get("sha1")
            .or_else(|| body.get("token"))
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Api {
                status: 200,
                message: "Forgejo access token response missing token field".to_string(),
            })
    }

    /// Builds an authenticated `reqwest::RequestBuilder` with Forgejo/Gitea token (or basic) auth.
    pub fn authorized(&self, builder: RequestBuilder) -> RequestBuilder {
        let builder = match &self.auth {
            ForgejoAuth::Token(token) => builder.header("Authorization", format!("token {token}")),
            ForgejoAuth::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
        };
        builder.header("Accept", "application/json")
    }
}

#[derive(Serialize)]
struct GiteaCreatePrBody<'a> {
    title: &'a str,
    body: &'a str,
    head: &'a str,
    base: &'a str,
}

#[derive(Serialize)]
struct GiteaMergeBody<'a> {
    #[serde(rename = "Do")]
    merge_style: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_commit_id: Option<&'a str>,
}

#[derive(Serialize)]
struct MarkdownCommentBody<'a> {
    body: &'a str,
}

#[derive(Deserialize)]
struct GiteaGitHubBranchRef {
    #[serde(rename = "ref")]
    branch_ref: String,
    #[serde(default)]
    sha: String,
}

#[derive(Deserialize)]
struct GiteaGitHubPrResponse {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    merged: bool,
    head: GiteaGitHubBranchRef,
    base: GiteaGitHubBranchRef,
    html_url: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    merge_commit_sha: Option<String>,
}

/// Page size requested from forge list endpoints (GitHub/GitLab cap at 100, Gitea at its
/// configured `MAX_RESPONSE_ITEMS`, default 50).
pub(crate) const LIST_PAGE_SIZE: usize = 50;

/// Hard cap on pages fetched per listing (`LIST_PAGE_SIZE * MAX_LIST_PAGES` PRs) so a huge or
/// misbehaving forge cannot pin a request handler; truncation is logged.
pub(crate) const MAX_LIST_PAGES: usize = 40;

/// Returns `true` when an RFC 8288 `Link` header advertises a `rel="next"` page.
pub(crate) fn link_header_has_next(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(reqwest::header::LINK)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|part| {
            part.split(';')
                .skip(1)
                .any(|p| matches!(p.trim(), "rel=\"next\"" | "rel=next"))
        })
}

/// Decides whether another page must be fetched: an explicit `next` link wins; otherwise a full
/// page implies more results (Gitea does not always emit `Link` headers).
pub(crate) fn page_has_more(batch_len: usize, has_next_link: bool) -> bool {
    batch_len > 0 && (has_next_link || batch_len >= LIST_PAGE_SIZE)
}

fn warn_list_truncated(what: &str, fetched: usize) {
    tracing::warn!(
        fetched,
        max_pages = MAX_LIST_PAGES,
        "Forge listing of {what} truncated at page cap; older entries are omitted"
    );
}

fn rewrite_local_forge_url(base_url: &str, html_url: String) -> String {
    let base = base_url.trim_end_matches('/');
    if !(base.starts_with("http://127.0.0.1:") || base.starts_with("http://localhost:")) {
        return html_url;
    }
    if let Some(after_scheme) = html_url
        .strip_prefix("http://")
        .or_else(|| html_url.strip_prefix("https://"))
        && let Some((host_port, path)) = after_scheme.split_once('/')
        && (host_port.contains(".svc.cluster.local")
            || host_port.starts_with("git.example.com")
            || host_port.starts_with("localhost")
            || host_port.starts_with("127.0.0.1"))
    {
        return format!("{base}/{path}");
    }
    html_url
}

impl GiteaGitHubPrResponse {
    fn into_pr_info(self, explicit_tenant: Option<String>, base_url: &str) -> PullRequestInfo {
        let state = if self.merged || self.state.eq_ignore_ascii_case("merged") {
            PullRequestState::Merged
        } else if self.state.eq_ignore_ascii_case("open") {
            PullRequestState::Open
        } else {
            PullRequestState::Closed
        };
        let tenant = explicit_tenant
            .or_else(|| parse_proposal_branch_tenant(&self.head.branch_ref).map(ToOwned::to_owned));
        PullRequestInfo {
            id: self.number.to_string(),
            title: self.title,
            body: self.body.unwrap_or_default(),
            state,
            source_branch: self.head.branch_ref,
            target_branch: self.base.branch_ref,
            head_sha: self.head.sha,
            tenant,
            url: rewrite_local_forge_url(base_url, self.html_url),
            created_at: self.created_at,
            updated_at: self.updated_at,
            comments: Vec::new(),
            diff_stat: None,
            // GitHub reports a speculative test-merge SHA for open PRs; only trust it once merged.
            merge_commit_sha: if state == PullRequestState::Merged {
                self.merge_commit_sha.filter(|s| !s.trim().is_empty())
            } else {
                None
            },
            base_sha: None,
        }
    }
}

async fn create_gitea_github_pr<F>(
    client: &Client,
    base_url: &str,
    repo_api_url: &str,
    authorize: F,
    req: CreatePrRequest,
) -> Result<PullRequestInfo, ForgeError>
where
    F: FnOnce(RequestBuilder) -> RequestBuilder,
{
    if req.title.trim().is_empty() {
        return Err(ForgeError::InvalidInput(
            "pull request title must not be empty".to_string(),
        ));
    }
    let url = format!("{repo_api_url}/pulls");
    let payload = GiteaCreatePrBody {
        title: &req.title,
        body: &req.body,
        head: &req.source_branch,
        base: &req.target_branch,
    };
    let resp = authorize(client.post(&url)).json(&payload).send().await?;
    if !resp.status().is_success() {
        return Err(map_error_response(resp, None, false).await);
    }
    let parsed: GiteaGitHubPrResponse = resp.json().await?;
    Ok(parsed.into_pr_info(req.tenant, base_url))
}

async fn get_gitea_github_pr<F>(
    client: &Client,
    base_url: &str,
    repo_api_url: &str,
    authorize: F,
    pr_id: &str,
) -> Result<PullRequestInfo, ForgeError>
where
    F: FnOnce(RequestBuilder) -> RequestBuilder,
{
    let id = validate_pr_number(pr_id)?;
    let url = format!("{repo_api_url}/pulls/{id}");
    let resp = authorize(client.get(&url)).send().await?;
    if !resp.status().is_success() {
        return Err(map_error_response(resp, Some(id), false).await);
    }
    let parsed: GiteaGitHubPrResponse = resp.json().await?;
    Ok(parsed.into_pr_info(None, base_url))
}

async fn post_gitea_github_comment<F>(
    client: &Client,
    repo_api_url: &str,
    authorize: F,
    pr_id: &str,
    markdown_report: &str,
) -> Result<(), ForgeError>
where
    F: FnOnce(RequestBuilder) -> RequestBuilder,
{
    let id = validate_pr_number(pr_id)?;
    if markdown_report.trim().is_empty() {
        return Err(ForgeError::InvalidInput(
            "markdown_report must not be empty".to_string(),
        ));
    }
    let url = format!("{repo_api_url}/issues/{id}/comments");
    let resp = authorize(client.post(&url))
        .json(&MarkdownCommentBody {
            body: markdown_report,
        })
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(map_error_response(resp, Some(id), false).await);
    }
    Ok(())
}

async fn list_gitea_github_prs<F>(
    client: &Client,
    base_url: &str,
    repo_api_url: &str,
    page_size_param: &str,
    authorize: F,
    tenant: Option<&str>,
    state: Option<PullRequestState>,
) -> Result<Vec<PullRequestInfo>, ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    let state_param = match state {
        None => "all",
        Some(PullRequestState::Open) => "open",
        Some(PullRequestState::Closed | PullRequestState::Merged) => "closed",
    };
    let mut items: Vec<GiteaGitHubPrResponse> = Vec::new();
    for page in 1..=MAX_LIST_PAGES {
        let url = format!(
            "{repo_api_url}/pulls?state={state_param}&{page_size_param}={LIST_PAGE_SIZE}&page={page}"
        );
        let resp = authorize(client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let has_next_link = link_header_has_next(resp.headers());
        let batch: Vec<GiteaGitHubPrResponse> = resp.json().await?;
        let batch_len = batch.len();
        items.extend(batch);
        if !page_has_more(batch_len, has_next_link) {
            break;
        }
        if page == MAX_LIST_PAGES {
            warn_list_truncated("pull requests", items.len());
        }
    }
    let prs = items
        .into_iter()
        .map(|item| item.into_pr_info(None, base_url))
        .filter(|pr| tenant.is_none_or(|t| matches_tenant(pr, t)))
        .filter(|pr| state.is_none_or(|s| pr.state == s))
        .collect();
    Ok(prs)
}

async fn patch_gitea_github_pr_state<F>(
    client: &Client,
    base_url: &str,
    repo_api_url: &str,
    authorize: F,
    pr_id: &str,
    target_state: &str,
) -> Result<PullRequestInfo, ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    let id = validate_pr_number(pr_id)?;
    let url = format!("{repo_api_url}/pulls/{id}");
    let resp = authorize(client.patch(&url))
        .json(&serde_json::json!({ "state": target_state }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(map_error_response(resp, Some(id), false).await);
    }
    get_gitea_github_pr(client, base_url, repo_api_url, authorize, id).await
}

#[async_trait]
impl ForgeProvider for ForgejoGiteaProvider {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        create_gitea_github_pr(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            req,
        )
        .await
    }

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        get_gitea_github_pr(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
        )
        .await
    }

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError> {
        post_gitea_github_comment(
            &self.client,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
            markdown_report,
        )
        .await
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
        list_gitea_github_prs(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            "limit",
            |b| self.authorized(b),
            tenant,
            state,
        )
        .await
    }

    async fn merge_pull_request(
        &self,
        pr_id: &str,
        expected_head_sha: Option<&str>,
    ) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let head_commit_id = expected_head_sha.map(validate_commit_sha).transpose()?;
        let url = format!("{}/pulls/{id}/merge", self.repo_api_url());
        for attempt in 0..6 {
            let resp = self
                .authorized(self.client.post(&url))
                .json(&GiteaMergeBody {
                    merge_style: "merge",
                    head_commit_id,
                })
                .send()
                .await?;
            if resp.status().is_success() {
                return self.get_pull_request(id).await;
            }
            if resp.status() == reqwest::StatusCode::CONFLICT {
                let body = resp.text().await.unwrap_or_default();
                if attempt < 5 && body.contains("Please try again later") {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                return Err(ForgeError::MergeConflict(id.to_string(), body));
            }
            return Err(map_error_response(resp, Some(id), true).await);
        }
        unreachable!("loop returns on final attempt")
    }

    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        patch_gitea_github_pr_state(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
            "closed",
        )
        .await
    }

    async fn reopen_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        patch_gitea_github_pr_state(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
            "open",
        )
        .await
    }

    async fn ensure_repository(
        &self,
        private: bool,
    ) -> Result<RepositoryEnsureOutcome, ForgeError> {
        let opts = RepositoryOptions {
            private,
            auto_init: false,
            default_branch: None,
            description: None,
        };
        Ok(self
            .ensure_named_repo(&self.owner, &self.repo, &opts, ExistingRepo::Accept)
            .await?
            .outcome)
    }

    fn for_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        self.scoped_to(owner, name)
    }

    async fn ensure_named_repository(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
    ) -> Result<RepositoryInfo, ForgeError> {
        self.ensure_named_repo(owner, name, opts, ExistingRepo::Verify)
            .await
    }

    fn read_credential_kind(&self) -> Result<RepoCredentialKind, ForgeError> {
        Ok(RepoCredentialKind::HttpsToken)
    }

    /// Requires a basic-auth provider for the token-owning bot (Forgejo mints tokens only as
    /// the owner over basic auth) and Forgejo 15+ (repository-specific tokens; Gitea fails
    /// closed).
    async fn create_repo_read_token(
        &self,
        repo: &RepositoryTarget,
        name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        self.create_repo_token(repo, name).await
    }

    async fn revoke_repo_read_token(
        &self,
        _repo: &RepositoryTarget,
        token_id: &str,
    ) -> Result<(), ForgeError> {
        self.revoke_repo_token(token_id).await
    }

    async fn ensure_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        self.ensure_repo_deploy_key(repo, title, public_key, read_only)
            .await
    }

    async fn list_deploy_keys(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        self.list_repo_deploy_keys(repo).await
    }

    async fn revoke_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        self.revoke_repo_deploy_key(repo, key_id).await
    }

    async fn ensure_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        self.ensure_repo_main_protection(repo).await
    }

    async fn disable_forks(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        self.repo_fork_policy().await
    }

    async fn ensure_read_collaborator(
        &self,
        repo: &RepositoryTarget,
        username: &str,
    ) -> Result<CollaboratorOutcome, ForgeError> {
        self.ensure_repo_read_collaborator(repo, username).await
    }

    async fn remove_collaborator(
        &self,
        repo: &RepositoryTarget,
        username: &str,
    ) -> Result<(), ForgeError> {
        self.remove_repo_collaborator(repo, username).await
    }

    async fn list_forks(&self, repo: &RepositoryTarget) -> Result<Vec<ForkInfo>, ForgeError> {
        self.list_repo_forks(repo).await
    }
}

// ============================================================================
// GitHub Provider (REST API v3)
// ============================================================================

/// GitHub REST v3 [`ForgeProvider`] adapter (`/repos/{owner}/{repo}/...`).
#[derive(Clone)]
pub struct GitHubProvider {
    pub(crate) client: Client,
    pub(crate) base_url: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) token: String,
}

impl fmt::Debug for GitHubProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubProvider")
            .field("base_url", &self.base_url)
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl GitHubProvider {
    /// Creates a new [`GitHubProvider`] (use `"https://api.github.com"` for GitHub.com).
    pub fn new(
        base_url: impl AsRef<str>,
        owner: impl AsRef<str>,
        repo: impl AsRef<str>,
        token: impl Into<String>,
    ) -> Result<Self, ForgeError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "GitHub token must not be empty".to_string(),
            ));
        }
        Ok(Self {
            client: build_http_client()?,
            base_url: normalize_base_url(base_url.as_ref())?,
            owner: validate_slug(owner.as_ref(), "owner")?,
            repo: validate_slug(repo.as_ref(), "repo")?,
            token,
        })
    }

    /// Returns the base repository API endpoint URL.
    pub fn repo_api_url(&self) -> String {
        format!("{}/repos/{}/{}", self.base_url, self.owner, self.repo)
    }

    /// Builds an authenticated `reqwest::RequestBuilder` with GitHub REST v3 headers.
    pub fn authorized(&self, builder: RequestBuilder) -> RequestBuilder {
        builder
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }
}

#[derive(Serialize)]
struct GitHubMergeBody<'a> {
    merge_method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<&'a str>,
}

#[async_trait]
impl ForgeProvider for GitHubProvider {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        create_gitea_github_pr(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            req,
        )
        .await
    }

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        get_gitea_github_pr(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
        )
        .await
    }

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError> {
        post_gitea_github_comment(
            &self.client,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
            markdown_report,
        )
        .await
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
        list_gitea_github_prs(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            "per_page",
            |b| self.authorized(b),
            tenant,
            state,
        )
        .await
    }

    async fn merge_pull_request(
        &self,
        pr_id: &str,
        expected_head_sha: Option<&str>,
    ) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let sha = expected_head_sha.map(validate_commit_sha).transpose()?;
        let url = format!("{}/pulls/{id}/merge", self.repo_api_url());
        let resp = self
            .authorized(self.client.put(&url))
            .json(&GitHubMergeBody {
                merge_method: "rebase",
                sha,
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(id), true).await);
        }
        self.get_pull_request(id).await
    }

    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        patch_gitea_github_pr_state(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
            "closed",
        )
        .await
    }

    async fn reopen_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        patch_gitea_github_pr_state(
            &self.client,
            &self.base_url,
            &self.repo_api_url(),
            |b| self.authorized(b),
            pr_id,
            "open",
        )
        .await
    }

    async fn ensure_repository(
        &self,
        private: bool,
    ) -> Result<RepositoryEnsureOutcome, ForgeError> {
        let opts = RepositoryOptions {
            private,
            auto_init: false,
            default_branch: None,
            description: None,
        };
        Ok(self
            .ensure_named_repo(&self.owner, &self.repo, &opts, ExistingRepo::Accept)
            .await?
            .outcome)
    }

    fn for_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        self.scoped_to(owner, name)
    }

    async fn ensure_named_repository(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
    ) -> Result<RepositoryInfo, ForgeError> {
        self.ensure_named_repo(owner, name, opts, ExistingRepo::Verify)
            .await
    }

    fn read_credential_kind(&self) -> Result<RepoCredentialKind, ForgeError> {
        Ok(RepoCredentialKind::SshDeployKey)
    }

    /// GitHub has no repository-scoped read token for a PAT owner; use a read-only deploy key
    /// ([`ForgeProvider::ensure_deploy_key`]).
    async fn create_repo_read_token(
        &self,
        _repo: &RepositoryTarget,
        _name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        Err(ForgeError::Unsupported(
            "create_repo_read_token (GitHub: use a read-only SSH deploy key)".to_string(),
        ))
    }

    async fn ensure_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        self.ensure_repo_deploy_key(repo, title, public_key, read_only)
            .await
    }

    async fn list_deploy_keys(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        self.list_repo_deploy_keys(repo).await
    }

    async fn revoke_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        self.revoke_repo_deploy_key(repo, key_id).await
    }

    async fn ensure_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        self.ensure_repo_main_protection(repo).await
    }

    async fn disable_forks(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        self.disable_repo_forks(repo).await
    }

    async fn ensure_read_collaborator(
        &self,
        repo: &RepositoryTarget,
        username: &str,
    ) -> Result<CollaboratorOutcome, ForgeError> {
        self.ensure_repo_read_collaborator(repo, username).await
    }

    async fn remove_collaborator(
        &self,
        repo: &RepositoryTarget,
        username: &str,
    ) -> Result<(), ForgeError> {
        self.remove_repo_collaborator(repo, username).await
    }

    async fn list_forks(&self, repo: &RepositoryTarget) -> Result<Vec<ForkInfo>, ForgeError> {
        self.list_repo_forks(repo).await
    }
}

// ============================================================================
// GitLab Provider (REST API v4)
// ============================================================================

/// GitLab REST v4 [`ForgeProvider`] adapter (`/api/v4/projects/{encoded_project}/...`).
#[derive(Clone)]
pub struct GitLabProvider {
    pub(crate) client: Client,
    pub(crate) base_url: String,
    pub(crate) project_path: String,
    pub(crate) encoded_project: String,
    pub(crate) token: String,
}

impl fmt::Debug for GitLabProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitLabProvider")
            .field("base_url", &self.base_url)
            .field("project_path", &self.project_path)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl GitLabProvider {
    /// Creates a new [`GitLabProvider`] (e.g. `base_url = "https://gitlab.com"`, `project_path = "org/infra-gitops"`).
    pub fn new(
        base_url: impl AsRef<str>,
        project_path: impl AsRef<str>,
        token: impl Into<String>,
    ) -> Result<Self, ForgeError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "GitLab token must not be empty".to_string(),
            ));
        }
        let clean_project = validate_slug(project_path.as_ref(), "project_path")?;
        let encoded_project = url_encode_project_path(&clean_project);
        Ok(Self {
            client: build_http_client()?,
            base_url: normalize_base_url(base_url.as_ref())?,
            project_path: clean_project,
            encoded_project,
            token,
        })
    }

    /// Returns the base GitLab REST v4 project API URL.
    pub fn project_api_url(&self) -> String {
        format!("{}/api/v4/projects/{}", self.base_url, self.encoded_project)
    }

    /// Builds an authenticated `reqwest::RequestBuilder` with `PRIVATE-TOKEN` header.
    pub fn authorized(&self, builder: RequestBuilder) -> RequestBuilder {
        builder
            .header("PRIVATE-TOKEN", &self.token)
            .header("Accept", "application/json")
    }
}

#[derive(Serialize)]
struct GitLabCreateMrBody<'a> {
    title: &'a str,
    description: &'a str,
    source_branch: &'a str,
    target_branch: &'a str,
}

#[derive(Deserialize)]
struct GitLabMrResponse {
    iid: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: String,
    source_branch: String,
    target_branch: String,
    #[serde(default)]
    sha: String,
    web_url: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    #[serde(default)]
    squash_commit_sha: Option<String>,
    #[serde(default)]
    diff_refs: Option<GitLabDiffRefs>,
}

#[derive(Deserialize)]
struct GitLabDiffRefs {
    #[serde(default)]
    base_sha: Option<String>,
}

impl GitLabMrResponse {
    fn into_pr_info(self, explicit_tenant: Option<String>) -> PullRequestInfo {
        let state = match self.state.to_ascii_lowercase().as_str() {
            "merged" => PullRequestState::Merged,
            "opened" | "open" => PullRequestState::Open,
            _ => PullRequestState::Closed,
        };
        let tenant = explicit_tenant
            .or_else(|| parse_proposal_branch_tenant(&self.source_branch).map(ToOwned::to_owned));
        PullRequestInfo {
            id: self.iid.to_string(),
            title: self.title,
            body: self.description.unwrap_or_default(),
            state,
            source_branch: self.source_branch,
            target_branch: self.target_branch,
            head_sha: self.sha,
            tenant,
            url: self.web_url,
            created_at: self.created_at,
            updated_at: self.updated_at,
            comments: Vec::new(),
            diff_stat: None,
            merge_commit_sha: self
                .merge_commit_sha
                .or(self.squash_commit_sha)
                .filter(|s| !s.trim().is_empty()),
            base_sha: self
                .diff_refs
                .and_then(|d| d.base_sha)
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

#[async_trait]
impl ForgeProvider for GitLabProvider {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        if req.title.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "merge request title must not be empty".to_string(),
            ));
        }
        let url = format!("{}/merge_requests", self.project_api_url());
        let payload = GitLabCreateMrBody {
            title: &req.title,
            description: &req.body,
            source_branch: &req.source_branch,
            target_branch: &req.target_branch,
        };
        let resp = self
            .authorized(self.client.post(&url))
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let parsed: GitLabMrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(req.tenant))
    }

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let iid = validate_pr_number(pr_id)?;
        let url = format!("{}/merge_requests/{iid}", self.project_api_url());
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(iid), false).await);
        }
        let parsed: GitLabMrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(None))
    }

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError> {
        let iid = validate_pr_number(pr_id)?;
        if markdown_report.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "markdown_report must not be empty".to_string(),
            ));
        }
        let url = format!("{}/merge_requests/{iid}/notes", self.project_api_url());
        let resp = self
            .authorized(self.client.post(&url))
            .json(&MarkdownCommentBody {
                body: markdown_report,
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(iid), false).await);
        }
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
        let state_param = match state {
            None => "all",
            Some(PullRequestState::Open) => "opened",
            Some(PullRequestState::Merged) => "merged",
            Some(PullRequestState::Closed) => "closed",
        };
        let mut items: Vec<GitLabMrResponse> = Vec::new();
        for page in 1..=MAX_LIST_PAGES {
            let url = format!(
                "{}/merge_requests?state={state_param}&per_page={LIST_PAGE_SIZE}&page={page}",
                self.project_api_url()
            );
            let resp = self.authorized(self.client.get(&url)).send().await?;
            if !resp.status().is_success() {
                return Err(map_error_response(resp, None, false).await);
            }
            let has_next = link_header_has_next(resp.headers())
                || resp
                    .headers()
                    .get("x-next-page")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| !v.trim().is_empty());
            let batch: Vec<GitLabMrResponse> = resp.json().await?;
            let batch_len = batch.len();
            items.extend(batch);
            if !page_has_more(batch_len, has_next) {
                break;
            }
            if page == MAX_LIST_PAGES {
                warn_list_truncated("merge requests", items.len());
            }
        }
        let prs = items
            .into_iter()
            .map(|item| item.into_pr_info(None))
            .filter(|pr| tenant.is_none_or(|t| matches_tenant(pr, t)))
            .collect();
        Ok(prs)
    }

    async fn merge_pull_request(
        &self,
        pr_id: &str,
        expected_head_sha: Option<&str>,
    ) -> Result<PullRequestInfo, ForgeError> {
        let iid = validate_pr_number(pr_id)?;
        let url = format!("{}/merge_requests/{iid}/merge", self.project_api_url());
        let mut req = self.authorized(self.client.put(&url));
        if let Some(sha) = expected_head_sha {
            // GitLab rejects the merge (409) if `sha` no longer matches the MR source HEAD.
            req = req.json(&serde_json::json!({ "sha": validate_commit_sha(sha)? }));
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(iid), true).await);
        }
        let parsed: GitLabMrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(None))
    }

    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let iid = validate_pr_number(pr_id)?;
        let url = format!("{}/merge_requests/{iid}", self.project_api_url());
        let resp = self
            .authorized(self.client.put(&url))
            .json(&serde_json::json!({ "state_event": "close" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(iid), false).await);
        }
        let parsed: GitLabMrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(None))
    }

    async fn reopen_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let iid = validate_pr_number(pr_id)?;
        let url = format!("{}/merge_requests/{iid}", self.project_api_url());
        let resp = self
            .authorized(self.client.put(&url))
            .json(&serde_json::json!({ "state_event": "reopen" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(iid), false).await);
        }
        let parsed: GitLabMrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(None))
    }

    /// Creates the project inside its namespace (group or user, resolved via
    /// `/api/v4/namespaces/{path}`). Non-private projects are created `internal`, never `public`.
    async fn ensure_repository(
        &self,
        private: bool,
    ) -> Result<RepositoryEnsureOutcome, ForgeError> {
        let (namespace, name) = self.project_path.rsplit_once('/').ok_or_else(|| {
            ForgeError::InvalidInput(format!(
                "GitLab project path '{}' must include a namespace",
                self.project_path
            ))
        })?;
        let opts = RepositoryOptions {
            private,
            auto_init: false,
            default_branch: None,
            description: None,
        };
        Ok(self
            .ensure_named_repo(namespace, name, &opts, ExistingRepo::Accept)
            .await?
            .outcome)
    }

    fn for_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        self.scoped_to(owner, name)
    }

    async fn ensure_named_repository(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
    ) -> Result<RepositoryInfo, ForgeError> {
        self.ensure_named_repo(owner, name, opts, ExistingRepo::Verify)
            .await
    }

    fn read_credential_kind(&self) -> Result<RepoCredentialKind, ForgeError> {
        Ok(RepoCredentialKind::HttpsToken)
    }

    async fn create_repo_read_token(
        &self,
        repo: &RepositoryTarget,
        name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        self.create_repo_token(repo, name).await
    }

    async fn revoke_repo_read_token(
        &self,
        repo: &RepositoryTarget,
        token_id: &str,
    ) -> Result<(), ForgeError> {
        self.revoke_repo_token(repo, token_id).await
    }

    async fn ensure_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        self.ensure_repo_deploy_key(repo, title, public_key, read_only)
            .await
    }

    async fn list_deploy_keys(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        self.list_repo_deploy_keys(repo).await
    }

    async fn revoke_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        self.revoke_repo_deploy_key(repo, key_id).await
    }

    async fn ensure_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        self.ensure_repo_main_protection(repo).await
    }

    async fn disable_forks(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        self.disable_repo_forks(repo).await
    }

    async fn ensure_read_collaborator(
        &self,
        repo: &RepositoryTarget,
        username: &str,
    ) -> Result<CollaboratorOutcome, ForgeError> {
        self.ensure_repo_read_collaborator(repo, username).await
    }

    async fn remove_collaborator(
        &self,
        repo: &RepositoryTarget,
        username: &str,
    ) -> Result<(), ForgeError> {
        self.remove_repo_collaborator(repo, username).await
    }

    async fn list_forks(&self, repo: &RepositoryTarget) -> Result<Vec<ForkInfo>, ForgeError> {
        self.list_repo_forks(repo).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{request_line, spawn_mock_http_server};
    use base64::Engine as _;

    /// Per-run random Basic-auth password so no credential literal lives in the test source.
    fn random_test_password() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    #[test]
    fn test_remote_provider_url_builders_debug_redaction_and_validation() {
        let forgejo = ForgejoGiteaProvider::new(
            "https://forgejo.internal/",
            "platform",
            "infra-gitops",
            "gitea-secret-pat",
        )
        .unwrap();
        assert_eq!(
            forgejo.repo_api_url(),
            "https://forgejo.internal/api/v1/repos/platform/infra-gitops"
        );
        assert!(!format!("{forgejo:?}").contains("gitea-secret-pat"));
        assert!(format!("{forgejo:?}").contains("[REDACTED]"));

        let gh = GitHubProvider::new(
            "https://api.github.com/",
            "keel-io",
            "infra-gitops",
            "ghp_secret123",
        )
        .unwrap();
        assert_eq!(
            gh.repo_api_url(),
            "https://api.github.com/repos/keel-io/infra-gitops"
        );
        assert!(!format!("{gh:?}").contains("ghp_secret123"));

        let gl = GitLabProvider::new(
            "https://gitlab.com/",
            "keel-io/platform/infra-gitops",
            "glpat-secret123",
        )
        .unwrap();
        assert_eq!(
            gl.project_api_url(),
            "https://gitlab.com/api/v4/projects/keel-io%2Fplatform%2Finfra-gitops"
        );
        assert!(!format!("{gl:?}").contains("glpat-secret123"));

        // Invalid inputs
        assert!(ForgejoGiteaProvider::new("ftp://bad", "o", "r", "tok").is_err());
        assert!(GitHubProvider::new("https://api.github.com", "o", "r", " ").is_err());
        assert!(GitLabProvider::new("https://gitlab.com", "../bad", "tok").is_err());
    }

    #[tokio::test]
    async fn list_pull_requests_follows_pages_until_exhausted() {
        let pr_json = |n: usize| {
            format!(
                r#"{{"number": {n}, "title": "PR {n}", "state": "open", "merged": false,
                "head": {{"ref": "keel/acme/deploy-{n}-1", "sha": "abc{n}"}},
                "base": {{"ref": "main", "sha": "000"}},
                "html_url": "http://forge.local/pulls/{n}",
                "created_at": "2026-11-14T10:00:00Z", "updated_at": "2026-11-14T10:00:00Z"}}"#
            )
        };
        let page1 = format!(
            "[{}]",
            (1..=LIST_PAGE_SIZE)
                .map(pr_json)
                .collect::<Vec<_>>()
                .join(",")
        );
        let page2 = format!("[{}]", pr_json(LIST_PAGE_SIZE + 1));
        let (url, handle) = spawn_mock_http_server(vec![(200, page1), (200, page2)]).await;
        let provider = ForgejoGiteaProvider::new(&url, "keel", "gitops", "tok").unwrap();
        let prs = provider
            .list_pull_requests(Some("acme"), Some(PullRequestState::Open))
            .await
            .unwrap();
        assert_eq!(prs.len(), LIST_PAGE_SIZE + 1);
        let reqs = handle.await.unwrap();
        assert!(reqs[0].contains("limit=50&page=1"));
        assert!(reqs[1].contains("limit=50&page=2"));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#
                .parse()
                .unwrap(),
        );
        assert!(link_header_has_next(&headers));
        headers.insert(
            reqwest::header::LINK,
            r#"<https://api.github.com/x?page=1>; rel="prev""#.parse().unwrap(),
        );
        assert!(!link_header_has_next(&headers));
        assert!(page_has_more(LIST_PAGE_SIZE, false));
        assert!(page_has_more(3, true));
        assert!(!page_has_more(3, false));
        assert!(!page_has_more(0, true));
    }

    #[tokio::test]
    async fn test_forgejo_and_github_providers_via_trait_object() {
        let open_json = r#"{
            "number": 42,
            "title": "Deploy Redis",
            "body": "Adds redis release",
            "state": "open",
            "merged": false,
            "head": { "ref": "keel/acme/deploy-redis-1700000000", "sha": "abc123def456" },
            "base": { "ref": "main", "sha": "000111222333" },
            "html_url": "http://forge.local/pulls/42",
            "created_at": "2026-11-14T10:00:00Z",
            "updated_at": "2026-11-14T10:00:00Z"
        }"#
        .to_string();

        let merged_json = r#"{
            "number": 42,
            "title": "Deploy Redis",
            "body": "Adds redis release",
            "state": "closed",
            "merged": true,
            "head": { "ref": "keel/acme/deploy-redis-1700000000", "sha": "abc123def456" },
            "base": { "ref": "main", "sha": "abc123def456" },
            "html_url": "http://forge.local/pulls/42",
            "created_at": "2026-11-14T10:00:00Z",
            "updated_at": "2026-11-14T10:05:00Z"
        }"#
        .to_string();

        let list_json = format!("[{open_json}]");

        // 1. ForgejoGiteaProvider full round-trip through `&dyn ForgeProvider`
        let (fg_url, fg_handle) = spawn_mock_http_server(vec![
            (201, open_json.clone()),
            (201, r#"{"id": 1}"#.to_string()),
            (200, list_json.clone()),
            (200, r#"{}"#.to_string()),
            (200, merged_json.clone()),
        ])
        .await;

        let fg_provider =
            ForgejoGiteaProvider::new(&fg_url, "keel", "gitops", "fg-token-xyz").unwrap();
        let dyn_forge: &dyn ForgeProvider = &fg_provider;

        let created = dyn_forge
            .create_pull_request(CreatePrRequest::new(
                "Deploy Redis",
                "Adds redis release",
                "keel/acme/deploy-redis-1700000000",
            ))
            .await
            .unwrap();
        assert_eq!(created.id, "42");
        assert_eq!(created.tenant.as_deref(), Some("acme"));
        assert_eq!(created.state, PullRequestState::Open);

        dyn_forge
            .post_preview_comment("42", "## Preview OK")
            .await
            .unwrap();

        let acme_prs = dyn_forge
            .list_open_pull_requests(Some("acme"))
            .await
            .unwrap();
        assert_eq!(acme_prs.len(), 1);

        let merged = dyn_forge
            .merge_pull_request("42", Some("abc1234def"))
            .await
            .unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);

        let fg_reqs = fg_handle.await.unwrap();
        assert!(
            fg_reqs
                .iter()
                .any(|r| r.contains(r#""head_commit_id":"abc1234def""#))
        );
        assert!(
            fg_reqs[0]
                .to_ascii_lowercase()
                .contains("authorization: token fg-token-xyz")
        );

        // 2. GitHubProvider round-trip + error mapping through `&dyn ForgeProvider`
        let (gh_url, gh_handle) = spawn_mock_http_server(vec![
            (201, open_json.clone()),
            (200, list_json.clone()),
            (200, r#"{"merged": true}"#.to_string()),
            (200, merged_json.clone()),
            (404, r#"{"message": "Not Found"}"#.to_string()),
            (
                409,
                r#"{"message": "Head branch was modified"}"#.to_string(),
            ),
        ])
        .await;

        let gh_provider = GitHubProvider::new(&gh_url, "keel", "gitops", "gh-token-xyz").unwrap();
        let dyn_gh: &dyn ForgeProvider = &gh_provider;

        let gh_pr = dyn_gh
            .create_pull_request(CreatePrRequest::new(
                "Deploy Redis",
                "Adds redis release",
                "keel/acme/deploy-redis-1700000000",
            ))
            .await
            .unwrap();
        assert_eq!(gh_pr.id, "42");
        assert!(
            dyn_gh
                .list_open_pull_requests(Some("other-tenant"))
                .await
                .unwrap()
                .is_empty()
        );
        let gh_merged = dyn_gh
            .merge_pull_request("42", Some("abc1234def"))
            .await
            .unwrap();
        assert_eq!(gh_merged.state, PullRequestState::Merged);
        assert!(matches!(
            dyn_gh.get_pull_request("404").await,
            Err(ForgeError::NotFound(_))
        ));
        assert!(matches!(
            dyn_gh.merge_pull_request("42", None).await,
            Err(ForgeError::MergeConflict(_, _))
        ));

        let gh_reqs = gh_handle.await.unwrap();
        assert!(gh_reqs.iter().any(|r| r.contains(r#""sha":"abc1234def""#)));
        assert!(
            gh_reqs[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer gh-token-xyz")
        );
    }

    #[tokio::test]
    async fn test_gitlab_provider_roundtrip_via_trait_object() {
        let mr_open = r#"{
            "iid": 7,
            "title": "Provision VPC",
            "description": "AWS VPC stack",
            "state": "opened",
            "source_branch": "keel/finops/provision-vpc-1700000099",
            "target_branch": "main",
            "sha": "999888777666",
            "web_url": "http://gitlab.local/org/repo/-/merge_requests/7",
            "created_at": "2026-11-14T10:00:00Z",
            "updated_at": "2026-11-14T10:00:00Z"
        }"#
        .to_string();

        let mr_merged = mr_open.replace(r#""state": "opened""#, r#""state": "merged""#);
        let mr_list = format!("[{mr_open}]");

        let (gl_url, gl_handle) = spawn_mock_http_server(vec![
            (201, mr_open.clone()),
            (200, mr_open.clone()),
            (201, r#"{"id": 10}"#.to_string()),
            (200, mr_list),
            (200, mr_merged),
        ])
        .await;

        let gl_provider = GitLabProvider::new(&gl_url, "org/repo", "gl-token-xyz").unwrap();
        let dyn_gl: &dyn ForgeProvider = &gl_provider;

        let created = dyn_gl
            .create_pull_request(CreatePrRequest::new(
                "Provision VPC",
                "AWS VPC stack",
                "keel/finops/provision-vpc-1700000099",
            ))
            .await
            .unwrap();
        assert_eq!(created.id, "7");
        assert_eq!(created.tenant.as_deref(), Some("finops"));

        let fetched = dyn_gl.get_pull_request("7").await.unwrap();
        assert_eq!(fetched.head_sha, "999888777666");

        dyn_gl
            .post_preview_comment("7", "### PKO Diff\n+ vpc")
            .await
            .unwrap();

        let listed = dyn_gl
            .list_open_pull_requests(Some("finops"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);

        let merged = dyn_gl
            .merge_pull_request("7", Some("999888777666"))
            .await
            .unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);

        let gl_reqs = gl_handle.await.unwrap();
        assert!(
            gl_reqs
                .iter()
                .any(|r| r.contains(r#""sha":"999888777666""#))
        );
        assert!(
            gl_reqs[0]
                .to_ascii_lowercase()
                .contains("private-token: gl-token-xyz")
        );
        assert!(gl_reqs[0].contains("/api/v4/projects/org%2Frepo/merge_requests"));

        assert!(dyn_gl.get_pull_request("not-a-number").await.is_err());
        assert!(dyn_gl.post_preview_comment("7", " ").await.is_err());
        assert!(
            dyn_gl
                .create_pull_request(CreatePrRequest::new(" ", "desc", "keel/a/b-1"))
                .await
                .is_err()
        );

        let mr_closed = r#"{
            "iid": 8,
            "title": "Closed MR",
            "description": null,
            "state": "closed",
            "source_branch": "keel/finops/old-1",
            "target_branch": "main",
            "sha": "111",
            "web_url": "http://gitlab.local/org/repo/-/merge_requests/8",
            "created_at": "2026-11-14T10:00:00Z",
            "updated_at": "2026-11-14T10:00:00Z"
        }"#
        .to_string();
        let (err_url, err_handle) = spawn_mock_http_server(vec![
            (200, mr_closed),
            (500, r#"{"error":"internal"}"#.to_string()),
            (201, r#"{"id":99}"#.to_string()),
        ])
        .await;
        let gl_err = GitLabProvider::new(&err_url, "org/repo", "tok").unwrap();
        let closed_info = gl_err.get_pull_request("8").await.unwrap();
        assert_eq!(closed_info.state, PullRequestState::Closed);
        assert!(matches!(
            gl_err.list_open_pull_requests(None).await,
            Err(ForgeError::Api { status: 500, .. })
        ));
        let gh_comment = GitHubProvider::new(&err_url, "org", "repo", "tok").unwrap();
        gh_comment
            .post_preview_comment("8", "Preview comment")
            .await
            .unwrap();
        err_handle.await.unwrap();
    }

    #[tokio::test]
    async fn remote_providers_reopen_pull_request_roundtrip() {
        let open_pr = r#"{
            "number": 12,
            "title": "Reopened PR",
            "body": null,
            "state": "open",
            "merged": false,
            "html_url": "http://forge.local/org/repo/pulls/12",
            "head": { "ref": "keel/acme/app-12", "sha": "abc123" },
            "base": { "ref": "main" },
            "created_at": "2026-11-14T10:00:00Z",
            "updated_at": "2026-11-14T10:05:00Z"
        }"#
        .to_string();

        // ForgejoGiteaProvider: PATCH /pulls/12 -> 200, GET /pulls/12 -> 200
        let (fg_url, fg_handle) =
            spawn_mock_http_server(vec![(200, open_pr.clone()), (200, open_pr.clone())]).await;
        let fg = ForgejoGiteaProvider::new(&fg_url, "org", "repo", "fg-tok").unwrap();
        let fg_reopened = fg.reopen_pull_request("12").await.unwrap();
        assert_eq!(fg_reopened.state, PullRequestState::Open);
        let fg_reqs = fg_handle.await.unwrap();
        assert!(fg_reqs[0].starts_with("PATCH /api/v1/repos/org/repo/pulls/12"));
        assert!(fg_reqs[0].contains(r#""state":"open""#));

        // GitHubProvider: PATCH /pulls/12 -> 200, GET /pulls/12 -> 200
        let (gh_url, gh_handle) =
            spawn_mock_http_server(vec![(200, open_pr.clone()), (200, open_pr)]).await;
        let gh = GitHubProvider::new(&gh_url, "org", "repo", "gh-tok").unwrap();
        let gh_reopened = gh.reopen_pull_request("12").await.unwrap();
        assert_eq!(gh_reopened.state, PullRequestState::Open);
        let gh_reqs = gh_handle.await.unwrap();
        assert!(gh_reqs[0].starts_with("PATCH /repos/org/repo/pulls/12"));
        assert!(gh_reqs[0].contains(r#""state":"open""#));

        // GitLabProvider: PUT /merge_requests/12 -> 200
        let open_mr = r#"{
            "iid": 12,
            "title": "Reopened MR",
            "description": null,
            "state": "opened",
            "source_branch": "keel/acme/app-12",
            "target_branch": "main",
            "sha": "abc123",
            "web_url": "http://gitlab.local/org/repo/-/merge_requests/12",
            "created_at": "2026-11-14T10:00:00Z",
            "updated_at": "2026-11-14T10:05:00Z"
        }"#
        .to_string();
        let (gl_url, gl_handle) = spawn_mock_http_server(vec![(200, open_mr)]).await;
        let gl = GitLabProvider::new(&gl_url, "org/repo", "gl-tok").unwrap();
        let gl_reopened = gl.reopen_pull_request("12").await.unwrap();
        assert_eq!(gl_reopened.state, PullRequestState::Open);
        let gl_reqs = gl_handle.await.unwrap();
        assert!(gl_reqs[0].starts_with("PUT /api/v4/projects/org%2Frepo/merge_requests/12"));
        assert!(gl_reqs[0].contains(r#""state_event":"reopen""#));
    }

    #[test]
    fn parse_remote_repo_url_supports_https_http_ssh_and_scp() {
        let gh = parse_remote_repo_url("https://github.com/acme/infra-gitops.git").unwrap();
        assert_eq!(gh.origin_url, "https://github.com");
        assert_eq!(gh.host, "github.com");
        assert_eq!(gh.owner, "acme");
        assert_eq!(gh.repo, "infra-gitops");
        assert_eq!(gh.project_path, "acme/infra-gitops");

        let gl = parse_remote_repo_url("https://gitlab.com/org/platform/infra.git").unwrap();
        assert_eq!(gl.origin_url, "https://gitlab.com");
        assert_eq!(gl.owner, "org/platform");
        assert_eq!(gl.repo, "infra");
        assert_eq!(gl.project_path, "org/platform/infra");

        let fg = parse_remote_repo_url(
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
        )
        .unwrap();
        assert_eq!(
            fg.origin_url,
            "http://forgejo-http.keel-system.svc.cluster.local:3000"
        );
        assert_eq!(fg.owner, "keel");
        assert_eq!(fg.repo, "infra-gitops");

        let scp = parse_remote_repo_url("git@github.com:acme/infra-gitops.git").unwrap();
        assert_eq!(scp.origin_url, "https://github.com");
        assert_eq!(scp.owner, "acme");
        assert_eq!(scp.repo, "infra-gitops");

        assert!(parse_remote_repo_url("/local/path/repo.git").is_err());
        assert!(parse_remote_repo_url("").is_err());
    }

    #[tokio::test]
    async fn forgejo_ensure_repository_existing_user_fallback_and_basic_auth() {
        // 1. Repository already exists.
        let (url, handle) = spawn_mock_http_server(vec![(200, "{}".to_string())]).await;
        let p = ForgejoGiteaProvider::new(&url, "keel", "infra-gitops", "fg-tok").unwrap();
        assert_eq!(
            p.ensure_repository(true).await.unwrap(),
            RepositoryEnsureOutcome::Existed
        );
        let reqs = handle.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "GET /api/v1/repos/keel/infra-gitops HTTP/1.1"
        );

        // 2. Missing; owner is the authenticated user (not an org) -> created via /user/repos.
        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"id":1,"login":"keel"}"#.to_string()),
            (201, r#"{"id":1}"#.to_string()),
        ])
        .await;
        let password = random_test_password();
        let p =
            ForgejoGiteaProvider::with_basic_auth(&url, "keel", "infra-gitops", "keel", &password)
                .unwrap();
        assert!(!format!("{p:?}").contains(&password));
        assert_eq!(
            p.ensure_repository(true).await.unwrap(),
            RepositoryEnsureOutcome::Created
        );
        let reqs = handle.await.unwrap();
        assert_eq!(request_line(&reqs[1]), "GET /api/v1/user HTTP/1.1");
        assert_eq!(request_line(&reqs[2]), "POST /api/v1/user/repos HTTP/1.1");
        assert!(reqs[2].contains(r#""private":true"#), "{}", reqs[2]);
        assert!(reqs[2].contains(r#""name":"infra-gitops""#));
        assert!(reqs[2].contains(r#""auto_init":false"#));
        let expected_auth = format!(
            "authorization: basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("keel:{password}"))
        );
        assert!(
            reqs[0]
                .to_ascii_lowercase()
                .contains(&expected_auth.to_ascii_lowercase())
        );

        // 3. Org owner; creation conflict (concurrent create) is reported as existing.
        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"login":"keel"}"#.to_string()),
            (200, r#"{"id":7,"username":"acme"}"#.to_string()),
            (409, r#"{"message":"exists"}"#.to_string()),
            (
                200,
                r#"{"name":"infra","owner":{"login":"acme"}}"#.to_string(),
            ),
        ])
        .await;
        let p = ForgejoGiteaProvider::new(&url, "acme", "infra", "fg-tok").unwrap();
        assert_eq!(
            p.ensure_repository(false).await.unwrap(),
            RepositoryEnsureOutcome::Existed
        );
        let reqs = handle.await.unwrap();
        assert_eq!(request_line(&reqs[2]), "GET /api/v1/orgs/acme HTTP/1.1");
        assert_eq!(
            request_line(&reqs[3]),
            "POST /api/v1/orgs/acme/repos HTTP/1.1"
        );
        assert!(reqs[3].contains(r#""private":false"#));

        // 4. Lookup failures (non-404) surface as API errors; a 403 create is not retried
        //    against another owner.
        let (url, handle) = spawn_mock_http_server(vec![(500, "boom".to_string())]).await;
        let p = ForgejoGiteaProvider::new(&url, "acme", "infra", "fg-tok").unwrap();
        let err = p.ensure_repository(true).await.unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 500, .. }), "{err}");
        handle.await.unwrap();

        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (
                403,
                r#"{"message":"token does not have required scope read:user"}"#.to_string(),
            ),
            (200, r#"{"id":7}"#.to_string()),
            (403, "forbidden".to_string()),
        ])
        .await;
        let p = ForgejoGiteaProvider::new(&url, "acme", "infra", "fg-tok").unwrap();
        let err = p.ensure_repository(true).await.unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 403, .. }), "{err}");
        assert_eq!(handle.await.unwrap().len(), 4);

        // 5. An owner that is neither an org nor the token owner is refused before any create.
        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"login":"keel"}"#.to_string()),
            (404, "{}".to_string()),
        ])
        .await;
        let p = ForgejoGiteaProvider::new(&url, "mallory", "infra", "fg-tok").unwrap();
        let err = p.ensure_repository(true).await.unwrap_err();
        assert!(matches!(err, ForgeError::InvalidInput(_)), "{err}");
        assert_eq!(handle.await.unwrap().len(), 3);

        let password = random_test_password();
        assert!(ForgejoGiteaProvider::with_basic_auth(&url, "o", "r", " ", &password).is_err());
        let empty_password = String::new();
        assert!(
            ForgejoGiteaProvider::with_basic_auth(&url, "o", "r", "u", &empty_password).is_err()
        );
    }

    #[tokio::test]
    async fn github_ensure_repository_creates_in_org_or_user() {
        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"login":"acme","type":"Organization"}"#.to_string()),
            (201, r#"{"id":1}"#.to_string()),
        ])
        .await;
        let p = GitHubProvider::new(&url, "acme", "infra", "gh-tok").unwrap();
        assert_eq!(
            p.ensure_repository(true).await.unwrap(),
            RepositoryEnsureOutcome::Created
        );
        let reqs = handle.await.unwrap();
        assert_eq!(request_line(&reqs[0]), "GET /repos/acme/infra HTTP/1.1");
        assert_eq!(request_line(&reqs[1]), "GET /users/acme HTTP/1.1");
        assert_eq!(request_line(&reqs[2]), "POST /orgs/acme/repos HTTP/1.1");
        assert!(
            reqs[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer gh-tok")
        );
        assert!(reqs[2].contains(r#""private":true"#));

        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"login":"octocat","type":"User"}"#.to_string()),
            (200, r#"{"login":"Octocat"}"#.to_string()),
            (201, r#"{"id":2}"#.to_string()),
        ])
        .await;
        let p = GitHubProvider::new(&url, "octocat", "infra", "gh-tok").unwrap();
        assert_eq!(
            p.ensure_repository(false).await.unwrap(),
            RepositoryEnsureOutcome::Created
        );
        let reqs = handle.await.unwrap();
        assert_eq!(request_line(&reqs[2]), "GET /user HTTP/1.1");
        assert_eq!(request_line(&reqs[3]), "POST /user/repos HTTP/1.1");

        // A 422 that is not "already exists" is an error.
        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"login":"octocat","type":"User"}"#.to_string()),
            (200, r#"{"login":"octocat"}"#.to_string()),
            (422, r#"{"message":"Validation Failed"}"#.to_string()),
        ])
        .await;
        let p = GitHubProvider::new(&url, "octocat", "infra", "gh-tok").unwrap();
        let err = p.ensure_repository(true).await.unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 422, .. }), "{err}");
        handle.await.unwrap();

        // Another user's account is never a create target.
        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"login":"someone","type":"User"}"#.to_string()),
            (200, r#"{"login":"octocat"}"#.to_string()),
        ])
        .await;
        let p = GitHubProvider::new(&url, "someone", "infra", "gh-tok").unwrap();
        let err = p.ensure_repository(true).await.unwrap_err();
        assert!(matches!(err, ForgeError::InvalidInput(_)), "{err}");
        assert_eq!(handle.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn gitlab_ensure_repository_resolves_namespace_and_visibility() {
        let (url, handle) = spawn_mock_http_server(vec![(200, r#"{"id":5}"#.to_string())]).await;
        let p = GitLabProvider::new(&url, "team/sub/infra", "gl-tok").unwrap();
        assert_eq!(
            p.ensure_repository(true).await.unwrap(),
            RepositoryEnsureOutcome::Existed
        );
        handle.await.unwrap();

        let (url, handle) = spawn_mock_http_server(vec![
            (404, "{}".to_string()),
            (200, r#"{"id":42,"kind":"group"}"#.to_string()),
            (201, r#"{"id":7}"#.to_string()),
        ])
        .await;
        let p = GitLabProvider::new(&url, "team/sub/infra", "gl-tok").unwrap();
        assert_eq!(
            p.ensure_repository(false).await.unwrap(),
            RepositoryEnsureOutcome::Created
        );
        let reqs = handle.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "GET /api/v4/projects/team%2Fsub%2Finfra HTTP/1.1"
        );
        assert_eq!(
            request_line(&reqs[1]),
            "GET /api/v4/namespaces/team%2Fsub HTTP/1.1"
        );
        assert_eq!(request_line(&reqs[2]), "POST /api/v4/projects HTTP/1.1");
        assert!(reqs[2].contains(r#""namespace_id":42"#), "{}", reqs[2]);
        assert!(reqs[2].contains(r#""visibility":"internal""#));
        assert!(reqs[2].contains(r#""path":"infra""#));
        assert!(
            reqs[2]
                .to_ascii_lowercase()
                .contains("private-token: gl-tok")
        );

        // Unknown namespace is an error (never silently created under another owner).
        let (url, handle) =
            spawn_mock_http_server(vec![(404, "{}".to_string()), (404, "{}".to_string())]).await;
        let p = GitLabProvider::new(&url, "ghost/infra", "gl-tok").unwrap();
        let err = p.ensure_repository(true).await.unwrap_err();
        assert!(
            matches!(err, ForgeError::NotFound(ref ns) if ns == "ghost"),
            "{err}"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn forgejo_create_access_token_requires_basic_auth() {
        let (url, handle) = spawn_mock_http_server(vec![
            (
                201,
                r#"{"id":3,"name":"keel-stage0","sha1":"abc123"}"#.to_string(),
            ),
            (201, r#"{"id":4}"#.to_string()),
            (401, r#"{"message":"bad creds"}"#.to_string()),
        ])
        .await;
        let password = random_test_password();
        let p =
            ForgejoGiteaProvider::with_basic_auth(&url, "keel", "infra-gitops", "keel", &password)
                .unwrap();
        let token = p
            .create_access_token("keel-stage0", &["read:repository"])
            .await
            .unwrap();
        assert_eq!(token, "abc123");
        assert!(p.create_access_token("missing", &[]).await.is_err());
        let err = p.create_access_token("denied", &[]).await.unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 401, .. }), "{err}");
        let reqs = handle.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "POST /api/v1/users/keel/tokens HTTP/1.1"
        );
        assert!(
            reqs[0].contains(r#""scopes":["read:repository"]"#),
            "{}",
            reqs[0]
        );

        let token_auth = ForgejoGiteaProvider::new(&url, "keel", "r", "tok").unwrap();
        assert!(matches!(
            token_auth.create_access_token("x", &[]).await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(p.create_access_token(" ", &[]).await.is_err());
    }
}
