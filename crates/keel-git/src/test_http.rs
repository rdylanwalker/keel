//! Test-only canned HTTP server shared by the forge adapter tests.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawns a lightweight local HTTP server returning canned status/JSON responses sequentially.
/// The join handle yields every raw request it received, in order.
pub(crate) async fn spawn_mock_http_server(
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
                204 => "No Content",
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

/// First line of a captured raw request (`METHOD /path HTTP/1.1`).
pub(crate) fn request_line(req: &str) -> &str {
    req.lines().next().unwrap_or_default()
}

/// JSON body of a captured raw request (`Value::Null` when absent or not JSON).
pub(crate) fn request_body(req: &str) -> serde_json::Value {
    req.split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str(body).ok())
        .unwrap_or(serde_json::Value::Null)
}
