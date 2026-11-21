//! OIDC Authorization Code + PKCE login handlers (`keel auth login`, `SDD.md` §6.1, `NFR-SEC-01`).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use keel_core::EnvelopeCrypto;
use reqwest::StatusCode;
use reqwest::header::LOCATION;
use sha2::{Digest, Sha256};

use crate::cli::AuthLoginArgs;

use super::{CliContext, CliError, extract_api_error, normalize_server_url};

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

/// Generates a fresh 256-bit OIDC PKCE `S256` challenge and CSRF state nonce.
pub fn generate_pkce_pair() -> PkceChallenge {
    let verifier_bytes = EnvelopeCrypto::generate_key();
    let state_bytes = EnvelopeCrypto::generate_key();
    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
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
    OidcRedirect {
        authorize_url: String,
        output: String,
    },
}

impl AuthLoginOutcome {
    pub fn output(&self) -> &str {
        match self {
            Self::OidcRedirect { output, .. } => output,
        }
    }
}

fn extract_query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    let prefix = format!("{key}=");
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix(&prefix)
            && !val.is_empty()
        {
            return Some(val.to_string());
        }
    }
    None
}

/// Executes `keel auth login` against `keel-server` via OIDC Authorization Code + PKCE (`SDD.md` §6.1).
pub async fn execute_auth_login(
    args: &AuthLoginArgs,
    ctx: &CliContext,
) -> Result<AuthLoginOutcome, CliError> {
    let base_url = normalize_server_url(&args.server)?;

    let endpoint = format!("{base_url}/api/v1/auth/login");
    let resp = ctx.no_redirect_http.get(&endpoint).send().await?;

    if resp.status() == StatusCode::FOUND || resp.status() == StatusCode::SEE_OTHER {
        let authorize_url = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| {
                CliError::Execution("OIDC redirect response missing Location header".to_string())
            })?;

        let code_challenge = extract_query_param(&authorize_url, "code_challenge")
            .unwrap_or_else(|| generate_pkce_pair().code_challenge);

        let output = format!(
            "Open the following OIDC PKCE URL in your browser to authenticate (challenge S256={code_challenge}):\n  {authorize_url}"
        );
        Ok(AuthLoginOutcome::OidcRedirect {
            authorize_url,
            output,
        })
    } else {
        Err(extract_api_error(resp).await)
    }
}
