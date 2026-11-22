//! Forgejo/Gitea organisation, service-user, team and repository-scoped token helpers.
//!
//! These are Forgejo-specific REST calls used by Stage 0 (`keel bootstrap init --with-forge`) to
//! lay the foundations for per-tenant repositories (`SDD.md` §6.2): the `keel-tenants`
//! organisation, a dedicated non-admin bot user that owns per-repository read tokens, and the
//! read-only team that gives the bot access to tenant repositories. They stay in `keel-git` so
//! every forge REST interaction remains behind the forge adapters (`AGENTS.md` §3.5).
//!
//! Every `ensure_*` call is idempotent: it looks the object up first, creates it when missing
//! and re-applies the hardened settings when it already exists. Objects that grant access to
//! tenant repositories are never adopted blindly: a pre-existing service user must carry the id
//! Keel recorded when it created it, the organisation's `Owners` team must contain exactly the
//! expected accounts, and unmanaged teams must not reach any repository.

use std::collections::BTreeSet;
use std::fmt;

use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub use crate::forge::RepositoryTarget;

use crate::forge::ForgeError;
use crate::http::{PageCap, delete_ignoring_missing, fetch_pages};
use crate::remote::{ForgejoAuth, ForgejoGiteaProvider, map_error_response, validate_slug};

/// Result of an idempotent `ensure_*` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The object did not exist and was created.
    Created,
    /// The object already existed (its managed settings were re-applied where applicable).
    Existed,
}

impl ProvisionOutcome {
    /// Lower-case label for bootstrap notes (`created` / `already present`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Existed => "already present",
        }
    }
}

/// Name of the built-in Forgejo/Gitea team whose members own an organisation.
pub const ORG_OWNERS_TEAM: &str = "Owners";

/// A dedicated, non-admin Forgejo service account (`POST /api/v1/admin/users`).
pub struct ServiceUserSpec<'a> {
    /// Login name.
    pub username: &'a str,
    /// Required by Forgejo; never used for mail delivery. Use an address no identity provider can
    /// issue (for example under the reserved `.invalid` TLD) so SSO account linking by email can
    /// never sign in as the service user.
    pub email: &'a str,
    /// Display name.
    pub full_name: &'a str,
    /// Password (applied on create and re-applied on every run so it matches its Secret).
    pub password: &'a str,
    /// Forgejo id Keel recorded when it created the account. An existing account is adopted only
    /// when its id matches; `None` refuses every existing account (fail closed), because someone
    /// else may have registered the name first.
    pub adopt_id: Option<i64>,
}

impl fmt::Debug for ServiceUserSpec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceUserSpec")
            .field("username", &self.username)
            .field("email", &self.email)
            .field("full_name", &self.full_name)
            .field("password", &"[REDACTED]")
            .field("adopt_id", &self.adopt_id)
            .finish()
    }
}

/// Result of [`ForgejoGiteaProvider::ensure_service_user`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceUser {
    /// Forgejo user id (record it so later runs can adopt the account).
    pub id: i64,
    /// Whether the account was created by this call.
    pub outcome: ProvisionOutcome,
}

/// An organisation team (`POST /api/v1/orgs/{org}/teams`).
#[derive(Debug, Clone)]
pub struct TeamSpec<'a> {
    /// Team name (unique within the organisation).
    pub name: &'a str,
    /// Human-readable description.
    pub description: &'a str,
    /// Base permission: `read`, `write` or `admin`.
    pub permission: &'a str,
    /// Per-unit access (`repo.code` -> `read`, ...). Units not listed get no access.
    pub units: &'a [(&'a str, &'a str)],
    /// Whether the team automatically covers every current and future organisation repository.
    pub includes_all_repositories: bool,
    /// Whether members may create repositories in the organisation.
    pub can_create_org_repo: bool,
}

impl TeamSpec<'_> {
    fn body(&self) -> Value {
        let units: Vec<&str> = self.units.iter().map(|(unit, _)| *unit).collect();
        let units_map: serde_json::Map<String, Value> = self
            .units
            .iter()
            .map(|(unit, access)| ((*unit).to_string(), Value::from(*access)))
            .collect();
        json!({
            "name": self.name,
            "description": self.description,
            "permission": self.permission,
            "units": units,
            "units_map": units_map,
            "includes_all_repositories": self.includes_all_repositories,
            "can_create_org_repo": self.can_create_org_repo,
        })
    }
}

/// An organisation team as listed by `GET /api/v1/orgs/{org}/teams`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TeamInfo {
    /// Team id.
    pub id: i64,
    /// Team name.
    pub name: String,
    /// Base permission (`none`, `read`, `write`, `admin`, `owner`).
    #[serde(default)]
    pub permission: String,
    /// Whether the team covers every current and future organisation repository.
    #[serde(default)]
    pub includes_all_repositories: bool,
}

/// A freshly minted access token. `Debug` never prints the secret.
#[derive(Clone)]
pub struct MintedAccessToken {
    /// Forgejo token id (for later revocation through [`ForgejoGiteaProvider::delete_access_token`]).
    pub id: i64,
    /// Token secret (`sha1` in the API response). Only returned once by Forgejo.
    pub token: String,
}

impl fmt::Debug for MintedAccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintedAccessToken")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Metadata of an existing access token (`GET /api/v1/users/{u}/tokens`). Never the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessTokenInfo {
    /// Token id.
    pub id: i64,
    /// Token name.
    pub name: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Lower-case `owner/name` of the repositories the token is restricted to; `None` when the
    /// token is not repository-restricted (or the forge does not report a restriction).
    pub repositories: Option<Vec<String>>,
}

/// Credentials removed from a service user by
/// [`ForgejoGiteaProvider::remove_stray_user_credentials`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrayCredentialReport {
    /// Names of the deleted access tokens.
    pub tokens: Vec<String>,
    /// Number of deleted SSH public keys.
    pub ssh_keys: usize,
    /// Number of deleted GPG keys.
    pub gpg_keys: usize,
    /// Number of deleted OAuth2 applications.
    pub oauth2_apps: usize,
}

impl StrayCredentialReport {
    /// `true` when nothing had to be removed.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.ssh_keys == 0 && self.gpg_keys == 0 && self.oauth2_apps == 0
    }
}

#[derive(Deserialize)]
struct OrgResponse {
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    repo_admin_change_team_access: Option<bool>,
}

#[derive(Deserialize)]
struct RepoMeta {
    #[serde(default)]
    owner: Option<Value>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    full_name: Option<String>,
}

impl RepoMeta {
    /// Lower-case `owner/name`. `owner` is a login string in token responses and a user object
    /// in repository listings.
    fn full_name(self) -> Option<String> {
        let full = self.full_name.or_else(|| {
            let owner = match self.owner? {
                Value::String(s) => s,
                Value::Object(o) => o.get("login")?.as_str()?.to_string(),
                _ => return None,
            };
            Some(format!("{owner}/{}", self.name?))
        })?;
        Some(full.to_ascii_lowercase())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    repositories: Option<Vec<RepoMeta>>,
}

impl TokenResponse {
    fn granted_repositories(repos: Option<Vec<RepoMeta>>) -> Option<BTreeSet<String>> {
        repos.map(|r| r.into_iter().filter_map(RepoMeta::full_name).collect())
    }
}

#[derive(Deserialize)]
struct UserResponse {
    #[serde(default)]
    id: i64,
    login: String,
}

#[derive(Deserialize)]
struct IdOnly {
    id: i64,
}

impl ForgejoGiteaProvider {
    pub(crate) fn basic_auth_username(&self, action: &str) -> Result<String, ForgeError> {
        match &self.auth {
            ForgejoAuth::Basic { username, .. } => validate_slug(username, "username"),
            ForgejoAuth::Token(_) => Err(ForgeError::InvalidInput(format!(
                "{action} requires Forgejo/Gitea basic auth"
            ))),
        }
    }

    /// `GET`s every page of a list endpoint; fails closed instead of truncating.
    pub(crate) async fn get_all<T: DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<Vec<T>, ForgeError> {
        fetch_pages(
            &self.client,
            &self.api(path),
            "limit",
            |b| self.authorized(b),
            what,
            PageCap::FailClosed,
        )
        .await
    }

    /// `DELETE`, treating 404 as already deleted.
    async fn delete_path(&self, path: &str, what: &str) -> Result<(), ForgeError> {
        delete_ignoring_missing(&self.client, &self.api(path), |b| self.authorized(b), what).await
    }

    /// Validates a token-minting request: requires basic auth (Forgejo/Gitea only mint tokens over
    /// basic auth) and a non-empty `name`. Returns the basic-auth username.
    fn token_minting_user(&self, action: &str, name: &str) -> Result<String, ForgeError> {
        let user = self.basic_auth_username(action)?;
        if name.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "access token name must not be empty".to_string(),
            ));
        }
        Ok(user)
    }

    /// `POST /api/v1/users/{user}/tokens` with `body` plus `name`. Returns the response JSON.
    async fn post_user_token(
        &self,
        user: &str,
        name: &str,
        mut body: Value,
    ) -> Result<Value, ForgeError> {
        body["name"] = Value::from(name);
        let resp = self
            .authorized(self.client.post(self.api(&format!("/users/{user}/tokens"))))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("access token"), false).await);
        }
        Ok(resp.json().await?)
    }

    /// Mints a personal access token for the basic-auth user (`POST /api/v1/users/{user}/tokens`).
    /// Forgejo/Gitea only accept basic auth on this endpoint, so token-authenticated providers
    /// return [`ForgeError::InvalidInput`].
    pub async fn create_access_token(
        &self,
        name: &str,
        scopes: &[&str],
    ) -> Result<String, ForgeError> {
        let user = self.token_minting_user("minting a Forgejo/Gitea access token", name)?;
        let body = self
            .post_user_token(&user, name, json!({ "scopes": scopes }))
            .await?;
        body.get("sha1")
            .or_else(|| body.get("token"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Api {
                status: 200,
                message: "Forgejo access token response missing token field".to_string(),
            })
    }

    /// Returns the login of the authenticated account (`GET /api/v1/user`). Used to verify that
    /// stored credentials still work.
    pub async fn authenticated_login(&self) -> Result<String, ForgeError> {
        let resp = self
            .authorized(self.client.get(self.api("/user")))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("user"), false).await);
        }
        Ok(resp.json::<UserResponse>().await?.login)
    }

    /// Ensures organisation `org` exists with `visibility` (`public`, `limited` or `private`),
    /// `description` and `repo_admin_change_team_access: false`.
    ///
    /// Creation (`POST /api/v1/orgs`) makes the authenticated user the organisation owner. An
    /// existing organisation whose managed settings drifted is patched back. This does **not**
    /// prove who controls an existing organisation: call
    /// [`ForgejoGiteaProvider::verify_organization_control`] before trusting it.
    pub async fn ensure_organization(
        &self,
        org: &str,
        visibility: &str,
        description: &str,
    ) -> Result<ProvisionOutcome, ForgeError> {
        let org = validate_slug(org, "organization")?;
        if !matches!(visibility, "public" | "limited" | "private") {
            return Err(ForgeError::InvalidInput(format!(
                "invalid organization visibility '{visibility}'"
            )));
        }
        if self
            .reconcile_org_settings(&org, visibility, description)
            .await?
        {
            return Ok(ProvisionOutcome::Existed);
        }
        let resp = self
            .authorized(self.client.post(self.api("/orgs")))
            .json(&json!({
                "username": org,
                "visibility": visibility,
                "description": description,
                "repo_admin_change_team_access": false,
            }))
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(ProvisionOutcome::Created);
        }
        // 422: created concurrently, or the name belongs to a user. Only an organisation counts.
        if resp.status() == StatusCode::UNPROCESSABLE_ENTITY {
            let err = map_error_response(resp, Some("organization"), false).await;
            if self
                .reconcile_org_settings(&org, visibility, description)
                .await?
            {
                return Ok(ProvisionOutcome::Existed);
            }
            return Err(err);
        }
        Err(map_error_response(resp, Some("organization"), false).await)
    }

    /// `Ok(true)` when `org` exists (managed settings corrected if needed), `Ok(false)` on 404.
    ///
    /// The `PATCH` always carries every managed field: Forgejo's org edit handler writes omitted
    /// text fields as empty strings.
    async fn reconcile_org_settings(
        &self,
        org: &str,
        visibility: &str,
        description: &str,
    ) -> Result<bool, ForgeError> {
        let url = self.api(&format!("/orgs/{org}"));
        let resp = self.authorized(self.client.get(&url)).send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("organization"), false).await);
        }
        let current: OrgResponse = resp.json().await?;
        let drifted = current.visibility.as_deref() != Some(visibility)
            || current.description.as_deref().unwrap_or_default() != description
            || current.repo_admin_change_team_access != Some(false);
        if drifted {
            let resp = self
                .authorized(self.client.patch(&url))
                .json(&json!({
                    "visibility": visibility,
                    "description": description,
                    "repo_admin_change_team_access": false,
                }))
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(map_error_response(resp, Some("organization"), false).await);
            }
        }
        Ok(true)
    }

    /// Fails closed unless `org` is controlled only by Keel:
    ///
    /// - the `ORG_OWNERS_TEAM` team's members are exactly `owners` (case-insensitive);
    /// - every other team not named in `managed_teams` has neither
    ///   `includes_all_repositories` nor access to any repository.
    ///
    /// An organisation squatted before Stage 0 created it (for example by an SSO user) therefore
    /// stops the bootstrap instead of silently receiving every tenant repository.
    pub async fn verify_organization_control(
        &self,
        org: &str,
        owners: &[&str],
        managed_teams: &[&str],
    ) -> Result<(), ForgeError> {
        let org = validate_slug(org, "organization")?;
        let teams = self.list_org_teams(&org).await?;
        let owners_team = teams
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(ORG_OWNERS_TEAM))
            .ok_or_else(|| ForgeError::Api {
                status: 409,
                message: format!("organization `{org}` has no `{ORG_OWNERS_TEAM}` team"),
            })?;
        let want: BTreeSet<String> = owners.iter().map(|o| o.to_ascii_lowercase()).collect();
        let have: BTreeSet<String> = self
            .list_team_members(owners_team.id)
            .await?
            .into_iter()
            .map(|m| m.to_ascii_lowercase())
            .collect();
        if have != want {
            return Err(ForgeError::Api {
                status: 409,
                message: format!(
                    "organization `{org}` is owned by [{}], expected exactly [{}]; it was not created by Keel or its owners were changed. Remove the extra owners (or delete the organization) in Forgejo and re-run",
                    have.into_iter().collect::<Vec<_>>().join(", "),
                    want.into_iter().collect::<Vec<_>>().join(", ")
                ),
            });
        }
        for team in &teams {
            if team.id == owners_team.id
                || managed_teams
                    .iter()
                    .any(|m| team.name.eq_ignore_ascii_case(m))
            {
                continue;
            }
            let repos = if team.includes_all_repositories {
                Vec::new()
            } else {
                self.list_team_repositories(team.id).await?
            };
            if team.includes_all_repositories || !repos.is_empty() {
                return Err(ForgeError::Api {
                    status: 409,
                    message: format!(
                        "team `{}` in organization `{org}` is not managed by Keel but has access to {}; delete it or remove its repositories in Forgejo and re-run",
                        team.name,
                        if team.includes_all_repositories {
                            "every repository".to_string()
                        } else {
                            repos.join(", ")
                        }
                    ),
                });
            }
        }
        Ok(())
    }

    /// Lists the teams of `org`.
    pub async fn list_org_teams(&self, org: &str) -> Result<Vec<TeamInfo>, ForgeError> {
        let org = validate_slug(org, "organization")?;
        self.get_all(&format!("/orgs/{org}/teams"), "organization teams")
            .await
    }

    /// Lists the logins of team `team_id`'s members.
    pub async fn list_team_members(&self, team_id: i64) -> Result<Vec<String>, ForgeError> {
        let users: Vec<UserResponse> = self
            .get_all(&format!("/teams/{team_id}/members"), "team members")
            .await?;
        Ok(users.into_iter().map(|u| u.login).collect())
    }

    /// Lists the lower-case `owner/name` of the repositories team `team_id` can access.
    pub async fn list_team_repositories(&self, team_id: i64) -> Result<Vec<String>, ForgeError> {
        let repos: Vec<RepoMeta> = self
            .get_all(&format!("/teams/{team_id}/repos"), "team repositories")
            .await?;
        Ok(repos.into_iter().filter_map(RepoMeta::full_name).collect())
    }

    /// Ensures a restricted, non-admin service user exists (site-admin credentials required).
    ///
    /// An existing account is adopted only when its id equals `spec.adopt_id`; otherwise the call
    /// fails before changing anything. After the lookup/creation the user is always patched
    /// (`PATCH /api/v1/admin/users/{u}`) to: not admin, restricted (sees only
    /// organisations/repositories it is explicitly given), no repository or organisation creation,
    /// no git hooks or local imports, private visibility, no forced password change,
    /// `spec.email` and `spec.password` (so re-runs heal drift and the password always matches
    /// the Secret it was read from).
    pub async fn ensure_service_user(
        &self,
        spec: &ServiceUserSpec<'_>,
    ) -> Result<ServiceUser, ForgeError> {
        let username = validate_slug(spec.username, "username")?;
        if spec.password.is_empty() || spec.email.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "service user email and password must not be empty".to_string(),
            ));
        }
        let user = match self.lookup_user(&username).await? {
            Some(id) => ServiceUser {
                id: Self::adopt(&username, id, spec.adopt_id)?,
                outcome: ProvisionOutcome::Existed,
            },
            None => {
                let resp = self
                    .authorized(self.client.post(self.api("/admin/users")))
                    .json(&json!({
                        "username": username,
                        "login_name": username,
                        "source_id": 0,
                        "email": spec.email,
                        "full_name": spec.full_name,
                        "password": spec.password,
                        "must_change_password": false,
                        "restricted": true,
                        "visibility": "private",
                        "send_notify": false,
                    }))
                    .send()
                    .await?;
                if resp.status().is_success() {
                    ServiceUser {
                        id: resp.json::<IdOnly>().await?.id,
                        outcome: ProvisionOutcome::Created,
                    }
                } else if resp.status() == StatusCode::UNPROCESSABLE_ENTITY {
                    // 422 also covers a rejected password, a taken email or a reserved name:
                    // only a concurrently created account counts as existing.
                    let err = map_error_response(resp, Some("user"), false).await;
                    match self.lookup_user(&username).await? {
                        Some(id) => ServiceUser {
                            id: Self::adopt(&username, id, spec.adopt_id)?,
                            outcome: ProvisionOutcome::Existed,
                        },
                        None => return Err(err),
                    }
                } else {
                    return Err(map_error_response(resp, Some("user"), false).await);
                }
            }
        };
        let resp = self
            .authorized(
                self.client
                    .patch(self.api(&format!("/admin/users/{username}"))),
            )
            .json(&json!({
                "login_name": username,
                "source_id": 0,
                "email": spec.email,
                "password": spec.password,
                "must_change_password": false,
                "admin": false,
                "restricted": true,
                "active": true,
                "prohibit_login": false,
                "max_repo_creation": 0,
                "allow_create_organization": false,
                "allow_git_hook": false,
                "allow_import_local": false,
                "visibility": "private",
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("user"), false).await);
        }
        Ok(user)
    }

    /// `Some(id)` when `username` exists, `None` on 404.
    async fn lookup_user(&self, username: &str) -> Result<Option<i64>, ForgeError> {
        let resp = self
            .authorized(self.client.get(self.api(&format!("/users/{username}"))))
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("user"), false).await);
        }
        Ok(Some(resp.json::<UserResponse>().await?.id))
    }

    fn adopt(username: &str, id: i64, adopt_id: Option<i64>) -> Result<i64, ForgeError> {
        if id > 0 && adopt_id == Some(id) {
            return Ok(id);
        }
        Err(ForgeError::Api {
            status: 409,
            message: format!(
                "Forgejo user `{username}` (id {id}) already exists but Keel has no record of creating it; refusing to adopt an account someone else may control. Delete it in Forgejo (Site administration -> User accounts) and re-run"
            ),
        })
    }

    /// Ensures team `spec.name` exists in `org` with exactly `spec`'s settings, returning its id.
    /// An existing team is patched (`PATCH /api/v1/teams/{id}`) so permission drift is reverted.
    pub async fn ensure_team(
        &self,
        org: &str,
        spec: &TeamSpec<'_>,
    ) -> Result<(i64, ProvisionOutcome), ForgeError> {
        let org = validate_slug(org, "organization")?;
        validate_slug(spec.name, "team")?;
        if !matches!(spec.permission, "read" | "write" | "admin") {
            return Err(ForgeError::InvalidInput(format!(
                "invalid team permission '{}'",
                spec.permission
            )));
        }
        let existing = self
            .list_org_teams(&org)
            .await?
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(spec.name));
        if let Some(team) = existing {
            let resp = self
                .authorized(self.client.patch(self.api(&format!("/teams/{}", team.id))))
                .json(&spec.body())
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(map_error_response(resp, Some("team"), false).await);
            }
            return Ok((team.id, ProvisionOutcome::Existed));
        }
        let resp = self
            .authorized(self.client.post(self.api(&format!("/orgs/{org}/teams"))))
            .json(&spec.body())
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("team"), false).await);
        }
        let team: IdOnly = resp.json().await?;
        Ok((team.id, ProvisionOutcome::Created))
    }

    /// Adds `username` to team `team_id` (`PUT /api/v1/teams/{id}/members/{username}`). Adding
    /// an existing member is a no-op on the forge.
    pub async fn ensure_team_member(&self, team_id: i64, username: &str) -> Result<(), ForgeError> {
        let username = validate_slug(username, "username")?;
        let resp = self
            .authorized(
                self.client
                    .put(self.api(&format!("/teams/{team_id}/members/{username}"))),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(map_error_response(resp, Some("team member"), false).await);
        }
        Ok(())
    }

    /// Makes `members` the exact membership of team `team_id`: adds missing members and removes
    /// every other account (`DELETE /api/v1/teams/{id}/members/{u}`). Returns the removed logins.
    pub async fn ensure_team_members_exactly(
        &self,
        team_id: i64,
        members: &[&str],
    ) -> Result<Vec<String>, ForgeError> {
        let mut removed = Vec::new();
        for current in self.list_team_members(team_id).await? {
            if members.iter().any(|m| m.eq_ignore_ascii_case(&current)) {
                continue;
            }
            let login = validate_slug(&current, "username")?;
            self.delete_path(&format!("/teams/{team_id}/members/{login}"), "team member")
                .await?;
            removed.push(login);
        }
        for member in members {
            self.ensure_team_member(team_id, member).await?;
        }
        Ok(removed)
    }

    /// Mints an access token for the basic-auth user that only works on `repositories`
    /// (`POST /api/v1/users/{user}/tokens` with `repositories: [{owner, name}]`; repository-specific
    /// access tokens need Forgejo 15 or later).
    ///
    /// Fails closed: when the forge does not echo back exactly the requested scopes and repository set
    /// (for example a Gitea or Forgejo before 15 that silently ignores `repositories` and would
    /// mint an account-wide token), the token is deleted and an error is returned.
    pub async fn create_repository_scoped_token(
        &self,
        name: &str,
        scopes: &[&str],
        repositories: &[RepositoryTarget],
    ) -> Result<MintedAccessToken, ForgeError> {
        let user = self.token_minting_user("minting a repository-scoped token", name)?;
        if repositories.is_empty() {
            return Err(ForgeError::InvalidInput(
                "a repository-scoped token needs at least one repository".to_string(),
            ));
        }
        let targets: Vec<Value> = repositories
            .iter()
            .map(|r| json!({ "owner": r.owner, "name": r.name }))
            .collect();
        let body: TokenResponse = serde_json::from_value(
            self.post_user_token(
                &user,
                name,
                json!({ "scopes": scopes, "repositories": targets }),
            )
            .await?,
        )?;
        let token = body.sha1.filter(|s| !s.is_empty());
        let wanted: BTreeSet<String> = repositories
            .iter()
            .map(|r| format!("{}/{}", r.owner, r.name).to_ascii_lowercase())
            .collect();
        let granted = TokenResponse::granted_repositories(body.repositories);
        let wanted_scopes: BTreeSet<&str> = scopes.iter().copied().collect();
        let granted_scopes: Option<BTreeSet<&str>> = body
            .scopes
            .as_ref()
            .map(|s| s.iter().map(String::as_str).collect());
        let scopes_ok = granted_scopes.as_ref() == Some(&wanted_scopes);
        match (token, granted) {
            (Some(token), Some(granted)) if granted == wanted && scopes_ok => {
                Ok(MintedAccessToken { id: body.id, token })
            }
            (_, granted) => {
                if body.id > 0
                    && let Err(err) = self.delete_access_token(body.id).await
                {
                    tracing::warn!(error = %err, token_id = body.id, "Could not delete a Forgejo token that is not restricted to the requested repositories; delete it manually");
                }
                Err(ForgeError::Api {
                    status: 200,
                    message: format!(
                        "Forgejo did not restrict the access token to the requested scopes and repositories (scopes: {}; repositories: {}); repository-specific access tokens require Forgejo 15 or later",
                        granted_scopes
                            .map(|g| g.into_iter().collect::<Vec<_>>().join(","))
                            .unwrap_or_else(|| "not reported".to_string()),
                        granted
                            .map(|g| g.into_iter().collect::<Vec<_>>().join(","))
                            .unwrap_or_else(|| "all repositories".to_string())
                    ),
                })
            }
        }
    }

    /// Lists the basic-auth user's access tokens (`GET /api/v1/users/{u}/tokens`).
    pub async fn list_access_tokens(&self) -> Result<Vec<AccessTokenInfo>, ForgeError> {
        let user = self.basic_auth_username("listing access tokens")?;
        let tokens: Vec<TokenResponse> = self
            .get_all(&format!("/users/{user}/tokens"), "access tokens")
            .await?;
        Ok(tokens
            .into_iter()
            .map(|t| AccessTokenInfo {
                id: t.id,
                name: t.name,
                scopes: t.scopes.unwrap_or_default(),
                repositories: TokenResponse::granted_repositories(t.repositories)
                    .filter(|r| !r.is_empty())
                    .map(|r| r.into_iter().collect()),
            })
            .collect())
    }

    /// Deletes an access token of the basic-auth user (`DELETE /api/v1/users/{u}/tokens/{id}`).
    /// A missing token (404) counts as deleted.
    pub async fn delete_access_token(&self, id: i64) -> Result<(), ForgeError> {
        let user = self.basic_auth_username("deleting an access token")?;
        self.delete_path(&format!("/users/{user}/tokens/{id}"), "access token")
            .await
    }

    /// Removes every credential of the basic-auth user that Keel did not mint: access tokens for
    /// which `keep_token` is false, and **all** SSH keys, GPG keys and OAuth2 applications (a
    /// service user that only owns repository-scoped HTTPS tokens needs none of them).
    pub async fn remove_stray_user_credentials(
        &self,
        keep_token: impl Fn(&AccessTokenInfo) -> bool,
    ) -> Result<StrayCredentialReport, ForgeError> {
        self.basic_auth_username("removing service user credentials")?;
        let mut report = StrayCredentialReport::default();
        for token in self.list_access_tokens().await? {
            if !keep_token(&token) {
                self.delete_access_token(token.id).await?;
                report.tokens.push(token.name);
            }
        }
        for (list, what, counter) in [
            ("/user/keys", "SSH key", &mut report.ssh_keys),
            ("/user/gpg_keys", "GPG key", &mut report.gpg_keys),
            (
                "/user/applications/oauth2",
                "OAuth2 application",
                &mut report.oauth2_apps,
            ),
        ] {
            let items: Vec<IdOnly> = self.get_all(list, what).await?;
            for item in items {
                self.delete_path(&format!("{list}/{}", item.id), what)
                    .await?;
                *counter += 1;
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{request_body, request_line, spawn_mock_http_server};

    fn password() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    fn admin(url: &str, pw: &str) -> ForgejoGiteaProvider {
        ForgejoGiteaProvider::with_basic_auth(url, "keel", "infra-gitops", "keel", pw).unwrap()
    }

    #[tokio::test]
    async fn ensure_organization_creates_patches_and_tolerates_races() {
        let pw = password();
        // Missing -> created private.
        let (url, h) =
            spawn_mock_http_server(vec![(404, "{}".into()), (201, r#"{"id":7}"#.into())]).await;
        let out = admin(&url, &pw)
            .ensure_organization("keel-tenants", "private", "Tenant repos")
            .await
            .unwrap();
        assert_eq!(out, ProvisionOutcome::Created);
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "GET /api/v1/orgs/keel-tenants HTTP/1.1"
        );
        assert_eq!(request_line(&reqs[1]), "POST /api/v1/orgs HTTP/1.1");
        let body = request_body(&reqs[1]);
        assert_eq!(body["username"], "keel-tenants");
        assert_eq!(body["visibility"], "private");
        assert_eq!(body["repo_admin_change_team_access"], false);

        // Existing but public -> patched back with every managed field, so Forgejo's edit
        // handler never blanks the description.
        let (url, h) = spawn_mock_http_server(vec![
            (
                200,
                r#"{"visibility":"public","description":"Tenant repos","repo_admin_change_team_access":false}"#.into(),
            ),
            (200, "{}".into()),
        ])
        .await;
        let out = admin(&url, &pw)
            .ensure_organization("keel-tenants", "private", "Tenant repos")
            .await
            .unwrap();
        assert_eq!(out, ProvisionOutcome::Existed);
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[1]),
            "PATCH /api/v1/orgs/keel-tenants HTTP/1.1"
        );
        assert_eq!(
            request_body(&reqs[1]),
            json!({
                "visibility": "private",
                "description": "Tenant repos",
                "repo_admin_change_team_access": false,
            })
        );

        // Description or team-access drift alone is also repaired.
        let (url, h) = spawn_mock_http_server(vec![
            (
                200,
                r#"{"visibility":"private","description":"","repo_admin_change_team_access":true}"#
                    .into(),
            ),
            (200, "{}".into()),
        ])
        .await;
        admin(&url, &pw)
            .ensure_organization("keel-tenants", "private", "Tenant repos")
            .await
            .unwrap();
        let reqs = h.await.unwrap();
        assert_eq!(request_body(&reqs[1])["description"], "Tenant repos");
        assert_eq!(
            request_body(&reqs[1])["repo_admin_change_team_access"],
            false
        );

        // Existing and in sync -> single GET.
        let (url, h) = spawn_mock_http_server(vec![(
            200,
            r#"{"visibility":"private","description":"","repo_admin_change_team_access":false}"#
                .into(),
        )])
        .await;
        assert_eq!(
            admin(&url, &pw)
                .ensure_organization("keel-tenants", "private", "")
                .await
                .unwrap(),
            ProvisionOutcome::Existed
        );
        assert_eq!(h.await.unwrap().len(), 1);

        // Concurrent creation (422) -> re-checked as an organisation.
        let (url, h) = spawn_mock_http_server(vec![
            (404, "{}".into()),
            (422, r#"{"message":"exists"}"#.into()),
            (
                200,
                r#"{"visibility":"private","description":"","repo_admin_change_team_access":false}"#.into(),
            ),
        ])
        .await;
        assert_eq!(
            admin(&url, &pw)
                .ensure_organization("keel-tenants", "private", "")
                .await
                .unwrap(),
            ProvisionOutcome::Existed
        );
        h.await.unwrap();

        // 422 because a *user* holds the name -> error, never treated as an organisation.
        let (url, h) = spawn_mock_http_server(vec![
            (404, "{}".into()),
            (422, r#"{"message":"user already exists"}"#.into()),
            (404, "{}".into()),
        ])
        .await;
        let err = admin(&url, &pw)
            .ensure_organization("keel-tenants", "private", "")
            .await
            .unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 422, .. }), "{err}");
        h.await.unwrap();

        // Missing write:organization scope (403) surfaces.
        let (url, h) =
            spawn_mock_http_server(vec![(404, "{}".into()), (403, "scope".into())]).await;
        let err = admin(&url, &pw)
            .ensure_organization("keel-tenants", "private", "")
            .await
            .unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 403, .. }), "{err}");
        h.await.unwrap();

        assert!(
            admin(&url, &pw)
                .ensure_organization("keel-tenants", "secret", "")
                .await
                .is_err()
        );
        assert!(
            admin(&url, &pw)
                .ensure_organization("../x", "private", "")
                .await
                .is_err()
        );
    }

    const BOT_EMAIL: &str = "keel-tenant-reader-0123abcd@users.noreply.invalid";

    #[tokio::test]
    async fn ensure_service_user_creates_restricted_user_and_always_hardens_it() {
        let pw = password();
        let bot_pw = password();
        let spec = ServiceUserSpec {
            username: "keel-tenant-reader",
            email: BOT_EMAIL,
            full_name: "Keel tenant reader",
            password: &bot_pw,
            adopt_id: None,
        };
        assert!(!format!("{spec:?}").contains(&bot_pw));

        let (url, h) = spawn_mock_http_server(vec![
            (404, "{}".into()),
            (201, r#"{"id":3,"login":"keel-tenant-reader"}"#.into()),
            (200, "{}".into()),
        ])
        .await;
        let user = admin(&url, &pw).ensure_service_user(&spec).await.unwrap();
        assert_eq!(
            user,
            ServiceUser {
                id: 3,
                outcome: ProvisionOutcome::Created
            }
        );
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "GET /api/v1/users/keel-tenant-reader HTTP/1.1"
        );
        assert_eq!(request_line(&reqs[1]), "POST /api/v1/admin/users HTTP/1.1");
        let create = request_body(&reqs[1]);
        assert_eq!(create["username"], "keel-tenant-reader");
        assert_eq!(create["email"], BOT_EMAIL);
        assert_eq!(create["password"], bot_pw.as_str());
        assert_eq!(create["must_change_password"], false);
        assert_eq!(create["restricted"], true);
        assert_eq!(create["send_notify"], false);
        assert_eq!(
            request_line(&reqs[2]),
            "PATCH /api/v1/admin/users/keel-tenant-reader HTTP/1.1"
        );
        let patch = request_body(&reqs[2]);
        for (key, want) in [
            ("admin", json!(false)),
            ("restricted", json!(true)),
            ("must_change_password", json!(false)),
            ("prohibit_login", json!(false)),
            ("max_repo_creation", json!(0)),
            ("allow_create_organization", json!(false)),
            ("allow_git_hook", json!(false)),
            ("allow_import_local", json!(false)),
            ("visibility", json!("private")),
            ("email", json!(BOT_EMAIL)),
            ("password", json!(bot_pw)),
        ] {
            assert_eq!(patch[key], want, "{key}");
        }

        // Existing user created by Keel (recorded id) -> no create, still hardened.
        let adopt = ServiceUserSpec {
            adopt_id: Some(3),
            ..spec
        };
        let (url, h) = spawn_mock_http_server(vec![
            (200, r#"{"id":3,"login":"keel-tenant-reader"}"#.into()),
            (200, "{}".into()),
        ])
        .await;
        assert_eq!(
            admin(&url, &pw).ensure_service_user(&adopt).await.unwrap(),
            ServiceUser {
                id: 3,
                outcome: ProvisionOutcome::Existed
            }
        );
        let reqs = h.await.unwrap();
        assert!(request_line(&reqs[1]).starts_with("PATCH "));

        // Create race (422 and the account now exists with the recorded id) -> existing;
        // non-admin caller (403 on patch) -> error.
        let (url, h) = spawn_mock_http_server(vec![
            (404, "{}".into()),
            (422, "{}".into()),
            (200, r#"{"id":3,"login":"keel-tenant-reader"}"#.into()),
            (403, "not admin".into()),
        ])
        .await;
        let err = admin(&url, &pw)
            .ensure_service_user(&adopt)
            .await
            .unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 403, .. }), "{err}");
        h.await.unwrap();

        // Lookup failure surfaces.
        let (url, h) = spawn_mock_http_server(vec![(500, "boom".into())]).await;
        assert!(admin(&url, &pw).ensure_service_user(&spec).await.is_err());
        h.await.unwrap();

        let empty = ServiceUserSpec {
            password: "",
            ..spec
        };
        assert!(admin(&url, &pw).ensure_service_user(&empty).await.is_err());
    }

    #[tokio::test]
    async fn ensure_service_user_refuses_to_adopt_an_unknown_account() {
        let pw = password();
        let bot_pw = password();
        let spec = ServiceUserSpec {
            username: "keel-tenant-reader",
            email: BOT_EMAIL,
            full_name: "Keel tenant reader",
            password: &bot_pw,
            adopt_id: None,
        };
        // No Secret record (adopt_id None): an existing account is someone else's. Nothing is
        // patched, so its password, flags and team membership stay untouched.
        let (url, h) = spawn_mock_http_server(vec![(
            200,
            r#"{"id":9,"login":"keel-tenant-reader"}"#.into(),
        )])
        .await;
        let err = admin(&url, &pw)
            .ensure_service_user(&spec)
            .await
            .unwrap_err();
        assert!(matches!(err, ForgeError::Api { status: 409, .. }), "{err}");
        assert!(err.to_string().contains("refusing to adopt"), "{err}");
        assert_eq!(h.await.unwrap().len(), 1);

        // A recorded id that does not match (account deleted and re-registered) -> refused too.
        let other = ServiceUserSpec {
            adopt_id: Some(3),
            ..spec
        };
        let (url, h) = spawn_mock_http_server(vec![(
            200,
            r#"{"id":9,"login":"keel-tenant-reader"}"#.into(),
        )])
        .await;
        assert!(matches!(
            admin(&url, &pw).ensure_service_user(&other).await,
            Err(ForgeError::Api { status: 409, .. })
        ));
        h.await.unwrap();

        // Squatted between lookup and create (422, then found) -> refused, never patched.
        let (url, h) = spawn_mock_http_server(vec![
            (404, "{}".into()),
            (422, r#"{"message":"user already exists"}"#.into()),
            (200, r#"{"id":9,"login":"keel-tenant-reader"}"#.into()),
        ])
        .await;
        assert!(matches!(
            admin(&url, &pw).ensure_service_user(&spec).await,
            Err(ForgeError::Api { status: 409, .. })
        ));
        assert_eq!(h.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn ensure_service_user_reports_the_real_422_cause() {
        let pw = password();
        let bot_pw = password();
        let spec = ServiceUserSpec {
            username: "keel-tenant-reader",
            email: BOT_EMAIL,
            full_name: "Keel tenant reader",
            password: &bot_pw,
            adopt_id: None,
        };
        // 422 for a taken email / weak password while the user still does not exist: the
        // original error is returned and no PATCH is sent to a missing user.
        let (url, h) = spawn_mock_http_server(vec![
            (404, "{}".into()),
            (422, r#"{"message":"e-mail already in use"}"#.into()),
            (404, "{}".into()),
        ])
        .await;
        let err = admin(&url, &pw)
            .ensure_service_user(&spec)
            .await
            .unwrap_err();
        match err {
            ForgeError::Api { status, message } => {
                assert_eq!(status, 422);
                assert!(message.contains("e-mail already in use"), "{message}");
            }
            other => panic!("unexpected {other}"),
        }
        let reqs = h.await.unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(reqs.iter().all(|r| !request_line(r).starts_with("PATCH")));
    }

    #[tokio::test]
    async fn verify_organization_control_fails_closed_on_squatted_orgs() {
        let pw = password();
        let teams = r#"[{"id":1,"name":"Owners","permission":"owner","includes_all_repositories":true},{"id":4,"name":"tenant-readers","permission":"read","includes_all_repositories":true},{"id":5,"name":"empty","permission":"read","includes_all_repositories":false}]"#;

        // Owned by Keel only; the unmanaged team reaches nothing -> ok.
        let (url, h) = spawn_mock_http_server(vec![
            (200, teams.into()),
            (200, r#"[{"id":1,"login":"keel"}]"#.into()),
            (200, "[]".into()),
        ])
        .await;
        admin(&url, &pw)
            .verify_organization_control("keel-tenants", &["keel"], &["tenant-readers"])
            .await
            .unwrap();
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "GET /api/v1/orgs/keel-tenants/teams?limit=50&page=1 HTTP/1.1"
        );
        assert_eq!(
            request_line(&reqs[1]),
            "GET /api/v1/teams/1/members?limit=50&page=1 HTTP/1.1"
        );
        assert_eq!(
            request_line(&reqs[2]),
            "GET /api/v1/teams/5/repos?limit=50&page=1 HTTP/1.1"
        );

        // Squatted: the creator is still an owner -> error.
        let (url, h) = spawn_mock_http_server(vec![
            (200, teams.into()),
            (
                200,
                r#"[{"id":1,"login":"keel"},{"id":7,"login":"mallory"}]"#.into(),
            ),
        ])
        .await;
        let err = admin(&url, &pw)
            .verify_organization_control("keel-tenants", &["keel"], &["tenant-readers"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("mallory"), "{err}");
        h.await.unwrap();

        // An unmanaged team with repository access -> error.
        let (url, h) = spawn_mock_http_server(vec![
            (200, teams.into()),
            (200, r#"[{"id":1,"login":"keel"}]"#.into()),
            (
                200,
                r#"[{"id":8,"name":"tenant-acme","full_name":"keel-tenants/tenant-acme"}]"#.into(),
            ),
        ])
        .await;
        let err = admin(&url, &pw)
            .verify_organization_control("keel-tenants", &["keel"], &["tenant-readers"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("keel-tenants/tenant-acme"), "{err}");
        h.await.unwrap();

        // An unmanaged team covering every repository -> error without listing repos.
        let (url, h) = spawn_mock_http_server(vec![
            (
                200,
                r#"[{"id":1,"name":"Owners","includes_all_repositories":true},{"id":6,"name":"spies","permission":"read","includes_all_repositories":true}]"#.into(),
            ),
            (200, r#"[{"id":1,"login":"keel"}]"#.into()),
        ])
        .await;
        let err = admin(&url, &pw)
            .verify_organization_control("keel-tenants", &["keel"], &["tenant-readers"])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("spies") && err.contains("every repository"),
            "{err}"
        );
        h.await.unwrap();

        // No Owners team at all -> error.
        let (url, h) = spawn_mock_http_server(vec![(200, "[]".into())]).await;
        assert!(
            admin(&url, &pw)
                .verify_organization_control("keel-tenants", &["keel"], &[])
                .await
                .is_err()
        );
        h.await.unwrap();
    }

    #[tokio::test]
    async fn team_membership_is_reconciled_exactly() {
        let pw = password();
        let (url, h) = spawn_mock_http_server(vec![
            (
                200,
                r#"[{"id":2,"login":"keel-tenant-reader"},{"id":7,"login":"mallory"}]"#.into(),
            ),
            (204, String::new()),
            (204, String::new()),
        ])
        .await;
        let removed = admin(&url, &pw)
            .ensure_team_members_exactly(4, &["keel-tenant-reader"])
            .await
            .unwrap();
        assert_eq!(removed, vec!["mallory".to_string()]);
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[1]),
            "DELETE /api/v1/teams/4/members/mallory HTTP/1.1"
        );
        assert_eq!(
            request_line(&reqs[2]),
            "PUT /api/v1/teams/4/members/keel-tenant-reader HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn stray_service_user_credentials_are_removed() {
        let pw = password();
        let (url, h) = spawn_mock_http_server(vec![
            (
                200,
                r#"[{"id":21,"name":"keel-tenant-repo-acme","scopes":["read:repository"],"repositories":[{"owner":"keel-tenants","name":"tenant-acme"}]},{"id":22,"name":"mallory","scopes":["all"]}]"#.into(),
            ),
            (204, String::new()),
            (200, r#"[{"id":31,"key":"ssh-ed25519 AAAA"}]"#.into()),
            (204, String::new()),
            (200, r#"[{"id":41}]"#.into()),
            (204, String::new()),
            (200, "[]".into()),
        ])
        .await;
        let bot = ForgejoGiteaProvider::with_basic_auth(
            &url,
            "keel-tenants",
            "x",
            "keel-tenant-reader",
            &pw,
        )
        .unwrap();
        let report = bot
            .remove_stray_user_credentials(|t| {
                t.name.starts_with("keel-tenant-repo-")
                    && t.repositories
                        .as_ref()
                        .is_some_and(|r| r.iter().all(|r| r.starts_with("keel-tenants/")))
            })
            .await
            .unwrap();
        assert_eq!(
            report,
            StrayCredentialReport {
                tokens: vec!["mallory".to_string()],
                ssh_keys: 1,
                gpg_keys: 1,
                oauth2_apps: 0,
            }
        );
        assert!(!report.is_empty());
        assert!(StrayCredentialReport::default().is_empty());
        let reqs = h.await.unwrap();
        let lines: Vec<&str> = reqs.iter().map(|r| request_line(r)).collect();
        assert_eq!(
            lines,
            vec![
                "GET /api/v1/users/keel-tenant-reader/tokens?limit=50&page=1 HTTP/1.1",
                "DELETE /api/v1/users/keel-tenant-reader/tokens/22 HTTP/1.1",
                "GET /api/v1/user/keys?limit=50&page=1 HTTP/1.1",
                "DELETE /api/v1/user/keys/31 HTTP/1.1",
                "GET /api/v1/user/gpg_keys?limit=50&page=1 HTTP/1.1",
                "DELETE /api/v1/user/gpg_keys/41 HTTP/1.1",
                "GET /api/v1/user/applications/oauth2?limit=50&page=1 HTTP/1.1",
            ]
        );

        // Token-authenticated providers cannot act as the service user.
        let token_auth = ForgejoGiteaProvider::new(&url, "keel-tenants", "x", "tok").unwrap();
        assert!(matches!(
            token_auth.remove_stray_user_credentials(|_| true).await,
            Err(ForgeError::InvalidInput(_))
        ));
    }

    const READERS: TeamSpec<'static> = TeamSpec {
        name: "tenant-readers",
        description: "read",
        permission: "read",
        units: &[("repo.code", "read")],
        includes_all_repositories: true,
        can_create_org_repo: false,
    };

    #[tokio::test]
    async fn ensure_team_creates_or_patches_and_adds_members() {
        let pw = password();
        let (url, h) = spawn_mock_http_server(vec![
            (200, r#"[{"id":1,"name":"Owners"}]"#.into()),
            (201, r#"{"id":9,"name":"tenant-readers"}"#.into()),
            (204, String::new()),
        ])
        .await;
        let p = admin(&url, &pw);
        let (id, out) = p.ensure_team("keel-tenants", &READERS).await.unwrap();
        assert_eq!((id, out), (9, ProvisionOutcome::Created));
        p.ensure_team_member(id, "keel-tenant-reader")
            .await
            .unwrap();
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "GET /api/v1/orgs/keel-tenants/teams?limit=50&page=1 HTTP/1.1"
        );
        assert_eq!(
            request_line(&reqs[1]),
            "POST /api/v1/orgs/keel-tenants/teams HTTP/1.1"
        );
        let body = request_body(&reqs[1]);
        assert_eq!(body["permission"], "read");
        assert_eq!(body["units"], json!(["repo.code"]));
        assert_eq!(body["units_map"], json!({"repo.code": "read"}));
        assert_eq!(body["includes_all_repositories"], true);
        assert_eq!(body["can_create_org_repo"], false);
        assert_eq!(
            request_line(&reqs[2]),
            "PUT /api/v1/teams/9/members/keel-tenant-reader HTTP/1.1"
        );

        // Existing team -> patched (drift reverted), id reused.
        let (url, h) = spawn_mock_http_server(vec![
            (
                200,
                r#"[{"id":1,"name":"Owners"},{"id":4,"name":"tenant-readers"}]"#.into(),
            ),
            (200, r#"{"id":4,"name":"tenant-readers"}"#.into()),
        ])
        .await;
        let (id, out) = admin(&url, &pw)
            .ensure_team("keel-tenants", &READERS)
            .await
            .unwrap();
        assert_eq!((id, out), (4, ProvisionOutcome::Existed));
        let reqs = h.await.unwrap();
        assert_eq!(request_line(&reqs[1]), "PATCH /api/v1/teams/4 HTTP/1.1");
        assert_eq!(request_body(&reqs[1])["permission"], "read");

        // Missing organisation / failed membership surface as errors.
        let (url, h) = spawn_mock_http_server(vec![(404, "{}".into()), (404, "{}".into())]).await;
        let p = admin(&url, &pw);
        assert!(matches!(
            p.ensure_team("keel-tenants", &READERS).await,
            Err(ForgeError::NotFound(_))
        ));
        assert!(p.ensure_team_member(4, "keel-tenant-reader").await.is_err());
        h.await.unwrap();

        let bad = TeamSpec {
            permission: "owner",
            ..READERS
        };
        assert!(p.ensure_team("keel-tenants", &bad).await.is_err());
    }

    #[tokio::test]
    async fn repository_scoped_token_is_restricted_or_fails_closed() {
        let pw = password();
        let bot = ForgejoGiteaProvider::with_basic_auth(
            "http://127.0.0.1:1",
            "keel-tenants",
            "x",
            "keel-tenant-reader",
            &pw,
        )
        .unwrap();
        let target = RepositoryTarget::new("keel-tenants", "tenant-acme").unwrap();
        // Validation happens before any request.
        assert!(
            bot.create_repository_scoped_token("t", &["read:repository"], &[])
                .await
                .is_err()
        );
        assert!(
            bot.create_repository_scoped_token(
                " ",
                &["read:repository"],
                std::slice::from_ref(&target)
            )
            .await
            .is_err()
        );
        let token_auth =
            ForgejoGiteaProvider::new("http://127.0.0.1:1", "keel-tenants", "x", "tok").unwrap();
        assert!(matches!(
            token_auth
                .create_repository_scoped_token("t", &[], std::slice::from_ref(&target))
                .await,
            Err(ForgeError::InvalidInput(_))
        ));
        assert!(RepositoryTarget::new("keel-tenants", "..").is_err());

        // Restricted as requested.
        let (url, h) = spawn_mock_http_server(vec![(
            201,
            r#"{"id":12,"name":"t","sha1":"secret-tok","scopes":["read:repository"],"repositories":[{"id":5,"owner":"keel-tenants","name":"tenant-acme","full_name":"keel-tenants/tenant-acme"}]}"#.into(),
        )])
        .await;
        let bot = ForgejoGiteaProvider::with_basic_auth(
            &url,
            "keel-tenants",
            "x",
            "keel-tenant-reader",
            &pw,
        )
        .unwrap();
        let minted = bot
            .create_repository_scoped_token(
                "t",
                &["read:repository"],
                std::slice::from_ref(&target),
            )
            .await
            .unwrap();
        assert_eq!(minted.id, 12);
        assert_eq!(minted.token, "secret-tok");
        assert!(!format!("{minted:?}").contains("secret-tok"));
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[0]),
            "POST /api/v1/users/keel-tenant-reader/tokens HTTP/1.1"
        );
        let body = request_body(&reqs[0]);
        assert_eq!(body["scopes"], json!(["read:repository"]));
        assert_eq!(
            body["repositories"],
            json!([{"owner": "keel-tenants", "name": "tenant-acme"}])
        );

        // Forge ignored `repositories` (account-wide token) -> deleted, error.
        let (url, h) = spawn_mock_http_server(vec![
            (
                201,
                r#"{"id":13,"sha1":"wide-tok","repositories":null}"#.into(),
            ),
            (204, String::new()),
        ])
        .await;
        let bot = ForgejoGiteaProvider::with_basic_auth(
            &url,
            "keel-tenants",
            "x",
            "keel-tenant-reader",
            &pw,
        )
        .unwrap();
        let err = bot
            .create_repository_scoped_token(
                "t",
                &["read:repository"],
                std::slice::from_ref(&target),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("all repositories"), "{err}");
        assert!(err.contains("Forgejo 15"), "{err}");
        assert!(!err.contains("wide-tok"));
        let reqs = h.await.unwrap();
        assert_eq!(
            request_line(&reqs[1]),
            "DELETE /api/v1/users/keel-tenant-reader/tokens/13 HTTP/1.1"
        );

        // Right repository but broader (or unreported) scopes -> deleted, error.
        for body in [
            r#"{"id":15,"sha1":"broad-tok","scopes":["read:repository","write:repository"],"repositories":[{"owner":"keel-tenants","name":"tenant-acme"}]}"#,
            r#"{"id":15,"sha1":"broad-tok","repositories":[{"owner":"keel-tenants","name":"tenant-acme"}]}"#,
        ] {
            let (url, h) =
                spawn_mock_http_server(vec![(201, body.into()), (204, String::new())]).await;
            let bot = ForgejoGiteaProvider::with_basic_auth(
                &url,
                "keel-tenants",
                "x",
                "keel-tenant-reader",
                &pw,
            )
            .unwrap();
            let err = bot
                .create_repository_scoped_token(
                    "t",
                    &["read:repository"],
                    std::slice::from_ref(&target),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("scopes"), "{err}");
            assert!(!err.contains("broad-tok"));
            let reqs = h.await.unwrap();
            assert_eq!(
                request_line(&reqs[1]),
                "DELETE /api/v1/users/keel-tenant-reader/tokens/15 HTTP/1.1"
            );
        }

        // Restricted to a different repository -> deleted, error.
        let (url, h) = spawn_mock_http_server(vec![
            (
                201,
                r#"{"id":14,"sha1":"x","repositories":[{"owner":"keel","name":"infra-gitops"}]}"#
                    .into(),
            ),
            (404, "{}".into()),
        ])
        .await;
        let bot = ForgejoGiteaProvider::with_basic_auth(
            &url,
            "keel-tenants",
            "x",
            "keel-tenant-reader",
            &pw,
        )
        .unwrap();
        let err = bot
            .create_repository_scoped_token("t", &["read:repository"], &[target])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("keel/infra-gitops"), "{err}");
        h.await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_login_and_token_deletion() {
        let pw = password();
        let (url, h) = spawn_mock_http_server(vec![
            (200, r#"{"id":3,"login":"keel-tenant-reader"}"#.into()),
            (401, r#"{"message":"bad"}"#.into()),
            (500, "boom".into()),
        ])
        .await;
        let bot = ForgejoGiteaProvider::with_basic_auth(
            &url,
            "keel-tenants",
            "x",
            "keel-tenant-reader",
            &pw,
        )
        .unwrap();
        assert_eq!(
            bot.authenticated_login().await.unwrap(),
            "keel-tenant-reader"
        );
        assert!(matches!(
            bot.authenticated_login().await,
            Err(ForgeError::Api { status: 401, .. })
        ));
        assert!(bot.delete_access_token(1).await.is_err());
        let reqs = h.await.unwrap();
        assert_eq!(request_line(&reqs[0]), "GET /api/v1/user HTTP/1.1");
        assert_eq!(ProvisionOutcome::Created.label(), "created");
        assert_eq!(ProvisionOutcome::Existed.label(), "already present");
    }
}
