//! PR proposal synthesis, ephemeral PKO preview reports, and merge/reject/reopen endpoints (`SDD.md` §7.3).

mod bundle;
mod create;
mod guards;
mod lifecycle;
mod list;
mod manifests;
mod pr;
mod preview;
mod preview_inputs;
mod secrets;
mod tenant_stack;

pub use create::create_proposal;
pub(crate) use guards::ensure_tenant_repo_initialised;
pub use lifecycle::{merge_proposal, reject_proposal, reopen_proposal};
pub use list::{list_proposals, list_scope_proposals};
pub use preview::get_proposal_preview;
#[cfg(test)]
pub(crate) use {list::ListProposalsQuery, preview::synthesize_report_from_git_diff};

use keel_pko::{PkoStack, PreviewReport};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proposal_repo::ScopedProposal;

/// Request body for `POST /api/v1/tenants/{tenant}/proposals`. The tenant comes from the path; a
/// body `tenant` must be absent or equal to it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalRequest {
    #[serde(default)]
    pub tenant: String,
    pub catalog_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub values: Value,
    #[serde(default)]
    pub canary: bool,
    #[serde(default)]
    pub target_cluster: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub target_namespace: Option<String>,
    #[serde(default, alias = "action_engine")]
    pub action_engine: Option<String>,
    /// Edit only (apps and infra stacks): apply `values` as a JSON Merge Patch over the
    /// instance's committed values instead of replacing them, so a partial values file (the
    /// CLI) keeps every other key, including the schema defaults filled in at install; a `null`
    /// removes a key. Ignored on a first install.
    #[serde(default)]
    pub merge_stored_values: bool,
}

/// Response body for `POST /api/v1/tenants/{tenant}/proposals`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalResponse {
    /// The PR, with the scope and repository it lives in (its id is per repository).
    pub proposal: ScopedProposal,
    pub branch: String,
    pub preview_stack: PkoStack,
    pub report: PreviewReport,
}

/// Locks held by a tenant proposal from its first read to its push: `git_lock` (tenant
/// repositories only; the platform checkout synced to `main`) and the tenant repository lock.
pub(super) type ProposalGuards = (
    Option<tokio::sync::OwnedMutexGuard<()>>,
    tokio::sync::OwnedMutexGuard<()>,
);
