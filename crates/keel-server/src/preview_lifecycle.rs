//! Ephemeral PKO preview lifecycle orchestration (`SDD.md` §4.2, `NFR-REL-02`).
//!
//! - [`launch_pko_preview`]: server-side applies a preview `Stack` (`spec.preview: true`) into
//!   `keel-previews` and spawns a watcher publishing honest SSE progress
//!   (Pending → Running → Succeeded/Failed).
//! - [`sync_preview`]: maps live PKO status to a [`PreviewRunStatus`] and, once a run for a new
//!   commit finishes, collects the plan from the workspace log, persists it in the cluster
//!   (report `ConfigMap` + `keel.io/preview-reported-commit` annotation) and updates the PR.
//! - [`reconcile_previews_once`] / [`spawn_preview_reconciler`]: restart-/replica-safe GC and
//!   re-publication driven only by cluster state and forge PR states (no in-memory state).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use keel_core::KEEL_PREVIEWS_NAMESPACE;
use keel_git::{ForgeError, PullRequestState};
use keel_kube::{CrdReconcileStatus, CrdWatchEvent, PreviewCluster, WatchedCrdKind};
use keel_pko::{
    ANNOTATION_HEAD_SHA, ANNOTATION_REPORTED_COMMIT, GcPlan, PREVIEW_LOG_TAIL_LINES, PrLifecycle,
    PreviewPhase, PreviewRecord, PreviewReport, PreviewResourceCandidate, PreviewRunStatus,
    PreviewSecretCopy, PreviewStack, build_pko_preview_report, format_markdown_report,
    is_tenant_scoped_backend, map_pko_preview_status, plan_preview_gc, plan_preview_reconcile,
    select_update_name,
};

use crate::state::AppState;

/// Poll interval of the per-preview watcher spawned at launch.
pub const PREVIEW_WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// Upper bound for the per-preview watcher; the background reconciler takes over afterwards.
pub const PREVIEW_WATCH_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Default background reconciler interval (jittered by up to +25%).
pub const PREVIEW_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// Deterministic preview `Stack` name for a repository-scoped preview key
/// ([`keel_pko::preview_key`]: `preview-stack-pr-<key>`, hash-shortened when long).
pub fn preview_stack_name(preview_key: &str) -> String {
    keel_pko::preview_stack_name(preview_key).unwrap_or_else(|_| {
        format!(
            "preview-stack-pr-{}",
            preview_key
                .trim()
                .trim_start_matches('#')
                .to_ascii_lowercase()
        )
    })
}

fn telemetry_status(phase: PreviewPhase) -> CrdReconcileStatus {
    match phase {
        PreviewPhase::Pending | PreviewPhase::Running => CrdReconcileStatus::Previewing,
        PreviewPhase::Succeeded => CrdReconcileStatus::Ready,
        PreviewPhase::Failed => CrdReconcileStatus::Failed,
        PreviewPhase::NotExecuted => CrdReconcileStatus::Unknown,
    }
}

/// `PR #<number> in <repository>` of a repository-scoped preview key (`platform` or
/// `tenant-<id>`), the repository identity carried by preview SSE events.
pub fn preview_pr_label(preview_key: &str) -> String {
    match keel_pko::parse_preview_key(preview_key) {
        (Some(tenant), number) => format!("PR #{number} in tenant-{tenant}"),
        (None, number) => format!("PR #{number} in {}", keel_pko::PLATFORM_REPO_LABEL),
    }
}

fn publish_phase(state: &AppState, pr_id: &str, stack_name: &str, run: &PreviewRunStatus) {
    let mut message = format!(
        "PKO preview {} for {}",
        run.phase.label(),
        preview_pr_label(pr_id)
    );
    if let Some(m) = &run.message {
        message.push_str(": ");
        message.push_str(m);
    }
    state.telemetry.publish(CrdWatchEvent::new(
        WatchedCrdKind::Stack,
        KEEL_PREVIEWS_NAMESPACE,
        stack_name,
        telemetry_status(run.phase),
        message,
        run.commit.clone(),
    ));
}

/// Applies `bundle` to the live cluster and spawns its watcher.
///
/// Returns the initial run status: `Pending` (with `stack` set) when applied, `Failed` with the
/// API/admission error when the apply was rejected, or `NotExecuted` without a live cluster.
///
/// `secret_copies` are the per-preview Secrets (read-only Git credentials, state passphrase,
/// provider credentials) planned by `attach_preview_cluster_inputs` /
/// `EphemeralPreviewParams::inherit_tenant_credentials`. Order matters: PKO resolves the preview
/// `Stack`'s `gitAuth` (and workspace `envRefs`) as soon as it sees the object, and a missing
/// Secret stalls it with `SpecInvalid`, which PKO never retries until the spec changes. So:
///
/// 1. copy every Secret (no owner yet, labelled `keel.io/pr-id`); on failure nothing is applied;
/// 2. apply the `Stack`;
/// 3. adopt the copies (ownerReference → the live `Stack` UID) so they are GC'd with it.
///
/// Copies are removed again when the launch fails before a `Stack` of this PR existed; an
/// adoption failure tears the whole preview down (`Stack` + copies), so a running preview's
/// credentials are always owned by its `Stack`. Copies orphaned by a crash between the steps are
/// deleted by label on PR merge/close ([`gc_preview_for_pr`], [`reconcile_previews_once`]).
pub async fn launch_pko_preview(
    state: &AppState,
    bundle: &PreviewStack,
    secret_copies: &[PreviewSecretCopy],
) -> PreviewRunStatus {
    let Some(cluster) = state.preview_cluster.clone() else {
        return PreviewRunStatus::not_executed(
            "No live cluster connected: PKO preview not executed",
        );
    };
    // OPEN-9b defense in depth: the preview only ever holds the tenant-scoped state identity.
    if let Some(backend) = bundle.stack.spec.backend.as_deref()
        && !is_tenant_scoped_backend(backend, &bundle.tenant)
    {
        return PreviewRunStatus::not_executed(format!(
            "Preview backend is outside the `tenants/{}` state prefix: PKO preview not executed",
            bundle.tenant
        ));
    }
    // Defense in depth: only previews whose Git source was resolved from the tenant's own
    // platform-written source are ever applied (never one without `projectRepo`).
    if bundle.stack.spec.git_source.is_none() {
        return PreviewRunStatus::not_executed(format!(
            "Tenant `{}` has no resolved Git source for the preview: PKO preview not executed",
            bundle.tenant
        ));
    }
    let stack = match bundle.to_json_value() {
        Ok(doc) => doc,
        Err(e) => return PreviewRunStatus::apply_failed(format!("preview synthesis failed: {e}")),
    };
    let stack_name = bundle.stack.metadata.name.clone();
    let pr_id = bundle
        .stack
        .metadata
        .labels
        .get("keel.io/pr-id")
        .cloned()
        .unwrap_or_else(|| bundle.pr_id.clone());
    // A re-launch (new commit) updates a live preview whose adopted copies must survive a failed
    // re-launch; an unreadable Stack is treated as live (never delete copies in use).
    let preexisting = !matches!(cluster.get_preview_stack(&stack_name).await, Ok(None));
    let cleanup_copies = || async {
        if preexisting {
            return;
        }
        if let Err(e) = cluster.delete_preview_secrets(&pr_id).await {
            tracing::warn!(pr_id = %bundle.pr_id, error = %e, "failed to delete preview Secret copies; they are removed by label on PR merge/close");
        }
    };

    let copied = match cluster
        .copy_preview_secrets(&bundle.tenant, &pr_id, secret_copies)
        .await
    {
        Ok(copied) => copied,
        Err(e) => {
            tracing::warn!(pr_id = %bundle.pr_id, error = %e, "failed to copy preview credentials");
            cleanup_copies().await;
            let run = PreviewRunStatus::not_executed(format!(
                "Credentials for the preview are unavailable ({e}): PKO preview not executed"
            ));
            publish_phase(state, &bundle.pr_id, &stack_name, &run);
            return run;
        }
    };
    if let Err(e) = cluster.apply_preview(&stack).await {
        tracing::warn!(pr_id = %bundle.pr_id, error = %e, "failed to apply ephemeral preview Stack");
        cleanup_copies().await;
        let run = PreviewRunStatus::apply_failed(format!("Applying preview Stack failed: {e}"));
        publish_phase(state, &bundle.pr_id, &stack_name, &run);
        return run;
    }
    if let Err(e) = cluster.adopt_preview_secrets(&stack_name, &copied).await {
        tracing::warn!(pr_id = %bundle.pr_id, error = %e, "failed to attach preview Secret copies to the preview Stack; tearing the preview down");
        let _ = cluster.delete_preview_object("Stack", &stack_name).await;
        if let Err(e) = cluster.delete_preview_secrets(&pr_id).await {
            tracing::warn!(pr_id = %bundle.pr_id, error = %e, "failed to delete preview Secret copies; they are removed by label on PR merge/close");
        }
        let run = PreviewRunStatus::not_executed(format!(
            "Preview credentials could not be attached to the preview Stack ({e}): PKO preview not executed"
        ));
        publish_phase(state, &bundle.pr_id, &stack_name, &run);
        return run;
    }
    let run = PreviewRunStatus {
        phase: PreviewPhase::Pending,
        message: Some("Preview Stack applied; waiting for PKO".to_string()),
        stack: Some(format!("{KEEL_PREVIEWS_NAMESPACE}/{stack_name}")),
        commit: bundle
            .stack
            .metadata
            .annotations
            .get(ANNOTATION_HEAD_SHA)
            .cloned(),
        update: None,
    };
    publish_phase(state, &bundle.pr_id, &stack_name, &run);
    spawn_preview_watch(state.clone(), bundle.pr_id.clone(), stack_name);
    run
}

/// Annotation of a preview `Stack` re-pinned to a PR head Keel rebuilt itself (a stale proposal
/// rebuilt on `main`): nobody reviewed that head yet, so its merge waits until the PKO preview
/// of it has finished ([`rebuilt_head_preview_pending`]).
pub const ANNOTATION_REBUILT_HEAD: &str = "keel.io/rebuilt-head";

/// Re-runs the live preview of `preview_key` at `head_sha` after Keel replaced the PR head
/// (a stale proposal rebuilt on `main`): the preview `Stack` is re-applied pinned to the new
/// head (`spec.commit`, also for a preview that tracked the PR branch, so the spec and thus
/// `metadata.generation` always change and PKO starts a new run) with the `keel.io/head-sha`
/// and [`ANNOTATION_REBUILT_HEAD`] annotations, so PKO previews exactly the commit that will
/// be merged. `None` when the PR has no preview `Stack` (a Flux-only change, whose
/// creation-time synthesized report stays and whose Git diff is recomputed on read, or no live
/// cluster) or its spec has no Git source to re-pin.
pub async fn rerun_preview_at_commit(
    state: &AppState,
    preview_key: &str,
    head_sha: &str,
) -> Option<PreviewRunStatus> {
    let cluster = state.preview_cluster.clone()?;
    let stack_name = preview_stack_name(preview_key);
    let live = cluster
        .get_preview_stack(&stack_name)
        .await
        .ok()
        .flatten()?;
    let doc = repinned_preview_stack(&live, head_sha)?;
    let run = match cluster.apply_preview(&doc).await {
        Ok(()) => PreviewRunStatus {
            phase: PreviewPhase::Pending,
            message: Some(
                "Preview Stack re-pinned to the updated PR head; waiting for PKO".to_string(),
            ),
            stack: Some(format!("{KEEL_PREVIEWS_NAMESPACE}/{stack_name}")),
            commit: Some(head_sha.to_string()),
            update: None,
        },
        Err(e) => {
            tracing::warn!(preview_key, error = %e, "failed to re-apply the preview Stack at the updated PR head");
            PreviewRunStatus::apply_failed(format!("Re-applying preview Stack failed: {e}"))
        }
    };
    publish_phase(state, preview_key, &stack_name, &run);
    if run.phase == PreviewPhase::Pending {
        spawn_preview_watch(state.clone(), preview_key.to_string(), stack_name);
    }
    Some(run)
}

/// The applyable part of a live preview `Stack` (`apiVersion`, `kind`, name, namespace, labels,
/// annotations, `spec`) pinned to `head_sha`: `spec.commit` with a full-history checkout
/// (`shallow: false`, like [`keel_pko::PkoGitSource::commit`]) replacing a tracked `branch`, and
/// the `keel.io/head-sha` / [`ANNOTATION_REBUILT_HEAD`] annotations. Server-populated metadata
/// and `status` are dropped. `None` without a Git source (`spec.projectRepo`).
fn repinned_preview_stack(live: &serde_json::Value, head_sha: &str) -> Option<serde_json::Value> {
    use serde_json::{Value, json};
    let meta = live.get("metadata")?;
    let mut spec = live.get("spec")?.clone();
    let spec_map = spec.as_object_mut()?;
    spec_map.get("projectRepo").and_then(Value::as_str)?;
    spec_map.remove("branch");
    spec_map.insert("commit".to_string(), Value::String(head_sha.to_string()));
    spec_map.insert("shallow".to_string(), Value::Bool(false));
    let mut annotations = meta
        .get("annotations")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for key in [ANNOTATION_HEAD_SHA, ANNOTATION_REBUILT_HEAD] {
        annotations.insert(key.to_string(), Value::String(head_sha.to_string()));
    }
    Some(json!({
        "apiVersion": live.get("apiVersion")?,
        "kind": live.get("kind")?,
        "metadata": {
            "name": meta.get("name")?,
            "namespace": meta.get("namespace")?,
            "labels": meta.get("labels").cloned().unwrap_or_else(|| json!({})),
            "annotations": annotations,
        },
        "spec": spec,
    }))
}

/// The run of the preview of `preview_key` when the merge of `head_sha` must wait for it: the
/// preview `Stack` was re-pinned to `head_sha` by a stale-proposal rebuild
/// ([`ANNOTATION_REBUILT_HEAD`]) and PKO has not finished previewing that commit yet. `None`
/// otherwise (no live cluster or preview `Stack`, another head, or the run is over).
pub async fn rebuilt_head_preview_pending(
    state: &AppState,
    preview_key: &str,
    head_sha: &str,
) -> Option<PreviewRunStatus> {
    let cluster = state.preview_cluster.clone()?;
    let stack = cluster
        .get_preview_stack(&preview_stack_name(preview_key))
        .await
        .ok()
        .flatten()?;
    let update = match select_update_name(&stack) {
        Some(name) => cluster.get_update(&name).await.ok().flatten(),
        None => None,
    };
    let run = map_pko_preview_status(&stack, update.as_ref());
    rebuilt_head_awaits_preview(&stack, &run, head_sha).then_some(run)
}

/// `true` while the preview `Stack` was re-pinned to the rebuilt `head_sha` and neither a
/// published result nor a finished run for that commit exists.
fn rebuilt_head_awaits_preview(
    stack: &serde_json::Value,
    run: &PreviewRunStatus,
    head_sha: &str,
) -> bool {
    let same = |sha: &str| sha.eq_ignore_ascii_case(head_sha);
    let annotation = |key: &str| {
        stack
            .pointer("/metadata/annotations")
            .and_then(|a| a.get(key))
            .and_then(serde_json::Value::as_str)
    };
    if !annotation(ANNOTATION_REBUILT_HEAD).is_some_and(same) {
        return false;
    }
    if annotation(ANNOTATION_REPORTED_COMMIT).is_some_and(same) {
        return false;
    }
    !(run.phase.is_terminal() && run.commit.as_deref().is_none_or(same))
}

/// Polls one preview until its run is terminal and published (or [`PREVIEW_WATCH_TIMEOUT`]).
pub fn spawn_preview_watch(state: AppState, pr_id: String, stack_name: String) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + PREVIEW_WATCH_TIMEOUT;
        let mut last_phase: Option<PreviewPhase> = Some(PreviewPhase::Pending);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(PREVIEW_WATCH_INTERVAL).await;
            let Some(run) = sync_preview(&state, &pr_id, &stack_name).await else {
                return; // Stack deleted (merged/closed) or cluster unavailable.
            };
            if last_phase != Some(run.phase) {
                if !run.phase.is_terminal() {
                    publish_phase(&state, &pr_id, &stack_name, &run);
                }
                last_phase = Some(run.phase);
            }
            if run.phase.is_terminal() {
                return;
            }
        }
    });
}

/// Observes a preview `Stack`; publishes results once per finished PKO commit.
///
/// Idempotent and safe to run concurrently from the watcher, the reconciler and other replicas:
/// the `keel.io/preview-reported-commit` annotation is set before the PR comment is posted.
pub async fn sync_preview(
    state: &AppState,
    pr_id: &str,
    stack_name: &str,
) -> Option<PreviewRunStatus> {
    let cluster = state.preview_cluster.clone()?;
    let stack = match cluster.get_preview_stack(stack_name).await {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!(stack_name, error = %e, "preview Stack read failed");
            return None;
        }
    };
    let update = match select_update_name(&stack) {
        Some(name) => cluster.get_update(&name).await.ok().flatten(),
        None => None,
    };
    let run = map_pko_preview_status(&stack, update.as_ref());
    let Some(mut record) = PreviewRecord::from_stack_value(&stack) else {
        return Some(run);
    };
    record.run = run.clone();
    if record.needs_publish() {
        publish_results(state, cluster.as_ref(), pr_id, stack_name, &run).await;
    }
    Some(run)
}

async fn publish_results(
    state: &AppState,
    cluster: &dyn PreviewCluster,
    pr_id: &str,
    stack_name: &str,
    run: &PreviewRunStatus,
) {
    let log = if run.phase == PreviewPhase::Succeeded {
        cluster
            .workspace_log(stack_name, PREVIEW_LOG_TAIL_LINES)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let report = build_pko_preview_report(run, log.as_deref());
    let reported = run.commit.clone().unwrap_or_else(|| "-".to_string());
    let annotations = BTreeMap::from([(ANNOTATION_REPORTED_COMMIT.to_string(), reported)]);
    if let Err(e) = cluster
        .annotate_preview_stack(stack_name, &annotations)
        .await
    {
        tracing::warn!(stack_name, error = %e, "could not record published preview commit");
        return;
    }
    match serde_json::to_string(&report) {
        Ok(json) => {
            if let Err(e) = cluster.store_report(stack_name, &json).await {
                tracing::warn!(stack_name, error = %e, "could not persist preview report");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not serialize preview report"),
    }
    // `pr_id` is the repository-scoped preview key: comment on the PR in its own repository.
    match crate::proposal_repo::forge_for_preview_key(state, pr_id) {
        Ok((forge, number)) => {
            let markdown = format_markdown_report(&report, &number);
            if let Err(e) = forge.post_preview_comment(&number, &markdown).await {
                tracing::warn!(pr_id, error = %e, "failed to post PKO preview comment");
            }
        }
        Err(e) => {
            tracing::warn!(pr_id, error = %e, "cannot resolve the PR repository of a preview")
        }
    }
    if let Some(entry) = state.preview_cache.write().await.get_mut(pr_id) {
        entry.report = report.clone();
    }
    publish_phase(state, pr_id, stack_name, run);
}

/// Resolves the best available report for a PR from the cluster:
/// the persisted PKO report (with a live `run` overlay while a newer commit is being previewed),
/// or `(None, live_status)` while the first run has not completed yet.
pub async fn load_cluster_preview(
    state: &AppState,
    pr_id: &str,
) -> (Option<PreviewReport>, Option<PreviewRunStatus>) {
    let Some(cluster) = state.preview_cluster.clone() else {
        return (None, None);
    };
    let stack_name = preview_stack_name(pr_id);
    let live = match cluster.get_preview_stack(&stack_name).await {
        Ok(Some(stack)) => {
            let update = match select_update_name(&stack) {
                Some(n) => cluster.get_update(&n).await.ok().flatten(),
                None => None,
            };
            Some(map_pko_preview_status(&stack, update.as_ref()))
        }
        _ => None,
    };
    let stored = cluster
        .load_report(&stack_name)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<PreviewReport>(&raw).ok());
    match (stored, live) {
        (Some(mut report), Some(live)) => {
            if !live.phase.is_terminal() {
                report.run = Some(live.clone());
            }
            (Some(report), Some(live))
        }
        (stored, live) => (stored, live),
    }
}

fn candidates_from(values: &[serde_json::Value]) -> Vec<PreviewResourceCandidate> {
    values
        .iter()
        .filter_map(PreviewResourceCandidate::from_json_value)
        .collect()
}

async fn execute_gc(state: &AppState, cluster: &dyn PreviewCluster, plan: &GcPlan, reason: &str) {
    for target in &plan.targets {
        let kind = target.kind.kind_str();
        match cluster.delete_preview_object(kind, &target.name).await {
            Ok(_) => {
                if kind == "Stack" {
                    let _ = cluster
                        .delete_preview_object(
                            "ConfigMap",
                            &keel_kube::preview_report_configmap_name(&target.name),
                        )
                        .await;
                    state.telemetry.publish(CrdWatchEvent::new(
                        WatchedCrdKind::Stack,
                        KEEL_PREVIEWS_NAMESPACE,
                        &target.name,
                        CrdReconcileStatus::Deleted,
                        format!(
                            "Garbage-collected ephemeral preview for {reason} {}",
                            preview_pr_label(&target.pr_id)
                        ),
                        None,
                    ));
                }
            }
            Err(e) => tracing::warn!(
                name = %target.name,
                kind,
                error = %e,
                "failed to garbage-collect preview resource"
            ),
        }
    }
}

/// Garbage-collects every preview object of `pr_id` (merge/reject path). Reads candidates from
/// the cluster (falling back to the in-memory cache only when no preview runner is configured).
pub async fn gc_preview_for_pr(state: &AppState, pr_id: &str, reason: &str) -> GcPlan {
    let cached = state.preview_cache.write().await.remove(pr_id);
    if let Some(cluster) = state.preview_cluster.clone() {
        let values = cluster.list_preview_stacks().await.unwrap_or_default();
        let plan = plan_preview_gc(&candidates_from(&values), &[pr_id]);
        execute_gc(state, cluster.as_ref(), &plan, reason).await;
        // Adopted copies go with their Stack; this also removes copies never adopted.
        if let Err(e) = cluster.delete_preview_secrets(pr_id).await {
            tracing::warn!(pr_id, error = %e, "failed to garbage-collect preview Secret copies");
        }
        return plan;
    }
    match cached {
        Some(entry) => {
            let plan = plan_preview_gc(&entry.candidates, &[pr_id]);
            let _ = state.state_provider.execute_preview_gc(&plan).await;
            plan
        }
        None => GcPlan::default(),
    }
}

/// Outcome of one background reconciliation pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreviewReconcileOutcome {
    pub deleted: usize,
    pub published: usize,
    pub head_updates: usize,
    pub unknown_prs: usize,
}

/// One restart-safe reconciliation pass over all `keel-previews` preview objects.
pub async fn reconcile_previews_once(state: &AppState) -> PreviewReconcileOutcome {
    let mut outcome = PreviewReconcileOutcome::default();
    let Some(cluster) = state.preview_cluster.clone() else {
        return outcome;
    };
    let Ok(stacks) = cluster.list_preview_stacks().await else {
        return outcome;
    };
    let candidates = candidates_from(&stacks);
    let records: Vec<PreviewRecord> = stacks
        .iter()
        .filter_map(|s| {
            let mut rec = PreviewRecord::from_stack_value(s)?;
            // `from_stack_value` maps status without the Update; good enough for planning, the
            // publish path re-reads the authoritative Update.
            rec.run = map_pko_preview_status(s, None);
            Some(rec)
        })
        .collect();

    // PRs of per-preview Secret copies too, so copies never adopted by a Stack (launch failure,
    // keel-server stopped mid-launch) are still removed once their PR is merged/closed.
    let secret_pr_ids = match cluster.list_preview_secret_pr_ids().await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::debug!(error = %e, "could not list preview Secret copies");
            BTreeSet::new()
        }
    };
    let pr_ids: BTreeSet<String> = records
        .iter()
        .map(|r| r.pr_id.clone())
        .chain(
            candidates
                .iter()
                .filter_map(|c| c.labels.get("keel.io/pr-id").cloned()),
        )
        .chain(secret_pr_ids.iter().cloned())
        .collect();
    let mut pr_states: BTreeMap<String, PrLifecycle> = BTreeMap::new();
    for id in pr_ids {
        // Preview keys are repository-scoped (`<tenant>-<n>` for a tenant's own repository):
        // resolve each PR in its own repository; an unresolvable repository leaves the preview
        // untouched (never GC on a configuration error).
        let (forge, number) = match crate::proposal_repo::forge_for_preview_key(state, &id) {
            Ok(resolved) => resolved,
            Err(e) => {
                outcome.unknown_prs += 1;
                tracing::debug!(pr_id = %id, error = %e, "preview repository unresolvable; skipping preview");
                continue;
            }
        };
        match forge.get_pull_request(&number).await {
            Ok(pr) => {
                let lifecycle = match pr.state {
                    PullRequestState::Open => PrLifecycle::Open {
                        head_sha: pr.head_sha,
                    },
                    PullRequestState::Merged => PrLifecycle::Merged,
                    PullRequestState::Closed => PrLifecycle::Closed,
                };
                pr_states.insert(id, lifecycle);
            }
            Err(ForgeError::NotFound(_)) => {
                pr_states.insert(id, PrLifecycle::NotFound);
            }
            Err(e) => {
                outcome.unknown_prs += 1;
                tracing::debug!(pr_id = %id, error = %e, "forge PR lookup failed; skipping preview");
            }
        }
    }

    let plan = plan_preview_reconcile(&candidates, &records, &pr_states);
    outcome.deleted = plan.gc.targets.len();
    execute_gc(state, cluster.as_ref(), &plan.gc, "merged/closed").await;
    // Open PRs are never swept: their copies may belong to a launch in progress (copies are
    // written before the Stack exists).
    for pr_id in &secret_pr_ids {
        if matches!(pr_states.get(pr_id), Some(state) if !matches!(state, PrLifecycle::Open { .. }))
        {
            match cluster.delete_preview_secrets(pr_id).await {
                Ok(n) => outcome.deleted += n,
                Err(e) => {
                    tracing::warn!(pr_id = %pr_id, error = %e, "failed to garbage-collect preview Secret copies")
                }
            }
        }
    }

    for (pr_id, head_sha) in &plan.head_moved {
        let annotations = BTreeMap::from([(ANNOTATION_HEAD_SHA.to_string(), head_sha.clone())]);
        if cluster
            .annotate_preview_stack(&preview_stack_name(pr_id), &annotations)
            .await
            .is_ok()
        {
            outcome.head_updates += 1;
        }
    }
    for pr_id in &plan.publish {
        let stack_name = preview_stack_name(pr_id);
        if sync_preview(state, pr_id, &stack_name).await.is_some() {
            outcome.published += 1;
        }
    }
    outcome
}

pub(crate) fn jitter(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let max_extra = (base.as_millis() as u64 / 4).max(1);
    base + Duration::from_millis(nanos % max_extra)
}

/// Spawns the background preview reconciler (no-op without a preview runner).
pub fn spawn_preview_reconciler(
    state: AppState,
    interval: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    state.preview_cluster.as_ref()?;
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(jitter(interval)).await;
            let outcome = reconcile_previews_once(&state).await;
            if outcome != PreviewReconcileOutcome::default() {
                tracing::info!(?outcome, "preview reconciler pass");
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_names_and_jitter_are_deterministic_and_bounded() {
        assert_eq!(preview_stack_name("#12"), "preview-stack-pr-12");
        assert_eq!(preview_stack_name(" 7 "), "preview-stack-pr-7");
        assert_eq!(preview_stack_name("acme-7"), "preview-stack-pr-acme-7");
        assert!(preview_stack_name(&format!("{}-7", "t".repeat(50))).len() <= 50);
        assert_eq!(preview_pr_label("acme-7"), "PR #7 in tenant-acme");
        assert_eq!(preview_pr_label("7"), "PR #7 in platform");
        let j = jitter(Duration::from_secs(60));
        assert!(j >= Duration::from_secs(60) && j <= Duration::from_secs(75));
        assert_eq!(
            telemetry_status(PreviewPhase::Succeeded),
            CrdReconcileStatus::Ready
        );
        assert_eq!(
            telemetry_status(PreviewPhase::Running),
            CrdReconcileStatus::Previewing
        );
    }

    #[test]
    fn repinned_preview_stacks_keep_the_spec_and_drop_server_fields() {
        let live = serde_json::json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": {
                "name": "preview-stack-pr-acme-2",
                "namespace": "keel-previews",
                "uid": "u-1",
                "resourceVersion": "42",
                "managedFields": [{}],
                "finalizers": ["finalizer.stack.pulumi.com"],
                "labels": { "keel.io/pr-id": "acme-2" },
                "annotations": { "keel.io/head-sha": "old", "keel.io/preview-reported-commit": "old" },
            },
            "spec": {
                "preview": true,
                "projectRepo": "https://forge.local/acme/gitops.git",
                "commit": "old",
                "shallow": false,
                "repoDir": "tenants/acme/infra/vm2",
            },
            "status": { "lastUpdate": {} },
        });
        let doc = repinned_preview_stack(&live, "new").unwrap();
        assert_eq!(doc["spec"]["commit"], "new");
        assert_eq!(doc["spec"]["shallow"], false);
        assert_eq!(
            doc["metadata"]["annotations"]["keel.io/rebuilt-head"],
            "new"
        );
        assert_eq!(doc["spec"]["preview"], true);
        assert_eq!(doc["spec"]["repoDir"], "tenants/acme/infra/vm2");
        assert_eq!(doc["metadata"]["annotations"]["keel.io/head-sha"], "new");
        assert_eq!(
            doc["metadata"]["annotations"]["keel.io/preview-reported-commit"],
            "old"
        );
        assert_eq!(doc["metadata"]["labels"]["keel.io/pr-id"], "acme-2");
        for dropped in ["uid", "resourceVersion", "managedFields", "finalizers"] {
            assert!(doc["metadata"].get(dropped).is_none(), "{dropped}");
        }
        assert!(doc.get("status").is_none());

        // A branch-tracking preview (platform repository) is switched to the commit pin: the
        // spec changes, so the generation moves and PKO starts a new run.
        let mut branch = live.clone();
        branch["spec"] = serde_json::json!({
            "preview": true,
            "projectRepo": "https://forge.local/keel/gitops.git",
            "branch": "keel/acme/x",
            "shallow": true,
        });
        let doc = repinned_preview_stack(&branch, "new").unwrap();
        assert_eq!(doc["spec"]["commit"], "new");
        assert!(doc["spec"].get("branch").is_none());
        assert_eq!(doc["spec"]["shallow"], false);
        assert_ne!(doc["spec"], branch["spec"]);
        assert!(repinned_preview_stack(&serde_json::json!({}), "new").is_none());
        let mut no_source = live.clone();
        no_source["spec"] = serde_json::json!({ "preview": true });
        assert!(repinned_preview_stack(&no_source, "new").is_none());
    }

    /// A branch-tracking preview with an old `Succeeded` run, re-pinned to a rebuilt head: the
    /// old run no longer counts (new generation), and the merge of the rebuilt head waits until
    /// PKO previewed that commit.
    #[test]
    fn rebuilt_heads_wait_for_their_own_preview_run() {
        let live = serde_json::json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": {
                "name": "preview-stack-pr-platform-7",
                "namespace": "keel-previews",
                "generation": 1,
                "labels": { "keel.io/pr-id": "platform-7" },
                "annotations": {
                    "keel.io/head-sha": "old",
                    "keel.io/preview-reported-commit": "old",
                },
            },
            "spec": {
                "preview": true,
                "projectRepo": "https://forge.local/keel/gitops.git",
                "branch": "keel/acme/x",
            },
            "status": {
                "lastUpdate": {
                    "state": "succeeded",
                    "generation": 1,
                    "lastAttemptedCommit": "old",
                },
            },
        });
        let run = map_pko_preview_status(&live, None);
        assert_eq!(run.phase, PreviewPhase::Succeeded);
        assert!(
            !rebuilt_head_awaits_preview(&live, &run, "old"),
            "not rebuilt"
        );

        // The API server bumps the generation on the spec change and keeps the old status.
        let mut stack = repinned_preview_stack(&live, "new").unwrap();
        stack["metadata"]["generation"] = serde_json::json!(2);
        stack["status"] = live["status"].clone();
        let run = map_pko_preview_status(&stack, None);
        assert_eq!(run.phase, PreviewPhase::Pending, "{run:?}");
        assert!(rebuilt_head_awaits_preview(&stack, &run, "new"));
        assert!(rebuilt_head_awaits_preview(&stack, &run, "NEW"));
        // Another head (a later push) is not this gate's business.
        assert!(!rebuilt_head_awaits_preview(&stack, &run, "other"));

        // Running, then finished for the rebuilt commit.
        stack["status"] = serde_json::json!({ "currentUpdate": { "name": "u2", "commit": "new" } });
        let run = map_pko_preview_status(&stack, None);
        assert_eq!(run.phase, PreviewPhase::Running);
        assert!(rebuilt_head_awaits_preview(&stack, &run, "new"));
        stack["status"] = serde_json::json!({
            "lastUpdate": { "state": "failed", "generation": 2, "lastAttemptedCommit": "new" },
        });
        let run = map_pko_preview_status(&stack, None);
        assert_eq!(run.phase, PreviewPhase::Failed);
        assert!(!rebuilt_head_awaits_preview(&stack, &run, "new"));

        // Published results for the rebuilt commit always release the merge.
        stack["status"] = serde_json::json!({});
        stack["metadata"]["annotations"]["keel.io/preview-reported-commit"] =
            serde_json::json!("new");
        let run = map_pko_preview_status(&stack, None);
        assert!(!rebuilt_head_awaits_preview(&stack, &run, "new"));
    }
}
