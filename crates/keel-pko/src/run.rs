//! Real PKO v2 ephemeral preview execution model (`SDD.md` §4.2, `NFR-REL-02`).
//!
//! PKO v2 does **not** persist per-resource step data for `spec.preview: true` stacks: the
//! `pulumi.com/v1` `Stack` status only carries `conditions` (`Ready`/`Reconciling`/`Stalled`),
//! `currentUpdate` and `lastUpdate` (`state: succeeded|failed`, `lastAttemptedCommit`, `message`),
//! and each run is recorded as an `auto.pulumi.com/v1alpha1` `Update` (`spec.type: preview`,
//! conditions `Progressing`/`Failed`/`Complete`, `status.message`, `startTime`/`endTime`).
//! The only place the actual `pulumi preview` resource plan is emitted is the workspace pod's
//! `pulumi` container log (Pulumi diff display: `<type>: (<op>)` + `[urn=...]` + `Resources:`).
//!
//! This module provides the pure, unit-tested mappings used by the control plane:
//! - [`map_pko_preview_status`]: `Stack` (+ optional `Update`) JSON → [`PreviewRunStatus`],
//! - [`parse_pulumi_console_preview`]: workspace log text → [`PreviewReport`] (`source: pko`),
//! - [`plan_preview_reconcile`]: live preview records × forge PR states → GC/refresh plan.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gc::{GcPlan, PreviewResourceCandidate, plan_preview_gc};
use crate::preview::{PreviewReport, PreviewSource, PreviewSummary, parse_preview_value};
use crate::stack::{ANNOTATION_BRANCH, LABEL_PR_ID, LABEL_TENANT, sanitize_pr_id};

/// Annotation recording the PR head commit SHA the preview `Stack` was created/refreshed for.
pub const ANNOTATION_HEAD_SHA: &str = "keel.io/head-sha";
/// Annotation recording the PKO commit whose results were last published (PR comment + report).
pub const ANNOTATION_REPORTED_COMMIT: &str = "keel.io/preview-reported-commit";
/// `ConfigMap` data key holding the serialized [`PreviewReport`] JSON.
pub const PREVIEW_REPORT_CONFIGMAP_KEY: &str = "report.json";
/// Maximum workspace log lines scanned for the latest `pulumi preview` plan.
pub const PREVIEW_LOG_TAIL_LINES: i64 = 4000;

/// Lifecycle phase of an ephemeral PKO preview run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewPhase {
    /// Preview `Stack` applied; PKO has not started a preview `Update` yet.
    #[default]
    Pending,
    /// PKO preview `Update` in progress.
    Running,
    /// `pulumi preview` completed successfully.
    Succeeded,
    /// `pulumi preview` (or applying the preview `Stack`) failed.
    Failed,
    /// No PKO preview executed (no live cluster, or a Flux-only change); report is synthesized.
    NotExecuted,
}

impl PreviewPhase {
    /// Returns `true` once the run reached a final state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::NotExecuted)
    }

    /// Human-readable label for CLI/PR comment output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::NotExecuted => "Not executed",
        }
    }
}

/// Observed status of the ephemeral PKO preview run backing a proposal.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRunStatus {
    pub phase: PreviewPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// `<namespace>/<name>` of the preview `Stack` — only set when it was actually applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// Git commit SHA the PKO run evaluated (normalized from `<branch>@sha1:<sha>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Name of the PKO `auto.pulumi.com/v1alpha1` `Update` object for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
}

impl PreviewRunStatus {
    /// Status for proposals where no PKO preview was (or can be) executed.
    pub fn not_executed(reason: impl Into<String>) -> Self {
        Self {
            phase: PreviewPhase::NotExecuted,
            message: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Status for a preview `Stack` that failed to apply (e.g. admission denial).
    pub fn apply_failed(reason: impl Into<String>) -> Self {
        Self {
            phase: PreviewPhase::Failed,
            message: Some(reason.into()),
            ..Self::default()
        }
    }
}

/// Normalizes PKO commit references (`main@sha1:<sha>`, `sha1:<sha>`, `<sha>`) to the bare SHA.
pub fn normalize_pko_commit(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sha = trimmed
        .rsplit_once("sha1:")
        .map(|(_, s)| s)
        .or_else(|| trimmed.rsplit_once('@').map(|(_, s)| s))
        .unwrap_or(trimmed);
    (!sha.is_empty()).then(|| sha.to_string())
}

fn condition<'a>(obj: &'a Value, cond_type: &str) -> Option<&'a Value> {
    obj.pointer("/status/conditions")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(cond_type))
}

fn condition_is_true(obj: &Value, cond_type: &str) -> bool {
    condition(obj, cond_type)
        .and_then(|c| c.get("status"))
        .and_then(Value::as_str)
        == Some("True")
}

fn condition_message(obj: &Value, cond_type: &str) -> Option<String> {
    condition(obj, cond_type)
        .and_then(|c| c.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

fn str_at(obj: &Value, pointer: &str) -> Option<String> {
    obj.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Returns the PKO `Update` name that best describes the latest preview run of `stack`
/// (`status.currentUpdate.name` while running, else `status.lastUpdate.name`).
pub fn select_update_name(stack: &Value) -> Option<String> {
    str_at(stack, "/status/currentUpdate/name").or_else(|| str_at(stack, "/status/lastUpdate/name"))
}

/// Placeholder PKO writes into `Stack` `status.outputs` for outputs Pulumi marks secret.
pub const PKO_SECRET_OUTPUT_PLACEHOLDER: &str = "[secret]";

/// Returns the exported outputs of a live PKO v2 `Stack` from its `status.outputs`.
///
/// Only the Pulumi Kubernetes Operator writes `status.outputs`: on a successful update it copies
/// the update's `<update>-stack-outputs` Secret into the Stack status, masking secret outputs as
/// [`PKO_SECRET_OUTPUT_PLACEHOLDER`]. Tenant identities cannot write `stacks/status` (Kyverno
/// `restrict-tenant-pulumi-objects`), so unlike a Secret in the tenant namespace these values
/// cannot be forged by an app deployed into the tenant. String outputs are returned as-is, other
/// JSON values in compact JSON form; masked secret outputs and `null`s are skipped.
pub fn stack_status_outputs(stack: &Value) -> BTreeMap<String, String> {
    let Some(outputs) = stack.pointer("/status/outputs").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    outputs
        .iter()
        .filter_map(|(key, value)| {
            let text = match value {
                Value::Null => return None,
                Value::String(s) if s == PKO_SECRET_OUTPUT_PLACEHOLDER => return None,
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some((key.clone(), text))
        })
        .collect()
}

/// Maps a live PKO `Stack` object (and optionally its latest `Update`) to a [`PreviewRunStatus`].
///
/// Precedence: an `Update` with terminal conditions is authoritative; otherwise `Stack`
/// `currentUpdate` ⇒ Running, `lastUpdate.state` for the current generation ⇒ terminal,
/// `Stalled=True` ⇒ Failed, anything else ⇒ Pending.
pub fn map_pko_preview_status(stack: &Value, update: Option<&Value>) -> PreviewRunStatus {
    let stack_ref = match (
        str_at(stack, "/metadata/namespace"),
        str_at(stack, "/metadata/name"),
    ) {
        (Some(ns), Some(name)) => Some(format!("{ns}/{name}")),
        _ => None,
    };
    let generation = stack
        .pointer("/metadata/generation")
        .and_then(Value::as_i64);
    let commit = str_at(stack, "/status/currentUpdate/commit")
        .or_else(|| str_at(stack, "/status/lastUpdate/lastAttemptedCommit"))
        .and_then(|c| normalize_pko_commit(&c));
    let mut status = PreviewRunStatus {
        phase: PreviewPhase::Pending,
        message: None,
        stack: stack_ref,
        commit,
        update: select_update_name(stack),
    };

    if let Some(upd) = update {
        let msg = condition_message(upd, "Failed")
            .or_else(|| str_at(upd, "/status/message"))
            .map(|m| m.trim_matches('"').to_string())
            .filter(|m| !m.is_empty());
        if condition_is_true(upd, "Failed") {
            status.phase = PreviewPhase::Failed;
            status.message = msg.or_else(|| Some("PKO preview update failed".to_string()));
            return status;
        }
        if condition_is_true(upd, "Complete") {
            status.phase = PreviewPhase::Succeeded;
            status.message = Some("PKO preview completed".to_string());
            return status;
        }
        if condition_is_true(upd, "Progressing") {
            status.phase = PreviewPhase::Running;
            status.message = Some("PKO preview in progress".to_string());
            return status;
        }
    }

    if stack
        .pointer("/status/currentUpdate")
        .is_some_and(|v| !v.is_null())
    {
        status.phase = PreviewPhase::Running;
        status.message = Some("PKO preview in progress".to_string());
        return status;
    }

    let last_gen = stack
        .pointer("/status/lastUpdate/generation")
        .and_then(Value::as_i64);
    let last_state = str_at(stack, "/status/lastUpdate/state");
    let current_generation = match (generation, last_gen) {
        (Some(g), Some(lg)) => lg >= g,
        (None, _) => true,
        (Some(_), None) => false,
    };
    if current_generation && let Some(state) = last_state.as_deref() {
        match state {
            "succeeded" => {
                status.phase = PreviewPhase::Succeeded;
                status.message = Some("PKO preview completed".to_string());
                return status;
            }
            "failed" => {
                status.phase = PreviewPhase::Failed;
                status.message = str_at(stack, "/status/lastUpdate/message")
                    .map(|m| m.trim_matches('"').to_string())
                    .or_else(|| condition_message(stack, "Ready"))
                    .or_else(|| Some("PKO preview failed".to_string()));
                return status;
            }
            _ => {}
        }
    }

    if condition_is_true(stack, "Stalled") {
        status.phase = PreviewPhase::Failed;
        status.message =
            condition_message(stack, "Stalled").or_else(|| Some("PKO stack stalled".to_string()));
        return status;
    }
    if condition_is_true(stack, "Reconciling") {
        status.phase = PreviewPhase::Running;
        status.message = condition_message(stack, "Reconciling")
            .or_else(|| Some("PKO preview reconciling".to_string()));
        return status;
    }
    status.message = condition_message(stack, "Ready")
        .or_else(|| Some("Waiting for PKO to start the preview".to_string()));
    status
}

/// Strips the PKO agent's structured log prefix (`<ts>\tINFO\tpulumi\t<message>`).
fn console_message(line: &str) -> Option<&str> {
    if let Some((_, msg)) = line.split_once("\tpulumi\t") {
        return Some(msg);
    }
    // Non-pulumi agent logs (`server`, `cmd.serve.grpc`) are tab-separated as well: skip them.
    if line.split('\t').count() >= 3 {
        return None;
    }
    Some(line)
}

fn parse_resource_header(content: &str) -> Option<(String, String)> {
    let trimmed = content.trim();
    let body = trimmed.strip_suffix(')')?;
    let (lhs, op) = body.rsplit_once(": (")?;
    let op = op.trim();
    if op.is_empty() || !op.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
        return None;
    }
    let ty = lhs
        .trim_start_matches(['+', '-', '~', '=', '>', '<', ' '])
        .trim();
    if ty.is_empty() || ty.contains(' ') || !ty.contains(':') {
        return None;
    }
    Some((ty.to_string(), op.to_string()))
}

fn parse_resources_summary(lines: &[&str]) -> Option<PreviewSummary> {
    let start = lines.iter().rposition(|l| l.trim() == "Resources:")?;
    let mut summary = PreviewSummary::default();
    let mut any = false;
    for line in &lines[start + 1..] {
        let t = line.trim();
        if t.is_empty() {
            if any {
                break;
            }
            continue;
        }
        let digits_start = t.find(|c: char| c.is_ascii_digit());
        let Some(ds) = digits_start else { break };
        let rest = &t[ds..];
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let Ok(n) = rest[..num_end].parse::<usize>() else {
            break;
        };
        let label = rest[num_end..].trim();
        match label {
            l if l.starts_with("to create") => summary.create += n,
            l if l.starts_with("to update") => summary.update += n,
            l if l.starts_with("to delete") => summary.delete += n,
            l if l.starts_with("to replace") => summary.replace += n,
            l if l.starts_with("unchanged") => summary.same += n,
            l if l.starts_with("changes") || l.starts_with("change") => {}
            _ => break,
        }
        any = true;
    }
    any.then_some(summary)
}

/// Resource entry accumulated while scanning Pulumi diff display output.
struct ConsoleStep {
    ty: String,
    op: String,
    urn: Option<String>,
    fields: Vec<String>,
    min_indent: Option<usize>,
}

/// Parses the Pulumi diff display emitted by a PKO workspace pod (`pulumi` container log) for the
/// **latest** preview run into a [`PreviewReport`] with `source: pko`.
///
/// Returns `None` when the log contains no recognizable preview output (e.g. logs rotated, or the
/// run produced no plan) so callers never fabricate a PKO report.
pub fn parse_pulumi_console_preview(log: &str) -> Option<PreviewReport> {
    let messages: Vec<&str> = log.lines().filter_map(console_message).collect();
    let start = messages
        .iter()
        .rposition(|m| {
            let t = m.trim_start();
            t.starts_with("Previewing update") || t.starts_with("Previewing (")
        })
        .unwrap_or(0);
    let section = &messages[start..];

    let mut steps: Vec<Value> = Vec::new();
    let mut current: Option<ConsoleStep> = None;
    let flush = |cur: Option<ConsoleStep>, steps: &mut Vec<Value>| {
        if let Some(step) = cur {
            let ty = step.ty;
            let urn = step
                .urn
                .unwrap_or_else(|| format!("urn:pulumi:preview::unknown::{ty}::{ty}"));
            let fields: Vec<String> = if matches!(step.op.as_str(), "update" | "replace") {
                step.fields
            } else {
                Vec::new()
            };
            steps.push(json!({ "op": step.op, "urn": urn, "type": ty, "changedFields": fields }));
        }
    };

    for msg in section {
        if msg.trim() == "Resources:" {
            break;
        }
        if let Some((ty, op)) = parse_resource_header(msg) {
            flush(current.take(), &mut steps);
            current = Some(ConsoleStep {
                ty,
                op,
                urn: None,
                fields: Vec::new(),
                min_indent: None,
            });
            continue;
        }
        let Some(cur) = current.as_mut() else {
            continue;
        };
        let t = msg.trim();
        if let Some(urn) = t.strip_prefix("[urn=").and_then(|u| u.strip_suffix(']')) {
            cur.urn = Some(urn.to_string());
            continue;
        }
        if t.starts_with('[') {
            continue;
        }
        let indent = msg.len() - msg.trim_start().len();
        let prop = t
            .strip_prefix("~ ")
            .or_else(|| t.strip_prefix("+ "))
            .or_else(|| t.strip_prefix("- "))
            .or_else(|| t.strip_prefix("+-"));
        if let Some(prop) = prop
            && let Some((name, _)) = prop.split_once(':')
        {
            let name = name.trim().trim_matches('"');
            if name.is_empty() || name.contains(' ') {
                continue;
            }
            match cur.min_indent {
                Some(min) if indent > min => continue,
                Some(min) if indent < min => {
                    cur.min_indent = Some(indent);
                    cur.fields.clear();
                }
                None => cur.min_indent = Some(indent),
                _ => {}
            }
            if !cur.fields.iter().any(|f| f == name) {
                cur.fields.push(name.to_string());
            }
        }
    }
    flush(current.take(), &mut steps);

    let summary_block = parse_resources_summary(section);
    if steps.is_empty() && summary_block.is_none() {
        return None;
    }

    // Unknown display ops (e.g. `read-replacement`) are dropped instead of failing the report.
    steps.retain(|s| {
        parse_preview_value(&json!({ "steps": [s] })).is_ok()
            || s.get("op").and_then(Value::as_str) == Some("refresh")
    });
    let mut report = parse_preview_value(&json!({ "steps": steps })).ok()?;
    if let Some(block) = summary_block {
        report.summary = block;
    }
    report.source = PreviewSource::Pko;
    Some(report)
}

/// Builds the final report for a terminal PKO preview run.
///
/// - `Succeeded` + parsed log plan ⇒ that plan (`source: pko`).
/// - `Succeeded` without readable plan ⇒ empty `pko` report whose status message says so.
/// - `Failed` ⇒ empty `pko` report carrying the PKO failure message.
pub fn build_pko_preview_report(
    status: &PreviewRunStatus,
    workspace_log: Option<&str>,
) -> PreviewReport {
    let mut run = status.clone();
    let parsed = workspace_log.and_then(parse_pulumi_console_preview);
    let mut report = match (status.phase, parsed) {
        (PreviewPhase::Succeeded, Some(r)) => r,
        (PreviewPhase::Succeeded, None) => {
            run.message = Some(
                "PKO preview succeeded, but the per-resource plan could not be read from the workspace logs"
                    .to_string(),
            );
            PreviewReport::default()
        }
        _ => PreviewReport::default(),
    };
    report.source = PreviewSource::Pko;
    report.run = Some(run);
    report
}

/// Identifiers persisted on a live preview `Stack` (labels/annotations), read back after restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewRecord {
    pub pr_id: String,
    pub stack_name: String,
    pub tenant: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub reported_commit: Option<String>,
    pub run: PreviewRunStatus,
}

impl PreviewRecord {
    /// Extracts a record from a live `keel-previews` preview `Stack` object.
    pub fn from_stack_value(stack: &Value) -> Option<Self> {
        let name = str_at(stack, "/metadata/name")?;
        let label = |k: &str| {
            stack
                .pointer("/metadata/labels")
                .and_then(|l| l.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let annotation = |k: &str| {
            stack
                .pointer("/metadata/annotations")
                .and_then(|l| l.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let pr_id = label(LABEL_PR_ID)
            .or_else(|| name.strip_prefix("preview-stack-pr-").map(str::to_string))
            .and_then(|id| sanitize_pr_id(&id).ok())?;
        Some(Self {
            pr_id,
            stack_name: name,
            tenant: label(LABEL_TENANT),
            branch: annotation(ANNOTATION_BRANCH),
            head_sha: annotation(ANNOTATION_HEAD_SHA),
            reported_commit: annotation(ANNOTATION_REPORTED_COMMIT),
            run: map_pko_preview_status(stack, None),
        })
    }

    /// Returns `true` once the PKO run for the current commit finished but its results were not
    /// yet published (fresh run, server restart mid-run, or a new commit pushed to the PR branch).
    pub fn needs_publish(&self) -> bool {
        self.run.phase.is_terminal()
            && self.run.phase != PreviewPhase::NotExecuted
            && match (&self.run.commit, &self.reported_commit) {
                (Some(c), Some(r)) => c != r,
                (None, Some(_)) => false,
                (_, None) => true,
            }
    }
}

/// Forge-reported lifecycle of the PR backing a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycle {
    Open {
        head_sha: String,
    },
    Closed,
    Merged,
    /// The forge reported 404 for this PR id.
    NotFound,
}

/// Actions the background preview reconciler must perform.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreviewReconcilePlan {
    /// Preview resources of merged/closed/missing PRs.
    pub gc: GcPlan,
    /// Open PRs whose finished PKO run must be collected and published.
    pub publish: Vec<String>,
    /// Open PRs whose head SHA moved since the preview was recorded: `(pr_id, new_head_sha)`.
    pub head_moved: Vec<(String, String)>,
}

/// Plans preview GC / re-publication from persisted cluster records and forge PR states.
///
/// PRs whose state could not be determined (absent from `pr_states`, e.g. transient forge
/// errors) are left untouched so a forge outage can never wipe live previews.
pub fn plan_preview_reconcile(
    candidates: &[PreviewResourceCandidate],
    records: &[PreviewRecord],
    pr_states: &BTreeMap<String, PrLifecycle>,
) -> PreviewReconcilePlan {
    let finished: Vec<&str> = pr_states
        .iter()
        .filter(|(_, s)| !matches!(s, PrLifecycle::Open { .. }))
        .map(|(id, _)| id.as_str())
        .collect();
    let mut plan = PreviewReconcilePlan {
        gc: plan_preview_gc(candidates, &finished),
        ..PreviewReconcilePlan::default()
    };
    for record in records {
        let Some(PrLifecycle::Open { head_sha }) = pr_states.get(&record.pr_id) else {
            continue;
        };
        if !head_sha.is_empty() && record.head_sha.as_deref() != Some(head_sha.as_str()) {
            plan.head_moved
                .push((record.pr_id.clone(), head_sha.clone()));
        }
        if record.needs_publish() {
            plan.publish.push(record.pr_id.clone());
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::PreviewResourceKind;
    use crate::preview::ChangeOp;

    #[test]
    fn stack_status_outputs_reads_operator_written_status_only() {
        let stack = json!({
            "spec": { "config": { "sshHost": "10.9.9.9" } },
            "status": {
                "outputs": {
                    "sshHost": "203.0.113.10",
                    "sshPort": 2222,
                    "ready": true,
                    "tags": { "env": "prod" },
                    "adminPassword": "[secret]",
                    "nothing": null
                }
            }
        });
        let outputs = stack_status_outputs(&stack);
        assert_eq!(
            outputs.get("sshHost").map(String::as_str),
            Some("203.0.113.10")
        );
        assert_eq!(outputs.get("sshPort").map(String::as_str), Some("2222"));
        assert_eq!(outputs.get("ready").map(String::as_str), Some("true"));
        assert_eq!(
            outputs.get("tags").map(String::as_str),
            Some(r#"{"env":"prod"}"#)
        );
        // Masked secret outputs and nulls are not usable outputs.
        assert!(!outputs.contains_key("adminPassword"));
        assert!(!outputs.contains_key("nothing"));
        assert_eq!(outputs.len(), 4);

        assert!(stack_status_outputs(&json!({ "status": {} })).is_empty());
        assert!(stack_status_outputs(&json!({ "status": { "outputs": "x" } })).is_empty());
        assert!(stack_status_outputs(&json!({})).is_empty());
    }

    fn preview_stack(generation: i64, status: Value) -> Value {
        json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": {
                "name": "preview-stack-pr-7",
                "namespace": "keel-previews",
                "generation": generation,
                "labels": { "keel.io/pr-id": "7", "keel.io/tenant": "acme", "keel.io/ephemeral-preview": "true" },
                "annotations": { "keel.io/branch": "keel/acme/deploy-1", "keel.io/head-sha": "abc" }
            },
            "spec": { "preview": true },
            "status": status
        })
    }

    #[test]
    fn maps_stack_and_update_status_to_phases() {
        let pending = preview_stack(1, json!({}));
        let s = map_pko_preview_status(&pending, None);
        assert_eq!(s.phase, PreviewPhase::Pending);
        assert_eq!(s.stack.as_deref(), Some("keel-previews/preview-stack-pr-7"));

        let running = preview_stack(
            1,
            json!({ "currentUpdate": { "name": "u1", "commit": "keel/acme/deploy-1@sha1:deadbeef" } }),
        );
        let s = map_pko_preview_status(&running, None);
        assert_eq!(s.phase, PreviewPhase::Running);
        assert_eq!(s.commit.as_deref(), Some("deadbeef"));
        assert_eq!(s.update.as_deref(), Some("u1"));

        let ok = preview_stack(
            2,
            json!({ "lastUpdate": { "name": "u2", "generation": 2, "state": "succeeded", "lastAttemptedCommit": "b@sha1:cafe" } }),
        );
        let s = map_pko_preview_status(&ok, None);
        assert_eq!(s.phase, PreviewPhase::Succeeded);
        assert_eq!(s.commit.as_deref(), Some("cafe"));

        // A terminal lastUpdate for an older generation is not the current run.
        let stale = preview_stack(
            3,
            json!({ "lastUpdate": { "generation": 2, "state": "succeeded" } }),
        );
        assert_eq!(
            map_pko_preview_status(&stale, None).phase,
            PreviewPhase::Pending
        );

        let failed = preview_stack(
            1,
            json!({ "lastUpdate": { "generation": 1, "state": "failed", "message": "\"error: provider denied\"" } }),
        );
        let s = map_pko_preview_status(&failed, None);
        assert_eq!(s.phase, PreviewPhase::Failed);
        assert_eq!(s.message.as_deref(), Some("error: provider denied"));

        let stalled = preview_stack(
            1,
            json!({ "conditions": [{ "type": "Stalled", "status": "True", "message": "SourceUnavailable", "reason": "x", "lastTransitionTime": "t" }] }),
        );
        let s = map_pko_preview_status(&stalled, None);
        assert_eq!(s.phase, PreviewPhase::Failed);
        assert_eq!(s.message.as_deref(), Some("SourceUnavailable"));

        let reconciling = preview_stack(
            1,
            json!({ "conditions": [{ "type": "Reconciling", "status": "True", "message": "", "reason": "x", "lastTransitionTime": "t" }] }),
        );
        assert_eq!(
            map_pko_preview_status(&reconciling, None).phase,
            PreviewPhase::Running
        );

        // `Update` conditions are authoritative.
        let upd_failed = json!({ "status": { "conditions": [
            { "type": "Failed", "status": "True", "message": "preview failed: 1 error", "reason": "UpdateFailed", "lastTransitionTime": "t" }
        ] } });
        let s = map_pko_preview_status(&running, Some(&upd_failed));
        assert_eq!(s.phase, PreviewPhase::Failed);
        assert_eq!(s.message.as_deref(), Some("preview failed: 1 error"));
        let upd_complete = json!({ "status": { "conditions": [
            { "type": "Complete", "status": "True", "message": "", "reason": "Updated", "lastTransitionTime": "t" },
            { "type": "Failed", "status": "False", "message": "", "reason": "UpdateSucceeded", "lastTransitionTime": "t" }
        ] } });
        assert_eq!(
            map_pko_preview_status(&pending, Some(&upd_complete)).phase,
            PreviewPhase::Succeeded
        );
        let upd_prog = json!({ "status": { "conditions": [
            { "type": "Progressing", "status": "True", "message": "", "reason": "x", "lastTransitionTime": "t" }
        ] } });
        assert_eq!(
            map_pko_preview_status(&pending, Some(&upd_prog)).phase,
            PreviewPhase::Running
        );
    }

    #[test]
    fn normalizes_pko_commit_references() {
        assert_eq!(
            normalize_pko_commit("main@sha1:abc").as_deref(),
            Some("abc")
        );
        assert_eq!(normalize_pko_commit("sha1:abc").as_deref(), Some("abc"));
        assert_eq!(normalize_pko_commit("abc").as_deref(), Some("abc"));
        assert_eq!(normalize_pko_commit("  "), None);
    }

    const PKO_LOG: &str = "2026-11-21T02:09:51.883Z\tINFO\tserver\tselected a stack\t{\"name\": \"x\"}\n\
2026-11-21T02:09:52.515Z\tINFO\tpulumi\tPreviewing update (old):\n\
2026-11-21T02:09:52.516Z\tINFO\tpulumi\t  + kubernetes:core/v1:Secret: (create)\n\
2026-11-21T02:09:52.517Z\tINFO\tpulumi\tResources:\n\
2026-11-21T02:09:52.517Z\tINFO\tpulumi\t    + 1 to create\n\
2026-11-21T02:10:52.515Z\tINFO\tpulumi\tPreviewing update (default-web):\n\
2026-11-21T02:10:52.564Z\tINFO\tpulumi\t  pulumi:pulumi:Stack: (same)\n\
2026-11-21T02:10:52.564Z\tINFO\tpulumi\t    [urn=urn:pulumi:default-web::web::pulumi:pulumi:Stack::web-default-web]\n\
2026-11-21T02:10:52.570Z\tINFO\tpulumi\t    + kubernetes:core/v1:ConfigMap: (create)\n\
2026-11-21T02:10:52.570Z\tINFO\tpulumi\t        [urn=urn:pulumi:default-web::web::kubernetes:core/v1:ConfigMap::cfg]\n\
2026-11-21T02:10:52.571Z\tINFO\tpulumi\t        + data: {\n\
2026-11-21T02:10:52.571Z\tINFO\tpulumi\t            + key: \"v\"\n\
2026-11-21T02:10:52.572Z\tINFO\tpulumi\t    ~ kubernetes:apps/v1:Deployment: (update)\n\
2026-11-21T02:10:52.572Z\tINFO\tpulumi\t        [id=tenant-acme/web]\n\
2026-11-21T02:10:52.572Z\tINFO\tpulumi\t        [urn=urn:pulumi:default-web::web::kubernetes:apps/v1:Deployment::web]\n\
2026-11-21T02:10:52.573Z\tINFO\tpulumi\t      ~ spec: {\n\
2026-11-21T02:10:52.573Z\tINFO\tpulumi\t          ~ replicas: 1 => 3\n\
2026-11-21T02:10:52.573Z\tINFO\tpulumi\t        }\n\
2026-11-21T02:10:52.574Z\tINFO\tpulumi\t    - kubernetes:core/v1:Service: (delete)\n\
2026-11-21T02:10:52.574Z\tINFO\tpulumi\t        [urn=urn:pulumi:default-web::web::kubernetes:core/v1:Service::old-svc]\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\tResources:\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\t    + 1 to create\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\t    ~ 1 to update\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\t    - 1 to delete\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\t    3 changes. 4 unchanged\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\t\n\
2026-11-21T02:10:52.806Z\tINFO\tpulumi\tDuration: 1s\n";

    #[test]
    fn parses_latest_preview_from_pko_workspace_log() {
        let report = parse_pulumi_console_preview(PKO_LOG).expect("plan parsed");
        assert_eq!(report.source, PreviewSource::Pko);
        let ops: Vec<(ChangeOp, &str)> = report
            .resources
            .iter()
            .map(|r| (r.op, r.name.as_str()))
            .collect();
        assert_eq!(
            ops,
            vec![
                (ChangeOp::Same, "web-default-web"),
                (ChangeOp::Create, "cfg"),
                (ChangeOp::Update, "web"),
                (ChangeOp::Delete, "old-svc"),
            ]
        );
        let deploy = &report.resources[2];
        assert_eq!(deploy.resource_type, "kubernetes:apps/v1:Deployment");
        assert_eq!(deploy.changed_fields, vec!["spec".to_string()]);
        assert!(report.resources[1].changed_fields.is_empty());
        assert_eq!(report.summary.create, 1);
        assert_eq!(report.summary.update, 1);
        assert_eq!(report.summary.delete, 1);
        // "3 changes. 4 unchanged" is not a "N unchanged" line: same stays from the block (0).
        assert_eq!(report.summary.same, 0);
    }

    #[test]
    fn parses_unchanged_summary_and_rejects_unrelated_logs() {
        let log = "Previewing update (s):\n  pulumi:pulumi:Stack: (same)\n    [urn=urn:pulumi:s::p::pulumi:pulumi:Stack::p-s]\nResources:\n    7 unchanged\n\nDuration: 1s\n";
        let report = parse_pulumi_console_preview(log).unwrap();
        assert_eq!(report.summary.same, 7);
        assert_eq!(report.summary.total(), 7);

        assert!(parse_pulumi_console_preview("2026\tINFO\tserver\tup completed\n").is_none());
        assert!(parse_pulumi_console_preview("").is_none());
        // Unknown display ops are dropped rather than failing the whole plan.
        let odd = "Previewing update (s):\n  + aws:s3:Bucket: (create)\n    [urn=urn:pulumi:s::p::aws:s3:Bucket::b]\n  ~ aws:s3:Thing: (weird-op)\n";
        let r = parse_pulumi_console_preview(odd).unwrap();
        assert_eq!(r.resources.len(), 1);
    }

    #[test]
    fn builds_terminal_reports_honestly() {
        let ok = PreviewRunStatus {
            phase: PreviewPhase::Succeeded,
            ..PreviewRunStatus::default()
        };
        let r = build_pko_preview_report(&ok, Some(PKO_LOG));
        assert_eq!(r.source, PreviewSource::Pko);
        assert_eq!(r.resources.len(), 4);
        assert_eq!(r.run.as_ref().unwrap().phase, PreviewPhase::Succeeded);

        let r = build_pko_preview_report(&ok, None);
        assert!(r.resources.is_empty());
        assert!(
            r.run
                .as_ref()
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .contains("could not be read")
        );

        let failed = PreviewRunStatus {
            phase: PreviewPhase::Failed,
            message: Some("boom".into()),
            ..PreviewRunStatus::default()
        };
        let r = build_pko_preview_report(&failed, Some(PKO_LOG));
        assert!(r.resources.is_empty());
        assert_eq!(r.run.unwrap().message.as_deref(), Some("boom"));
    }

    fn record(pr: &str, head: &str, reported: Option<&str>, phase: PreviewPhase) -> PreviewRecord {
        PreviewRecord {
            pr_id: pr.to_string(),
            stack_name: format!("preview-stack-pr-{pr}"),
            tenant: Some("acme".into()),
            branch: None,
            head_sha: Some(head.to_string()),
            reported_commit: reported.map(str::to_string),
            run: PreviewRunStatus {
                phase,
                commit: Some(head.to_string()),
                ..PreviewRunStatus::default()
            },
        }
    }

    fn candidates(prs: &[&str]) -> Vec<PreviewResourceCandidate> {
        prs.iter()
            .flat_map(|pr| {
                [(PreviewResourceKind::Stack, format!("preview-stack-pr-{pr}"))]
                    .into_iter()
                    .map(|(kind, name)| PreviewResourceCandidate {
                        kind,
                        name,
                        namespace: "keel-previews".into(),
                        labels: BTreeMap::from([
                            ("keel.io/pr-id".to_string(), pr.to_string()),
                            ("keel.io/ephemeral-preview".to_string(), "true".to_string()),
                        ]),
                    })
            })
            .collect()
    }

    #[test]
    fn reconcile_plan_gcs_finished_prs_and_republishes_open_ones() {
        let cands = candidates(&["1", "2", "3", "4", "5"]);
        let records = vec![
            record("1", "a1", None, PreviewPhase::Succeeded),
            record("2", "b1", Some("b1"), PreviewPhase::Succeeded),
            record("3", "c1", None, PreviewPhase::Running),
            record("4", "d1", None, PreviewPhase::Failed),
            record("5", "e1", None, PreviewPhase::Succeeded),
        ];
        let states = BTreeMap::from([
            (
                "1".to_string(),
                PrLifecycle::Open {
                    head_sha: "a1".into(),
                },
            ),
            (
                "2".to_string(),
                PrLifecycle::Open {
                    head_sha: "b2".into(),
                },
            ),
            ("3".to_string(), PrLifecycle::Merged),
            ("4".to_string(), PrLifecycle::Closed),
            // PR 5 unknown (forge error) => untouched.
        ]);
        let plan = plan_preview_reconcile(&cands, &records, &states);
        let deleted: Vec<&str> = plan.gc.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(deleted, vec!["preview-stack-pr-3", "preview-stack-pr-4"]);
        assert_eq!(plan.publish, vec!["1".to_string()]);
        assert_eq!(plan.head_moved, vec![("2".to_string(), "b2".to_string())]);

        let gone = BTreeMap::from([("1".to_string(), PrLifecycle::NotFound)]);
        let plan = plan_preview_reconcile(&cands, &records, &gone);
        assert_eq!(plan.gc.targets.len(), 1);
        assert!(plan.publish.is_empty());
    }

    #[test]
    fn preview_record_round_trips_from_stack_metadata() {
        let stack = preview_stack(
            1,
            json!({ "lastUpdate": { "generation": 1, "state": "succeeded", "lastAttemptedCommit": "x@sha1:abc" } }),
        );
        let rec = PreviewRecord::from_stack_value(&stack).unwrap();
        assert_eq!(rec.pr_id, "7");
        assert_eq!(rec.tenant.as_deref(), Some("acme"));
        assert_eq!(rec.head_sha.as_deref(), Some("abc"));
        assert_eq!(rec.branch.as_deref(), Some("keel/acme/deploy-1"));
        assert!(rec.needs_publish());
        let mut published = rec.clone();
        published.reported_commit = Some("abc".into());
        assert!(!published.needs_publish());
        assert!(PreviewRecord::from_stack_value(&json!({})).is_none());
    }
}
