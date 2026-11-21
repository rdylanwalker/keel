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

impl AuthSession {
    /// Returns `true` when the caller holds cluster-administrator privileges.
    pub fn is_admin(&self) -> bool {
        self.claims.auth_phase == AuthPhase::BreakGlass
            || self
                .claims
                .groups
                .iter()
                .any(|g| matches!(g.as_str(), "keel-admins" | "admin" | "admins"))
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
    /// (member of `keel-admins`, `tenant-<tenant_id>-admin`, or `<tenant_id>-admin`).
    pub fn is_tenant_admin(&self, tenant_id: &str) -> bool {
        if self.is_admin() {
            return true;
        }
        let prefixed = format!("tenant-{tenant_id}-admin");
        let short = format!("{tenant_id}-admin");
        self.claims
            .groups
            .iter()
            .any(|g| g == &prefixed || g == &short)
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

    /// Enforces access to `tenant_id` (cluster admin, tenant admin, or member of `<tenant_id>` / `tenant-<tenant_id>`).
    pub fn require_tenant_access(&self, tenant_id: &str) -> Result<(), ServerError> {
        if self.is_tenant_admin(tenant_id) {
            return Ok(());
        }
        let prefixed = format!("tenant-{tenant_id}");
        if self
            .claims
            .groups
            .iter()
            .any(|g| g == tenant_id || g == &prefixed)
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

async fn resolve_oidc_endpoints(issuer_url: &str, ca_pem: Option<&[u8]>) -> ResolvedOidcEndpoints {
    let issuer = issuer_url.trim_end_matches('/');
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    if let Ok(client) = build_oidc_http_client(2, ca_pem)
        && let Ok(resp) = client.get(&discovery_url).send().await
        && resp.status().is_success()
        && let Ok(doc) = resp.json::<OidcDiscoveryDocument>().await
    {
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
            return ResolvedOidcEndpoints {
                authorization_endpoint: format!("{issuer}/auth"),
                token_endpoint: format!("{issuer}/token"),
                jwks_uri: format!("{issuer}/keys"),
                expected_issuer,
            };
        }
        return ResolvedOidcEndpoints {
            authorization_endpoint: doc.authorization_endpoint,
            token_endpoint: doc.token_endpoint,
            jwks_uri: doc.jwks_uri.unwrap_or_else(|| format!("{issuer}/keys")),
            expected_issuer,
        };
    }
    // Standard Dex OIDC endpoints fallback
    ResolvedOidcEndpoints {
        authorization_endpoint: format!("{issuer}/auth"),
        token_endpoint: format!("{issuer}/token"),
        jwks_uri: format!("{issuer}/keys"),
        expected_issuer: issuer.to_string(),
    }
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
}

#[derive(Debug, Deserialize)]
pub struct OidcLoginQuery {
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// `GET /api/v1/auth/login`
///
/// Initiates the OIDC Authorization Code + PKCE (`S256`) flow, storing the PKCE verifier
/// in a short-lived `AES-256-GCM` encrypted `HttpOnly; SameSite=Lax` cookie.
pub async fn oidc_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OidcLoginQuery>,
) -> Result<Response, ServerError> {
    let oidc_guard = state.oidc_config.read().await;
    let Some(oidc) = oidc_guard.as_ref().filter(|c| c.enabled) else {
        return Err(ServerError::ServiceUnavailable(
            "OIDC is not configured".to_string(),
        ));
    };

    let mut verifier_bytes = [0u8; 32];
    let mut state_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut verifier_bytes);
    rand::rng().fill_bytes(&mut state_bytes);

    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let state_nonce = URL_SAFE_NO_PAD.encode(state_bytes);
    let challenge_digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(challenge_digest);

    let secure = is_secure_request(&headers);
    let validated_host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| is_valid_host_header(h));

    let redirect_uri = if let Some(requested) = query.redirect_uri.filter(|u| !u.trim().is_empty())
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

    let endpoints = resolve_oidc_endpoints(&oidc.issuer_url, state.oidc_ca_pem.as_deref()).await;
    let authorization_endpoint = endpoints.authorization_endpoint;
    let scopes = oidc.scopes.join("%20");
    let sep = if authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let force_reauth = query.prompt.as_deref() == Some("login")
        || extract_cookie_value(&headers, LOGGED_OUT_COOKIE_NAME).is_some_and(|v| v == "1");
    let prompt_param = if force_reauth { "&prompt=login" } else { "" };
    let auth_url = format!(
        "{authorization_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&scope={scopes}&state={state_nonce}&code_challenge={code_challenge}&code_challenge_method=S256{prompt_param}",
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

    let pkce_cookie_raw = extract_cookie_value(&headers, PKCE_COOKIE_NAME)
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

    let endpoints = resolve_oidc_endpoints(&oidc.issuer_url, state.oidc_ca_pem.as_deref()).await;
    let client = build_oidc_http_client(5, state.oidc_ca_pem.as_deref())?;
    let token_res = client
        .post(&endpoints.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", pkce.redirect_uri.as_str()),
            ("client_id", oidc.client_id.as_str()),
            ("code_verifier", pkce.code_verifier.as_str()),
        ])
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
    let phase = *state.auth_phase.read().await;
    let claims = SessionClaims {
        sub: jwt_claims.sub.clone(),
        email: jwt_claims.email,
        name: jwt_claims.name,
        groups: jwt_claims.groups,
        auth_phase: phase,
        exp: Utc::now().timestamp() + SESSION_TTL_SECONDS,
    };

    let secure = is_secure_request(&headers);
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

/// Enforces same-origin `Origin` / `Referer` validation on cookie-authenticated state-changing requests.
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
    if is_mutating {
        let headers = req.headers();
        let has_bearer = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer "));
        let has_session_cookie =
            extract_cookie_value(headers, SESSION_COOKIE_NAME).is_some_and(|v| !v.is_empty());

        if has_session_cookie && !has_bearer {
            let host = headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::trim);
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
            } else if let Some(referer_raw) = referer_hdr {
                let referer_url = Url::parse(referer_raw)
                    .map_err(|_| ServerError::Forbidden("Invalid Referer header".to_string()))?;
                if !url_matches_host(&referer_url, host) {
                    return Err(ServerError::Forbidden(
                        "Cross-origin Referer rejected by CSRF guard".to_string(),
                    ));
                }
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

        let break_glass = AuthSession {
            claims: SessionClaims {
                sub: "breakglass".to_string(),
                email: None,
                name: None,
                groups: vec![],
                auth_phase: AuthPhase::BreakGlass,
                exp: Utc::now().timestamp() + 3600,
            },
        };
        assert!(break_glass.is_admin());

        let tenant_admin = AuthSession {
            claims: SessionClaims {
                sub: "bob".to_string(),
                email: None,
                name: None,
                groups: vec!["acme-admin".to_string()],
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
        assert!(tenant_member.require_tenant_access("acme").is_ok());

        let key = [7u8; 32];
        let token = encrypt_session_token(&key, &admin.claims).unwrap();
        let decoded = decrypt_session_token(&key, &token).unwrap();
        assert_eq!(decoded, admin.claims);
        assert!(decrypt_session_token(&key, "!!!not-base64!!!").is_err());
        assert!(decrypt_session_token(&[8u8; 32], &token).is_err());
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
            .with_state(state.clone());

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
            .oneshot(
                Request::get("/api/v1/auth/callback?state=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_code.status(), StatusCode::BAD_REQUEST);
    }
}
