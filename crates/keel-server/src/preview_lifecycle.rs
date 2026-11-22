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

fn jitter(base: Duration) -> Duration {
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
}
