//! CLI authentication (`keel auth login|status|logout`, `SDD.md` §6.1, §7.3, `NFR-SEC-01`).
//!
//! `keel auth login` implements the RFC 8252 native-app loopback flow against keel-server:
//! the CLI listens on `http://127.0.0.1:<random>/callback`, opens
//! `GET /api/v1/auth/cli/login?redirect_uri=…&state=…&code_challenge=…&code_challenge_method=S256`,
//! verifies `state` on the callback, and exchanges the single-use code with its PKCE verifier at
//! `POST /api/v1/auth/cli/exchange` for an opaque session token stored `0600`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use keel_core::EnvelopeCrypto;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

use crate::cli::{AuthLoginArgs, AuthServerArgs};

use super::{CliContext, CliError, extract_api_error, normalize_server_url};

/// Server endpoint starting the CLI loopback login.
pub const CLI_LOGIN_PATH: &str = "/api/v1/auth/cli/login";
/// Server endpoint exchanging the one-time code for a session token.
pub const CLI_EXCHANGE_PATH: &str = "/api/v1/auth/cli/exchange";

/// PKCE `S256` verifier/challenge pair (`NFR-SEC-01`).
#[derive(Clone, PartialEq, Eq)]
pub struct PkceChallenge {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

impl std::fmt::Debug for PkceChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkceChallenge")
            .field("code_verifier", &"[REDACTED]")
            .field("code_challenge", &self.code_challenge)
            .field("state", &self.state)
            .finish()
    }
}

/// Generates a fresh 256-bit PKCE `S256` verifier (43 chars) and a 128-bit CSRF state nonce.
pub fn generate_pkce_pair() -> PkceChallenge {
    let verifier_bytes = Zeroizing::new(EnvelopeCrypto::generate_key());
    let state_bytes = EnvelopeCrypto::generate_key();
    let code_verifier = URL_SAFE_NO_PAD.encode(*verifier_bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(digest);
    let state = URL_SAFE_NO_PAD.encode(&state_bytes[..16]);
    PkceChallenge {
        code_verifier,
        code_challenge,
        state,
    }
}

/// Outcome of `keel auth login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthLoginOutcome {
    LoggedIn {
        server: String,
        subject: String,
        expires_at: Option<i64>,
        output: String,
    },
}

impl AuthLoginOutcome {
    pub fn output(&self) -> &str {
        match self {
            Self::LoggedIn { output, .. } => output,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeResponse {
    session_token: String,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    is_admin: Option<bool>,
}

/// Builds the browser URL for the CLI loopback login.
pub(crate) fn build_cli_login_url(
    base_url: &str,
    redirect_uri: &str,
    pkce: &PkceChallenge,
    force_prompt: bool,
) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    q.append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &pkce.state)
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("code_challenge_method", "S256");
    if force_prompt {
        q.append_pair("prompt", "login");
    }
    format!("{base_url}{CLI_LOGIN_PATH}?{}", q.finish())
}

/// Parsed loopback callback.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallbackResult {
    Code { code: String, state: String },
    Error(String),
    NotCallback,
}

/// Parses the request line of an HTTP request hitting the loopback listener.
pub(crate) fn parse_callback_request(request: &str) -> CallbackResult {
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|rest| rest.split(' ').next())
    else {
        return CallbackResult::NotCallback;
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        return CallbackResult::NotCallback;
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" if error.is_some() => {
                error = Some(format!("{}: {v}", error.unwrap_or_default()))
            }
            _ => {}
        }
    }
    if let Some(err) = error {
        return CallbackResult::Error(err);
    }
    match (code, state) {
        (Some(code), Some(state)) if !code.is_empty() => CallbackResult::Code { code, state },
        _ => CallbackResult::Error("callback missing code/state".to_string()),
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Keel CLI</title></head><body style=\"font-family:sans-serif\"><h2>{body}</h2></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Waits for the browser to hit `/callback` with the matching `state`; returns the code.
async fn await_callback(listener: TcpListener, expected_state: &str) -> Result<String, CliError> {
    loop {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut buf = vec![0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        match parse_callback_request(&request) {
            CallbackResult::NotCallback => {
                respond(&mut stream, "404 Not Found", "Not found").await;
            }
            CallbackResult::Error(err) => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "Keel login failed. Return to the terminal.",
                )
                .await;
                return Err(CliError::Execution(format!(
                    "login was not completed: {err}"
                )));
            }
            CallbackResult::Code { code, state } => {
                if state != expected_state {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "Keel login failed (state mismatch).",
                    )
                    .await;
                    return Err(CliError::Execution(
                        "login callback state mismatch (possible CSRF); try `keel auth login` again".to_string(),
                    ));
                }
                respond(
                    &mut stream,
                    "200 OK",
                    "Keel CLI login complete. You can close this tab.",
                )
                .await;
                return Ok(code);
            }
        }
    }
}

/// Best-effort browser launch via argv-only commands (never a shell).
fn open_browser(url: &str) -> bool {
    let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(browser) = std::env::var("BROWSER")
        && !browser.trim().is_empty()
        && !browser.contains(char::is_whitespace)
    {
        candidates.push((browser, vec![url.to_string()]));
    }
    if cfg!(target_os = "macos") {
        candidates.push(("open".into(), vec![url.into()]));
    } else if cfg!(windows) {
        candidates.push((
            "rundll32".into(),
            vec!["url.dll,FileProtocolHandler".into(), url.into()],
        ));
    } else {
        candidates.push(("xdg-open".into(), vec![url.into()]));
        candidates.push(("wslview".into(), vec![url.into()]));
    }
    candidates.into_iter().any(|(prog, args)| {
        std::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    })
}

/// Executes `keel auth login` (RFC 8252 loopback + PKCE).
pub async fn execute_auth_login(
    args: &AuthLoginArgs,
    ctx: &CliContext,
) -> Result<AuthLoginOutcome, CliError> {
    execute_auth_login_with(args, ctx, |url| {
        eprintln!("Open this URL in your browser to log in to Keel:\n  {url}");
        if !args.no_browser && open_browser(url) {
            eprintln!("(A browser window was opened; waiting for the login to complete…)");
        }
    })
    .await
}

/// [`execute_auth_login`] with an injectable "open URL" hook (tests drive the browser leg).
pub(crate) async fn execute_auth_login_with(
    args: &AuthLoginArgs,
    ctx: &CliContext,
    open_url: impl FnOnce(&str),
) -> Result<AuthLoginOutcome, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    // Preflight: fail fast (before opening a browser) when the server is unreachable or OIDC is
    // not configured. Older servers without the status endpoint are tolerated.
    let status = ctx
        .http
        .get(format!("{base_url}/api/v1/auth/status"))
        .send()
        .await?;
    if status.status().is_success()
        && let Ok(v) = status.json::<Value>().await
        && v.get("oidcEnabled").and_then(Value::as_bool) == Some(false)
    {
        return Err(CliError::Api {
            status: 503,
            message: format!(
                "OIDC login is not configured on {base_url} (start `keel serve` with an OIDC issuer in .keel/config.yaml)"
            ),
        });
    }
    let pkce = generate_pkce_pair();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let login_url = build_cli_login_url(&base_url, &redirect_uri, &pkce, args.force_prompt);
    // Second preflight: the login endpoint itself answers `503` (no redirect) when the server
    // cannot reach the IdP's discovery document; report it here instead of opening a browser on
    // an error page and waiting for a callback that never comes. The flow it starts is stateless
    // (an unused PKCE cookie), so probing it is harmless.
    let probe = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    if let Ok(resp) = probe.get(&login_url).send().await
        && resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return Err(extract_api_error(resp).await);
    }
    open_url(&login_url);

    let code = tokio::time::timeout(
        Duration::from_secs(args.timeout_secs.max(1)),
        await_callback(listener, &pkce.state),
    )
    .await
    .map_err(|_| {
        CliError::Execution(format!(
            "timed out after {}s waiting for the browser login callback",
            args.timeout_secs
        ))
    })??;
    let code = Zeroizing::new(code);

    let resp = ctx
        .http
        .post(format!("{base_url}{CLI_EXCHANGE_PATH}"))
        .json(&json!({
            "code": code.as_str(),
            "codeVerifier": pkce.code_verifier,
            "redirectUri": redirect_uri,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err = extract_api_error(resp).await;
        if status == 401 {
            return Err(CliError::Api {
                status,
                message: "login code exchange was rejected (expired, reused, or PKCE mismatch); run `keel auth login` again".to_string(),
            });
        }
        return Err(err);
    }
    let exchanged: ExchangeResponse = resp.json().await?;
    let token = Zeroizing::new(exchanged.session_token);
    ctx.save_session_token(&base_url, &token)?;

    let subject = exchanged
        .email
        .clone()
        .or(exchanged.sub.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let mut output = format!("Logged in to {base_url} as {subject}");
    if exchanged.is_admin == Some(true) {
        output.push_str(" (admin)");
    }
    output.push('\n');
    if let Some(exp) = exchanged.expires_at
        && let Some(dt) = chrono::DateTime::from_timestamp(exp, 0)
    {
        output.push_str(&format!("Session expires: {}\n", dt.to_rfc3339()));
    }
    output.push_str(&format!(
        "Session stored in {} (0600)\n",
        ctx.credentials_path().display()
    ));
    Ok(AuthLoginOutcome::LoggedIn {
        server: base_url,
        subject,
        expires_at: exchanged.expires_at,
        output,
    })
}

fn no_session_message(base_url: &str, ctx: &CliContext) -> String {
    let others = ctx.stored_session_servers();
    let mut msg =
        format!("Not logged in to {base_url}. Run `keel auth login --server {base_url}`.\n");
    if !others.is_empty() {
        msg.push_str(&format!(
            "Stored sessions exist for: {} (pass one via --server or KEEL_SERVER)\n",
            others.join(", ")
        ));
    }
    msg
}

/// Executes `keel auth status`.
pub async fn execute_auth_status(
    args: &AuthServerArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    let Some(token) = ctx.load_session_token(&base_url) else {
        return Ok(no_session_message(&base_url, ctx));
    };
    let resp = ctx
        .http
        .get(format!("{base_url}/api/v1/auth/status"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }
    let v: Value = resp.json().await?;
    if v.get("authenticated").and_then(Value::as_bool) != Some(true) {
        return Ok(format!(
            "Stored session for {base_url} is no longer valid (expired or revoked). Run `keel auth login --server {base_url}`.\n"
        ));
    }
    let who = v
        .get("email")
        .or_else(|| v.get("sub"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let groups = v
        .get("groups")
        .and_then(Value::as_array)
        .map(|g| {
            g.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Ok(format!(
        "Server:   {base_url}\nUser:     {who}\nAdmin:    {}\nGroups:   {}\nPhase:    {}\n",
        v.get("isAdmin").and_then(Value::as_bool).unwrap_or(false),
        if groups.is_empty() { "-" } else { &groups },
        v.get("phase")
            .map(|p| p.to_string().trim_matches('"').to_string())
            .unwrap_or_default(),
    ))
}

/// Executes `keel auth logout` (best-effort server logout, then local removal).
pub async fn execute_auth_logout(
    args: &AuthServerArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    let base_url = normalize_server_url(&args.server)?;
    if let Some(token) = ctx.load_session_token(&base_url) {
        let _ = ctx
            .http
            .post(format!("{base_url}/api/v1/auth/logout"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await;
    }
    if ctx.remove_session_token(&base_url)? {
        Ok(format!(
            "Logged out of {base_url}; local session removed.\n"
        ))
    } else {
        Ok(format!("No stored session for {base_url}.\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::response::Redirect;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pkce_meets_server_contract() {
        let p = generate_pkce_pair();
        assert_eq!(p.code_verifier.len(), 43);
        assert!(
            p.code_verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        );
        assert_eq!(
            p.code_challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(p.code_verifier.as_bytes()))
        );
        assert!(p.state.len() >= 8);
        assert!(!format!("{p:?}").contains(&p.code_verifier));
        let url = build_cli_login_url("http://k", "http://127.0.0.1:5/callback", &p, true);
        assert!(url.starts_with(
            "http://k/api/v1/auth/cli/login?redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback"
        ));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.ends_with("&prompt=login"));
    }

    #[test]
    fn callback_parsing() {
        assert_eq!(
            parse_callback_request("GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n"),
            CallbackResult::Code {
                code: "abc".into(),
                state: "xyz".into()
            }
        );
        assert_eq!(
            parse_callback_request("GET /favicon.ico HTTP/1.1\r\n"),
            CallbackResult::NotCallback
        );
        assert!(matches!(
            parse_callback_request("GET /callback?error=access_denied&error_description=no HTTP/1.1"),
            CallbackResult::Error(e) if e.contains("access_denied")
        ));
        assert!(matches!(
            parse_callback_request("GET /callback?state=x HTTP/1.1"),
            CallbackResult::Error(_)
        ));
    }

    #[tokio::test]
    async fn login_reports_unreachable_idp_without_opening_a_browser() {
        let app = Router::new().route(
            CLI_LOGIN_PATH,
            get(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": "OIDC discovery for the configured issuer `https://idp.keel.local` failed"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = std::env::temp_dir().join(format!("keel-auth-test-{}", uuid::Uuid::new_v4()));
        let ctx = CliContext::with_credentials_path(dir.join("creds.json"));
        let args = AuthLoginArgs {
            server: format!("http://127.0.0.1:{}", addr.port()),
            no_browser: true,
            force_prompt: false,
            timeout_secs: 20,
        };
        let opened = std::sync::atomic::AtomicBool::new(false);
        let err = execute_auth_login_with(&args, &ctx, |_| {
            opened.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await
        .unwrap_err();
        assert!(!opened.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            err.to_string()
                .contains("OIDC discovery for the configured issuer"),
            "{err}"
        );
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn loopback_login_end_to_end_against_mock_server() {
        let seen = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let seen_login = seen.clone();
        let seen_ex = seen.clone();
        let app = Router::new()
            .route(
                CLI_LOGIN_PATH,
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let seen = seen_login.clone();
                    async move {
                        seen.lock().unwrap().insert("challenge".into(), q["code_challenge"].clone());
                        Redirect::to(&format!("{}?code=one-time&state={}", q["redirect_uri"], q["state"]))
                    }
                }),
            )
            .route(
                CLI_EXCHANGE_PATH,
                post(move |Json(body): Json<Value>| {
                    let seen = seen_ex.clone();
                    async move {
                        let verifier = body["codeVerifier"].as_str().unwrap().to_string();
                        let challenge = seen.lock().unwrap()["challenge"].clone();
                        assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())), challenge);
                        assert_eq!(body["code"], "one-time");
                        assert!(body["redirectUri"].as_str().unwrap().starts_with("http://127.0.0.1:"));
                        Json(json!({"sessionToken": "sess-123", "tokenType": "Bearer", "expiresAt": 1900000000, "sub": "u1", "email": "a@b.c", "phase": "oidcActive", "isAdmin": true}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = std::env::temp_dir().join(format!("keel-auth-test-{}", uuid::Uuid::new_v4()));
        let ctx = CliContext::with_credentials_path(dir.join("creds.json"));
        let args = AuthLoginArgs {
            server: format!("http://localhost:{}", addr.port()),
            no_browser: true,
            force_prompt: false,
            timeout_secs: 20,
        };
        let outcome = execute_auth_login_with(&args, &ctx, |url| {
            let url = url.to_string();
            tokio::spawn(async move {
                // Simulated browser: follows the server redirect to the loopback callback.
                let _ = reqwest::get(url).await;
            });
        })
        .await
        .unwrap();
        assert!(outcome.output().contains("a@b.c (admin)"));
        // Stored under the normalized key; `localhost` and `127.0.0.1` are equivalent.
        assert_eq!(
            ctx.load_session_token(&format!("http://127.0.0.1:{}", addr.port()))
                .as_deref(),
            Some("sess-123")
        );
        let out = execute_auth_logout(
            &AuthServerArgs {
                server: format!("http://127.0.0.1:{}", addr.port()),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.contains("local session removed"));
        assert!(
            ctx.load_session_token(&format!("http://127.0.0.1:{}", addr.port()))
                .is_none()
        );
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
}
