//! Proposal preview reports: synthesized reports, `GET .../preview` and PR diff reads.

use std::path::Path;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use keel_core::validate_k8s_manifests_yaml;
use keel_git::{PullRequestComment, PullRequestInfo, PullRequestState, validate_commit_sha};
use keel_pko::{
    ChangeOp, PreviewReport, PreviewRunStatus, PreviewSource, PreviewSummary, ResourceDiff,
    format_markdown_report,
};
use serde::Serialize;
use serde_json::Value;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::proposal_repo::{ProposalScope, ScopeRepo};
use crate::state::AppState;

/// What a proposal changes, compared against the target branch (`main`).
pub(super) struct ProposedChange<'a> {
    /// Resource type of the proposed top-level object (`HelmRelease`, `Stack`, ...).
    pub resource_type: &'a str,
    pub instance_name: &'a str,
    /// The instance directory already exists on `main` (an edit, not a first install).
    pub is_update: bool,
    /// Rendered manifest documents the proposal commits (`manifests.yaml`), if any.
    pub rendered_manifests: Option<&'a str>,
    /// The instance's rendered manifest documents on `main`, if any.
    pub previous_rendered_manifests: Option<&'a str>,
}

/// `apiVersion`/`kind`/`namespace`/`name` identity of a rendered manifest document.
fn manifest_identity(doc: &Value, default_name: &str) -> (String, String, String, String) {
    let field = |ptr: &str| doc.pointer(ptr).and_then(Value::as_str);
    (
        field("/apiVersion").unwrap_or("v1").to_string(),
        field("/kind").unwrap_or("Resource").to_string(),
        field("/metadata/namespace").unwrap_or_default().to_string(),
        field("/metadata/name").unwrap_or(default_name).to_string(),
    )
}

/// Builds the creation-time report: a control-plane synthesis (`source: synthesized`) listing
/// the proposed top-level object plus every rendered manifest document (UI-14), annotated with
/// the PKO run status. Operations compare against `main`: an edit of an existing instance is an
/// update (`~`), a rendered document is created, updated, unchanged or deleted depending on the
/// instance's documents on `main`. Real PKO results replace it once the run finishes.
pub(super) fn synthesize_proposal_report(
    pr_id: &str,
    tenant: &str,
    change: &ProposedChange<'_>,
    run: PreviewRunStatus,
) -> PreviewReport {
    let ProposedChange {
        resource_type,
        instance_name,
        is_update,
        rendered_manifests,
        previous_rendered_manifests,
    } = *change;
    let mut resources = vec![ResourceDiff {
        urn: format!("urn:pulumi:pr-{pr_id}::{tenant}::{resource_type}::{instance_name}"),
        resource_type: resource_type.to_string(),
        name: instance_name.to_string(),
        op: if is_update {
            ChangeOp::Update
        } else {
            ChangeOp::Create
        },
        changed_fields: vec!["spec".to_string()],
    }];
    let parse = |raw: Option<&str>| {
        raw.and_then(|r| validate_k8s_manifests_yaml(r).ok())
            .unwrap_or_default()
    };
    let mut previous: Vec<_> = parse(previous_rendered_manifests)
        .into_iter()
        .map(|doc| (manifest_identity(&doc, instance_name), doc))
        .collect();
    let mut push = |(api_v, kind, _ns, name): (String, String, String, String), op| {
        let k8s_type = format!("kubernetes:{api_v}:{kind}");
        resources.push(ResourceDiff {
            urn: format!("urn:pulumi:pr-{pr_id}::{tenant}::{k8s_type}::{name}"),
            resource_type: k8s_type,
            name,
            op,
            changed_fields: if op == ChangeOp::Same {
                Vec::new()
            } else {
                vec!["metadata".to_string(), "spec".to_string()]
            },
        });
    };
    for doc in parse(rendered_manifests) {
        let id = manifest_identity(&doc, instance_name);
        let op = match previous.iter().position(|(prev_id, _)| *prev_id == id) {
            Some(i) if previous.remove(i).1 == doc => ChangeOp::Same,
            Some(_) => ChangeOp::Update,
            None => ChangeOp::Create,
        };
        push(id, op);
    }
    for (id, _) in previous {
        push(id, ChangeOp::Delete);
    }
    let mut summary = PreviewSummary::default();
    for r in &resources {
        match r.op {
            ChangeOp::Create => summary.create += 1,
            ChangeOp::Update => summary.update += 1,
            ChangeOp::Delete => summary.delete += 1,
            ChangeOp::Replace => summary.replace += 1,
            ChangeOp::Same => summary.same += 1,
            ChangeOp::Drift => summary.drift += 1,
        }
    }
    PreviewReport {
        summary,
        resources,
        source: PreviewSource::Synthesized,
        run: Some(run),
    }
}

/// Response body for `GET /api/v1/tenants/{tenant}/proposals/{pr_id}/preview`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalPreviewResponse {
    pub pr_id: String,
    /// Repository-scoped key of the PR's preview (`keel.io/pr-id` label, preview `Stack` name
    /// `preview-stack-pr-<key>`, SSE events).
    pub preview_key: String,
    pub scope: String,
    pub repository: crate::proposal_repo::ProposalRepository,
    pub url: String,
    pub state: PullRequestState,
    pub source_branch: String,
    pub target_branch: String,
    pub report: PreviewReport,
    pub comments: Vec<PullRequestComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_diff: Option<String>,
}

pub(crate) fn synthesize_report_from_git_diff(pr_id: &str, git_diff: &str) -> PreviewReport {
    let mut resources = Vec::new();
    let mut summary = PreviewSummary::default();

    let mut current_path: Option<String> = None;
    let mut current_op = ChangeOp::Update;

    let flush = |path_opt: Option<String>,
                 op: ChangeOp,
                 resources: &mut Vec<ResourceDiff>,
                 summary: &mut PreviewSummary| {
        let Some(path) = path_opt else { return };
        let parts: Vec<&str> = path.split('/').collect();
        let (tenant, resource_type, name) = if parts.first() == Some(&"tenants") && parts.len() >= 4
        {
            let tenant = parts[1];
            let file_name = parts.last().copied().unwrap_or("");
            if parts[2] == "infra" {
                let res_name = if parts.len() >= 5 {
                    parts[3].to_string()
                } else {
                    file_name.trim_end_matches(".yaml").to_string()
                };
                if file_name == "stack.yaml" || parts.len() == 4 {
                    (tenant, "pulumi:pulumi.com/v1:Stack", res_name)
                } else {
                    return;
                }
            } else if parts[2] == "apps" {
                let res_name = if parts.len() >= 5 {
                    parts[3].to_string()
                } else {
                    file_name.trim_end_matches(".yaml").to_string()
                };
                if file_name == "helmrelease.yaml"
                    || file_name == "helmrelease-canary.yaml"
                    || parts.len() == 4
                {
                    (
                        tenant,
                        "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
                        res_name,
                    )
                } else if file_name == "flux-kustomization.yaml" {
                    (
                        tenant,
                        "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization",
                        res_name,
                    )
                } else if file_name == "stack.yaml" {
                    (tenant, "pulumi:pulumi.com/v1:Stack", res_name)
                } else {
                    return;
                }
            } else if parts[2] == "bundles" && parts.len() >= 6 {
                let res_name = format!("{}-{}", parts[3], parts[4]);
                if file_name == "stack.yaml" {
                    (tenant, "pulumi:pulumi.com/v1:Stack", res_name)
                } else if file_name == "helmrelease.yaml" {
                    (
                        tenant,
                        "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
                        res_name,
                    )
                } else {
                    return;
                }
            } else if file_name == "rbac.yaml" {
                (
                    tenant,
                    "k8s:rbac.authorization.k8s.io/v1:RoleBinding",
                    format!("tenant-{tenant}"),
                )
            } else {
                (tenant, "gitops:manifest", path.clone())
            }
        } else if parts.first() == Some(&"catalog") && parts.len() >= 4 {
            let kind = match parts[1] {
                "apps" => "keel.io/v1alpha1:CatalogApplication",
                "infra" => "keel.io/v1alpha1:CatalogInfraBlueprint",
                "bundles" => "keel.io/v1alpha1:CatalogBundle",
                _ => "keel.io/v1alpha1:CatalogItem",
            };
            ("catalog", kind, parts[2].to_string())
        } else {
            ("system", "gitops:manifest", path.clone())
        };

        match op {
            ChangeOp::Create => summary.create += 1,
            ChangeOp::Update => summary.update += 1,
            ChangeOp::Delete => summary.delete += 1,
            ChangeOp::Replace => summary.replace += 1,
            ChangeOp::Same => summary.same += 1,
            ChangeOp::Drift => summary.drift += 1,
        }

        resources.push(ResourceDiff {
            urn: format!("urn:pulumi:pr-{pr_id}::{tenant}::{resource_type}::{name}"),
            resource_type: resource_type.to_string(),
            name,
            op,
            changed_fields: vec!["spec".to_string()],
        });
    };

    for line in git_diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            flush(
                current_path.take(),
                current_op,
                &mut resources,
                &mut summary,
            );
            current_op = ChangeOp::Update;
            if let Some((_, b_path)) = rest.split_once(" b/") {
                current_path = Some(b_path.to_string());
            }
        } else if line.starts_with("new file mode ") {
            current_op = ChangeOp::Create;
        } else if line.starts_with("deleted file mode ") {
            current_op = ChangeOp::Delete;
        }
    }
    flush(
        current_path.take(),
        current_op,
        &mut resources,
        &mut summary,
    );

    PreviewReport {
        resources,
        summary,
        source: PreviewSource::Synthesized,
        run: None,
    }
}

/// Banner prepended to PR comments for control-plane-synthesized previews.
const SIMULATED_PREVIEW_NOTICE: &str = "> **Simulated diff — PKO preview not executed.** This report was synthesized by the Keel control plane from the proposed manifests; it is not the output of a `pulumi preview` run and does not include policy evaluation.\n\n";

/// Renders the PR comment markdown, flagging synthesized reports as simulated.
pub(super) fn simulated_preview_markdown(report: &PreviewReport, pr_id: &str) -> String {
    let body = format_markdown_report(report, pr_id);
    if report.source != PreviewSource::Synthesized {
        return body;
    }
    let pko_in_flight = report.run.as_ref().is_some_and(|r| {
        matches!(
            r.phase,
            keel_pko::PreviewPhase::Pending | keel_pko::PreviewPhase::Running
        )
    });
    if pko_in_flight {
        format!("{PROVISIONAL_PREVIEW_NOTICE}{body}")
    } else {
        format!("{SIMULATED_PREVIEW_NOTICE}{body}")
    }
}

/// Banner for the creation-time comment while the real PKO preview is still running.
const PROVISIONAL_PREVIEW_NOTICE: &str = "> **Provisional diff — PKO preview running.** This summary was synthesized by the Keel control plane from the proposed manifests; the real `pulumi preview` results will be posted as a follow-up comment when the ephemeral PKO preview completes.\n\n";

/// `GET /api/v1/tenants/{tenant}/proposals/{pr_id}/preview`
pub async fn get_proposal_preview(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((scope, pr_id)): AxumPath<(String, String)>,
) -> Result<Json<ProposalPreviewResponse>, ServerError> {
    let scope = ProposalScope::parse(&scope)?;
    scope.require_access(&auth)?;
    let repo = ScopeRepo::resolve(&state, &scope)?;
    let pr = state.proposal_cache.get_pull_request(&repo, &pr_id).await?;
    let preview_key = repo.preview_key(&pr.id)?;
    // Cluster-persisted PKO report first (survives restarts / shared across replicas), then the
    // optional in-memory cache, then a synthesis from the Git diff.
    let (cluster_report, live_run) =
        crate::preview_lifecycle::load_cluster_preview(&state, &preview_key).await;
    let cached_report = state
        .preview_cache
        .read()
        .await
        .get(&preview_key)
        .map(|c| c.report.clone());

    let git_diff = compute_pr_git_diff(&state, &repo, &pr).await;

    let report = match (cluster_report, cached_report) {
        (Some(r), _) => r,
        (None, cached) => {
            let mut r = match cached {
                Some(r) => r,
                None => match git_diff.as_deref() {
                    Some(diff) => synthesize_report_from_git_diff(&preview_key, diff),
                    None => PreviewReport::default(),
                },
            };
            if let Some(live) = live_run {
                r.run = Some(live);
            }
            r
        }
    };

    Ok(Json(ProposalPreviewResponse {
        pr_id: pr.id,
        preview_key,
        scope: scope.as_str().to_string(),
        repository: repo.repository(),
        url: pr.url,
        state: pr.state,
        source_branch: pr.source_branch,
        target_branch: pr.target_branch,
        report,
        comments: pr.comments,
        git_diff,
    }))
}

/// Private ref namespace used by read-only handlers so their fetches never race the
/// `refs/remotes/origin/*` updates performed by mutation paths under `git_lock`.
pub(super) const READ_REF_NAMESPACE: &str = "keel-read/origin";

/// Repeated read-path fetches of the same branch are coalesced within this window.
const READ_FETCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on a read-path network fetch; the git child is killed on expiry.
const READ_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

type FetchDebounceMap = std::collections::HashMap<(std::path::PathBuf, String), std::time::Instant>;

static LAST_READ_FETCH: std::sync::LazyLock<std::sync::Mutex<FetchDebounceMap>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(FetchDebounceMap::new()));

/// Drops the read-fetch debounce of `workdir`, so the next read fetches every branch again: called
/// after Keel itself moved a branch (merge, stale-proposal rebuild) so diffs never lag behind it.
pub(super) fn forget_read_fetches(workdir: &Path) {
    LAST_READ_FETCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(dir, _), _| dir != workdir);
}

/// Fetches `branches` into [`READ_REF_NAMESPACE`] **without** taking `git_lock`, debounced per
/// `(workdir, branch)` and bounded by a short timeout. Failures are logged and tolerated: callers
/// fall back to the last known `origin/*` refs.
pub(super) async fn fetch_branches_for_read(state: &AppState, workdir: &Path, branches: &[&str]) {
    let now = std::time::Instant::now();
    let due: Vec<&str> = {
        let mut map = LAST_READ_FETCH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.retain(|_, at| now.duration_since(*at) < READ_FETCH_DEBOUNCE * 6);
        branches
            .iter()
            .copied()
            .filter(|b| {
                let key = (workdir.to_path_buf(), (*b).to_string());
                match map.get(&key) {
                    Some(at) if now.duration_since(*at) < READ_FETCH_DEBOUNCE => false,
                    _ => {
                        map.insert(key, now);
                        true
                    }
                }
            })
            .collect()
    };
    if due.is_empty() {
        return;
    }
    if let Err(e) = state
        .git_runner
        .fetch_branches_isolated(
            workdir,
            "origin",
            &due,
            READ_REF_NAMESPACE,
            READ_FETCH_TIMEOUT,
        )
        .await
    {
        tracing::debug!(error = %e, "read-path git fetch failed; using last known refs");
    }
}

/// Resolves the freshest locally known ref for remote `branch`: the read-path namespace when
/// present, else `origin/<branch>`.
async fn resolve_read_ref(state: &AppState, workdir: &Path, branch: &str) -> String {
    let isolated = format!("refs/{READ_REF_NAMESPACE}/{branch}");
    if state
        .git_runner
        .ref_exists(workdir, &isolated)
        .await
        .unwrap_or(false)
    {
        isolated
    } else {
        format!("origin/{branch}")
    }
}

/// Computes the diff a PR actually proposes (OPEN-2), never a two-dot diff against current `main`:
///
/// - open/closed PRs: `<target>...<head>` (merge-base three-dot),
/// - merged PRs: `<recorded base>...<head>` when the forge recorded a base, else the merge
///   commit's first-parent diff, else three-dot against the target.
///
/// Platform repository: reads refs/objects only (no checkout), so it does not need `git_lock`. A
/// tenant repository clone is created/refreshed under its own lock first.
async fn compute_pr_git_diff(
    state: &AppState,
    repo: &ScopeRepo,
    pr: &PullRequestInfo,
) -> Option<String> {
    // A tenant repository is only cloned (under its lock) when this replica has no clone yet;
    // otherwise the refs below are fetched without the lock, debounced.
    if repo.is_dedicated()
        && !tokio::fs::try_exists(repo.workdir.join(".git"))
            .await
            .unwrap_or(false)
    {
        let _guard = repo.lock(state).await;
        if let Err(e) = repo.sync_main(state).await {
            tracing::debug!(error = %e, "tenant repository clone unavailable for the PR diff");
            return None;
        }
    }
    if !tokio::fs::try_exists(repo.workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        return None;
    }
    let mut branches = vec![pr.target_branch.as_str()];
    if pr.state != PullRequestState::Merged {
        branches.push(pr.source_branch.as_str());
    }
    fetch_branches_for_read(state, &repo.workdir, &branches).await;

    let target_ref = resolve_read_ref(state, &repo.workdir, &pr.target_branch).await;
    let runner = &state.git_runner;
    let dir = &repo.workdir;
    let head_sha = validate_commit_sha(&pr.head_sha).ok();
    let diff = match pr.state {
        PullRequestState::Merged => {
            let base = pr
                .base_sha
                .as_deref()
                .and_then(|b| validate_commit_sha(b).ok());
            let merge = pr
                .merge_commit_sha
                .as_deref()
                .and_then(|m| validate_commit_sha(m).ok());
            let mut out = None;
            if let (Some(base), Some(head)) = (base, head_sha) {
                out = runner.diff_refs(dir, base, head).await.ok();
            }
            if out.as_deref().is_none_or(str::is_empty)
                && let Some(merge) = merge
            {
                out = runner.diff_first_parent(dir, merge).await.ok();
            }
            if out.as_deref().is_none_or(str::is_empty)
                && let Some(head) = head_sha
            {
                out = runner.diff_refs(dir, &target_ref, head).await.ok();
            }
            out
        }
        PullRequestState::Open | PullRequestState::Closed => {
            let source_ref = resolve_read_ref(state, &repo.workdir, &pr.source_branch).await;
            match runner.diff_refs(dir, &target_ref, &source_ref).await {
                Ok(d) => Some(d),
                Err(_) => match head_sha {
                    Some(head) => runner.diff_refs(dir, &target_ref, head).await.ok(),
                    None => None,
                },
            }
        }
    };
    diff.filter(|d| !d.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(report: &PreviewReport) -> Vec<(String, ChangeOp)> {
        report
            .resources
            .iter()
            .map(|r| (r.name.clone(), r.op))
            .collect()
    }

    /// An edit of an existing instance previews as an update against `main`, never as a create
    /// (dry-run snag: "update podinfo" showed `+ Create HelmRelease`).
    #[test]
    fn proposal_report_compares_against_main() {
        let run = || PreviewRunStatus::not_executed("Flux-managed change");
        let hr = "flux:helm.toolkit.fluxcd.io/v2:HelmRelease";
        let first = synthesize_proposal_report(
            "1",
            "default",
            &ProposedChange {
                resource_type: hr,
                instance_name: "podinfo",
                is_update: false,
                rendered_manifests: None,
                previous_rendered_manifests: None,
            },
            run(),
        );
        assert_eq!(ops(&first), [("podinfo".to_string(), ChangeOp::Create)]);
        assert_eq!(first.summary.create, 1);

        let edit = synthesize_proposal_report(
            "3",
            "default",
            &ProposedChange {
                resource_type: hr,
                instance_name: "podinfo",
                is_update: true,
                rendered_manifests: None,
                previous_rendered_manifests: None,
            },
            run(),
        );
        assert_eq!(ops(&edit), [("podinfo".to_string(), ChangeOp::Update)]);
        assert_eq!((edit.summary.create, edit.summary.update), (0, 1));
        let md = format_markdown_report(&edit, "3");
        assert!(
            md.contains("`~ 1 Update`") && md.contains("`+ 0 Create`"),
            "{md}"
        );

        // Rendered documents: changed -> update, identical -> unchanged, new -> create, gone ->
        // delete.
        let cm = |name: &str, v: &str| {
            format!("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {name}\ndata:\n  k: {v}\n")
        };
        let before = [cm("changed", "a"), cm("same", "a"), cm("gone", "a")].join("---\n");
        let after = [cm("changed", "b"), cm("same", "a"), cm("new", "a")].join("---\n");
        let report = synthesize_proposal_report(
            "4",
            "default",
            &ProposedChange {
                resource_type: "pulumi:pulumi.com/v1:Stack",
                instance_name: "echo",
                is_update: true,
                rendered_manifests: Some(&after),
                previous_rendered_manifests: Some(&before),
            },
            run(),
        );
        assert_eq!(
            ops(&report),
            [
                ("echo".to_string(), ChangeOp::Update),
                ("changed".to_string(), ChangeOp::Update),
                ("same".to_string(), ChangeOp::Same),
                ("new".to_string(), ChangeOp::Create),
                ("gone".to_string(), ChangeOp::Delete),
            ]
        );
        let s = &report.summary;
        assert_eq!((s.create, s.update, s.same, s.delete), (1, 2, 1, 1));
    }

    #[test]
    fn synthesized_preview_markdown_is_marked_simulated() {
        let report = synthesize_report_from_git_diff("7", "");
        assert_eq!(report.source, PreviewSource::Synthesized);
        let md = simulated_preview_markdown(&report, "7");
        assert!(md.starts_with("> **Simulated diff"));
        let pko = PreviewReport {
            source: PreviewSource::Pko,
            ..PreviewReport::default()
        };
        assert!(!simulated_preview_markdown(&pko, "7").contains("Simulated diff"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["source"], "synthesized");
    }
}
