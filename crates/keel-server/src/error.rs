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

    #[error("Schema validation failed")]
    SchemaValidation(Vec<SchemaFieldError>),

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

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::Forbidden(msg) => (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::Conflict(msg) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::SchemaValidation(errors) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: "Payload failed JSON Schema validation".to_string(),
                    details: Some(errors),
                }),
            )
                .into_response(),
            Self::Core(CoreError::SchemaValidation(errors)) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: "Payload failed JSON Schema validation".to_string(),
                    details: Some(errors),
                }),
            )
                .into_response(),
            Self::Core(CoreError::Validation(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::Core(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: err.to_string(),
                    details: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::NotFound(id)) => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Proposal '{id}' not found"),
                    details: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::InvalidState(id, state)) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: format!("Proposal '{id}' cannot be modified in state '{state}'"),
                    details: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::MergeConflict(id, reason)) => (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: format!("Merge conflict on proposal '{id}': {reason}"),
                    details: None,
                }),
            )
                .into_response(),
            Self::Forge(ForgeError::InvalidInput(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::Forge(err) => (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: err.to_string(),
                    details: None,
                }),
            )
                .into_response(),
            Self::Git(GitError::InvalidArgument { field, reason }) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Invalid git argument ({field}): {reason}"),
                    details: None,
                }),
            )
                .into_response(),
            Self::Git(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: err.to_string(),
                    details: None,
                }),
            )
                .into_response(),
            Self::Pko(PkoError::Validation(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::Pko(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: err.to_string(),
                    details: None,
                }),
            )
                .into_response(),
            Self::Kube(KubeError::InvalidInput(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
            Self::Kube(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: err.to_string(),
                    details: None,
                }),
            )
                .into_response(),
            Self::Io(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: err.to_string(),
                    details: None,
                }),
            )
                .into_response(),
            Self::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: msg,
                    details: None,
                }),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                ServerError::SchemaValidation(vec![]),
                StatusCode::UNPROCESSABLE_ENTITY,
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
}
