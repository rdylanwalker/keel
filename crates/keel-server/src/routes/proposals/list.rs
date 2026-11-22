//! Proposal listings across the platform and tenant repositories.

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use keel_core::validate_dns_label;
use keel_git::PullRequestState;
use serde::{Deserialize, Serialize};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::proposal_repo::{ProposalScope, ScopeRepo, ScopedProposal};
use crate::routes::RepoReader;
use crate::state::AppState;

use super::preview::{READ_REF_NAMESPACE, fetch_branches_for_read};

/// Query parameters of the proposal listings.
#[derive(Debug, Deserialize)]
pub struct ListProposalsQuery {
    #[serde(default)]
    pub status: Option<String>,
}

/// A repository whose PRs could not be listed (forge rate limit or failure; never secrets).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnavailableRepository {
    pub scope: String,
    pub repository: crate::proposal_repo::ProposalRepository,
    pub reason: String,
}

/// Response body of `GET /api/v1/proposals` and `GET /api/v1/tenants/{tenant}/proposals`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalsListResponse {
    pub proposals: Vec<ScopedProposal>,
    /// Repositories whose PRs are missing (or served from a stale cache) in this listing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<UnavailableRepository>,
}

fn parse_status_filter(status: Option<&str>) -> Result<Option<PullRequestState>, ServerError> {
    match status {
        None | Some("open") => Ok(Some(PullRequestState::Open)),
        Some("merged") => Ok(Some(PullRequestState::Merged)),
        Some("closed") => Ok(Some(PullRequestState::Closed)),
        Some("all") => Ok(None),
        Some(other) => Err(ServerError::BadRequest(format!(
            "Invalid proposal status filter '{other}', expected open|merged|closed|all"
        ))),
    }
}

/// `GET /api/v1/tenants/{tenant}/proposals`: the PRs of one scope (a tenant's repository, or
/// `management` for the platform repository; cluster administrators only).
pub async fn list_scope_proposals(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(scope): AxumPath<String>,
    Query(query): Query<ListProposalsQuery>,
) -> Result<Json<ProposalsListResponse>, ServerError> {
    let scope = ProposalScope::parse(&scope)?;
    scope.require_access(&auth)?;
    let pr_state = parse_status_filter(query.status.as_deref())?;
    let repo = ScopeRepo::resolve(&state, &scope)?;
    let listing = state.proposal_cache.list(&repo, pr_state).await?;
    let unavailable = if listing.stale {
        vec![UnavailableRepository {
            scope: scope.as_str().to_string(),
            repository: repo.repository(),
            reason: "forge unavailable or rate limited; showing the last known proposals"
                .to_string(),
        }]
    } else {
        Vec::new()
    };
    Ok(Json(ProposalsListResponse {
        proposals: listing
            .proposals
            .into_iter()
            .map(|pr| repo.scoped(pr))
            .collect(),
        unavailable,
    }))
}

/// Tenant ids with a directory under `tenants/` on the platform `main`: freshly fetched
/// (debounced, without `git_lock`, into [`READ_REF_NAMESPACE`]) so tenants whose stub was merged
/// on the forge or by another replica are listed; else the committed `origin/main`, else the
/// worktree.
async fn known_tenant_ids(state: &AppState) -> Vec<String> {
    let workdir = state.repo_workdir.as_path();
    let fresh = if tokio::fs::try_exists(workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        fetch_branches_for_read(state, workdir, &["main"]).await;
        let read_main = format!("refs/{READ_REF_NAMESPACE}/main");
        if state
            .git_runner
            .ref_exists(workdir, &read_main)
            .await
            .unwrap_or(false)
        {
            state
                .git_runner
                .list_dir_at_ref(workdir, &read_main, "tenants")
                .await
                .ok()
        } else {
            None
        }
    } else {
        None
    };
    let mut ids = match (fresh, RepoReader::committed(state).await) {
        (Some(ids), _) => ids,
        (None, Some(reader)) => reader.list_dir("tenants").await,
        (None, None) => {
            let _guard = state.git_lock.lock().await;
            RepoReader::Worktree {
                root: &state.repo_workdir,
            }
            .list_dir("tenants")
            .await
        }
    };
    ids.retain(|id| {
        id != crate::proposal_repo::PLATFORM_SCOPE && validate_dns_label(id, "tenant").is_ok()
    });
    ids.sort();
    ids.dedup();
    ids
}

/// `GET /api/v1/proposals`: every proposal the caller may see, across repositories. Cluster
/// administrators get the platform repository plus every tenant repository; other users the
/// repositories of their tenants. Tenant repositories are listed with bounded concurrency
/// ([`crate::proposal_repo::PROPOSAL_LIST_CONCURRENCY`]) through a short TTL cache that backs
/// off per repository on forge rate limits (`Retry-After`); repositories that cannot be listed
/// are reported in `unavailable` instead of failing the whole listing.
pub async fn list_proposals(
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListProposalsQuery>,
) -> Result<Json<ProposalsListResponse>, ServerError> {
    use futures::StreamExt as _;
    let pr_state = parse_status_filter(query.status.as_deref())?;
    let tenants: Vec<String> = known_tenant_ids(&state)
        .await
        .into_iter()
        .filter(|t| auth.require_tenant_access(t).is_ok())
        .collect();
    let mut repos: Vec<ScopeRepo> = Vec::new();
    let mut unavailable = Vec::new();
    let mut tenants = tenants;
    if state.tenant_repos.is_dedicated() {
        if auth.is_admin() {
            let platform = ScopeRepo::platform(&state);
            // Tenants still being created (open platform stub PR) already have a repository
            // whose skeleton PR must be merged first: list them too.
            if let Ok(listing) = state
                .proposal_cache
                .list(&platform, Some(PullRequestState::Open))
                .await
            {
                for pr in &listing.proposals {
                    if let Some(t) =
                        crate::tenant_repos::create_tenant_branch_tenant(&pr.source_branch)
                        && t != crate::proposal_repo::PLATFORM_SCOPE
                        && validate_dns_label(t, "tenant").is_ok()
                        && !tenants.iter().any(|known| known == t)
                    {
                        tenants.push(t.to_string());
                    }
                }
                tenants.sort();
            }
            repos.push(platform);
        }
        for tenant in &tenants {
            match ScopeRepo::for_tenant(&state, tenant) {
                Ok(repo) => repos.push(repo),
                Err(err) => unavailable.push(UnavailableRepository {
                    scope: tenant.clone(),
                    repository: crate::proposal_repo::ProposalRepository {
                        label: format!("tenant-{tenant}"),
                        full_name: None,
                    },
                    reason: err.client_safe_reason(),
                }),
            }
        }
    } else if auth.is_admin() {
        // Local Git provider: one repository holds every tenant.
        repos.push(ScopeRepo::platform(&state));
    } else {
        for tenant in &tenants {
            repos.push(ScopeRepo::for_tenant(&state, tenant)?);
        }
    }
    let results: Vec<_> = futures::stream::iter(repos)
        .map(|repo| {
            let cache = state.proposal_cache.clone();
            async move {
                let listing = cache.list(&repo, pr_state).await;
                (repo, listing)
            }
        })
        .buffer_unordered(crate::proposal_repo::PROPOSAL_LIST_CONCURRENCY)
        .collect()
        .await;
    let mut proposals = Vec::new();
    for (repo, listing) in results {
        let platform_monorepo = !repo.is_dedicated() && repo.tenant().is_none();
        match listing {
            Ok(listing) => {
                if listing.stale {
                    unavailable.push(UnavailableRepository {
                        scope: repo.scope.as_str().to_string(),
                        repository: repo.repository(),
                        reason:
                            "forge unavailable or rate limited; showing the last known proposals"
                                .to_string(),
                    });
                }
                for pr in listing.proposals {
                    // Local Git provider: route each PR through its tenant's scope.
                    let scoped = match crate::proposal_repo::pr_tenant(&pr) {
                        Some(t)
                            if platform_monorepo
                                && !state.tenant_repos.is_dedicated()
                                && validate_dns_label(t, "tenant").is_ok()
                                && t != crate::proposal_repo::PLATFORM_SCOPE
                                && tenants.iter().any(|known| known == t) =>
                        {
                            let mut scoped = repo.scoped(pr.clone());
                            scoped.scope = t.to_string();
                            scoped
                        }
                        _ => repo.scoped(pr),
                    };
                    proposals.push(scoped);
                }
            }
            Err(err) => unavailable.push(UnavailableRepository {
                scope: repo.scope.as_str().to_string(),
                repository: repo.repository(),
                reason: err.client_safe_reason(),
            }),
        }
    }
    proposals.sort_by_key(|p| std::cmp::Reverse(p.pr.updated_at));
    unavailable.sort_by(|a, b| a.scope.cmp(&b.scope));
    Ok(Json(ProposalsListResponse {
        proposals,
        unavailable,
    }))
}
