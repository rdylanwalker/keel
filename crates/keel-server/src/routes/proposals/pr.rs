//! Shared proposal tail: PR title/body, opening the PR, and publishing its preview report.

use axum::Json;
use axum::http::StatusCode;
use keel_git::PullRequestInfo;
use keel_pko::{EphemeralPreviewParams, PreviewReport, PreviewResourceCandidate, PreviewStack};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::proposal_repo::ScopeRepo;
use crate::state::{AppState, CachedPreview};

use super::preview::simulated_preview_markdown;
use super::{CreateProposalRequest, CreateProposalResponse};

/// A proposal branch pushed to the tenant's repository: what its PR and preview derive from.
pub(super) struct PushedProposal<'a> {
    pub(super) branch: &'a str,
    pub(super) timestamp: u64,
    pub(super) action_label: &'a str,
    /// Repository-relative directory of the proposed resource (the preview's `repoDir`).
    pub(super) rel_dir: &'a str,
    pub(super) instance_name: &'a str,
    pub(super) commit_sha: &'a str,
    /// Verb of the generated PR title (`feat(<tenant>): <verb> <name>`).
    pub(super) verb: &'a str,
    /// Server-generated part of the PR body.
    pub(super) summary: &'a str,
}

/// An opened proposal PR with its repository-scoped preview key and the ephemeral preview
/// parameters, commit-pinned to the pushed head.
pub(super) struct OpenedProposal {
    pub(super) pr: PullRequestInfo,
    pub(super) preview_key: String,
    pub(super) preview_params: EphemeralPreviewParams,
}

/// Opens the PR of a pushed proposal branch via `ForgeProvider`, invalidates the proposal cache,
/// audits the creation and derives the ephemeral preview parameters (`SDD.md` §4.2).
pub(super) async fn open_proposal_pr(
    state: &AppState,
    auth: &AuthSession,
    repo: &ScopeRepo,
    req: &CreateProposalRequest,
    pushed: PushedProposal<'_>,
) -> Result<OpenedProposal, ServerError> {
    let pr_title = proposal_pr_title(
        req.title.as_deref(),
        &req.tenant,
        pushed.verb,
        pushed.instance_name,
    );
    let pr_body = proposal_pr_body(
        req.title.as_deref(),
        &pr_title,
        req.body.as_deref(),
        pushed.summary,
    );
    let pr = repo
        .open_pull_request(auth, &req.tenant, pushed.branch, pr_title, pr_body)
        .await?;
    state.proposal_cache.invalidate(&repo.repository().label);
    audit_proposal_event("create", auth, repo, &pr);
    let preview_key = repo.preview_key(&pr.id)?;

    let mut preview_params = EphemeralPreviewParams::for_action(
        &preview_key,
        &req.tenant,
        pushed.action_label,
        pushed.timestamp,
        pushed.rel_dir,
        format!(
            "organization/{}-pr-{preview_key}/{}",
            pushed.instance_name, req.tenant
        ),
        &state.backend_url,
    )?;
    preview_params.head_sha = Some(pushed.commit_sha.to_string());
    preview_params.repo = Some(repo.repository().label);
    Ok(OpenedProposal {
        pr,
        preview_key,
        preview_params,
    })
}

/// Posts the (simulated or provisional) preview report as a PR comment (best effort), caches the
/// preview under `preview_key` and builds the `201 Created` proposal response.
pub(super) async fn publish_proposal(
    state: &AppState,
    repo: &ScopeRepo,
    pr: PullRequestInfo,
    preview_key: String,
    branch: String,
    preview_stack: PreviewStack,
    report: PreviewReport,
) -> (StatusCode, Json<CreateProposalResponse>) {
    let markdown = simulated_preview_markdown(&report, &pr.id);
    if let Err(err) = repo.forge.post_preview_comment(&pr.id, &markdown).await {
        tracing::warn!(pr_id = %pr.id, error = %err, "Failed to post preview comment to forge");
    }

    let candidates = PreviewResourceCandidate::from_preview_bundle(&preview_stack).to_vec();
    state.preview_cache.write().await.insert(
        preview_key,
        CachedPreview {
            report: report.clone(),
            preview_stack: preview_stack.clone(),
            candidates,
        },
    );

    (
        StatusCode::CREATED,
        Json(CreateProposalResponse {
            proposal: repo.scoped(pr),
            branch,
            preview_stack: preview_stack.stack,
            report,
            secret_notices: Vec::new(),
        }),
    )
}

/// Returns `true` when `title` already follows Conventional Commits (`type(scope)!: subject`).
fn is_conventional_commit_title(title: &str) -> bool {
    const TYPES: &[&str] = &[
        "feat", "fix", "chore", "docs", "refactor", "revert", "perf", "test", "ci", "build",
        "style",
    ];
    let Some((head, subject)) = title.split_once(": ") else {
        return false;
    };
    if subject.trim().is_empty() {
        return false;
    }
    let head = head.strip_suffix('!').unwrap_or(head);
    let (kind, scope) = match head.split_once('(') {
        Some((k, rest)) => match rest.strip_suffix(')') {
            Some(scope) if !scope.is_empty() && !scope.contains(['(', ')']) => (k, Some(scope)),
            _ => return false,
        },
        None => (head, None),
    };
    TYPES.contains(&kind) && scope.is_none_or(|s| !s.chars().any(char::is_whitespace))
}

/// Builds the PR title for a tenant workload proposal (UI-15): a caller-supplied title is kept
/// only when it is already a Conventional Commit; otherwise the server-generated
/// `feat(<tenant>): <verb> <name>` title is used so UI and CLI proposals are uniform.
fn proposal_pr_title(requested: Option<&str>, tenant: &str, verb: &str, name: &str) -> String {
    match requested.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) if is_conventional_commit_title(t) => t.to_string(),
        _ => format!("feat({tenant}): {verb} {name}"),
    }
}

/// Builds an informative PR body: caller-supplied body (or a non-conventional caller title that
/// was replaced by the generated title) followed by the server-generated summary.
fn proposal_pr_body(
    requested_title: Option<&str>,
    final_title: &str,
    requested_body: Option<&str>,
    summary: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = requested_title.map(str::trim).filter(|t| !t.is_empty())
        && t != final_title
    {
        parts.push(format!("**{t}**"));
    }
    if let Some(b) = requested_body.map(str::trim).filter(|b| !b.is_empty()) {
        parts.push(b.to_string());
    }
    parts.push(summary.to_string());
    parts.join("\n\n")
}

/// Emits a structured audit log line for proposal lifecycle actions (NFR-OBS). Never logs secrets.
pub(super) fn audit_proposal_event(
    action: &str,
    auth: &AuthSession,
    repo: &ScopeRepo,
    pr: &PullRequestInfo,
) {
    let tenant = repo
        .tenant()
        .or_else(|| crate::proposal_repo::pr_tenant(pr))
        .unwrap_or("-");
    tracing::info!(
        target: "keel::audit",
        action,
        user = %auth.claims.sub,
        tenant,
        scope = repo.scope.as_str(),
        repository = %repo.repository().label,
        pr_id = %pr.id,
        branch = %pr.source_branch,
        state = %pr.state,
        "proposal {action}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_titles_are_detected_and_enforced() {
        for ok in [
            "feat(acme): deploy redis",
            "fix(acme): bump chart",
            "chore(catalog): delete app podinfo",
            "feat!: breaking",
            "revert(core-addon): rollback kyverno",
        ] {
            assert!(is_conventional_commit_title(ok), "{ok}");
        }
        for bad in [
            "[New] feat(acme): deploy redis",
            "Deploy Redis for acme",
            "feat(acme):",
            "feat(): x",
            "feat(a b): x",
            "wip(acme): x",
        ] {
            assert!(!is_conventional_commit_title(bad), "{bad}");
        }
        assert_eq!(
            proposal_pr_title(Some("Deploy Redis for acme"), "acme", "deploy", "redis"),
            "feat(acme): deploy redis"
        );
        assert_eq!(
            proposal_pr_title(Some("fix(acme): pin redis"), "acme", "update", "redis"),
            "fix(acme): pin redis"
        );
        assert_eq!(
            proposal_pr_title(None, "acme", "provision", "vm-1"),
            "feat(acme): provision vm-1"
        );
        let body = proposal_pr_body(
            Some("Deploy Redis for acme"),
            "feat(acme): deploy redis",
            Some("please review"),
            "Automated proposal",
        );
        assert_eq!(
            body,
            "**Deploy Redis for acme**\n\nplease review\n\nAutomated proposal"
        );
        assert_eq!(
            proposal_pr_body(None, "feat(acme): deploy redis", None, "S"),
            "S"
        );
    }
}
