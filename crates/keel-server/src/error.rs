//! Unified HTTP error mapping for `keel-server`.
//!
//! Ensures errors are returned as structured JSON responses without leaking
//! sensitive internal details or secret material (`NFR-SEC-05`).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use keel_core::{CoreError, SchemaFieldError};
use keel_git::{ForgeError, GitError};
use keel_kube::KubeError;
use keel_pko::PkoError;
use serde::Serialize;

/// Structured JSON error envelope returned by all REST endpoints.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<SchemaFieldError>>,
    /// Opaque id correlating a generic 5xx response with the full server-side log entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Logs the full internal error server-side under a fresh correlation id and returns a generic
/// response body, so internal paths, command output, and upstream API messages never reach
/// clients (`NFR-SEC-05`).
fn opaque_error_response(
    status: StatusCode,
    category: &str,
    err: &dyn std::fmt::Display,
) -> Response {
    let correlation_id = uuid::Uuid::new_v4().to_string();
    tracing::error!(
        correlation_id = %correlation_id,
        status = status.as_u16(),
        category,
        error = %err,
        "request failed with internal error"
    );
    let error = if status == StatusCode::BAD_GATEWAY {
        format!("Upstream {category} request failed (correlation id: {correlation_id})")
    } else {
        format!("Internal server error (correlation id: {correlation_id})")
    };
    (
        status,
        Json(ErrorResponse {
            error,
            details: None,
            correlation_id: Some(correlation_id),
        }),
    )
        .into_response()
}

/// Unified server error type implementing [`IntoResponse`].
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("Git error: {0}")]
    Git(#[from] GitError),

    #[error("Forge error: {0}")]
    Forge(#[from] ForgeError),

    #[error("PKO error: {0}")]
    Pko(#[from] PkoError),

    #[error("Kubernetes error: {0}")]
    Kube(#[from] KubeError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl ServerError {
    /// One-line, client-safe reason for an error reported inside an otherwise successful
    /// response (for example a repository the proposal listing could not reach). Messages the
    /// server writes itself are kept; forge, git, I/O and other internal errors are logged in
    /// full under a correlation id and replaced by a generic, category-level reason, so upstream
    /// response bodies, internal URLs and server paths never reach clients (`NFR-SEC-05`).
    pub fn client_safe_reason(&self) -> String {
        match self {
            Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::BadRequest(msg)
            | Self::Conflict(msg)
            | Self::ServiceUnavailable(msg)
            | Self::Core(CoreError::Validation(msg))
            | Self::Forge(ForgeError::InvalidInput(msg))
            | Self::Pko(PkoError::Validation(msg))
            | Self::Kube(
                KubeError::InvalidInput(msg)
                | KubeError::NotFound(msg)
                | KubeError::Conflict(msg)
                | KubeError::TenantSourceUnavailable(msg)
                | KubeError::Unavailable(msg),
            ) => msg.clone(),
            Self::Forge(ForgeError::RateLimited { .. }) => {
                "The Git forge is rate limiting Keel; retry later".to_string()
            }
            other => {
                let category = match other {
                    Self::Forge(_) => "forge",
                    Self::Git(_) => "git",
                    Self::Kube(_) => "kubernetes",
                    _ => "internal",
                };
                let correlation_id = uuid::Uuid::new_v4().to_string();
                tracing::warn!(
                    correlation_id = %correlation_id,
                    category,
                    error = %other,
                    "reporting an internal error to the client as a generic reason"
                );
                format!("{category} unavailable (correlation id: {correlation_id})")
            }
        }
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Forbidden(msg) => (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Conflict(msg) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Core(CoreError::SchemaValidation(errors)) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: "Payload failed JSON Schema validation".to_string(),
                    details: Some(errors),
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Core(CoreError::Validation(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Core(err) => {
                opaque_error_response(StatusCode::INTERNAL_SERVER_ERROR, "core", &err)
            }
            Self::Forge(ForgeError::NotFound(id)) => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Proposal '{id}' not found"),
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::InvalidState(id, state)) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: format!("Proposal '{id}' cannot be modified in state '{state}'"),
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::MergeConflict(id, reason)) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: format!("Merge conflict on proposal '{id}': {reason}"),
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::InvalidInput(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::RateLimited { retry_after, .. }) => {
                let mut response = (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse {
                        error: "The Git forge is rate limiting Keel; retry later".to_string(),
                        details: None,
                        correlation_id: None,
                    }),
                )
                    .into_response();
                if let Some(wait) = retry_after
                    && let Ok(value) =
                        axum::http::HeaderValue::from_str(&wait.as_secs().to_string())
                {
                    response
                        .headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, value);
                }
                response
            }
            Self::Forge(err) => opaque_error_response(StatusCode::BAD_GATEWAY, "forge", &err),
            Self::Git(GitError::InvalidArgument { field, reason }) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid git argument ({field}): {reason}"),
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Git(err) => opaque_error_response(StatusCode::INTERNAL_SERVER_ERROR, "git", &err),
            Self::Pko(PkoError::Validation(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Pko(err) => opaque_error_response(StatusCode::INTERNAL_SERVER_ERROR, "pko", &err),
            Self::Kube(KubeError::InvalidInput(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Kube(KubeError::NotFound(msg)) => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Kube(KubeError::Conflict(msg)) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Kube(KubeError::TenantSourceUnavailable(msg)) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Kube(KubeError::Unavailable(msg)) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
            Self::Kube(err) => {
                opaque_error_response(StatusCode::INTERNAL_SERVER_ERROR, "kubernetes", &err)
            }
            Self::Io(err) => opaque_error_response(StatusCode::INTERNAL_SERVER_ERROR, "io", &err),
            Self::Internal(msg) => {
                opaque_error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_safe_reason_keeps_server_messages_and_hides_internal_details() {
        assert_eq!(
            ServerError::ServiceUnavailable("retry later".into()).client_safe_reason(),
            "retry later"
        );
        assert_eq!(
            ServerError::Forge(ForgeError::RateLimited {
                status: 429,
                retry_after: None,
            })
            .client_safe_reason(),
            "The Git forge is rate limiting Keel; retry later"
        );
        for err in [
            ServerError::Forge(ForgeError::Api {
                status: 500,
                message: "<secret body> https://forge.internal/api".into(),
            }),
            ServerError::Internal("/var/lib/keel/secret-path".into()),
            ServerError::Io(std::io::Error::other("/var/lib/keel/secret-path")),
        ] {
            let reason = err.client_safe_reason();
            assert!(reason.contains("correlation id"), "{reason}");
            assert!(!reason.contains("secret"), "{reason}");
            assert!(!reason.contains("forge.internal"), "{reason}");
        }
    }

    #[test]
    fn server_error_variants_map_to_expected_status_codes() {
        let cases = vec![
            (
                ServerError::Unauthorized("unauth".into()),
                StatusCode::UNAUTHORIZED,
            ),
            (
                ServerError::Forbidden("forbidden".into()),
                StatusCode::FORBIDDEN,
            ),
            (
                ServerError::NotFound("missing".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                ServerError::BadRequest("bad".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Conflict("conflict".into()),
                StatusCode::CONFLICT,
            ),
            (
                ServerError::ServiceUnavailable("down".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ServerError::Core(CoreError::SchemaValidation(vec![])),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ServerError::Core(CoreError::Validation("invalid".into())),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Core(CoreError::CommandFailed {
                    program: "sops".into(),
                    message: "err".into(),
                }),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ServerError::Forge(ForgeError::NotFound("1".into())),
                StatusCode::NOT_FOUND,
            ),
            (
                ServerError::Forge(ForgeError::InvalidState(
                    "1".into(),
                    keel_git::PullRequestState::Closed,
                )),
                StatusCode::CONFLICT,
            ),
            (
                ServerError::Forge(ForgeError::MergeConflict("1".into(), "dirty".into())),
                StatusCode::CONFLICT,
            ),
            (
                ServerError::Forge(ForgeError::InvalidInput("bad".into())),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Forge(ForgeError::Api {
                    status: 502,
                    message: "timeout".into(),
                }),
                StatusCode::BAD_GATEWAY,
            ),
            (
                ServerError::Git(GitError::InvalidArgument {
                    field: "branch",
                    reason: "bad".into(),
                }),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Git(GitError::Io(std::io::Error::other("git io"))),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ServerError::Pko(PkoError::Validation("bad stack".into())),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Pko(PkoError::PreviewParse("bad json".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ServerError::Kube(KubeError::InvalidInput("bad ns".into())),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Kube(KubeError::Json(
                    serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
                )),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ServerError::Io(std::io::Error::other("disk")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ServerError::Internal("oops".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];

        for (err, expected) in cases {
            let res = err.into_response();
            assert_eq!(res.status(), expected);
        }
    }

    async fn body_json(res: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn kube_not_found_and_conflict_map_to_4xx_with_message() {
        let res =
            ServerError::Kube(KubeError::NotFound("Stack 'db' not found".into())).into_response();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = body_json(res).await;
        assert_eq!(body["error"], "Stack 'db' not found");
        assert!(
            body.get("correlationId")
                .is_none_or(serde_json::Value::is_null)
        );

        let res = ServerError::Kube(KubeError::Conflict("Stack 'db' is protected".into()))
            .into_response();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = body_json(res).await;
        assert_eq!(body["error"], "Stack 'db' is protected");
    }

    #[tokio::test]
    async fn internal_errors_are_opaque_with_correlation_id() {
        let leaky = [
            ServerError::Internal("/var/lib/keel/secret-path failed".into()),
            ServerError::Io(std::io::Error::other("/var/lib/keel/secret-path")),
            ServerError::Core(CoreError::CommandFailed {
                program: "sops".into(),
                message: "/var/lib/keel/secret-path".into(),
            }),
            ServerError::Forge(ForgeError::Api {
                status: 500,
                message: "/var/lib/keel/secret-path".into(),
            }),
            ServerError::Pko(PkoError::PreviewParse("/var/lib/keel/secret-path".into())),
        ];
        for err in leaky {
            let body = body_json(err.into_response()).await;
            let msg = body["error"].as_str().unwrap();
            let cid = body["correlationId"].as_str().unwrap();
            assert!(
                !msg.contains("secret-path"),
                "error response leaked the internal path"
            );
            assert!(msg.contains(cid));
            assert!(uuid::Uuid::parse_str(cid).is_ok());
        }

        // 4xx validation messages stay informative and carry no correlation id.
        let body = body_json(
            ServerError::Core(CoreError::Validation("field x bad".into())).into_response(),
        )
        .await;
        assert_eq!(body["error"], "field x bad");
        assert!(body.get("correlationId").is_none());
    }
}
