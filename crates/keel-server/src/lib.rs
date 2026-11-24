//! `keel-server`
//!
//! Stateless Axum HTTP and SSE REST API server, OIDC PKCE
//! authentication handler, and embedded React SPA static asset host (`SDD.md` §6.1, §6.3, §7.3).

mod auth;
mod error;
mod preview_lifecycle;
mod proposal_repo;
mod routes;
mod security;
mod spa;
mod ssh;
mod stack_recovery;
mod state;
mod tenant_repos;

use axum::Router;
use axum::routing::{delete, get, post, put};
use tower_http::trace::TraceLayer;

pub use auth::{SESSION_COOKIE_NAME, SessionClaims, check_oidc_discovery, encrypt_session_token};
pub use error::ServerError;
pub use preview_lifecycle::{
    PREVIEW_RECONCILE_INTERVAL, gc_preview_for_pr, reconcile_previews_once,
    spawn_preview_reconciler,
};
pub use stack_recovery::{
    STALLED_STACK_RECOVERY_INTERVAL, recover_stalled_stacks_once, recover_tenant_stalled_stacks,
    spawn_stalled_stack_recovery,
};
pub use state::{
    AppState, ClusterStateProvider, KubeClusterStateProvider, ReleaseSummary,
    RepoClusterStateProvider, StackSummary,
};
pub use tenant_repos::{
    StepOutcome, TENANT_REPO_DESCRIPTION, TenantCredentialSecrets, TenantRepoCloneBase,
    TenantRepoForge, TenantRepoForgeKind, TenantRepoManifests, TenantRepoNaming, TenantRepos,
    complete_cluster_provisioning, generate_tenant_age_key, provision_forge, provisioning_error,
    reconcile_tenant_credential, tenant_push_url,
};

/// Builds the complete stateless Axum [`Router`] with all REST, SSE, auth, and embedded SPA routes.
pub fn build_router(state: AppState) -> Router {
    let security_state = state.clone();
    Router::new()
        // Operational endpoints
        .route("/healthz", get(routes::healthz))
        .route("/metrics", get(routes::metrics))
        // Authentication (OIDC Authorization Code + PKCE)
        .route("/api/v1/auth/status", get(auth::auth_status))
        .route("/api/v1/auth/login", get(auth::oidc_login))
        .route("/api/v1/auth/callback", get(auth::oidc_callback))
        .route("/api/v1/auth/logout", post(auth::logout))
        // CLI loopback login (RFC 8252): browser entrypoint + one-time code exchange
        .route("/api/v1/auth/cli/login", get(auth::cli_login))
        .route("/api/v1/auth/cli/exchange", post(auth::cli_exchange))
        // Protected REST & SSE API surface
        .route(
            "/api/v1/catalog",
            get(routes::get_catalog).post(routes::register_catalog_app),
        )
        .route(
            "/api/v1/catalog/{kind}/{id}",
            put(routes::update_catalog_item).delete(routes::delete_catalog_item),
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
            "/api/v1/tenants/{id}/credentials/repair",
            post(routes::repair_tenant_credentials),
        )
        // Proposals are scoped to a tenant (its own repository) or `management` (the platform
        // repository); PR ids are per repository. `GET /api/v1/proposals` aggregates.
        .route("/api/v1/proposals", get(routes::list_proposals))
        .route(
            "/api/v1/tenants/{id}/proposals",
            get(routes::list_scope_proposals).post(routes::create_proposal),
        )
        .route(
            "/api/v1/tenants/{id}/proposals/{pr_id}/preview",
            get(routes::get_proposal_preview),
        )
        .route(
            "/api/v1/tenants/{id}/proposals/{pr_id}/merge",
            post(routes::merge_proposal),
        )
        .route(
            "/api/v1/tenants/{id}/proposals/{pr_id}/reject",
            post(routes::reject_proposal),
        )
        .route(
            "/api/v1/tenants/{id}/proposals/{pr_id}/reopen",
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
            "/api/v1/stacks/{namespace}/{name}/reconcile",
            post(routes::reconcile_stack),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/force-delete",
            post(routes::force_delete_stack),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/unlock",
            post(routes::unlock_stack),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/config",
            get(routes::get_stack_config),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/ssh-info",
            get(routes::get_stack_ssh_info),
        )
        .route(
            "/api/v1/stacks/{namespace}/{name}/ssh/ws",
            get(routes::stack_ssh_ws),
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
            "/api/v1/management/addons",
            get(routes::list_management_addons),
        )
        .route(
            "/api/v1/management/addons/{id}/upgrade",
            post(routes::upgrade_management_addon),
        )
        .route(
            "/api/v1/management/addons/{id}/rollback",
            post(routes::rollback_management_addon),
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
        .layer(axum::middleware::from_fn(auth::csrf_origin_guard))
        // Outermost (after CSRF) so rejected responses carry the security headers too.
        .layer(axum::middleware::from_fn_with_state(
            security_state,
            security::security_headers,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
