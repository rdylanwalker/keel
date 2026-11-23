//! Authentication & stateless session cookie management (`SDD.md` §6.1, §6.3, §7.3).
//!
//! - Stage 0+: `GET /api/v1/auth/login` & `/callback` implement OIDC Authorization Code + PKCE
//!   against the in-cluster Dex OIDC provider (or external OIDC issuer) using stateless encrypted
//!   `keel_pkce` and `keel_session` cookies.

use axum::Json;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use keel_core::{AuthPhase, EnvelopeCrypto, percent_encode};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::ServerError;
use crate::state::AppState;

/// Cookie holding the stateless `AES-256-GCM` encrypted [`SessionClaims`].
pub const SESSION_COOKIE_NAME: &str = "keel_session";

/// Short-lived cookie holding the encrypted OIDC PKCE verifier & state nonce.
pub const PKCE_COOKIE_NAME: &str = "keel_pkce";

/// Marker cookie set on logout so the next OIDC login forces `prompt=login`.
pub const LOGGED_OUT_COOKIE_NAME: &str = "keel_logged_out";

const SESSION_TTL_SECONDS: i64 = 3600;
const PKCE_TTL_SECONDS: i64 = 300;

/// Authenticated session claims encrypted inside `keel_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionClaims {
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    pub auth_phase: AuthPhase,
    pub exp: i64,
}

/// Axum extractor enforcing a valid authenticated session cookie (or Bearer session token).
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub claims: SessionClaims,
}

/// Environment variable holding the comma-separated break-glass administrator allowlist
/// (OIDC `sub` or `email`, case-insensitive). Defaults to the Stage 0 bootstrap admin.
pub const BREAK_GLASS_ADMINS_ENV: &str = "KEEL_BREAK_GLASS_ADMINS";

/// Default break-glass administrator identity (the Stage 0 bootstrap admin, `SDD.md` §6.1).
pub const DEFAULT_BREAK_GLASS_ADMIN: &str = "admin@keel.local";

/// Group prefix mapping IdP groups to tenants: `tenant-<id>` (member), `tenant-<id>-admin` (admin).
pub const TENANT_GROUP_PREFIX: &str = "tenant-";

/// Returns the configured break-glass administrator allowlist (lower-cased).
fn break_glass_admin_allowlist() -> Vec<String> {
    let raw = std::env::var(BREAK_GLASS_ADMINS_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BREAK_GLASS_ADMIN.to_string());
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Environment variable holding extra comma-separated IdP groups granted cluster-administrator
/// privileges, in addition to the built-in [`BUILT_IN_ADMIN_GROUPS`] (e.g. a catalog-installed
/// keel-server serving a separate team whose IdP names its admin group differently).
pub const ADMIN_GROUPS_ENV: &str = "KEEL_ADMIN_GROUPS";

/// IdP groups that always grant cluster-administrator privileges.
pub const BUILT_IN_ADMIN_GROUPS: [&str; 3] = ["keel-admins", "admin", "admins"];

/// Parses a comma-separated [`ADMIN_GROUPS_ENV`] value (trimmed, blanks dropped; case-sensitive
/// like IdP group claims).
fn parse_admin_groups(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `group` grants cluster-administrator privileges: a built-in admin group or one of
/// `extra` ([`ADMIN_GROUPS_ENV`]).
fn is_admin_group(group: &str, extra: &[String]) -> bool {
    BUILT_IN_ADMIN_GROUPS.contains(&group) || extra.iter().any(|g| g == group)
}

/// Form of the OIDC authorization-code token exchange (PKCE). A confidential client also sends
/// its secret (`client_secret_post`); public clients omit it.
fn token_exchange_form<'a>(
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    client_secret: Option<&'a str>,
) -> Vec<(&'static str, &'a str)> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    form
}

/// Returns `true` when `claims` identify an allowlisted break-glass administrator.
fn is_break_glass_identity(claims: &SessionClaims) -> bool {
    let allow = break_glass_admin_allowlist();
    let sub = claims.sub.trim().to_ascii_lowercase();
    let email = claims
        .email
        .as_deref()
        .map(|e| e.trim().to_ascii_lowercase());
    allow
        .iter()
        .any(|a| *a == sub || email.as_deref() == Some(a.as_str()))
}

impl AuthSession {
    /// Returns `true` when the caller holds cluster-administrator privileges: membership of
    /// `keel-admins` / `admin` / `admins` or of a group listed in [`ADMIN_GROUPS_ENV`], or — only
    /// while the control plane is in the `BreakGlass` phase — being an allowlisted break-glass
    /// identity ([`BREAK_GLASS_ADMINS_ENV`], default `admin@keel.local`).
    /// Ordinary OIDC users never gain admin merely because the phase is `BreakGlass` (OPEN-12).
    pub fn is_admin(&self) -> bool {
        if self.claims.auth_phase == AuthPhase::BreakGlass && is_break_glass_identity(&self.claims)
        {
            return true;
        }
        let extra = parse_admin_groups(std::env::var(ADMIN_GROUPS_ENV).ok().as_deref());
        self.claims.groups.iter().any(|g| is_admin_group(g, &extra))
    }

    /// Enforces cluster-administrator privileges, returning `403 Forbidden` otherwise.
    pub fn require_admin(&self) -> Result<(), ServerError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(ServerError::Forbidden(
                "Administrator privileges required for this operation".to_string(),
            ))
        }
    }

    /// Returns `true` when the caller holds cluster-admin or tenant-scoped admin privileges for `tenant_id`
    /// (member of `keel-admins` or of the explicit group `tenant-<tenant_id>-admin`).
    ///
    /// Bare `<tenant_id>` / `<tenant_id>-admin` group names are intentionally **not** honored so an
    /// unrelated IdP group that happens to equal a tenant id cannot grant access (OPEN-12).
    pub fn is_tenant_admin(&self, tenant_id: &str) -> bool {
        if self.is_admin() {
            return true;
        }
        // The management pseudo-tenant is the control plane itself: no IdP group named after it
        // (`tenant-management`, `tenant-management-admin`) grants anything.
        if tenant_id == crate::state::MANAGEMENT_TENANT {
            return false;
        }
        let prefixed = format!("{TENANT_GROUP_PREFIX}{tenant_id}-admin");
        self.claims.groups.iter().any(|g| g == &prefixed)
    }

    /// Returns `true` when the caller administers at least one tenant (`tenant-<id>-admin`).
    pub fn is_any_tenant_admin(&self) -> bool {
        self.claims.groups.iter().any(|g| {
            g.strip_prefix(TENANT_GROUP_PREFIX)
                .and_then(|rest| rest.strip_suffix("-admin"))
                .is_some_and(|id| !id.is_empty() && id != crate::state::MANAGEMENT_TENANT)
        })
    }

    /// Tenant ids named by the caller's tenant groups (`tenant-<id>` member, `tenant-<id>-admin`
    /// admin), sorted and deduplicated. Says nothing about cluster administrators (who see every
    /// tenant) and does not check that the tenants exist.
    pub fn tenant_group_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .claims
            .groups
            .iter()
            .filter_map(|g| g.strip_prefix(TENANT_GROUP_PREFIX))
            .map(|rest| rest.strip_suffix("-admin").unwrap_or(rest))
            .filter(|id| {
                *id != crate::state::MANAGEMENT_TENANT
                    && keel_core::validate_dns_label(id, "tenant").is_ok()
            })
            .filter(|id| self.require_tenant_access(id).is_ok())
            .map(str::to_string)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Enforces tenant-scoped admin privileges for `tenant_id`, returning `403 Forbidden` otherwise.
    pub fn require_tenant_admin(&self, tenant_id: &str) -> Result<(), ServerError> {
        if self.is_tenant_admin(tenant_id) {
            Ok(())
        } else {
            Err(ServerError::Forbidden(format!(
                "Tenant administrator privileges required for '{tenant_id}'"
            )))
        }
    }

    /// Enforces access to `tenant_id` (cluster admin, tenant admin, or member of `tenant-<tenant_id>`).
    pub fn require_tenant_access(&self, tenant_id: &str) -> Result<(), ServerError> {
        if self.is_tenant_admin(tenant_id) {
            return Ok(());
        }
        let prefixed = format!("{TENANT_GROUP_PREFIX}{tenant_id}");
        if tenant_id != crate::state::MANAGEMENT_TENANT
            && self.claims.groups.iter().any(|g| g == &prefixed)
        {
            Ok(())
        } else {
            Err(ServerError::Forbidden(format!(
                "Access denied for tenant '{tenant_id}'"
            )))
        }
    }
}

impl FromRequestParts<AppState> for AuthSession {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_session_token(&parts.headers).ok_or_else(|| {
            ServerError::Unauthorized("Missing or invalid authentication session".to_string())
        })?;

        let session_key = state.secret_store.get_or_create_session_key().await?;
        let claims = decrypt_session_token(&session_key, &token)?;

        if claims.exp <= Utc::now().timestamp() {
            return Err(ServerError::Unauthorized(
                "Authentication session has expired".to_string(),
            ));
        }
        // Sessions are bound to the auth phase they were issued under: a phase transition
        // (e.g. leaving break-glass) invalidates every outstanding session (OPEN-12).
        let current_phase = *state.auth_phase.read().await;
        if claims.auth_phase != current_phase {
            return Err(ServerError::Unauthorized(
                "Authentication session was issued under a different auth phase; please log in again"
                    .to_string(),
            ));
        }

        Ok(Self { claims })
    }
}

/// Encrypts [`SessionClaims`] into an opaque URL-safe token using `AES-256-GCM`.
pub fn encrypt_session_token(
    session_key: &[u8; 32],
    claims: &SessionClaims,
) -> Result<String, ServerError> {
    let json = serde_json::to_vec(claims)
        .map_err(|e| ServerError::Internal(format!("Failed to serialize session claims: {e}")))?;
    let cipher = EnvelopeCrypto::new(*session_key);
    let b64_cipher = cipher.encrypt(&json)?;
    Ok(URL_SAFE_NO_PAD.encode(b64_cipher.as_bytes()))
}

/// Decrypts and authenticates an opaque `keel_session` token using `AES-256-GCM`.
pub fn decrypt_session_token(
    session_key: &[u8; 32],
    token: &str,
) -> Result<SessionClaims, ServerError> {
    let raw_bytes = URL_SAFE_NO_PAD
        .decode(token.trim())
        .map_err(|_| ServerError::Unauthorized("Malformed session token".to_string()))?;
    let b64_cipher = std::str::from_utf8(&raw_bytes)
        .map_err(|_| ServerError::Unauthorized("Malformed session token".to_string()))?;
    let cipher = EnvelopeCrypto::new(*session_key);
    let plaintext = cipher
        .decrypt(b64_cipher)
        .map_err(|_| ServerError::Unauthorized("Invalid or tampered session token".to_string()))?;
    serde_json::from_slice(&plaintext)
        .map_err(|_| ServerError::Unauthorized("Corrupted session claims".to_string()))
}

fn is_secure_request(headers: &HeaderMap) -> bool {
    if headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"))
    {
        return true;
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    !(host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]"))
}

fn build_session_cookie_header(
    token: &str,
    max_age: i64,
    secure: bool,
) -> Result<HeaderValue, ServerError> {
    let secure_attr = if secure { "Secure; " } else { "" };
    let cookie = format!(
        "{SESSION_COOKIE_NAME}={token}; HttpOnly; {secure_attr}SameSite=Strict; Path=/; Max-Age={max_age}"
    );
    HeaderValue::from_str(&cookie)
        .map_err(|e| ServerError::Internal(format!("Invalid session cookie header: {e}")))
}

fn build_pkce_cookie_header(
    token: &str,
    max_age: i64,
    secure: bool,
) -> Result<HeaderValue, ServerError> {
    let secure_attr = if secure { "Secure; " } else { "" };
    let cookie = format!(
        "{PKCE_COOKIE_NAME}={token}; HttpOnly; {secure_attr}SameSite=Lax; Path=/api/v1/auth; Max-Age={max_age}"
    );
    HeaderValue::from_str(&cookie)
        .map_err(|e| ServerError::Internal(format!("Invalid PKCE cookie header: {e}")))
}

fn build_logged_out_cookie_header(
    value: &str,
    max_age: i64,
    secure: bool,
) -> Result<HeaderValue, ServerError> {
    let secure_attr = if secure { "Secure; " } else { "" };
    let cookie = format!(
        "{LOGGED_OUT_COOKIE_NAME}={value}; HttpOnly; {secure_attr}SameSite=Lax; Path=/api/v1/auth; Max-Age={max_age}"
    );
    HeaderValue::from_str(&cookie)
        .map_err(|e| ServerError::Internal(format!("Invalid logged-out cookie header: {e}")))
}

fn extract_cookie_value<'a>(headers: &'a HeaderMap, cookie_name: &str) -> Option<&'a str> {
    for val in headers.get_all(COOKIE) {
        let Ok(raw) = val.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let trimmed = pair.trim();
            if let Some((name, value)) = trimmed.split_once('=')
                && name.trim() == cookie_name
            {
                return Some(value.trim());
            }
        }
    }
    None
}

fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(cookie_val) = extract_cookie_value(headers, SESSION_COOKIE_NAME)
        && !cookie_val.is_empty()
    {
        return Some(cookie_val.to_string());
    }
    if let Some(auth_hdr) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
        && let Some(bearer) = auth_hdr.strip_prefix("Bearer ")
    {
        let trimmed = bearer.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Response body for OIDC login callback.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthLoginResponse {
    pub authenticated: bool,
    pub sub: String,
    pub phase: AuthPhase,
}

/// Response body for `POST /api/v1/auth/logout`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthLogoutResponse {
    pub authenticated: bool,
}

#[derive(Debug, Deserialize)]
struct OidcDiscoveryDocument {
    #[serde(default)]
    issuer: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    jwks_uri: Option<String>,
}

struct ResolvedOidcEndpoints {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    expected_issuer: String,
}

fn build_oidc_http_client(
    timeout_secs: u64,
    ca_pem: Option<&[u8]>,
) -> Result<reqwest::Client, ServerError> {
    let mut builder =
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(timeout_secs));
    if let Some(pem) = ca_pem
        && let Ok(cert) = reqwest::Certificate::from_pem(pem)
    {
        builder = builder.add_root_certificate(cert);
    }
    builder
        .build()
        .map_err(|e| ServerError::Internal(format!("Failed to construct HTTP client: {e}")))
}

/// Renders an error and its `source()` chain (`reqwest` hides the DNS/connect cause there).
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

/// `cause` (an [`error_chain`] rendering) says the host name did not resolve.
fn is_dns_failure(cause: &str) -> bool {
    let cause = cause.to_ascii_lowercase();
    [
        "dns error",
        "failed to lookup",
        "name or service not known",
        "no such host",
    ]
    .iter()
    .any(|needle| cause.contains(needle))
}

/// Operator-facing explanation of a failed OIDC discovery for `issuer`. The hosts-file hint for
/// local bootstraps (e.g. `idp.keel.local`) is only given when the issuer host is a DNS name
/// other than `localhost` and the cause is a resolution failure; otherwise the IdP (or the
/// `keel serve` port-forward) on `host:port` is reported as unreachable.
pub fn oidc_discovery_failure_message(issuer: &str, cause: &str) -> String {
    let issuer = issuer.trim_end_matches('/');
    let parsed = reqwest::Url::parse(issuer).ok();
    let host = parsed.as_ref().and_then(|u| u.host());
    let host_port = parsed
        .as_ref()
        .and_then(|u| {
            let host = u.host_str()?;
            Some(match u.port_or_known_default() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            })
        })
        .unwrap_or_else(|| issuer.to_string());
    let prefix = format!(
        "OIDC discovery failed for issuer `{issuer}` ({cause}), so logins cannot start. \
         keel-server must reach `{issuer}/.well-known/openid-configuration` itself."
    );
    match host {
        Some(url::Host::Domain(name))
            if !name.eq_ignore_ascii_case("localhost") && is_dns_failure(cause) =>
        {
            format!(
                "{prefix} `{name}` does not resolve on the machine running keel-server: for a \
                 local kind bootstrap add `127.0.0.1 {name}` to /etc/hosts there (the `keel \
                 serve` process needs it, not only the browser), make sure the IdP is up, then \
                 retry the login"
            )
        }
        _ => format!(
            "{prefix} Make sure the IdP (or the `keel serve` port-forward to it) is listening \
             on `{host_port}` and that no other process holds that port, then retry the login"
        ),
    }
}

/// Fixed, client-facing text for a failed OIDC discovery: the cause (DNS/connect/TLS detail,
/// internal hosts) and the operator fix are only logged server-side (`NFR-SEC-05`).
fn oidc_discovery_client_message(issuer: &str) -> String {
    format!(
        "OIDC discovery for the configured issuer `{}` failed, so logins cannot start; see the \
         keel-server (`keel serve`) log for the cause and fix",
        issuer.trim_end_matches('/')
    )
}

/// Resolves the IdP endpoints from its OIDC discovery document. There is no guessed fallback:
/// an unreachable or invalid discovery document is an error whose message names the cause.
async fn resolve_oidc_endpoints(
    issuer_url: &str,
    ca_pem: Option<&[u8]>,
) -> Result<ResolvedOidcEndpoints, String> {
    let issuer = issuer_url.trim_end_matches('/');
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    let client = build_oidc_http_client(2, ca_pem).map_err(|e| e.to_string())?;
    let resp = client
        .get(&discovery_url)
        .send()
        .await
        .map_err(|e| error_chain(&e))?;
    if !resp.status().is_success() {
        return Err(format!("`{discovery_url}` returned HTTP {}", resp.status()));
    }
    let doc = resp.json::<OidcDiscoveryDocument>().await.map_err(|e| {
        format!(
            "`{discovery_url}` is not a valid discovery document: {}",
            error_chain(&e)
        )
    })?;
    let expected_issuer = doc
        .issuer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| issuer.to_string());
    // When `keel serve` port-forwards an in-cluster Dex instance to `127.0.0.1`,
    // Dex's discovery document advertises its configured issuer URL (e.g.
    // `.svc.cluster.local` or `dex.keel.local`), which a host browser without
    // custom `/etc/hosts` entries cannot resolve.
    if (issuer.starts_with("http://127.0.0.1:") || issuer.starts_with("http://localhost:"))
        && !doc.authorization_endpoint.starts_with(issuer)
    {
        return Ok(ResolvedOidcEndpoints {
            authorization_endpoint: format!("{issuer}/auth"),
            token_endpoint: format!("{issuer}/token"),
            jwks_uri: format!("{issuer}/keys"),
            expected_issuer,
        });
    }
    Ok(ResolvedOidcEndpoints {
        authorization_endpoint: doc.authorization_endpoint,
        token_endpoint: doc.token_endpoint,
        jwks_uri: doc.jwks_uri.unwrap_or_else(|| format!("{issuer}/keys")),
        expected_issuer,
    })
}

/// Startup probe for `keel serve`: `Err` carries [`oidc_discovery_failure_message`] when the
/// configured issuer's discovery document cannot be fetched.
pub async fn check_oidc_discovery(issuer_url: &str, ca_pem: Option<&[u8]>) -> Result<(), String> {
    resolve_oidc_endpoints(issuer_url, ca_pem)
        .await
        .map(|_| ())
        .map_err(|cause| oidc_discovery_failure_message(issuer_url, &cause))
}

/// [`resolve_oidc_endpoints`] for the login/callback handlers: a failure is logged loudly (with
/// its cause and fix) and becomes a `503` with a fixed message instead of a redirect to a
/// guessed endpoint.
async fn resolve_oidc_endpoints_or_unavailable(
    issuer_url: &str,
    ca_pem: Option<&[u8]>,
) -> Result<ResolvedOidcEndpoints, ServerError> {
    resolve_oidc_endpoints(issuer_url, ca_pem)
        .await
        .map_err(|cause| {
            tracing::warn!("{}", oidc_discovery_failure_message(issuer_url, &cause));
            ServerError::ServiceUnavailable(oidc_discovery_client_message(issuer_url))
        })
}

/// The request comes from a browser navigation (`Accept` includes `text/html`).
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/html"))
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Error response for the browser-navigated auth endpoints (`/auth/login`, `/auth/cli/login`,
/// `/auth/callback`): a minimal, escaped `text/html` page when a browser navigated there (a raw
/// JSON document is no error page), the regular JSON error otherwise. Both carry the same
/// client-safe reason and status.
fn browser_error_response(headers: &HeaderMap, err: ServerError) -> Response {
    if !accepts_html(headers) {
        return err.into_response();
    }
    let reason = err.client_safe_reason();
    let status = err.into_response().status();
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Keel sign-in unavailable</title></head><body>\
         <h1>Keel sign-in failed</h1><p>{}</p><p><a href=\"/\">Back to Keel</a></p>\
         </body></html>\n",
        escape_html(&reason)
    );
    (
        status,
        [
            (
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            ),
        ],
        body,
    )
        .into_response()
}

/// Response body for `GET /api/v1/auth/status`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatusResponse {
    pub phase: AuthPhase,
    pub oidc_enabled: bool,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    pub is_admin: bool,
}

/// `GET /api/v1/auth/status`
///
/// Reports the current authentication phase (`Bootstrap`, `OidcActive`, `BreakGlass`),
/// whether OIDC is enabled, and whether the caller holds a valid session cookie.
pub async fn auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthStatusResponse>, ServerError> {
    let phase = *state.auth_phase.read().await;
    let oidc_enabled = state
        .oidc_config
        .read()
        .await
        .as_ref()
        .is_some_and(|c| c.enabled);

    let mut authenticated = false;
    let mut sub = None;
    let mut name = None;
    let mut email = None;
    let mut groups = Vec::new();
    let mut is_admin = false;

    if let Some(token) = extract_session_token(&headers)
        && let Ok(session_key) = state.secret_store.get_or_create_session_key().await
        && let Ok(claims) = decrypt_session_token(&session_key, &token)
        && claims.exp > Utc::now().timestamp()
        && claims.auth_phase == phase
    {
        authenticated = true;
        sub = Some(claims.sub.clone());
        name = claims.name.clone();
        email = claims.email.clone();
        groups = claims.groups.clone();
        is_admin = AuthSession { claims }.is_admin();
    }

    Ok(Json(AuthStatusResponse {
        phase,
        oidc_enabled,
        authenticated,
        sub,
        name,
        email,
        groups,
        is_admin,
    }))
}

#[derive(Debug, Serialize, Deserialize)]
struct PkceStatePayload {
    state: String,
    code_verifier: String,
    redirect_uri: String,
    exp: i64,
    /// OIDC `nonce` sent in the authorization request and required in the returned `id_token`.
    #[serde(default)]
    nonce: Option<String>,
    /// Present when the flow was started by the CLI loopback login (`/api/v1/auth/cli/login`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli: Option<CliLoginBinding>,
}

/// CLI loopback binding carried through the browser OIDC round-trip inside the encrypted PKCE cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliLoginBinding {
    redirect_uri: String,
    state: String,
    code_challenge: String,
}

#[derive(Debug, Deserialize)]
pub struct OidcLoginQuery {
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

fn random_b64(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `GET /api/v1/auth/login`
///
/// Initiates the OIDC Authorization Code + PKCE (`S256`) flow, storing the PKCE verifier, state,
/// and OIDC `nonce` in a short-lived `AES-256-GCM` encrypted `HttpOnly; SameSite=Lax` cookie.
pub async fn oidc_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OidcLoginQuery>,
) -> Response {
    begin_oidc_flow(
        &state,
        &headers,
        query.redirect_uri,
        query.prompt.as_deref(),
        None,
    )
    .await
    .unwrap_or_else(|err| browser_error_response(&headers, err))
}

/// Query parameters for `GET /api/v1/auth/cli/login` (RFC 8252 loopback CLI login).
#[derive(Debug, Deserialize)]
pub struct CliLoginQuery {
    /// `http://127.0.0.1:<port>/callback` or `http://[::1]:<port>/callback`.
    pub redirect_uri: String,
    /// CLI-generated opaque state echoed back to the loopback listener.
    pub state: String,
    /// `BASE64URL(SHA256(code_verifier))`; the verifier never leaves the CLI until exchange.
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// `GET /api/v1/auth/cli/login`
///
/// Browser entrypoint for `keel auth login`: validates the CLI loopback `redirect_uri`, `state`,
/// and PKCE `code_challenge`, then starts the regular server-side OIDC flow. After the IdP
/// callback the browser is redirected to the loopback URI with a one-time `code` that the CLI
/// redeems at `POST /api/v1/auth/cli/exchange` together with its `code_verifier`.
pub async fn cli_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CliLoginQuery>,
) -> Response {
    cli_login_inner(&state, &headers, query)
        .await
        .unwrap_or_else(|err| browser_error_response(&headers, err))
}

async fn cli_login_inner(
    state: &AppState,
    headers: &HeaderMap,
    query: CliLoginQuery,
) -> Result<Response, ServerError> {
    let redirect_uri = validate_cli_loopback_redirect_uri(&query.redirect_uri)?;
    if !is_valid_cli_state(&query.state) {
        return Err(ServerError::BadRequest(
            "state must be 8-256 URL-safe characters".to_string(),
        ));
    }
    if query
        .code_challenge_method
        .as_deref()
        .is_some_and(|m| m != "S256")
    {
        return Err(ServerError::BadRequest(
            "code_challenge_method must be S256".to_string(),
        ));
    }
    if !is_valid_pkce_challenge(&query.code_challenge) {
        return Err(ServerError::BadRequest(
            "code_challenge must be a 43-128 character base64url S256 digest".to_string(),
        ));
    }
    begin_oidc_flow(
        state,
        headers,
        None,
        query.prompt.as_deref(),
        Some(CliLoginBinding {
            redirect_uri,
            state: query.state,
            code_challenge: query.code_challenge,
        }),
    )
    .await
}

async fn begin_oidc_flow(
    state: &AppState,
    headers: &HeaderMap,
    requested_redirect: Option<String>,
    prompt: Option<&str>,
    cli: Option<CliLoginBinding>,
) -> Result<Response, ServerError> {
    let oidc_guard = state.oidc_config.read().await;
    let Some(oidc) = oidc_guard.as_ref().filter(|c| c.enabled) else {
        return Err(ServerError::ServiceUnavailable(
            "OIDC is not configured".to_string(),
        ));
    };

    let code_verifier = random_b64(32);
    let state_nonce = random_b64(16);
    let oidc_nonce = random_b64(16);
    let challenge_digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(challenge_digest);

    let secure = is_secure_request(headers);
    let validated_host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| is_valid_host_header(h));

    let redirect_uri = if let Some(requested) = requested_redirect.filter(|u| !u.trim().is_empty())
    {
        validate_oidc_redirect_uri(&requested, &oidc.redirect_url, validated_host)?
    } else if let Some(host) = validated_host {
        let scheme = if secure { "https" } else { "http" };
        format!("{scheme}://{host}/api/v1/auth/callback")
    } else {
        oidc.redirect_url.clone()
    };

    let pkce_payload = PkceStatePayload {
        state: state_nonce.clone(),
        code_verifier,
        redirect_uri: redirect_uri.clone(),
        exp: Utc::now().timestamp() + PKCE_TTL_SECONDS,
        nonce: Some(oidc_nonce.clone()),
        cli,
    };

    let session_key = state.secret_store.get_or_create_session_key().await?;
    let pkce_json = serde_json::to_vec(&pkce_payload)
        .map_err(|e| ServerError::Internal(format!("Failed to serialize PKCE state: {e}")))?;
    let encrypted_pkce = URL_SAFE_NO_PAD.encode(
        EnvelopeCrypto::new(session_key)
            .encrypt(&pkce_json)?
            .as_bytes(),
    );
    let pkce_cookie = build_pkce_cookie_header(&encrypted_pkce, PKCE_TTL_SECONDS, secure)?;

    let endpoints =
        resolve_oidc_endpoints_or_unavailable(&oidc.issuer_url, state.oidc_ca_pem.as_deref())
            .await?;
    let authorization_endpoint = endpoints.authorization_endpoint;
    let scopes = oidc.scopes.join("%20");
    let sep = if authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let force_reauth = prompt == Some("login")
        || extract_cookie_value(headers, LOGGED_OUT_COOKIE_NAME).is_some_and(|v| v == "1");
    let prompt_param = if force_reauth { "&prompt=login" } else { "" };
    let auth_url = format!(
        "{authorization_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&scope={scopes}&state={state_nonce}&nonce={oidc_nonce}&code_challenge={code_challenge}&code_challenge_method=S256{prompt_param}",
        percent_encode(&oidc.client_id),
        percent_encode(&redirect_uri),
    );

    let location = HeaderValue::from_str(&auth_url)
        .map_err(|e| ServerError::Internal(format!("Invalid OIDC redirect URL: {e}")))?;
    let clear_logged_out_cookie = build_logged_out_cookie_header("", 0, secure)?;

    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(LOCATION, location);
    resp.headers_mut().append(SET_COOKIE, pkce_cookie);
    resp.headers_mut()
        .append(SET_COOKIE, clear_logged_out_cookie);
    Ok(resp)
}

#[derive(Debug, Deserialize)]
pub struct OidcCallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwtClaimsSubset {
    sub: String,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
}

/// `GET /api/v1/auth/callback`
///
/// Validates the OIDC PKCE state cookie, exchanges the authorization code at the
/// OIDC token endpoint, and issues a stateless `keel_session` cookie.
pub async fn oidc_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OidcCallbackQuery>,
) -> Response {
    oidc_callback_inner(&state, &headers, query)
        .await
        .unwrap_or_else(|err| browser_error_response(&headers, err))
}

async fn oidc_callback_inner(
    state: &AppState,
    headers: &HeaderMap,
    query: OidcCallbackQuery,
) -> Result<Response, ServerError> {
    if let Some(err) = query.error {
        return Err(ServerError::Unauthorized(format!(
            "OIDC provider returned error: {err}"
        )));
    }

    let code = query
        .code
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| ServerError::BadRequest("Missing OIDC authorization code".to_string()))?;
    let returned_state = query
        .state
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ServerError::BadRequest("Missing OIDC state parameter".to_string()))?;

    let oidc_guard = state.oidc_config.read().await;
    let Some(oidc) = oidc_guard.as_ref().filter(|c| c.enabled) else {
        return Err(ServerError::ServiceUnavailable(
            "OIDC is not configured".to_string(),
        ));
    };

    let pkce_cookie_raw = extract_cookie_value(headers, PKCE_COOKIE_NAME)
        .ok_or_else(|| ServerError::Unauthorized("Missing OIDC PKCE state cookie".to_string()))?;
    let session_key = state.secret_store.get_or_create_session_key().await?;

    let pkce_b64 = URL_SAFE_NO_PAD
        .decode(pkce_cookie_raw)
        .map_err(|_| ServerError::Unauthorized("Malformed PKCE cookie".to_string()))?;
    let pkce_b64_str = std::str::from_utf8(&pkce_b64)
        .map_err(|_| ServerError::Unauthorized("Malformed PKCE cookie".to_string()))?;
    let pkce_plaintext = EnvelopeCrypto::new(session_key)
        .decrypt(pkce_b64_str)
        .map_err(|_| ServerError::Unauthorized("Invalid PKCE state cookie".to_string()))?;
    let pkce: PkceStatePayload = serde_json::from_slice(&pkce_plaintext)
        .map_err(|_| ServerError::Unauthorized("Corrupted PKCE state cookie".to_string()))?;

    if pkce.exp <= Utc::now().timestamp() {
        return Err(ServerError::Unauthorized(
            "OIDC PKCE state has expired".to_string(),
        ));
    }
    if pkce.state != returned_state {
        return Err(ServerError::Unauthorized(
            "OIDC state mismatch (possible CSRF)".to_string(),
        ));
    }

    let endpoints =
        resolve_oidc_endpoints_or_unavailable(&oidc.issuer_url, state.oidc_ca_pem.as_deref())
            .await?;
    let client = build_oidc_http_client(5, state.oidc_ca_pem.as_deref())?;
    let token_res = client
        .post(&endpoints.token_endpoint)
        .form(&token_exchange_form(
            code.as_str(),
            pkce.redirect_uri.as_str(),
            oidc.client_id.as_str(),
            pkce.code_verifier.as_str(),
            state.oidc_client_secret.as_deref(),
        ))
        .send()
        .await
        .map_err(|e| ServerError::Internal(format!("OIDC token exchange failed: {e}")))?;

    if !token_res.status().is_success() {
        return Err(ServerError::Unauthorized(format!(
            "OIDC token endpoint rejected authorization code (HTTP {})",
            token_res.status()
        )));
    }

    let token_body: OidcTokenResponse = token_res
        .json()
        .await
        .map_err(|e| ServerError::Internal(format!("Invalid OIDC token response: {e}")))?;

    let id_token = token_body.id_token.ok_or_else(|| {
        ServerError::Unauthorized("OIDC token response missing id_token".to_string())
    })?;

    let jwt_claims = verify_oidc_id_token(
        &client,
        &endpoints.jwks_uri,
        &endpoints.expected_issuer,
        &oidc.client_id,
        &id_token,
    )
    .await?;
    if let Some(expected_nonce) = pkce.nonce.as_deref()
        && jwt_claims.nonce.as_deref() != Some(expected_nonce)
    {
        return Err(ServerError::Unauthorized(
            "OIDC id_token nonce mismatch (possible token replay)".to_string(),
        ));
    }
    let phase = *state.auth_phase.read().await;
    let claims = SessionClaims {
        sub: jwt_claims.sub.clone(),
        email: jwt_claims.email,
        name: jwt_claims.name,
        groups: jwt_claims.groups,
        auth_phase: phase,
        exp: Utc::now().timestamp() + SESSION_TTL_SECONDS,
    };

    let secure = is_secure_request(headers);
    if let Some(cli) = pkce.cli {
        // CLI loopback login: hand the browser a one-time code bound to the CLI's PKCE challenge
        // and loopback URI; no browser session cookie is issued.
        let code = issue_cli_code(&session_key, &claims, &cli)?;
        let target = format!(
            "{}?code={}&state={}",
            cli.redirect_uri,
            percent_encode(&code),
            percent_encode(&cli.state)
        );
        let location = HeaderValue::from_str(&target)
            .map_err(|e| ServerError::Internal(format!("Invalid CLI redirect URL: {e}")))?;
        tracing::info!(
            target: "keel::audit",
            action = "auth.cli_code_issued",
            user = %claims.sub,
            "issued one-time CLI login code"
        );
        let mut resp = StatusCode::FOUND.into_response();
        resp.headers_mut().insert(LOCATION, location);
        resp.headers_mut()
            .append(SET_COOKIE, build_pkce_cookie_header("", 0, secure)?);
        return Ok(resp);
    }

    let session_token = encrypt_session_token(&session_key, &claims)?;
    let session_cookie = build_session_cookie_header(&session_token, SESSION_TTL_SECONDS, secure)?;
    let clear_pkce_cookie = build_pkce_cookie_header("", 0, secure)?;

    let wants_html = headers
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));

    let mut resp = if wants_html {
        let mut redirect = StatusCode::FOUND.into_response();
        redirect
            .headers_mut()
            .insert(LOCATION, HeaderValue::from_static("/"));
        redirect
    } else {
        (
            StatusCode::OK,
            Json(AuthLoginResponse {
                authenticated: true,
                sub: jwt_claims.sub,
                phase,
            }),
        )
            .into_response()
    };
    resp.headers_mut().append(SET_COOKIE, session_cookie);
    resp.headers_mut().append(SET_COOKIE, clear_pkce_cookie);
    Ok(resp)
}

/// `POST /api/v1/auth/logout`
///
/// Clears the stateless `keel_session` and `keel_pkce` cookies by setting `Max-Age=0`.
pub async fn logout(headers: HeaderMap) -> Result<Response, ServerError> {
    let secure = is_secure_request(&headers);
    let clear_session_cookie = build_session_cookie_header("", 0, secure)?;
    let clear_pkce_cookie = build_pkce_cookie_header("", 0, secure)?;
    let logged_out_cookie = build_logged_out_cookie_header("1", 86400, secure)?;

    let mut resp = (
        StatusCode::OK,
        Json(AuthLogoutResponse {
            authenticated: false,
        }),
    )
        .into_response();
    resp.headers_mut().append(SET_COOKIE, clear_session_cookie);
    resp.headers_mut().append(SET_COOKIE, clear_pkce_cookie);
    resp.headers_mut().append(SET_COOKIE, logged_out_cookie);
    Ok(resp)
}

async fn verify_oidc_id_token(
    client: &reqwest::Client,
    jwks_uri: &str,
    expected_issuer: &str,
    client_id: &str,
    id_token: &str,
) -> Result<JwtClaimsSubset, ServerError> {
    let header = decode_header(id_token)
        .map_err(|_| ServerError::Unauthorized("Malformed OIDC id_token header".to_string()))?;

    if !matches!(
        header.alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::EdDSA
    ) {
        return Err(ServerError::Unauthorized(format!(
            "Unsupported OIDC id_token signing algorithm: {:?}",
            header.alg
        )));
    }

    let jwks_resp = client
        .get(jwks_uri)
        .send()
        .await
        .map_err(|e| ServerError::Internal(format!("Failed to fetch OIDC JWKS: {e}")))?;
    if !jwks_resp.status().is_success() {
        return Err(ServerError::Unauthorized(format!(
            "OIDC JWKS endpoint returned HTTP {}",
            jwks_resp.status()
        )));
    }
    let jwks: JwkSet = jwks_resp
        .json()
        .await
        .map_err(|e| ServerError::Internal(format!("Invalid OIDC JWKS payload: {e}")))?;

    let jwk = match header.kid.as_deref() {
        Some(kid) => jwks.find(kid).ok_or_else(|| {
            ServerError::Unauthorized("OIDC id_token kid not found in JWKS".to_string())
        })?,
        None if jwks.keys.len() == 1 => &jwks.keys[0],
        None => {
            return Err(ServerError::Unauthorized(
                "OIDC id_token missing kid and JWKS contains multiple keys".to_string(),
            ));
        }
    };

    let decoding_key = DecodingKey::from_jwk(jwk).map_err(|_| {
        ServerError::Unauthorized("Invalid JWK for id_token verification".to_string())
    })?;

    let mut validation = Validation::new(header.alg);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.set_audience(&[client_id]);
    validation.set_issuer(&[expected_issuer]);

    let token_data =
        decode::<JwtClaimsSubset>(id_token, &decoding_key, &validation).map_err(|e| {
            ServerError::Unauthorized(format!("OIDC id_token verification failed: {e}"))
        })?;

    Ok(token_data.claims)
}

/// Lifetime of a one-time CLI login code (seconds).
const CLI_CODE_TTL_SECONDS: i64 = 60;

/// Discriminator embedded in CLI codes so they can never be confused with session tokens.
const CLI_CODE_TYPE: &str = "keel-cli-code";

/// Encrypted payload of a one-time CLI login code.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliCodePayload {
    typ: String,
    jti: String,
    claims: SessionClaims,
    code_challenge: String,
    redirect_uri: String,
    exp: i64,
}

/// Process-local replay cache of redeemed CLI code ids (`jti` → expiry). Codes are additionally
/// bound to the CLI-held PKCE verifier and expire after [`CLI_CODE_TTL_SECONDS`], so a code is
/// useless without the verifier even on a replica that has not seen it.
static REDEEMED_CLI_CODES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, i64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn issue_cli_code(
    session_key: &[u8; 32],
    claims: &SessionClaims,
    cli: &CliLoginBinding,
) -> Result<String, ServerError> {
    let payload = CliCodePayload {
        typ: CLI_CODE_TYPE.to_string(),
        jti: random_b64(16),
        claims: claims.clone(),
        code_challenge: cli.code_challenge.clone(),
        redirect_uri: cli.redirect_uri.clone(),
        exp: Utc::now().timestamp() + CLI_CODE_TTL_SECONDS,
    };
    let json = serde_json::to_vec(&payload)
        .map_err(|e| ServerError::Internal(format!("Failed to serialize CLI code: {e}")))?;
    let sealed = EnvelopeCrypto::new(*session_key).encrypt(&json)?;
    Ok(URL_SAFE_NO_PAD.encode(sealed.as_bytes()))
}

/// Request body for `POST /api/v1/auth/cli/exchange`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliExchangeRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
}

/// Response body for `POST /api/v1/auth/cli/exchange`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliExchangeResponse {
    /// Opaque session token; send as `Authorization: Bearer <sessionToken>`.
    pub session_token: String,
    pub token_type: String,
    /// Unix timestamp (seconds) when the session expires.
    pub expires_at: i64,
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub phase: AuthPhase,
    pub is_admin: bool,
}

/// `POST /api/v1/auth/cli/exchange`
///
/// Redeems a one-time CLI login code (from the loopback redirect) for a Bearer session token.
/// Requires the PKCE `codeVerifier` whose S256 digest was sent to `/api/v1/auth/cli/login` and
/// the exact same loopback `redirectUri`. Codes expire after 60 s and are single-use.
pub async fn cli_exchange(
    State(state): State<AppState>,
    Json(req): Json<CliExchangeRequest>,
) -> Result<Json<CliExchangeResponse>, ServerError> {
    let invalid = || ServerError::Unauthorized("Invalid or expired CLI login code".to_string());
    if req.code.len() > 16 * 1024 || !is_valid_pkce_verifier(&req.code_verifier) {
        return Err(invalid());
    }
    let session_key = state.secret_store.get_or_create_session_key().await?;
    let raw = URL_SAFE_NO_PAD
        .decode(req.code.trim())
        .map_err(|_| invalid())?;
    let sealed = std::str::from_utf8(&raw).map_err(|_| invalid())?;
    let plain = EnvelopeCrypto::new(session_key)
        .decrypt(sealed)
        .map_err(|_| invalid())?;
    let payload: CliCodePayload = serde_json::from_slice(&plain).map_err(|_| invalid())?;
    let now = Utc::now().timestamp();
    if payload.typ != CLI_CODE_TYPE || payload.exp <= now {
        return Err(invalid());
    }
    if payload.redirect_uri != req.redirect_uri.trim() {
        return Err(invalid());
    }
    let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(req.code_verifier.as_bytes()));
    if !constant_time_eq(digest.as_bytes(), payload.code_challenge.as_bytes()) {
        return Err(invalid());
    }
    {
        let mut used = REDEEMED_CLI_CODES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        used.retain(|_, exp| *exp > now);
        if used.insert(payload.jti.clone(), payload.exp).is_some() {
            return Err(invalid());
        }
    }
    let phase = *state.auth_phase.read().await;
    if payload.claims.auth_phase != phase {
        return Err(invalid());
    }

    let mut claims = payload.claims;
    claims.exp = now + SESSION_TTL_SECONDS;
    let session_token = encrypt_session_token(&session_key, &claims)?;
    tracing::info!(
        target: "keel::audit",
        action = "auth.cli_login",
        user = %claims.sub,
        "CLI session issued via loopback login"
    );
    let is_admin = AuthSession {
        claims: claims.clone(),
    }
    .is_admin();
    Ok(Json(CliExchangeResponse {
        session_token,
        token_type: "Bearer".to_string(),
        expires_at: claims.exp,
        sub: claims.sub,
        email: claims.email,
        phase,
        is_admin,
    }))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Validates an RFC 8252 loopback redirect: `http://127.0.0.1:<port>/callback` or
/// `http://[::1]:<port>/callback` with an explicit non-zero port and no userinfo/query/fragment.
fn validate_cli_loopback_redirect_uri(raw: &str) -> Result<String, ServerError> {
    let bad = || {
        ServerError::BadRequest(
            "redirect_uri must be http://127.0.0.1:<port>/callback or http://[::1]:<port>/callback"
                .to_string(),
        )
    };
    let url = Url::parse(raw.trim()).map_err(|_| bad())?;
    let loopback_host = matches!(
        url.host(),
        Some(url::Host::Ipv4(ip)) if ip == std::net::Ipv4Addr::LOCALHOST
    ) || matches!(
        url.host(),
        Some(url::Host::Ipv6(ip)) if ip == std::net::Ipv6Addr::LOCALHOST
    );
    if url.scheme() != "http"
        || !loopback_host
        || url.port().is_none_or(|p| p == 0)
        || url.path() != "/callback"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(bad());
    }
    Ok(url.to_string())
}

fn is_url_safe_token(v: &str) -> bool {
    v.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

fn is_valid_cli_state(v: &str) -> bool {
    (8..=256).contains(&v.len()) && is_url_safe_token(v)
}

fn is_valid_pkce_challenge(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// RFC 7636 §4.1: 43-128 characters of `[A-Z a-z 0-9 - . _ ~]`.
fn is_valid_pkce_verifier(v: &str) -> bool {
    (43..=128).contains(&v.len()) && is_url_safe_token(v)
}

fn is_valid_host_header(host: &str) -> bool {
    !host.is_empty()
        && !host.contains('@')
        && !host.contains('/')
        && !host.contains('\\')
        && !host.chars().any(char::is_whitespace)
}

fn validate_oidc_redirect_uri(
    requested: &str,
    configured_redirect: &str,
    request_host: Option<&str>,
) -> Result<String, ServerError> {
    let parsed = Url::parse(requested.trim())
        .map_err(|_| ServerError::BadRequest("Invalid redirect_uri".to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.username() != ""
        || parsed.password().is_some()
    {
        return Err(ServerError::BadRequest(
            "redirect_uri must be an http(s) URL without credentials".to_string(),
        ));
    }

    if let Ok(configured) = Url::parse(configured_redirect.trim())
        && parsed.scheme() == configured.scheme()
        && parsed.host_str() == configured.host_str()
        && parsed.port_or_known_default() == configured.port_or_known_default()
        && parsed.path() == configured.path()
    {
        return Ok(parsed.to_string());
    }

    if let Some(host_hdr) = request_host {
        let parsed_authority = match parsed.port() {
            Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or_default()),
            None => parsed.host_str().unwrap_or_default().to_string(),
        };
        if parsed_authority.eq_ignore_ascii_case(host_hdr)
            && parsed.path() == "/api/v1/auth/callback"
        {
            return Ok(parsed.to_string());
        }
    }

    Err(ServerError::BadRequest(
        "redirect_uri is not in the allowed OIDC callback origins".to_string(),
    ))
}

/// Enforces same-origin validation on cookie-authenticated requests (OPEN-29):
///
/// - state-changing methods (`POST`/`PUT`/`PATCH`/`DELETE`) carrying the session cookie must
///   present a same-origin `Origin`, `Referer`, or `Sec-Fetch-Site: same-origin|none`; requests
///   where all three are absent are rejected;
/// - WebSocket upgrades must carry a same-origin `Origin` when cookie-authenticated;
/// - requests authenticated with `Authorization: Bearer` (CLI) are exempt, since browsers cannot
///   attach that header cross-site without a CORS preflight.
pub(crate) async fn csrf_origin_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, ServerError> {
    let method = req.method();
    let is_mutating = matches!(
        *method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::DELETE
            | axum::http::Method::PATCH
    );
    let headers = req.headers();
    let is_ws_upgrade = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"));
    if is_mutating || is_ws_upgrade {
        let has_bearer = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer "));
        let has_session_cookie =
            extract_cookie_value(headers, SESSION_COOKIE_NAME).is_some_and(|v| !v.is_empty());

        if has_session_cookie && !has_bearer {
            // Effective request authority: `Host` (HTTP/1.1) or the URI authority (HTTP/2).
            let host_owned = headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|h| h.trim().to_string())
                .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
            let host = host_owned.as_deref();
            let origin_hdr = headers
                .get(axum::http::header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                .map(str::trim);
            let referer_hdr = headers
                .get(axum::http::header::REFERER)
                .and_then(|v| v.to_str().ok())
                .map(str::trim);
            let sec_fetch_site = headers
                .get("sec-fetch-site")
                .and_then(|v| v.to_str().ok())
                .map(str::trim);

            if let Some(site) = sec_fetch_site
                && !site.eq_ignore_ascii_case("same-origin")
                && !site.eq_ignore_ascii_case("none")
            {
                return Err(ServerError::Forbidden(
                    "Cross-site request rejected by CSRF guard".to_string(),
                ));
            }

            if let Some(origin_raw) = origin_hdr {
                let origin_url = Url::parse(origin_raw)
                    .map_err(|_| ServerError::Forbidden("Invalid Origin header".to_string()))?;
                if !url_matches_host(&origin_url, host) {
                    return Err(ServerError::Forbidden(
                        "Cross-origin request rejected by CSRF guard".to_string(),
                    ));
                }
            } else if is_ws_upgrade && host.is_some() {
                return Err(ServerError::Forbidden(
                    "WebSocket upgrade requires a same-origin Origin header".to_string(),
                ));
            } else if let Some(referer_raw) = referer_hdr {
                let referer_url = Url::parse(referer_raw)
                    .map_err(|_| ServerError::Forbidden("Invalid Referer header".to_string()))?;
                if !url_matches_host(&referer_url, host) {
                    return Err(ServerError::Forbidden(
                        "Cross-origin Referer rejected by CSRF guard".to_string(),
                    ));
                }
            } else if sec_fetch_site.is_none() && host.is_some() {
                // No provenance signal at all on a cookie-authenticated mutation: fail closed.
                // (Requests without any authority cannot originate from a browser.)
                return Err(ServerError::Forbidden(
                    "Cookie-authenticated mutation without Origin/Referer/Sec-Fetch-Site rejected by CSRF guard"
                        .to_string(),
                ));
            }
        }
    }
    Ok(next.run(req).await)
}

fn url_matches_host(url: &Url, host_hdr: Option<&str>) -> bool {
    let Some(expected_host) = host_hdr else {
        return false;
    };
    let Some(url_host) = url.host_str() else {
        return false;
    };
    let authority = match url.port() {
        Some(p) => format!("{url_host}:{p}"),
        None => url_host.to_string(),
    };
    authority.eq_ignore_ascii_case(expected_host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::{get, post};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use keel_core::OidcConfig;
    use keel_git::{GitRunner, LocalGitProvider};
    use keel_kube::{InMemorySecretStore, TelemetryMux};
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn admin_groups_extend_built_ins_without_prefix_matching() {
        let extra = parse_admin_groups(Some(" platform-ops , ,team-a:admins "));
        assert_eq!(extra, vec!["platform-ops", "team-a:admins"]);
        assert!(parse_admin_groups(None).is_empty());
        assert!(parse_admin_groups(Some(" , ")).is_empty());
        for group in [
            "keel-admins",
            "admin",
            "admins",
            "platform-ops",
            "team-a:admins",
        ] {
            assert!(is_admin_group(group, &extra), "{group}");
        }
        for group in [
            "platform",
            "Platform-Ops",
            "tenant-acme-admin",
            "",
            "keel-admins-x",
        ] {
            assert!(!is_admin_group(group, &extra), "{group}");
        }
        assert!(!is_admin_group("platform-ops", &[]));
    }

    #[test]
    fn token_exchange_sends_client_secret_only_for_confidential_clients() {
        let public = token_exchange_form("c", "https://keel.example/cb", "keel", "v", None);
        assert!(public.iter().all(|(k, _)| *k != "client_secret"));
        assert!(public.contains(&("code_verifier", "v")));
        assert!(public.contains(&("client_id", "keel")));
        let confidential =
            token_exchange_form("c", "https://keel.example/cb", "keel", "v", Some("s3cr3t"));
        assert!(confidential.contains(&("client_secret", "s3cr3t")));
        assert!(confidential.contains(&("code_verifier", "v")));
    }

    #[test]
    fn oidc_client_secret_is_trimmed_and_blank_means_public_client() {
        let state = AppState::default_ephemeral();
        assert!(state.oidc_client_secret.is_none());
        let state = state.with_oidc_client_secret(Some("  ".to_string()));
        assert!(state.oidc_client_secret.is_none());
        let state = state.with_oidc_client_secret(Some(" abc \n".to_string()));
        assert_eq!(state.oidc_client_secret.as_deref(), Some("abc"));
    }

    const TEST_ED25519_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIEHTjl7kWr8Ms/aazqxV5Dg4GBB6U7dw6rkd1ozfHJpc\n-----END PRIVATE KEY-----\n";
    const TEST_ED25519_X: &str = "MzdB74SujNsw41A0r_Z-PuCEw_OqK4KToymtSTaOUA8";

    async fn make_auth_state_async(oidc: Option<OidcConfig>) -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        let workdir = tmp.path().join("work");
        tokio::fs::create_dir_all(&bare).await.unwrap();
        tokio::fs::create_dir_all(&workdir).await.unwrap();
        let state = AppState::new(
            Arc::new(LocalGitProvider::new(&bare)),
            Arc::new(InMemorySecretStore::new()),
            TelemetryMux::default(),
            GitRunner::new(),
            &workdir,
            bare.to_string_lossy().to_string(),
            AuthPhase::OidcActive,
        );
        if let Some(cfg) = oidc {
            *state.oidc_config.write().await = Some(cfg);
        }
        (tmp, state)
    }

    #[test]
    fn auth_session_rbac_and_token_roundtrip() {
        let admin = AuthSession {
            claims: SessionClaims {
                sub: "alice".to_string(),
                email: Some("alice@keel.local".to_string()),
                name: Some("Alice".to_string()),
                groups: vec!["keel-admins".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(admin.is_admin());
        assert!(admin.require_admin().is_ok());
        assert!(admin.is_tenant_admin("acme"));
        assert!(admin.require_tenant_admin("acme").is_ok());
        assert!(admin.require_tenant_access("acme").is_ok());

        // Break-glass phase grants admin only to the allowlisted bootstrap admin identity.
        let break_glass_admin = AuthSession {
            claims: SessionClaims {
                sub: "CiQ0ZDk5".to_string(),
                email: Some("Admin@Keel.Local".to_string()),
                name: None,
                groups: vec![],
                auth_phase: AuthPhase::BreakGlass,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(break_glass_admin.is_admin());
        let break_glass_other = AuthSession {
            claims: SessionClaims {
                sub: "mallory".to_string(),
                email: Some("mallory@keel.local".to_string()),
                name: None,
                groups: vec!["tenant-acme".to_string()],
                auth_phase: AuthPhase::BreakGlass,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(!break_glass_other.is_admin());
        assert!(break_glass_other.require_tenant_access("acme").is_ok());
        // Same identity outside break-glass is an ordinary user.
        let mut normal_phase_admin_email = break_glass_admin.clone();
        normal_phase_admin_email.claims.auth_phase = AuthPhase::OidcActive;
        assert!(!normal_phase_admin_email.is_admin());

        // Bare group names equal to a tenant id (or `<id>-admin`) grant nothing.
        let bare_groups = AuthSession {
            claims: SessionClaims {
                sub: "eve".to_string(),
                email: None,
                name: None,
                groups: vec!["acme".to_string(), "acme-admin".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(!bare_groups.is_tenant_admin("acme"));
        assert!(bare_groups.require_tenant_access("acme").is_err());
        assert!(!bare_groups.is_any_tenant_admin());

        let tenant_admin = AuthSession {
            claims: SessionClaims {
                sub: "bob".to_string(),
                email: None,
                name: None,
                groups: vec!["tenant-acme-admin".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(!tenant_admin.is_admin());
        assert!(tenant_admin.require_admin().is_err());
        assert!(tenant_admin.is_tenant_admin("acme"));
        assert!(!tenant_admin.is_tenant_admin("other"));
        assert!(tenant_admin.require_tenant_admin("other").is_err());
        assert!(tenant_admin.require_tenant_access("acme").is_ok());
        assert!(tenant_admin.require_tenant_access("other").is_err());
        assert!(tenant_admin.is_any_tenant_admin());

        let tenant_member = AuthSession {
            claims: SessionClaims {
                sub: "carol".to_string(),
                email: None,
                name: None,
                groups: vec!["tenant-acme".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(!tenant_member.is_tenant_admin("acme"));
        assert!(!tenant_member.is_any_tenant_admin());
        assert!(tenant_member.require_tenant_access("acme").is_ok());

        let key = [7u8; 32];
        let token = encrypt_session_token(&key, &admin.claims).unwrap();
        let decoded = decrypt_session_token(&key, &token).unwrap();
        assert_eq!(decoded, admin.claims);
        assert!(decrypt_session_token(&key, "!!!not-base64!!!").is_err());
        assert!(decrypt_session_token(&[8u8; 32], &token).is_err());
    }

    #[test]
    fn tenant_groups_name_tenants_and_never_the_management_plane() {
        let session = |groups: &[&str]| AuthSession {
            claims: SessionClaims {
                sub: "u".to_string(),
                email: None,
                name: None,
                groups: groups.iter().map(|g| g.to_string()).collect(),
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        let user = session(&[
            "tenant-acme",
            "tenant-beta-admin",
            "tenant-acme-admin",
            "tenant-management",
            "tenant-management-admin",
            "tenant-Bad_Id",
            "developers",
        ]);
        assert_eq!(user.tenant_group_ids(), vec!["acme", "beta"]);
        // An IdP group named after the management pseudo-tenant grants nothing.
        let mgmt = session(&["tenant-management", "tenant-management-admin"]);
        assert!(
            mgmt.require_tenant_access(crate::state::MANAGEMENT_TENANT)
                .is_err()
        );
        assert!(!mgmt.is_tenant_admin(crate::state::MANAGEMENT_TENANT));
        assert!(!mgmt.is_any_tenant_admin());
        assert!(mgmt.tenant_group_ids().is_empty());
        // Cluster administrators keep the management plane.
        let admin = session(&["keel-admins"]);
        assert!(
            admin
                .require_tenant_access(crate::state::MANAGEMENT_TENANT)
                .is_ok()
        );
        assert!(admin.tenant_group_ids().is_empty());
    }

    #[test]
    fn redirect_uri_and_host_validation_cases() {
        assert!(is_valid_host_header("keel.local:8080"));
        assert!(!is_valid_host_header(""));
        assert!(!is_valid_host_header("evil@keel.local"));
        assert!(!is_valid_host_header("keel.local/path"));

        assert!(
            validate_oidc_redirect_uri(
                "http://127.0.0.1:8080/api/v1/auth/callback",
                "http://127.0.0.1:8080/api/v1/auth/callback",
                None
            )
            .is_ok()
        );
        assert!(
            validate_oidc_redirect_uri(
                "https://keel.example.com/api/v1/auth/callback",
                "http://127.0.0.1:8080/api/v1/auth/callback",
                Some("keel.example.com")
            )
            .is_ok()
        );
        assert!(
            validate_oidc_redirect_uri(
                "ftp://keel.example.com/cb",
                "http://127.0.0.1:8080/api/v1/auth/callback",
                None
            )
            .is_err()
        );
        assert!(
            validate_oidc_redirect_uri(
                "https://evil.example.com/api/v1/auth/callback",
                "http://127.0.0.1:8080/api/v1/auth/callback",
                Some("keel.example.com")
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn auth_session_extractor_and_status_and_logout() {
        let (_tmp, state) = make_auth_state_async(None).await;
        let key = state
            .secret_store
            .get_or_create_session_key()
            .await
            .unwrap();

        let app = Router::new()
            .route("/status", get(auth_status))
            .route("/logout", post(logout))
            .route(
                "/protected",
                get(|session: AuthSession| async move { session.claims.sub }),
            )
            .with_state(state.clone());

        // Unauthenticated status
        let resp = app
            .clone()
            .oneshot(Request::get("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let status: AuthStatusResponse = serde_json::from_slice(&body).unwrap();
        assert!(!status.authenticated);
        assert!(!status.oidc_enabled);

        // Missing session -> 401
        let resp = app
            .clone()
            .oneshot(Request::get("/protected").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Expired session -> 401
        let expired_claims = SessionClaims {
            sub: "expired".to_string(),
            email: None,
            name: None,
            groups: vec![],
            auth_phase: AuthPhase::OidcActive,
            exp: Utc::now().timestamp() - 10,
        };
        let expired_tok = encrypt_session_token(&key, &expired_claims).unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::get("/protected")
                    .header(AUTHORIZATION, format!("Bearer {expired_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Session issued under a different auth phase -> 401 (re-evaluated on phase change)
        let other_phase = SessionClaims {
            sub: "alice".to_string(),
            email: None,
            name: None,
            groups: vec!["keel-admins".to_string()],
            auth_phase: AuthPhase::BreakGlass,
            exp: Utc::now().timestamp() + 3600,
        };
        let other_phase_tok = encrypt_session_token(&key, &other_phase).unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::get("/protected")
                    .header(AUTHORIZATION, format!("Bearer {other_phase_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Valid session cookie -> 200 and authenticated status
        let valid_claims = SessionClaims {
            sub: "alice".to_string(),
            email: Some("alice@keel.local".to_string()),
            name: Some("Alice".to_string()),
            groups: vec!["keel-admins".to_string()],
            auth_phase: AuthPhase::OidcActive,
            exp: Utc::now().timestamp() + 3600,
        };
        let valid_tok = encrypt_session_token(&key, &valid_claims).unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::get("/protected")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}={valid_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::get("/status")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}={valid_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let status: AuthStatusResponse = serde_json::from_slice(&body).unwrap();
        assert!(status.authenticated);
        assert!(status.is_admin);
        assert_eq!(status.sub.as_deref(), Some("alice"));

        // Logout clears cookies
        let resp = app
            .oneshot(Request::post("/logout").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookies: Vec<&str> = resp
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(cookies.iter().any(|c| c.contains("keel_session=;")));
        assert!(cookies.iter().any(|c| c.contains("keel_logged_out=1;")));
    }

    #[tokio::test]
    async fn csrf_origin_guard_blocks_cross_origin_cookie_mutations() {
        let app = Router::new()
            .route("/mutate", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn(csrf_origin_guard));

        // Cross-origin Origin header with session cookie -> 403
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .header("origin", "https://evil.example.com")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Cross-origin Referer header with session cookie -> 403
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .header("referer", "https://evil.example.com/page")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Same-origin Origin header -> 200
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local:8080")
                    .header("origin", "http://keel.local:8080")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Cross-site Sec-Fetch-Site header without Origin/Referer -> 403
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .header("sec-fetch-site", "cross-site")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Cookie-authenticated mutation with no Origin/Referer/Sec-Fetch-Site -> 403
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Sec-Fetch-Site: same-origin alone is sufficient provenance -> 200
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .header("sec-fetch-site", "same-origin")
                    .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Cookie-less mutation (e.g. CLI code exchange) is not subject to the cookie CSRF guard
        let resp = app
            .clone()
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // WebSocket upgrades: cookie-authenticated without Origin -> 403, cross-origin -> 403,
        // same-origin -> passes through to the handler.
        let ws_app = Router::new()
            .route("/ws", get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn(csrf_origin_guard));
        for (origin, expected) in [
            (None, StatusCode::FORBIDDEN),
            (Some("https://evil.example.com"), StatusCode::FORBIDDEN),
            (Some("https://keel.local"), StatusCode::OK),
        ] {
            let mut builder = Request::get("/ws")
                .header("host", "keel.local")
                .header("upgrade", "websocket")
                .header("connection", "upgrade")
                .header(COOKIE, format!("{SESSION_COOKIE_NAME}=tok"));
            if let Some(o) = origin {
                builder = builder.header("origin", o);
            }
            let resp = ws_app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), expected, "origin={origin:?}");
        }

        // Bearer token bypasses cookie CSRF guard -> 200
        let resp = app
            .oneshot(
                Request::post("/mutate")
                    .header("host", "keel.local")
                    .header("origin", "https://evil.example.com")
                    .header(AUTHORIZATION, "Bearer cli-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oidc_login_and_callback_end_to_end_with_mock_idp() {
        // Spin up a mock OIDC IdP with discovery, token, and JWKS endpoints
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idp_addr = listener.local_addr().unwrap();
        let issuer = format!("http://127.0.0.1:{}", idp_addr.port());
        let issuer_clone = issuer.clone();
        // Nonce the mock IdP embeds in issued id_tokens (set from each authorization request).
        let idp_nonce: Arc<std::sync::Mutex<String>> = Arc::default();
        let idp_nonce_tok = idp_nonce.clone();

        let idp_router = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get({
                    let iss = issuer_clone.clone();
                    move || {
                        let iss = iss.clone();
                        async move {
                            Json(json!({
                                "issuer": iss,
                                "authorization_endpoint": format!("{iss}/auth"),
                                "token_endpoint": format!("{iss}/token"),
                                "jwks_uri": format!("{iss}/keys")
                            }))
                        }
                    }
                }),
            )
            .route(
                "/keys",
                get(|| async {
                    Json(json!({
                        "keys": [{
                            "kty": "OKP",
                            "crv": "Ed25519",
                            "x": TEST_ED25519_X,
                            "kid": "test-ed25519",
                            "alg": "EdDSA",
                            "use": "sig"
                        }]
                    }))
                }),
            )
            .route(
                "/token",
                post({
                    let iss = issuer_clone.clone();
                    move || {
                        let iss = iss.clone();
                        let nonce = idp_nonce_tok.lock().unwrap().clone();
                        async move {
                            let mut header = Header::new(Algorithm::EdDSA);
                            header.kid = Some("test-ed25519".to_string());
                            let exp = Utc::now().timestamp() + 300;
                            let claims = json!({
                                "iss": iss,
                                "aud": "keel-cli",
                                "sub": "user-123",
                                "email": "user@keel.local",
                                "name": "Test User",
                                "groups": ["keel-admins"],
                                "nonce": nonce,
                                "exp": exp,
                                "iat": Utc::now().timestamp()
                            });
                            let key =
                                EncodingKey::from_ed_pem(TEST_ED25519_PEM.as_bytes()).unwrap();
                            let id_token = encode(&header, &claims, &key).unwrap();
                            Json(json!({ "id_token": id_token }))
                        }
                    }
                }),
            );

        tokio::spawn(async move {
            axum::serve(listener, idp_router).await.unwrap();
        });

        let oidc_cfg = OidcConfig {
            enabled: true,
            issuer_url: issuer.clone(),
            client_id: "keel-cli".to_string(),
            redirect_url: "http://127.0.0.1:8080/api/v1/auth/callback".to_string(),
            scopes: vec!["openid".to_string(), "profile".to_string()],
        };
        let (_tmp, state) = make_auth_state_async(Some(oidc_cfg)).await;
        let app = Router::new()
            .route("/api/v1/auth/login", get(oidc_login))
            .route("/api/v1/auth/callback", get(oidc_callback))
            .route("/api/v1/auth/cli/login", get(cli_login))
            .route("/api/v1/auth/cli/exchange", post(cli_exchange))
            .with_state(state.clone());
        let query_param = |loc: &str, key: &str| {
            Url::parse(loc)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        };

        // 1. Login initiates PKCE redirect and sets keel_pkce cookie
        let login_resp = app
            .clone()
            .oneshot(
                Request::get("/api/v1/auth/login?prompt=login")
                    .header("host", "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login_resp.status(), StatusCode::FOUND);
        let location = login_resp
            .headers()
            .get(LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.contains("code_challenge_method=S256"));
        assert!(location.contains("prompt=login"));
        let nonce = query_param(&location, "nonce").expect("nonce in authorization request");
        assert!(nonce.len() >= 16);
        *idp_nonce.lock().unwrap() = nonce.clone();
        let loc_url = Url::parse(&location).unwrap();
        let state_param = loc_url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();

        let pkce_cookie = login_resp
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with("keel_pkce="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        // 2. Callback with valid code + state + PKCE cookie returns JSON session
        let cb_resp = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/auth/callback?code=auth-code-1&state={state_param}"
                ))
                .header(COOKIE, &pkce_cookie)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cb_resp.status(), StatusCode::OK);
        let cb_cookies: Vec<&str> = cb_resp
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(cb_cookies.iter().any(|c| c.starts_with("keel_session=")));

        // 3. Callback with Accept: text/html redirects to /
        let html_resp = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/auth/callback?code=auth-code-2&state={state_param}"
                ))
                .header(COOKIE, &pkce_cookie)
                .header(ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(html_resp.status(), StatusCode::FOUND);
        assert_eq!(html_resp.headers().get(LOCATION).unwrap(), "/");

        // 4. Error paths on callback
        let err_resp = app
            .clone()
            .oneshot(
                Request::get("/api/v1/auth/callback?error=access_denied")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(err_resp.status(), StatusCode::UNAUTHORIZED);

        let mismatch_resp = app
            .clone()
            .oneshot(
                Request::get("/api/v1/auth/callback?code=c&state=wrong")
                    .header(COOKIE, &pkce_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mismatch_resp.status(), StatusCode::UNAUTHORIZED);

        let missing_code = app
            .clone()
            .oneshot(
                Request::get("/api/v1/auth/callback?state=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_code.status(), StatusCode::BAD_REQUEST);

        // 5. id_token carrying a different nonce is rejected (replay protection)
        *idp_nonce.lock().unwrap() = "some-other-nonce".to_string();
        let replay = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/auth/callback?code=auth-code-3&state={state_param}"
                ))
                .header(COOKIE, &pkce_cookie)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

        // 6. CLI loopback login: invalid loopback/challenge inputs are rejected up front
        let verifier = URL_SAFE_NO_PAD.encode([42u8; 32]);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let loopback = "http://127.0.0.1:53682/callback";
        for bad_query in [
            format!(
                "redirect_uri={}&state=cli-state-123&code_challenge={challenge}",
                percent_encode("http://evil.example.com:53682/callback")
            ),
            format!(
                "redirect_uri={}&state=cli-state-123&code_challenge={challenge}",
                percent_encode("http://127.0.0.1/callback")
            ),
            format!(
                "redirect_uri={}&state=x&code_challenge={challenge}",
                percent_encode(loopback)
            ),
            format!(
                "redirect_uri={}&state=cli-state-123&code_challenge=short",
                percent_encode(loopback)
            ),
            format!(
                "redirect_uri={}&state=cli-state-123&code_challenge={challenge}&code_challenge_method=plain",
                percent_encode(loopback)
            ),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::get(format!("/api/v1/auth/cli/login?{bad_query}"))
                        .header("host", "127.0.0.1:8080")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad_query}");
        }

        // 7. CLI loopback login happy path: login -> IdP -> callback -> loopback code -> exchange
        let cli_login_resp = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/auth/cli/login?redirect_uri={}&state=cli-state-123&code_challenge={challenge}&code_challenge_method=S256",
                    percent_encode(loopback)
                ))
                .header("host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cli_login_resp.status(), StatusCode::FOUND);
        let idp_location = cli_login_resp
            .headers()
            .get(LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        // The IdP is always redirected back to the server callback, never to the loopback URI.
        assert_eq!(
            query_param(&idp_location, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:8080/api/v1/auth/callback")
        );
        *idp_nonce.lock().unwrap() = query_param(&idp_location, "nonce").unwrap();
        let server_state = query_param(&idp_location, "state").unwrap();
        let cli_pkce_cookie = cli_login_resp
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with("keel_pkce="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let cli_cb = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/auth/callback?code=auth-code-cli&state={server_state}"
                ))
                .header(COOKIE, &cli_pkce_cookie)
                .header(ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cli_cb.status(), StatusCode::FOUND);
        assert!(
            !cli_cb
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .any(|c| c.starts_with("keel_session=") && !c.starts_with("keel_session=;")),
            "CLI login must not set a browser session cookie"
        );
        let loopback_location = cli_cb
            .headers()
            .get(LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(loopback_location.starts_with("http://127.0.0.1:53682/callback?code="));
        assert_eq!(
            query_param(&loopback_location, "state").as_deref(),
            Some("cli-state-123")
        );
        let cli_code = query_param(&loopback_location, "code").unwrap();

        let exchange = |code: String, verifier: String, redirect: &str| {
            Request::post("/api/v1/auth/cli/exchange")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "code": code, "codeVerifier": verifier, "redirectUri": redirect })
                        .to_string(),
                ))
                .unwrap()
        };
        // Wrong verifier and wrong redirect URI are rejected without consuming the code.
        let wrong_verifier = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let resp = app
            .clone()
            .oneshot(exchange(cli_code.clone(), wrong_verifier, loopback))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = app
            .clone()
            .oneshot(exchange(
                cli_code.clone(),
                verifier.clone(),
                "http://127.0.0.1:1/callback",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(exchange(cli_code.clone(), verifier.clone(), loopback))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let exchanged: CliExchangeResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(exchanged.token_type, "Bearer");
        assert_eq!(exchanged.sub, "user-123");
        assert!(exchanged.is_admin);
        let key = state
            .secret_store
            .get_or_create_session_key()
            .await
            .unwrap();
        let claims = decrypt_session_token(&key, &exchanged.session_token).unwrap();
        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.exp, exchanged.expires_at);

        // Single use: replaying the code fails.
        let resp = app
            .clone()
            .oneshot(exchange(cli_code.clone(), verifier.clone(), loopback))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // A CLI code is never accepted as a session token.
        assert!(decrypt_session_token(&key, &cli_code).is_err());
    }

    #[test]
    fn cli_loopback_and_pkce_validators() {
        assert_eq!(
            validate_cli_loopback_redirect_uri("http://127.0.0.1:8123/callback").unwrap(),
            "http://127.0.0.1:8123/callback"
        );
        assert!(validate_cli_loopback_redirect_uri("http://[::1]:8123/callback").is_ok());
        for bad in [
            "https://127.0.0.1:8123/callback",
            "http://localhost:8123/callback",
            "http://127.0.0.2:8123/callback",
            "http://127.0.0.1/callback",
            "http://127.0.0.1:8123/other",
            "http://127.0.0.1:8123/callback?x=1",
            "http://127.0.0.1:8123/callback#frag",
            "http://user@127.0.0.1:8123/callback",
            "http://10.0.0.1:8123/callback",
        ] {
            assert!(validate_cli_loopback_redirect_uri(bad).is_err(), "{bad}");
        }
        assert!(is_valid_cli_state("abcdefgh"));
        assert!(!is_valid_cli_state("short"));
        assert!(!is_valid_cli_state("has space in it"));
        assert!(is_valid_pkce_challenge(&"a".repeat(43)));
        assert!(!is_valid_pkce_challenge(&"a".repeat(42)));
        assert!(!is_valid_pkce_challenge(&format!("{}=", "a".repeat(43))));
        assert!(is_valid_pkce_verifier(&"a".repeat(43)));
        assert!(!is_valid_pkce_verifier(&"a".repeat(129)));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn discovery_failure_message_gives_hosts_hint_only_for_unresolved_dns_names() {
        let dns = "error sending request: client error (Connect): dns error: failed to lookup \
                   address information: Name or service not known";
        let msg = oidc_discovery_failure_message("https://idp.keel.local:8888/auth/v1/", dns);
        assert!(
            msg.contains("`127.0.0.1 idp.keel.local` to /etc/hosts"),
            "{msg}"
        );

        // Connection refused on a resolvable host (e.g. 8888 held by another process).
        let refused = "error sending request: client error (Connect): tcp connect error: \
                       Connection refused (os error 111)";
        let msg = oidc_discovery_failure_message("https://idp.keel.local:8888/auth/v1", refused);
        assert!(!msg.contains("/etc/hosts"), "{msg}");
        assert!(msg.contains("listening on `idp.keel.local:8888`"), "{msg}");

        // IP-literal and localhost issuers never get the hosts-file advice.
        for issuer in ["http://127.0.0.1:5556", "http://localhost:5556/dex"] {
            for cause in [dns, refused] {
                let msg = oidc_discovery_failure_message(issuer, cause);
                assert!(!msg.contains("/etc/hosts"), "{msg}");
                assert!(msg.contains(":5556`"), "{msg}");
            }
        }
        let msg = oidc_discovery_failure_message("https://idp.example.com", refused);
        assert!(msg.contains("`idp.example.com:443`"), "{msg}");
    }

    #[test]
    fn discovery_client_message_hides_the_cause() {
        let msg = oidc_discovery_client_message("https://idp.keel.local/");
        assert_eq!(
            msg,
            "OIDC discovery for the configured issuer `https://idp.keel.local` failed, so logins \
             cannot start; see the keel-server (`keel serve`) log for the cause and fix"
        );
    }

    #[test]
    fn browser_error_response_renders_escaped_html_only_for_browser_navigations() {
        let err = || ServerError::ServiceUnavailable("<script>alert('x')</script> & co".into());
        let mut headers = HeaderMap::new();
        let json = browser_error_response(&headers, err());
        assert_eq!(json.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            json.headers()[axum::http::header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );

        headers.insert(
            ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"),
        );
        let html = browser_error_response(&headers, err());
        assert_eq!(html.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            html.headers()[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let body =
            futures::executor::block_on(axum::body::to_bytes(html.into_body(), 64 * 1024)).unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains("<script>"), "{body}");
        assert!(
            body.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; co"),
            "{body}"
        );
    }
}
