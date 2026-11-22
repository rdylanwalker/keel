//! Shared forge REST plumbing: pagination, optional `GET`, idempotent `DELETE` and error mapping.
//! Used by every remote [`crate::ForgeProvider`] adapter and the Forgejo admin helpers.

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::forge::ForgeError;
use crate::remote::map_error_response;

/// Page size requested from forge list endpoints (GitHub/GitLab cap at 100, Gitea at its
/// configured `MAX_RESPONSE_ITEMS`, default 50).
pub(crate) const LIST_PAGE_SIZE: usize = 50;

/// Hard cap on pages fetched per listing (`LIST_PAGE_SIZE * MAX_LIST_PAGES` entries) so a huge
/// or misbehaving forge cannot pin a request handler.
pub(crate) const MAX_LIST_PAGES: usize = 40;

/// What [`fetch_pages`] does when more than [`MAX_LIST_PAGES`] pages exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageCap {
    /// Return the entries fetched so far and log a warning (display listings such as PRs).
    TruncateWithWarning,
    /// Fail: callers act on the complete set (stray deploy keys, tokens, team members, ...).
    FailClosed,
}

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

pub(crate) async fn error_for(resp: Response, what: &str) -> ForgeError {
    map_error_response(resp, Some(what), false).await
}

/// Parses a (possibly empty) JSON body.
pub(crate) async fn json_body(resp: Response) -> Result<Value, ForgeError> {
    let text = resp.text().await?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&text)?)
}

/// `GET` returning `Some(json)` on success and `None` on 404.
pub(crate) async fn get_optional<F>(
    client: &Client,
    url: &str,
    authorize: F,
    what: &str,
) -> Result<Option<Value>, ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    let resp = authorize(client.get(url)).send().await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(error_for(resp, what).await);
    }
    Ok(Some(json_body(resp).await?))
}

/// `GET`s every page of a list endpoint (`{page_size_param}=`[`LIST_PAGE_SIZE`]`&page=N`,
/// following `Link: rel="next"` / GitLab `X-Next-Page`). `cap` decides what happens beyond
/// [`MAX_LIST_PAGES`].
pub(crate) async fn fetch_pages<T, F>(
    client: &Client,
    url: &str,
    page_size_param: &str,
    authorize: F,
    what: &str,
    cap: PageCap,
) -> Result<Vec<T>, ForgeError>
where
    T: DeserializeOwned,
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    let sep = if url.contains('?') { '&' } else { '?' };
    let mut out = Vec::new();
    for page in 1..=MAX_LIST_PAGES {
        let resp = authorize(client.get(format!(
            "{url}{sep}{page_size_param}={LIST_PAGE_SIZE}&page={page}"
        )))
        .send()
        .await?;
        if !resp.status().is_success() {
            return Err(error_for(resp, what).await);
        }
        let has_next = link_header_has_next(resp.headers())
            || resp
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| !v.trim().is_empty());
        let batch: Vec<T> = resp.json().await?;
        let n = batch.len();
        out.extend(batch);
        if !page_has_more(n, has_next) {
            return Ok(out);
        }
    }
    match cap {
        PageCap::TruncateWithWarning => {
            tracing::warn!(
                fetched = out.len(),
                max_pages = MAX_LIST_PAGES,
                "Forge listing of {what} truncated at page cap; older entries are omitted"
            );
            Ok(out)
        }
        PageCap::FailClosed => Err(ForgeError::Api {
            status: 200,
            message: format!(
                "more than {} {what}; refusing to act on a partial listing",
                LIST_PAGE_SIZE * MAX_LIST_PAGES
            ),
        }),
    }
}

/// `DELETE`, treating 404 as already gone.
pub(crate) async fn delete_ignoring_missing<F>(
    client: &Client,
    url: &str,
    authorize: F,
    what: &str,
) -> Result<(), ForgeError>
where
    F: Fn(RequestBuilder) -> RequestBuilder,
{
    let resp = authorize(client.delete(url)).send().await?;
    if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
        return Ok(());
    }
    Err(error_for(resp, what).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::spawn_mock_http_server;

    #[test]
    fn link_headers_and_full_pages_drive_pagination() {
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
    async fn page_cap_truncates_or_fails_closed() {
        let full = format!(
            "[{}]",
            (0..LIST_PAGE_SIZE)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let pages = || vec![(200, full.clone()); MAX_LIST_PAGES];
        let client = Client::new();

        let (url, handle) = spawn_mock_http_server(pages()).await;
        let items: Vec<u64> = fetch_pages(
            &client,
            &format!("{url}/list"),
            "limit",
            |b| b,
            "things",
            PageCap::TruncateWithWarning,
        )
        .await
        .unwrap();
        assert_eq!(items.len(), LIST_PAGE_SIZE * MAX_LIST_PAGES);
        assert_eq!(handle.await.unwrap().len(), MAX_LIST_PAGES);

        let (url, _handle) = spawn_mock_http_server(pages()).await;
        let err = fetch_pages::<u64, _>(
            &client,
            &format!("{url}/list?x=1"),
            "limit",
            |b| b,
            "things",
            PageCap::FailClosed,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("refusing to act on a partial listing"),
            "{err}"
        );
    }
}
