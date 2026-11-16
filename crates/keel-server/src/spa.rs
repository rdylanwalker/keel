//! Embedded React + Shadcn SPA static asset host (`SDD.md` §7.1, §7.4).
//!
//! Uses `rust-embed` pointing to `../../ui/dist` with a graceful HTML fallback
//! when `ui/dist` has not been built yet (e.g. during standalone Rust tests).

use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
pub struct SpaAssets;

const FALLBACK_INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Keel Control Plane</title>
</head>
<body>
  <div id="root">
    <h1>Keel Control Plane</h1>
    <p>UI bundle not yet built. Run <code>pnpm --dir ui build</code> to embed the React SPA.</p>
  </div>
</body>
</html>
"#;

/// Serves embedded SPA files or falls back to `index.html` for client-side routing.
pub async fn spa_fallback(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');

    // Never hijack API routes with HTML
    if raw_path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "API endpoint not found").into_response();
    }

    let candidate = if raw_path.is_empty() {
        "index.html"
    } else {
        raw_path
    };

    if let Some(asset) = SpaAssets::get(candidate) {
        let mime = mime_type_for_path(candidate);
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime)],
            asset.data.into_owned(),
        )
            .into_response();
    }

    // For client-side routes without a file extension, serve index.html
    if !candidate.contains('.')
        && let Some(index) = SpaAssets::get("index.html")
    {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            index.data.into_owned(),
        )
            .into_response();
    }

    if candidate == "index.html" || !candidate.contains('.') {
        return Html(FALLBACK_INDEX_HTML).into_response();
    }

    (StatusCode::NOT_FOUND, "Asset not found").into_response()
}

fn mime_type_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
