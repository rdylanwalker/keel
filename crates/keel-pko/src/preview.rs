//! Pulumi `preview --json` and PKO v2 preview step diff parser and Markdown report formatter.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::PkoError;

/// Resource-level operation reported by `pulumi preview --json` or PKO v2 preview execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOp {
    Create,
    Update,
    Delete,
    Replace,
    Same,
    Drift,
}

impl ChangeOp {
    /// Returns the human-readable badge rendered in Markdown tables and CLI summaries.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Create => "+ Create",
            Self::Update => "~ Update",
            Self::Delete => "- Delete",
            Self::Replace => "! Replace",
            Self::Same => "= Same",
            Self::Drift => "! Drift",
        }
    }

    /// Returns `true` if this operation mutates or flags divergence from desired state.
    pub fn is_actionable(self) -> bool {
        !matches!(self, Self::Same)
    }
}

/// Parsed diff for an individual Pulumi resource.
///
/// Serialized camelCase on the wire (`resourceType`, `changedFields`) like the rest of the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDiff {
    pub urn: String,
    pub resource_type: String,
    pub name: String,
    pub op: ChangeOp,
    #[serde(default)]
    pub changed_fields: Vec<String>,
}

/// Aggregated counts across all `ChangeOp` variants in a `PreviewReport`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewSummary {
    pub create: usize,
    pub update: usize,
    pub delete: usize,
    pub replace: usize,
    pub same: usize,
    pub drift: usize,
}

impl PreviewSummary {
    /// Total number of evaluated resources.
    pub fn total(&self) -> usize {
        self.create + self.update + self.delete + self.replace + self.same + self.drift
    }

    /// Returns `true` if any resource will be deleted or replaced.
    pub fn has_destructive_changes(&self) -> bool {
        (self.delete + self.replace) > 0
    }

    fn increment(&mut self, op: ChangeOp) {
        match op {
            ChangeOp::Create => self.create += 1,
            ChangeOp::Update => self.update += 1,
            ChangeOp::Delete => self.delete += 1,
            ChangeOp::Replace => self.replace += 1,
            ChangeOp::Same => self.same += 1,
            ChangeOp::Drift => self.drift += 1,
        }
    }
}

/// Origin of a [`PreviewReport`]: an actual PKO/Pulumi preview run, or a control-plane
/// simulation synthesized from the proposed manifests/Git diff (no `pulumi preview` executed).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewSource {
    Pko,
    /// Default so reports of unknown provenance are never presented as real PKO output.
    #[default]
    Synthesized,
}

/// Structured Pulumi preview report consumed by Forge PR comments, CLI, and React GUI
/// (camelCase on the wire).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewReport {
    pub resources: Vec<ResourceDiff>,
    pub summary: PreviewSummary,
    #[serde(default)]
    pub source: PreviewSource,
    /// Status of the ephemeral PKO preview run (absent for offline reports).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<crate::run::PreviewRunStatus>,
}

/// Parses an already-deserialized `serde_json::Value` representing `pulumi preview --json`
/// or PKO v2 preview status output into a `PreviewReport`.
pub fn parse_preview_value(value: &Value) -> Result<PreviewReport, PkoError> {
    if let Some(arr) = value.as_array() {
        return build_report_from_steps(arr, &[]);
    }

    let Some(obj) = value.as_object() else {
        return Err(PkoError::PreviewParse(
            "expected JSON object or array for preview payload".to_string(),
        ));
    };

    // Support single Pulumi streaming event envelopes directly (`resourcePreEvent`, `summaryEvent`, etc.).
    if let Some(meta) = obj
        .get("resourcePreEvent")
        .and_then(|ev| ev.get("metadata"))
    {
        return build_report_from_steps(std::slice::from_ref(meta), &[]);
    }
    if obj.contains_key("summaryEvent")
        || obj.contains_key("preludeEvent")
        || obj.contains_key("diagnosticEvent")
    {
        return Ok(PreviewReport::default());
    }
    if obj.contains_key("op") && obj.contains_key("urn") {
        return build_report_from_steps(std::slice::from_ref(value), &[]);
    }

    // Unwrap PKO Stack CR or nested status object if present (`status.preview` or `preview`).
    if let Some(nested) = obj
        .get("status")
        .and_then(|s| s.get("preview"))
        .or_else(|| obj.get("preview"))
        && (nested.is_object() || nested.is_array())
    {
        return parse_preview_value(nested);
    }

    let steps = obj
        .get("steps")
        .or_else(|| obj.get("resources"))
        .and_then(Value::as_array)
        .map(Vec::as_slice);

    let drift_steps = obj
        .get("drift")
        .or_else(|| obj.get("driftChanges"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    match steps {
        Some(step_slice) => build_report_from_steps(step_slice, drift_steps),
        None if !drift_steps.is_empty() => build_report_from_steps(&[], drift_steps),
        None if obj.contains_key("changeSummary") => Ok(PreviewReport::default()),
        None => Err(PkoError::PreviewParse(
            "preview JSON object does not contain 'steps', 'resources', or 'changeSummary'"
                .to_string(),
        )),
    }
}

/// Formats a `PreviewReport` into a clean Markdown report suitable for Forge PR comments
/// (`ForgeProvider::post_preview_comment`) and terminal output (`keel preview status`).
pub fn format_markdown_report(report: &PreviewReport, pr_id: &str) -> String {
    let display_pr = format_pr_label(pr_id);
    let s = &report.summary;

    let mut out = String::new();
    out.push_str(&format!(
        "### Keel Infrastructure Preview — {display_pr}\n\n"
    ));
    if let Some(run) = &report.run {
        out.push_str(&format!(
            "**PKO preview:** {}",
            sanitize_md_inline(run.phase.label())
        ));
        if let Some(stack) = &run.stack {
            out.push_str(&format!(" (`{}`)", sanitize_md_cell(stack)));
        }
        if let Some(commit) = &run.commit {
            out.push_str(&format!(" @ `{}`", sanitize_md_cell(commit)));
        }
        if let Some(msg) = &run.message {
            out.push_str(&format!(" — {}", sanitize_md_inline(msg)));
        }
        out.push_str("\n\n");
    }

    out.push_str(&format!(
        "**Summary:** `+ {} Create` | `~ {} Update` | `- {} Delete` | `! {} Replace` | `= {} Same`",
        s.create, s.update, s.delete, s.replace, s.same
    ));
    if s.drift > 0 {
        out.push_str(&format!(" | `! {} Drift`", s.drift));
    }
    out.push_str("\n\n");

    if s.drift > 0 {
        out.push_str(&format!(
            "> **Warning:** Live infrastructure drift detected on **{}** resource(s).\n\n",
            s.drift
        ));
    }

    let actionable: Vec<&ResourceDiff> = report
        .resources
        .iter()
        .filter(|r| r.op.is_actionable())
        .collect();

    if actionable.is_empty() {
        out.push_str(&format!(
            "_No infrastructure mutations planned ({} resource(s) unchanged)._\n",
            s.same
        ));
        return out;
    }

    out.push_str("| Operation | Resource Type | Name | Changed Fields |\n");
    out.push_str("| :--- | :--- | :--- | :--- |\n");

    for res in actionable {
        let fields = if res.changed_fields.is_empty() {
            "-".to_string()
        } else {
            res.changed_fields
                .iter()
                .map(|f| format!("`{}`", sanitize_md_cell(f)))
                .collect::<Vec<_>>()
                .join(", ")
        };

        out.push_str(&format!(
            "| `{}` | `{}` | `{}` | {} |\n",
            res.op.badge(),
            sanitize_md_cell(&res.resource_type),
            sanitize_md_cell(&res.name),
            fields
        ));
    }

    out
}

#[derive(Debug)]
struct AggregatedResource {
    urn: String,
    resource_type: String,
    name: String,
    op: ChangeOp,
    changed_fields: BTreeSet<String>,
}

fn build_report_from_steps(
    steps: &[Value],
    explicit_drift_steps: &[Value],
) -> Result<PreviewReport, PkoError> {
    let mut order: Vec<String> = Vec::new();
    let mut by_urn: BTreeMap<String, AggregatedResource> = BTreeMap::new();

    for step in steps {
        ingest_step(step, false, &mut order, &mut by_urn)?;
    }
    for drift_step in explicit_drift_steps {
        ingest_step(drift_step, true, &mut order, &mut by_urn)?;
    }

    let mut summary = PreviewSummary::default();
    let mut resources = Vec::with_capacity(order.len());

    for urn in order {
        if let Some(entry) = by_urn.remove(&urn) {
            summary.increment(entry.op);
            resources.push(ResourceDiff {
                urn: entry.urn,
                resource_type: entry.resource_type,
                name: entry.name,
                op: entry.op,
                changed_fields: entry.changed_fields.into_iter().collect(),
            });
        }
    }

    Ok(PreviewReport {
        resources,
        summary,
        source: PreviewSource::Pko,
        run: None,
    })
}

fn ingest_step(
    step: &Value,
    force_drift: bool,
    order: &mut Vec<String>,
    by_urn: &mut BTreeMap<String, AggregatedResource>,
) -> Result<(), PkoError> {
    let Some(obj) = step.as_object() else {
        return Err(PkoError::PreviewParse(
            "preview step entry must be a JSON object".to_string(),
        ));
    };

    let urn = obj
        .get("urn")
        .and_then(Value::as_str)
        .ok_or_else(|| PkoError::PreviewParse("preview step missing 'urn' string".to_string()))?;

    if urn.trim().is_empty() {
        return Err(PkoError::PreviewParse(
            "preview step 'urn' must not be empty".to_string(),
        ));
    }

    let raw_op = if force_drift {
        "drift"
    } else {
        obj.get("op").and_then(Value::as_str).ok_or_else(|| {
            PkoError::PreviewParse(format!("preview step for '{urn}' missing 'op' string"))
        })?
    };

    let fields = extract_changed_fields(obj);
    let Some(mapped_op) = classify_op(raw_op, obj, &fields)? else {
        // Non-mutating auxiliary step (e.g., clean refresh before diff)
        return Ok(());
    };

    let (urn_type, urn_name) = parse_urn(urn);
    let explicit_type = obj
        .get("resource_type")
        .or_else(|| obj.get("type"))
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("newState")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            obj.get("oldState")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
        });

    let explicit_name = obj.get("name").and_then(Value::as_str);

    let resource_type = explicit_type
        .filter(|s| !s.is_empty())
        .unwrap_or(&urn_type)
        .to_string();
    let name = explicit_name
        .filter(|s| !s.is_empty())
        .unwrap_or(&urn_name)
        .to_string();

    if let Some(existing) = by_urn.get_mut(urn) {
        existing.op = merge_ops(existing.op, mapped_op);
        existing.changed_fields.extend(fields);
        if existing.resource_type == "unknown" && resource_type != "unknown" {
            existing.resource_type = resource_type;
        }
    } else {
        order.push(urn.to_string());
        by_urn.insert(
            urn.to_string(),
            AggregatedResource {
                urn: urn.to_string(),
                resource_type,
                name,
                op: mapped_op,
                changed_fields: fields,
            },
        );
    }

    Ok(())
}

fn classify_op(
    raw_op: &str,
    obj: &serde_json::Map<String, Value>,
    changed_fields: &BTreeSet<String>,
) -> Result<Option<ChangeOp>, PkoError> {
    let is_drifted_flag = obj.get("drifted").and_then(Value::as_bool).unwrap_or(false);

    match raw_op {
        "create" if is_drifted_flag => Ok(Some(ChangeOp::Drift)),
        "create" | "import" => Ok(Some(ChangeOp::Create)),
        "update" if is_drifted_flag => Ok(Some(ChangeOp::Drift)),
        "update" => Ok(Some(ChangeOp::Update)),
        "delete" => Ok(Some(ChangeOp::Delete)),
        "replace" | "create-replacement" | "delete-replaced" | "discard-replaced"
        | "import-replacement" => Ok(Some(ChangeOp::Replace)),
        "same" | "read" => {
            if is_drifted_flag {
                Ok(Some(ChangeOp::Drift))
            } else {
                Ok(Some(ChangeOp::Same))
            }
        }
        "drift" => Ok(Some(ChangeOp::Drift)),
        "refresh" => {
            if is_drifted_flag || !changed_fields.is_empty() || states_diverged(obj) {
                Ok(Some(ChangeOp::Drift))
            } else {
                Ok(None)
            }
        }
        "discard" | "remove-pending-replace" => Ok(None),
        other => Err(PkoError::PreviewParse(format!(
            "unsupported Pulumi change op '{other}'"
        ))),
    }
}

fn merge_ops(current: ChangeOp, incoming: ChangeOp) -> ChangeOp {
    // Replace sub-steps (`create-replacement`, `replace`, `delete-replaced`) always coalesce to `Replace`.
    if current == ChangeOp::Replace || incoming == ChangeOp::Replace {
        return ChangeOp::Replace;
    }
    // Create + Delete on the same URN in a single plan is a replacement.
    if (current == ChangeOp::Create && incoming == ChangeOp::Delete)
        || (current == ChangeOp::Delete && incoming == ChangeOp::Create)
    {
        return ChangeOp::Replace;
    }
    // An explicit planned mutation supersedes `Same`, while explicit `Drift` overrides `Same`.
    match (current, incoming) {
        (ChangeOp::Same, other) => other,
        (other, ChangeOp::Same) => other,
        (_, ChangeOp::Drift) => ChangeOp::Drift,
        (ChangeOp::Drift, other) => other,
        (_, latest) => latest,
    }
}

fn extract_changed_fields(obj: &serde_json::Map<String, Value>) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();

    if let Some(detailed) = obj.get("detailedDiff").and_then(Value::as_object) {
        fields.extend(detailed.keys().cloned());
    }

    for array_key in [
        "diffReasons",
        "diffs",
        "replaceKeys",
        "changedFields",
        "changed_fields",
    ] {
        if let Some(arr) = obj.get(array_key).and_then(Value::as_array) {
            for item in arr {
                if let Some(s) = item.as_str()
                    && !s.is_empty()
                {
                    fields.insert(s.to_string());
                }
            }
        }
    }

    if fields.is_empty()
        && let (Some(old_inputs), Some(new_inputs)) = (
            obj.get("oldState")
                .and_then(|s| s.get("inputs"))
                .and_then(Value::as_object),
            obj.get("newState")
                .and_then(|s| s.get("inputs"))
                .and_then(Value::as_object),
        )
    {
        let all_keys: BTreeSet<&String> = old_inputs.keys().chain(new_inputs.keys()).collect();
        for key in all_keys {
            if key.starts_with("__") {
                continue;
            }
            if old_inputs.get(key) != new_inputs.get(key) {
                fields.insert(key.clone());
            }
        }
    }

    fields
}

fn states_diverged(obj: &serde_json::Map<String, Value>) -> bool {
    let old_outputs = obj.get("oldState").and_then(|s| s.get("outputs"));
    let new_outputs = obj.get("newState").and_then(|s| s.get("outputs"));
    match (old_outputs, new_outputs) {
        (Some(old_v), Some(new_v)) => old_v != new_v,
        _ => false,
    }
}

fn parse_urn(urn: &str) -> (String, String) {
    // Standard Pulumi URN: `urn:pulumi:<stack>::<project>::<type>::<name>`
    let parts: Vec<&str> = urn.split("::").collect();
    if parts.len() >= 4 {
        let raw_type = parts[2];
        // For component-parented types (`pkg:mod:Parent$aws:s3/bucket:Bucket`), extract leaf resource type.
        let leaf_type = raw_type.rsplit('$').next().unwrap_or(raw_type);
        let name = parts[3..].join("::");
        (leaf_type.to_string(), name)
    } else {
        ("unknown".to_string(), urn.to_string())
    }
}

fn format_pr_label(pr_id: &str) -> String {
    let trimmed = pr_id.trim();
    if trimmed.is_empty() {
        "Proposal".to_string()
    } else if let Some(num) = trimmed.strip_prefix('#') {
        format!("PR #{num}")
    } else if trimmed.chars().all(|c| c.is_ascii_digit()) {
        format!("PR #{trimmed}")
    } else {
        format!("PR {}", sanitize_md_inline(trimmed))
    }
}

/// Escapes text rendered inside a backtick code span in a GFM table cell: backticks would close
/// the span (markdown injection, OPEN-28), pipes would split the cell, newlines break the row.
fn sanitize_md_cell(input: &str) -> String {
    input
        .replace('`', "'")
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

/// Escapes free text rendered as plain inline Markdown (no code span): neutralizes emphasis,
/// links, HTML and code-span delimiters so forge-/PKO-supplied messages render literally.
fn sanitize_md_inline(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\r' | '\n' => out.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '#' | '!' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_preview_json(raw: &str) -> Result<PreviewReport, PkoError> {
        let value: Value = serde_json::from_str(raw.trim())?;
        parse_preview_value(&value)
    }

    #[test]
    fn parses_all_change_ops_and_coalesces_replacements() {
        let raw = r#"
        {
          "steps": [
            {
              "op": "same",
              "urn": "urn:pulumi:prod::infra::pulumi:pulumi:Stack::infra-prod",
              "oldState": { "type": "pulumi:pulumi:Stack" },
              "newState": { "type": "pulumi:pulumi:Stack" }
            },
            {
              "op": "create",
              "urn": "urn:pulumi:prod::infra::proxmoxve:vm:VirtualMachine::worker-01",
              "newState": { "type": "proxmoxve:vm:VirtualMachine" },
              "detailedDiff": {
                "cpu": { "kind": "ADD" },
                "memory": { "kind": "ADD" }
              }
            },
            {
              "op": "update",
              "urn": "urn:pulumi:prod::infra::aws:rds/instance:Instance::app-db",
              "oldState": {
                "type": "aws:rds/instance:Instance",
                "inputs": { "allocatedStorage": 20, "engineVersion": "16.1", "__defaults": [] }
              },
              "newState": {
                "type": "aws:rds/instance:Instance",
                "inputs": { "allocatedStorage": 50, "engineVersion": "16.1", "__defaults": ["a"] }
              }
            },
            {
              "op": "create-replacement",
              "urn": "urn:pulumi:prod::infra::keel:net:Vpc$hcloud:index/server:Server::edge-01",
              "replaceKeys": ["location"]
            },
            {
              "op": "replace",
              "urn": "urn:pulumi:prod::infra::keel:net:Vpc$hcloud:index/server:Server::edge-01",
              "diffReasons": ["location", "serverType"]
            },
            {
              "op": "delete-replaced",
              "urn": "urn:pulumi:prod::infra::keel:net:Vpc$hcloud:index/server:Server::edge-01"
            },
            {
              "op": "delete",
              "urn": "urn:pulumi:prod::infra::aws:s3/bucket:Bucket::legacy-logs"
            },
            {
              "op": "refresh",
              "urn": "urn:pulumi:prod::infra::cloudflare:index/record:Record::api-dns",
              "oldState": { "outputs": { "value": "10.0.0.1" } },
              "newState": { "outputs": { "value": "10.0.0.99" } },
              "diffs": ["value"]
            }
          ],
          "changeSummary": {
            "create": 1,
            "update": 1,
            "delete": 1,
            "replace": 1,
            "same": 1
          }
        }
        "#;

        let report = parse_preview_json(raw).expect("parse preview json");
        assert_eq!(report.summary.create, 1);
        assert_eq!(report.summary.update, 1);
        assert_eq!(report.summary.delete, 1);
        assert_eq!(report.summary.replace, 1);
        assert_eq!(report.summary.same, 1);
        assert_eq!(report.summary.drift, 1);
        assert_eq!(report.summary.total(), 6);
        assert!(report.summary.has_destructive_changes());

        // Check replacement coalescing and component-parented URN parsing
        let replaced = report
            .resources
            .iter()
            .find(|r| r.name == "edge-01")
            .expect("edge-01 present");
        assert_eq!(replaced.op, ChangeOp::Replace);
        assert_eq!(replaced.resource_type, "hcloud:index/server:Server");
        assert_eq!(
            replaced.changed_fields,
            vec!["location".to_string(), "serverType".to_string()]
        );

        // Check fallback oldState/newState input diff on `app-db`
        let updated = report
            .resources
            .iter()
            .find(|r| r.name == "app-db")
            .expect("app-db present");
        assert_eq!(updated.op, ChangeOp::Update);
        assert_eq!(updated.changed_fields, vec!["allocatedStorage".to_string()]);

        // Check refresh drift detection
        let drifted = report
            .resources
            .iter()
            .find(|r| r.name == "api-dns")
            .expect("api-dns present");
        assert_eq!(drifted.op, ChangeOp::Drift);
        assert_eq!(drifted.changed_fields, vec!["value".to_string()]);
    }

    #[test]
    fn parses_engine_events_and_pko_status_wrappers() {
        // A summary-only engine event yields an empty report
        let summary_event = r#"{"sequence":1,"summaryEvent":{"resourceChanges":{"same":0}}}"#;
        let empty_report = parse_preview_json(summary_event).expect("summary event");
        assert_eq!(empty_report.summary.total(), 0);

        // PKO Stack status wrapper
        let pko_status = serde_json::json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "status": {
                "preview": {
                    "steps": [
                        {
                            "op": "refresh",
                            "urn": "urn:pulumi:prod::db::aws:rds/instance:Instance::clean-refresh"
                        },
                        {
                            "op": "same",
                            "urn": "urn:pulumi:prod::db::aws:rds/instance:Instance::clean-refresh",
                            "drifted": false
                        }
                    ],
                    "drift": [
                        {
                            "urn": "urn:pulumi:prod::db::aws:ec2/securityGroup:SecurityGroup::db-sg",
                            "changedFields": ["ingress"]
                        }
                    ]
                }
            }
        });
        let pko_report = parse_preview_value(&pko_status).expect("pko status parse");
        assert_eq!(pko_report.summary.same, 1);
        assert_eq!(pko_report.summary.drift, 1);
        assert!(!pko_report.summary.has_destructive_changes());
    }

    #[test]
    fn formats_markdown_report_for_actionable_and_noop_plans() {
        let raw = r#"
        [
          {
            "op": "create",
            "urn": "urn:pulumi:prod::infra::proxmoxve:vm:VirtualMachine::vm-01",
            "diffReasons": ["cpu", "memory"]
          },
          {
            "op": "update",
            "urn": "urn:pulumi:prod::infra::aws:rds/instance:Instance::pg-01",
            "diffReasons": ["allocatedStorage"]
          },
          {
            "op": "replace",
            "urn": "urn:pulumi:prod::infra::hcloud:index/server:Server::gw-01",
            "replaceKeys": ["location"]
          },
          {
            "op": "delete",
            "urn": "urn:pulumi:prod::infra::aws:s3/bucket:Bucket::old-bucket"
          },
          {
            "op": "drift",
            "urn": "urn:pulumi:prod::infra::cloudflare:index/record:Record::dns-01",
            "diffs": ["content"]
          },
          {
            "op": "same",
            "urn": "urn:pulumi:prod::infra::pulumi:pulumi:Stack::infra-prod"
          }
        ]
        "#;

        let report = parse_preview_json(raw).expect("parse array");
        let md = format_markdown_report(&report, "42");

        assert!(md.contains("### Keel Infrastructure Preview — PR #42"));
        assert!(md.contains("`+ 1 Create`"));
        assert!(md.contains("`~ 1 Update`"));
        assert!(md.contains("`- 1 Delete`"));
        assert!(md.contains("`! 1 Replace`"));
        assert!(md.contains("`= 1 Same`"));
        assert!(md.contains("`! 1 Drift`"));
        assert!(md.contains("Live infrastructure drift detected"));
        assert!(md.contains(
            "| `+ Create` | `proxmoxve:vm:VirtualMachine` | `vm-01` | `cpu`, `memory` |"
        ));
        assert!(md.contains(
            "| `~ Update` | `aws:rds/instance:Instance` | `pg-01` | `allocatedStorage` |"
        ));
        assert!(
            md.contains("| `! Replace` | `hcloud:index/server:Server` | `gw-01` | `location` |")
        );
        assert!(md.contains("| `- Delete` | `aws:s3/bucket:Bucket` | `old-bucket` | - |"));
        assert!(
            md.contains("| `! Drift` | `cloudflare:index/record:Record` | `dns-01` | `content` |")
        );
        // Unchanged `Same` stack resource should not clutter table rows
        assert!(!md.contains("| `= Same` |"));

        // Verify no-op report formatting
        let noop_report = PreviewReport {
            resources: vec![ResourceDiff {
                urn: "urn:pulumi:prod::infra::pulumi:pulumi:Stack::infra-prod".to_string(),
                resource_type: "pulumi:pulumi:Stack".to_string(),
                name: "infra-prod".to_string(),
                op: ChangeOp::Same,
                changed_fields: vec![],
            }],
            summary: PreviewSummary {
                same: 1,
                ..PreviewSummary::default()
            },
            ..PreviewReport::default()
        };
        let noop_md = format_markdown_report(&noop_report, "#99");
        assert!(noop_md.contains("PR #99"));
        assert!(noop_md.contains("No infrastructure mutations planned (1 resource(s) unchanged)"));
        assert!(!noop_md.contains("| Operation |"));
    }

    #[test]
    fn rejects_malformed_or_unsupported_preview_json() {
        assert!(parse_preview_json("   ").is_err());
        assert!(parse_preview_json("not-json").is_err());
        assert!(parse_preview_json("42").is_err());
        assert!(parse_preview_json("[123]").is_err());
        assert!(parse_preview_json(r#"{"unexpected": true}"#).is_err());
        assert!(parse_preview_json(r#"{"steps": [{"op": "create"}]}"#).is_err());
        assert!(parse_preview_json(r#"{"steps": [{"op": "create", "urn": "   "}]}"#).is_err());
        assert!(
            parse_preview_json(
                r#"{"steps": [{"op": "unknown-op", "urn": "urn:pulumi:a::b::c::d"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn covers_drift_flags_single_events_merge_ops_and_markdown_edge_cases() {
        // Single resourcePreEvent object
        let single_event = r#"{"resourcePreEvent":{"metadata":{"op":"import","urn":"short-urn","type":"custom:Mod:Res","name":"res|1","diffs":["field|a"]}}}"#;
        let single_rep = parse_preview_json(single_event).expect("single event");
        assert_eq!(single_rep.summary.create, 1);
        assert_eq!(single_rep.resources[0].resource_type, "custom:Mod:Res");
        assert_eq!(single_rep.resources[0].name, "res|1");

        let md_escaped = format_markdown_report(&single_rep, "  ");
        assert!(md_escaped.contains("### Keel Infrastructure Preview — Proposal"));
        assert!(md_escaped.contains("`res\\|1`"));
        assert!(md_escaped.contains("`field\\|a`"));

        let md_named_pr = format_markdown_report(&single_rep, "proposal-alpha");
        assert!(md_named_pr.contains("PR proposal-alpha"));

        // Single bare step object + changeSummary-only object + driftChanges-only object
        let bare_step =
            r#"{"op":"read","drifted":true,"urn":"urn:pulumi:dev::p::aws:s3/bucket:Bucket::b1"}"#;
        assert_eq!(parse_preview_json(bare_step).unwrap().summary.drift, 1);

        let summary_only = r#"{"changeSummary":{"same":0}}"#;
        assert_eq!(parse_preview_json(summary_only).unwrap().summary.total(), 0);

        let drift_only =
            r#"{"driftChanges":[{"urn":"urn:pulumi:dev::p::aws:s3/bucket:Bucket::b2"}]}"#;
        assert_eq!(parse_preview_json(drift_only).unwrap().summary.drift, 1);

        // Merge transitions: same -> update -> drift, create + delete -> replace, discard ignored
        let transitions = r#"
        {
          "steps": [
            { "op": "discard", "urn": "urn:pulumi:dev::p::pkg:m:T::ignored" },
            { "op": "same", "urn": "urn:pulumi:dev::p::pkg:m:T::r1" },
            { "op": "update", "urn": "urn:pulumi:dev::p::pkg:m:T::r1", "drifted": true },
            { "op": "same", "urn": "urn:pulumi:dev::p::pkg:m:T::r1" },
            { "op": "create", "urn": "urn:pulumi:dev::p::pkg:m:T::r2" },
            { "op": "delete", "urn": "urn:pulumi:dev::p::pkg:m:T::r2" },
            { "op": "create", "drifted": true, "urn": "urn:pulumi:dev::p::pkg:m:T::r3" }
          ]
        }
        "#;
        let merged = parse_preview_json(transitions).expect("merge transitions");
        assert_eq!(merged.summary.drift, 2);
        assert_eq!(merged.summary.replace, 1);
    }

    #[test]
    fn markdown_cells_neutralize_backtick_injection() {
        let report = PreviewReport {
            resources: vec![ResourceDiff {
                urn: "urn:pulumi:s::p::k:v:T::x".to_string(),
                resource_type: "k:v:T".to_string(),
                name: "evil` [click](https://evil.example) `".to_string(),
                op: ChangeOp::Create,
                changed_fields: vec!["a`b|c\nd".to_string()],
            }],
            summary: PreviewSummary {
                create: 1,
                ..PreviewSummary::default()
            },
            run: Some(crate::run::PreviewRunStatus {
                phase: crate::run::PreviewPhase::Failed,
                message: Some("**boom** <img src=x> [l](u)".to_string()),
                stack: Some("keel-previews/preview-stack-pr-1".to_string()),
                commit: Some("ab`c".to_string()),
                update: None,
            }),
            ..PreviewReport::default()
        };
        let md = format_markdown_report(&report, "1` <b>");
        let table_row = md.lines().find(|l| l.starts_with("| `+ Create`")).unwrap();
        // Exactly the 6 backticks wrapping op/type/name plus 2 for the single field: no injected spans.
        assert_eq!(table_row.matches('`').count(), 8);
        assert!(table_row.contains("evil' [click](https://evil.example) '"));
        assert!(table_row.contains("`a'b\\|c d`"));
        assert!(md.contains("**PKO preview:** Failed"));
        assert!(md.contains("\\*\\*boom\\*\\* \\<img src=x\\> \\[l\\](u)"));
        assert!(md.contains("@ `ab'c`"));
        assert!(md.contains("PR 1\\` \\<b\\>"));
    }

    #[test]
    fn report_wire_format_is_camel_case() {
        let report = PreviewReport {
            resources: vec![ResourceDiff {
                urn: "urn:pulumi:s::p::aws:ec2/instance:Instance::vm".to_string(),
                resource_type: "aws:ec2/instance:Instance".to_string(),
                name: "vm".to_string(),
                op: ChangeOp::Update,
                changed_fields: vec!["tags".to_string()],
            }],
            ..PreviewReport::default()
        };
        let wire = serde_json::to_value(&report).unwrap();
        assert_eq!(
            wire["resources"][0]["resourceType"],
            "aws:ec2/instance:Instance"
        );
        assert_eq!(wire["resources"][0]["changedFields"][0], "tags");
        assert!(wire["resources"][0].get("resource_type").is_none());
        assert!(wire["resources"][0].get("changed_fields").is_none());
        assert_eq!(
            serde_json::from_value::<PreviewReport>(wire).unwrap(),
            report
        );
    }
}
