//! Browser security response headers (`NFR-SEC-01`).
//!
//! Every response (embedded SPA, assets, REST/SSE API) carries a strict Content-Security-Policy plus
//! `X-Content-Type-Options`, `Referrer-Policy` and `X-Frame-Options`. The SPA bundle has no inline
//! scripts (Vite emits external module scripts only), so `script-src 'self'` holds;
//! `'wasm-unsafe-eval'` is required by the WASM-based SSH terminal (`@wterm/core` compiles its
//! embedded WASM module) and does **not** permit JavaScript `eval`. `form-action` additionally
//! allows the configured OIDC issuer origin so any IdP-bound form submission/redirect chain works.

use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::{
    CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

/// Returns the serialized `scheme://host[:port]` origin of an `http(s)` OIDC issuer URL.
pub(crate) fn issuer_origin(issuer_url: &str) -> Option<String> {
    let url = url::Url::parse(issuer_url.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let origin = url.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// Builds the Content-Security-Policy value, allowing `form-action` to the OIDC issuer origin.
pub fn content_security_policy(oidc_issuer_url: Option<&str>) -> String {
    let mut form_action = String::from("'self'");
    if let Some(origin) = oidc_issuer_url.and_then(issuer_origin) {
        form_action.push(' ');
        form_action.push_str(&origin);
    }
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; font-src 'self' data:; frame-ancestors 'none'; \
         base-uri 'self'; form-action {form_action}"
    )
}

/// Axum middleware appending the security headers to every response.
pub(crate) async fn security_headers(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let issuer = state
        .oidc_config
        .read()
        .await
        .as_ref()
        .filter(|c| c.enabled)
        .map(|c| c.issuer_url.clone());
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    if let Ok(csp) = HeaderValue::from_str(&content_security_policy(issuer.as_deref())) {
        headers.insert(CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_is_strict_and_allows_wasm_but_not_eval() {
        let csp = content_security_policy(None);
        assert!(csp.starts_with("default-src 'self'; "));
        assert!(csp.contains("script-src 'self' 'wasm-unsafe-eval';"));
        assert!(!csp.contains("'unsafe-eval'"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("connect-src 'self';"));
        assert!(csp.ends_with("form-action 'self'"));
    }

    #[test]
    fn csp_form_action_allows_only_issuer_origin() {
        let csp = content_security_policy(Some("https://idp.keel.local:8888/auth/v1"));
        assert!(csp.ends_with("form-action 'self' https://idp.keel.local:8888"));
        let csp = content_security_policy(Some("https://login.example.com/realms/x/"));
        assert!(csp.ends_with("form-action 'self' https://login.example.com"));
        // Non-http(s) or malformed issuers never inject anything into the policy.
        for bad in [
            "javascript:alert(1)",
            "not a url",
            "https://a.com; script-src *",
        ] {
            let csp = content_security_policy(Some(bad));
            assert!(!csp.contains("script-src *"), "{bad}");
            assert!(csp.ends_with("form-action 'self'"), "{bad}");
        }
        assert_eq!(issuer_origin("data:text/html,x"), None);
    }
}
