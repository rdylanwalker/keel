//! Remote Git Forge REST adapters (`ForgejoGiteaProvider`, `GitHubProvider`, `GitLabProvider`)
//! implementing [`ForgeProvider`] via `reqwest::Client` (`SDD.md` §3.2).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

use crate::forge::{
    CreatePrRequest, ForgeError, ForgeProvider, PullRequestInfo, PullRequestState, matches_tenant,
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

fn validate_slug(value: &str, field: &str) -> Result<String, ForgeError> {
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

fn url_encode_project_path(path: &str) -> String {
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

async fn map_error_response(
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

/// Forgejo / Gitea REST v1 [`ForgeProvider`] adapter (`/api/v1/repos/{owner}/{repo}/...`).
#[derive(Clone)]
pub struct ForgejoGiteaProvider {
    client: Client,
    base_url: String,
    owner: String,
    repo: String,
    token: String,
}

impl fmt::Debug for ForgejoGiteaProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgejoGiteaProvider")
            .field("base_url", &self.base_url)
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .field("token", &"[REDACTED]")
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
        format!(
            "{}/api/v1/repos/{}/{}",
            self.base_url, self.owner, self.repo
        )
    }

    /// Builds an authenticated `reqwest::RequestBuilder` with Forgejo/Gitea token auth.
    pub fn authorized(&self, builder: RequestBuilder) -> RequestBuilder {
        builder
            .header("Authorization", format!("token {}", self.token))
            .header("Accept", "application/json")
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
}

impl GiteaGitHubPrResponse {
    fn into_pr_info(self, explicit_tenant: Option<String>) -> PullRequestInfo {
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
            url: self.html_url,
            created_at: self.created_at,
            updated_at: self.updated_at,
            comments: Vec::new(),
            diff_stat: None,
        }
    }
}

#[async_trait]
impl ForgeProvider for ForgejoGiteaProvider {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        if req.title.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "pull request title must not be empty".to_string(),
            ));
        }
        let url = format!("{}/pulls", self.repo_api_url());
        let payload = GiteaCreatePrBody {
            title: &req.title,
            body: &req.body,
            head: &req.source_branch,
            base: &req.target_branch,
        };
        let resp = self
            .authorized(self.client.post(&url))
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let parsed: GiteaGitHubPrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(req.tenant))
    }

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let url = format!("{}/pulls/{id}", self.repo_api_url());
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(id), false).await);
        }
        let parsed: GiteaGitHubPrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(None))
    }

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError> {
        let id = validate_pr_number(pr_id)?;
        if markdown_report.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "markdown_report must not be empty".to_string(),
            ));
        }
        let url = format!("{}/issues/{id}/comments", self.repo_api_url());
        let resp = self
            .authorized(self.client.post(&url))
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
            Some(PullRequestState::Open) => "open",
            Some(PullRequestState::Closed | PullRequestState::Merged) => "closed",
        };
        let url = format!("{}/pulls?state={state_param}&limit=50", self.repo_api_url());
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let items: Vec<GiteaGitHubPrResponse> = resp.json().await?;
        let prs = items
            .into_iter()
            .map(|item| item.into_pr_info(None))
            .filter(|pr| tenant.is_none_or(|t| matches_tenant(pr, t)))
            .filter(|pr| state.is_none_or(|s| pr.state == s))
            .collect();
        Ok(prs)
    }

    async fn merge_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let url = format!("{}/pulls/{id}/merge", self.repo_api_url());
        for attempt in 0..6 {
            let resp = self
                .authorized(self.client.post(&url))
                .json(&GiteaMergeBody {
                    merge_style: "merge",
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
        let id = validate_pr_number(pr_id)?;
        let url = format!("{}/pulls/{id}", self.repo_api_url());
        let resp = self
            .authorized(self.client.patch(&url))
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(id), false).await);
        }
        self.get_pull_request(id).await
    }
}

// ============================================================================
// GitHub Provider (REST API v3)
// ============================================================================

/// GitHub REST v3 [`ForgeProvider`] adapter (`/repos/{owner}/{repo}/...`).
#[derive(Clone)]
pub struct GitHubProvider {
    client: Client,
    base_url: String,
    owner: String,
    repo: String,
    token: String,
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
}

#[async_trait]
impl ForgeProvider for GitHubProvider {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        if req.title.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "pull request title must not be empty".to_string(),
            ));
        }
        let url = format!("{}/pulls", self.repo_api_url());
        let payload = GiteaCreatePrBody {
            title: &req.title,
            body: &req.body,
            head: &req.source_branch,
            base: &req.target_branch,
        };
        let resp = self
            .authorized(self.client.post(&url))
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let parsed: GiteaGitHubPrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(req.tenant))
    }

    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let url = format!("{}/pulls/{id}", self.repo_api_url());
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(id), false).await);
        }
        let parsed: GiteaGitHubPrResponse = resp.json().await?;
        Ok(parsed.into_pr_info(None))
    }

    async fn post_preview_comment(
        &self,
        pr_id: &str,
        markdown_report: &str,
    ) -> Result<(), ForgeError> {
        let id = validate_pr_number(pr_id)?;
        if markdown_report.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "markdown_report must not be empty".to_string(),
            ));
        }
        let url = format!("{}/issues/{id}/comments", self.repo_api_url());
        let resp = self
            .authorized(self.client.post(&url))
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
            Some(PullRequestState::Open) => "open",
            Some(PullRequestState::Closed | PullRequestState::Merged) => "closed",
        };
        let url = format!(
            "{}/pulls?state={state_param}&per_page=50",
            self.repo_api_url()
        );
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let items: Vec<GiteaGitHubPrResponse> = resp.json().await?;
        let prs = items
            .into_iter()
            .map(|item| item.into_pr_info(None))
            .filter(|pr| tenant.is_none_or(|t| matches_tenant(pr, t)))
            .filter(|pr| state.is_none_or(|s| pr.state == s))
            .collect();
        Ok(prs)
    }

    async fn merge_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let url = format!("{}/pulls/{id}/merge", self.repo_api_url());
        let resp = self
            .authorized(self.client.put(&url))
            .json(&GitHubMergeBody {
                merge_method: "rebase",
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(id), true).await);
        }
        self.get_pull_request(id).await
    }

    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let id = validate_pr_number(pr_id)?;
        let url = format!("{}/pulls/{id}", self.repo_api_url());
        let resp = self
            .authorized(self.client.patch(&url))
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some(id), false).await);
        }
        self.get_pull_request(id).await
    }
}

// ============================================================================
// GitLab Provider (REST API v4)
// ============================================================================

/// GitLab REST v4 [`ForgeProvider`] adapter (`/api/v4/projects/{encoded_project}/...`).
#[derive(Clone)]
pub struct GitLabProvider {
    client: Client,
    base_url: String,
    project_path: String,
    encoded_project: String,
    token: String,
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
        let url = format!(
            "{}/merge_requests?state={state_param}&per_page=50",
            self.project_api_url()
        );
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, None, false).await);
        }
        let items: Vec<GitLabMrResponse> = resp.json().await?;
        let prs = items
            .into_iter()
            .map(|item| item.into_pr_info(None))
            .filter(|pr| tenant.is_none_or(|t| matches_tenant(pr, t)))
            .collect();
        Ok(prs)
    }

    async fn merge_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        let iid = validate_pr_number(pr_id)?;
        let url = format!("{}/merge_requests/{iid}/merge", self.project_api_url());
        let resp = self.authorized(self.client.put(&url)).send().await?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawns a lightweight local HTTP server returning canned status/JSON responses sequentially.
    async fn spawn_mock_http_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let handle = tokio::spawn(async move {
            let mut captured_requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap();
                captured_requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());

                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    404 => "Not Found",
                    409 => "Conflict",
                    _ => "Error",
                };
                let http_resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(http_resp.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            captured_requests
        });

        (base_url, handle)
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

        let merged = dyn_forge.merge_pull_request("42").await.unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);

        let fg_reqs = fg_handle.await.unwrap();
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
        let gh_merged = dyn_gh.merge_pull_request("42").await.unwrap();
        assert_eq!(gh_merged.state, PullRequestState::Merged);
        assert!(matches!(
            dyn_gh.get_pull_request("404").await,
            Err(ForgeError::NotFound(_))
        ));
        assert!(matches!(
            dyn_gh.merge_pull_request("42").await,
            Err(ForgeError::MergeConflict(_, _))
        ));

        let gh_reqs = gh_handle.await.unwrap();
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

        let merged = dyn_gl.merge_pull_request("7").await.unwrap();
        assert_eq!(merged.state, PullRequestState::Merged);

        let gl_reqs = gl_handle.await.unwrap();
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
}
