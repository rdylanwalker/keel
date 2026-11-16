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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_type_for_path_covers_known_and_unknown_extensions() {
        assert_eq!(mime_type_for_path("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            mime_type_for_path("app.js"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path("mod.mjs"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(mime_type_for_path("style.css"), "text/css; charset=utf-8");
        assert_eq!(
            mime_type_for_path("data.json"),
            "application/json; charset=utf-8"
        );
        assert_eq!(mime_type_for_path("logo.svg"), "image/svg+xml");
        assert_eq!(mime_type_for_path("pic.png"), "image/png");
        assert_eq!(mime_type_for_path("pic.jpg"), "image/jpeg");
        assert_eq!(mime_type_for_path("pic.jpeg"), "image/jpeg");
        assert_eq!(mime_type_for_path("fav.ico"), "image/x-icon");
        assert_eq!(mime_type_for_path("font.woff"), "font/woff");
        assert_eq!(mime_type_for_path("font.woff2"), "font/woff2");
        assert_eq!(
            mime_type_for_path("robots.txt"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path("archive.bin"),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn spa_fallback_handles_api_and_missing_assets() {
        let api_res = spa_fallback(Uri::from_static("/api/v1/nonexistent")).await;
        assert_eq!(api_res.status(), StatusCode::NOT_FOUND);

        let root_res = spa_fallback(Uri::from_static("/")).await;
        assert_eq!(root_res.status(), StatusCode::OK);

        let route_res = spa_fallback(Uri::from_static("/topology")).await;
        assert_eq!(route_res.status(), StatusCode::OK);

        let missing_asset = spa_fallback(Uri::from_static("/missing.nonexistent")).await;
        assert_eq!(missing_asset.status(), StatusCode::NOT_FOUND);
    }
}
