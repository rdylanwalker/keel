//! `keel-server`
//!
//! Stateless Axum HTTP and SSE REST API server, OIDC PKCE
//! authentication handler, and embedded React SPA static asset host (`SDD.md` §6.1, §6.3, §7.3).

pub mod auth;
pub mod error;
pub mod routes;
pub mod spa;
pub mod state;

use axum::Router;
use axum::routing::{delete, get, post, put};
use tower_http::trace::TraceLayer;

pub use auth::{
    AuthLoginResponse, AuthSession, AuthStatusResponse, PKCE_COOKIE_NAME, SESSION_COOKIE_NAME,
    SessionClaims, decrypt_session_token, encrypt_session_token,
};
pub use error::{ErrorResponse, ServerError};
pub use routes::healthz as health_check;
pub use state::{
    AppState, CachedPreview, ClusterStateProvider, KubeClusterStateProvider, ReleaseSummary,
    RepoClusterStateProvider, StackSummary,
};

/// Builds the complete stateless Axum [`Router`] with all REST, SSE, auth, and embedded SPA routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Operational endpoints
        .route("/healthz", get(routes::healthz))
        .route("/metrics", get(routes::metrics))
        // Authentication (OIDC Authorization Code + PKCE)
        .route("/api/v1/auth/status", get(auth::auth_status))
        .route("/api/v1/auth/login", get(auth::oidc_login))
        .route("/api/v1/auth/callback", get(auth::oidc_callback))
        // Protected REST & SSE API surface
        .route(
            "/api/v1/catalog",
            get(routes::get_catalog).post(routes::register_catalog_app),
        )
        .route(
            "/api/v1/catalog/{kind}/{id}",
            put(routes::update_catalog_item),
        )
        .route(
            "/api/v1/catalog/fetch-schema",
            post(routes::fetch_catalog_schema),
        )
        .route(
            "/api/v1/tenants",
            get(routes::list_tenants).post(routes::create_tenant),
        )
        .route("/api/v1/tenants/{id}", delete(routes::delete_tenant))
        .route(
            "/api/v1/proposals",
            get(routes::list_proposals).post(routes::create_proposal),
        )
        .route(
            "/api/v1/proposals/{pr_id}/preview",
            get(routes::get_proposal_preview),
        )
        .route(
            "/api/v1/proposals/{pr_id}/merge",
            post(routes::merge_proposal),
        )
        .route(
            "/api/v1/proposals/{pr_id}/reject",
            post(routes::reject_proposal),
        )
        .route(
            "/api/v1/proposals/{pr_id}/reopen",
            post(routes::reopen_proposal),
        )
        .route("/api/v1/stacks", get(routes::list_stacks))
        .route(
            "/api/v1/stacks/{namespace}/{name}/diagnostics",
            get(routes::get_stack_diagnostics),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/restart",
            post(routes::restart_stack_workspace),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/config",
            get(routes::get_stack_config),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}",
            delete(routes::delete_stack),
        )
        .route("/api/v1/releases", get(routes::list_releases))
        .route(
            "/api/v1/releases/{namespace}/{name}",
            delete(routes::delete_release),
        )
        .route(
            "/api/v1/releases/{namespace}/{name}/diagnostics",
            get(routes::get_release_diagnostics),
        )
        .route(
            "/api/v1/releases/{namespace}/{name}/config",
            get(routes::get_release_config),
        )
        .route(
            "/api/v1/releases/{namespace}/{name}/reconcile",
            post(routes::reconcile_release),
        )
        .route(
            "/api/v1/bundles/{namespace}/{name}/config",
            get(routes::get_bundle_config),
        )
        .route("/api/v1/providers", get(routes::list_providers))
        .route("/api/v1/providers/{id}", put(routes::toggle_provider))
        .route("/api/v1/events/stream", get(routes::events_stream))
        .route(
            "/api/v1/profile/git-credentials",
            put(routes::put_git_credentials),
        )
        // Embedded React + Shadcn SPA fallback
        .fallback(spa::spa_fallback)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Convenience constructor returning a [`Router`] backed by a default ephemeral [`AppState`].
pub fn create_router() -> Router {
    build_router(AppState::default_ephemeral())
}
