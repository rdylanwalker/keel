//! Pure garbage-collection planner for ephemeral PR preview CRs (`GitRepository` and `Stack`)
//! in the `keel-previews` namespace (`SDD.md` §4.2, `NFR-REL-01`, `NFR-REL-02`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::stack::{
    FLUX_GIT_REPO_API_VERSION, FLUX_GIT_REPO_KIND, FluxGitRepository, KEEL_PREVIEWS_NAMESPACE,
    LABEL_EPHEMERAL_PREVIEW, LABEL_PR_ID, LABEL_PROTECT, MANAGEMENT_CORE_STACK_NAME,
    PKO_STACK_API_VERSION, PKO_STACK_KIND, PkoStack, PreviewStack, sanitize_pr_id,
};

/// Kind of ephemeral Kubernetes Custom Resource managed in `keel-previews`.
///
/// Ordered so `Stack` precedes `GitRepository` during deletion, ensuring PKO stops reconciling
/// against the Flux source before the `GitRepository` is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PreviewResourceKind {
    Stack,
    GitRepository,
}

impl PreviewResourceKind {
    /// Returns the Kubernetes `apiVersion` for this resource kind.
    pub fn api_version(self) -> &'static str {
        match self {
            Self::Stack => PKO_STACK_API_VERSION,
            Self::GitRepository => FLUX_GIT_REPO_API_VERSION,
        }
    }

    /// Returns the Kubernetes `kind` string for this resource kind.
    pub fn kind_str(self) -> &'static str {
        match self {
            Self::Stack => PKO_STACK_KIND,
            Self::GitRepository => FLUX_GIT_REPO_KIND,
        }
    }
}

/// Snapshot of a live cluster resource evaluated by the preview garbage-collection planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewResourceCandidate {
    pub kind: PreviewResourceKind,
    pub name: String,
    pub namespace: String,
    pub labels: BTreeMap<String, String>,
}

impl PreviewResourceCandidate {
    /// Builds a candidate from a typed `PkoStack`.
    pub fn from_stack(stack: &PkoStack) -> Self {
        let mut labels = stack.metadata.labels.clone();
        if stack.is_protected() {
            labels.insert(LABEL_PROTECT.to_string(), "true".to_string());
        }
        Self {
            kind: PreviewResourceKind::Stack,
            name: stack.metadata.name.clone(),
            namespace: stack.metadata.namespace.clone(),
            labels,
        }
    }

    /// Builds a candidate from a typed `FluxGitRepository`.
    pub fn from_git_repository(repo: &FluxGitRepository) -> Self {
        Self {
            kind: PreviewResourceKind::GitRepository,
            name: repo.metadata.name.clone(),
            namespace: repo.metadata.namespace.clone(),
            labels: repo.metadata.labels.clone(),
        }
    }

    /// Builds both `(Stack, GitRepository)` candidates from a synthesized `PreviewStack` bundle.
    pub fn from_preview_bundle(bundle: &PreviewStack) -> [Self; 2] {
        [
            Self::from_stack(&bundle.stack),
            Self::from_git_repository(&bundle.git_repository),
        ]
    }

    /// Extracts a candidate from an unstructured Kubernetes resource JSON object (`DynamicObject`).
    pub fn from_json_value(value: &Value) -> Option<Self> {
        let kind_str = value.get("kind")?.as_str()?;
        let kind = match kind_str {
            PKO_STACK_KIND => PreviewResourceKind::Stack,
            FLUX_GIT_REPO_KIND => PreviewResourceKind::GitRepository,
            _ => return None,
        };

        let meta = value.get("metadata")?.as_object()?;
        let name = meta.get("name")?.as_str()?.to_string();
        let namespace = meta
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let mut labels = BTreeMap::new();
        if let Some(lbl_map) = meta.get("labels").and_then(Value::as_object) {
            for (k, v) in lbl_map {
                if let Some(s) = v.as_str() {
                    labels.insert(k.clone(), s.to_string());
                }
            }
        }

        if value
            .get("spec")
            .and_then(|s| s.get("protect"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            labels.insert(LABEL_PROTECT.to_string(), "true".to_string());
        }

        Some(Self {
            kind,
            name,
            namespace,
            labels,
        })
    }

    fn is_protected(&self) -> bool {
        self.name == MANAGEMENT_CORE_STACK_NAME
            || self.labels.get(LABEL_PROTECT).map(String::as_str) == Some("true")
    }

    fn resolve_pr_id(&self) -> Option<String> {
        if let Some(label_pr) = self.labels.get(LABEL_PR_ID)
            && let Ok(normalized) = sanitize_pr_id(label_pr)
        {
            return Some(normalized);
        }

        // Fallback to deterministic naming convention (`preview-stack-pr-<id>` / `preview-source-pr-<id>`)
        let suffix = match self.kind {
            PreviewResourceKind::Stack => self.name.strip_prefix("preview-stack-pr-"),
            PreviewResourceKind::GitRepository => self.name.strip_prefix("preview-source-pr-"),
        }?;
        sanitize_pr_id(suffix).ok()
    }

    fn is_ephemeral_preview_resource(&self) -> bool {
        if self.labels.get(LABEL_EPHEMERAL_PREVIEW).map(String::as_str) == Some("false") {
            return false;
        }
        self.labels.get(LABEL_EPHEMERAL_PREVIEW).map(String::as_str) == Some("true")
            || self.labels.contains_key(LABEL_PR_ID)
            || self.name.starts_with("preview-stack-pr-")
            || self.name.starts_with("preview-source-pr-")
    }
}

/// Concrete Kubernetes resource deletion instruction emitted by the GC planner.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceDeletionTarget {
    pub pr_id: String,
    pub kind: PreviewResourceKind,
    pub api_version: String,
    pub namespace: String,
    pub name: String,
}

/// Result of running the pure preview garbage-collection planner.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcPlan {
    pub targets: Vec<ResourceDeletionTarget>,
    pub skipped_non_preview_namespace: usize,
    pub skipped_protected: usize,
}

impl GcPlan {
    /// Returns `true` when no cluster deletions are required.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Returns all PKO `Stack` deletion targets in execution order.
    pub fn stacks_to_delete(&self) -> Vec<&ResourceDeletionTarget> {
        self.targets
            .iter()
            .filter(|t| t.kind == PreviewResourceKind::Stack)
            .collect()
    }

    /// Returns all Flux `GitRepository` deletion targets in execution order.
    pub fn git_repositories_to_delete(&self) -> Vec<&ResourceDeletionTarget> {
        self.targets
            .iter()
            .filter(|t| t.kind == PreviewResourceKind::GitRepository)
            .collect()
    }
}

/// Determines which ephemeral `Stack` and `GitRepository` resources in `keel-previews` must be
/// deleted given a list of closed or merged PR IDs.
pub fn plan_preview_gc<S: AsRef<str>>(
    candidates: &[PreviewResourceCandidate],
    closed_or_merged_pr_ids: &[S],
) -> GcPlan {
    let closed_set: BTreeSet<String> = closed_or_merged_pr_ids
        .iter()
        .filter_map(|id| sanitize_pr_id(id.as_ref()).ok())
        .collect();

    evaluate_candidates(candidates, |pr_id| closed_set.contains(pr_id))
}

/// Determines which orphaned ephemeral `Stack` and `GitRepository` resources in `keel-previews`
/// should be deleted by comparing live cluster candidates against the authoritative set of open PR IDs.
pub fn plan_orphan_preview_gc<S: AsRef<str>>(
    candidates: &[PreviewResourceCandidate],
    open_pr_ids: &[S],
) -> GcPlan {
    let open_set: BTreeSet<String> = open_pr_ids
        .iter()
        .filter_map(|id| sanitize_pr_id(id.as_ref()).ok())
        .collect();

    evaluate_candidates(candidates, |pr_id| !open_set.contains(pr_id))
}

fn evaluate_candidates<F>(candidates: &[PreviewResourceCandidate], should_delete_pr: F) -> GcPlan
where
    F: Fn(&str) -> bool,
{
    let mut plan = GcPlan::default();
    let mut seen: BTreeSet<(PreviewResourceKind, String)> = BTreeSet::new();

    for candidate in candidates {
        if candidate.namespace != KEEL_PREVIEWS_NAMESPACE {
            plan.skipped_non_preview_namespace += 1;
            continue;
        }

        if candidate.is_protected() {
            plan.skipped_protected += 1;
            continue;
        }

        if !candidate.is_ephemeral_preview_resource() {
            continue;
        }

        let Some(pr_id) = candidate.resolve_pr_id() else {
            continue;
        };

        if !should_delete_pr(&pr_id) {
            continue;
        }

        if seen.insert((candidate.kind, candidate.name.clone())) {
            plan.targets.push(ResourceDeletionTarget {
                pr_id,
                kind: candidate.kind,
                api_version: candidate.kind.api_version().to_string(),
                namespace: candidate.namespace.clone(),
                name: candidate.name.clone(),
            });
        }
    }

    // Order Stack deletions before GitRepository deletions for clean PKO teardown.
    plan.targets.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.pr_id.cmp(&b.pr_id))
            .then_with(|| a.name.cmp(&b.name))
    });

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{
        EphemeralPreviewParams, ManagementCoreStackParams, TenantStackParams,
        synthesize_ephemeral_preview, synthesize_management_core_stack, synthesize_tenant_stack,
    };

    fn make_preview(pr_id: &str, tenant: &str, ts: u64) -> PreviewStack {
        let params = EphemeralPreviewParams::for_action(
            pr_id,
            tenant,
            "provision-vm",
            ts,
            "https://forge.internal/keel/infra.git",
            "tenants/acme/infra/vm",
            "keel/acme-vm/prod",
            "s3://pulumi-state",
        )
        .expect("valid params");
        synthesize_ephemeral_preview(&params).expect("valid preview bundle")
    }

    #[test]
    fn plans_deletions_for_closed_prs_stacks_before_git_repositories() {
        let pr_10 = make_preview("10", "acme", 1_700_000_010);
        let pr_11 = make_preview("11", "acme", 1_700_000_011);
        let pr_12 = make_preview("12", "beta", 1_700_000_012);

        let mgmt = synthesize_management_core_stack(&ManagementCoreStackParams::new(
            "s3://pulumi-state",
            "flux-system",
            "clusters/management",
        ))
        .unwrap();

        let tenant_stack = synthesize_tenant_stack(&TenantStackParams::new(
            "acme",
            "prod-db",
            "keel/acme-db/prod",
            "s3://pulumi-state",
            "tenant-source",
            "tenants/acme/infra/db",
        ))
        .unwrap();

        let mut candidates = Vec::new();
        // Deliberately insert GitRepository before Stack and include duplicates to test ordering & dedup
        candidates.push(PreviewResourceCandidate::from_git_repository(
            &pr_12.git_repository,
        ));
        candidates.push(PreviewResourceCandidate::from_stack(&pr_12.stack));
        candidates.push(PreviewResourceCandidate::from_stack(&pr_12.stack));
        candidates.extend(PreviewResourceCandidate::from_preview_bundle(&pr_10));
        candidates.extend(PreviewResourceCandidate::from_preview_bundle(&pr_11));
        candidates.push(PreviewResourceCandidate::from_stack(&mgmt));
        candidates.push(PreviewResourceCandidate::from_stack(&tenant_stack));

        // Close PR #10 and PR #12; keep PR #11 open
        let plan = plan_preview_gc(&candidates, &["#10", "12", "   "]);
        assert!(!plan.is_empty());
        assert_eq!(plan.skipped_non_preview_namespace, 2);
        assert_eq!(plan.skipped_protected, 0);
        assert_eq!(plan.targets.len(), 4);

        // Stacks must all precede GitRepositories
        assert_eq!(plan.stacks_to_delete().len(), 2);
        assert_eq!(plan.git_repositories_to_delete().len(), 2);
        assert_eq!(plan.targets[0].kind, PreviewResourceKind::Stack);
        assert_eq!(plan.targets[0].name, "preview-stack-pr-10");
        assert_eq!(plan.targets[1].kind, PreviewResourceKind::Stack);
        assert_eq!(plan.targets[1].name, "preview-stack-pr-12");
        assert_eq!(plan.targets[2].kind, PreviewResourceKind::GitRepository);
        assert_eq!(plan.targets[2].name, "preview-source-pr-10");
        assert_eq!(plan.targets[3].kind, PreviewResourceKind::GitRepository);
        assert_eq!(plan.targets[3].name, "preview-source-pr-12");
    }

    #[test]
    fn never_deletes_protected_or_non_preview_resources_and_reconciles_orphans() {
        // Protected resource accidentally placed inside keel-previews must STILL be skipped
        let protected_in_previews = serde_json::json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": {
                "name": "preview-stack-pr-77",
                "namespace": "keel-previews",
                "labels": {
                    "keel.io/pr-id": "77",
                    "keel.io/ephemeral-preview": "true"
                }
            },
            "spec": {
                "protect": true
            }
        });

        // Unlabeled resources following naming convention in keel-previews
        let convention_named_repo = serde_json::json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": {
                "name": "preview-source-pr-88",
                "namespace": "keel-previews"
            }
        });
        let convention_named_stack = serde_json::json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": {
                "name": "preview-stack-pr-88",
                "namespace": "keel-previews"
            }
        });

        // Explicitly non-ephemeral resource in keel-previews
        let permanent_helper = serde_json::json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": {
                "name": "shared-catalog",
                "namespace": "keel-previews",
                "labels": {
                    "keel.io/ephemeral-preview": "false"
                }
            }
        });

        // Unrelated Kubernetes Kind should return None on from_json_value
        let unrelated_secret = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "s", "namespace": "keel-previews" }
        });
        assert!(PreviewResourceCandidate::from_json_value(&unrelated_secret).is_none());

        let pr_50 = make_preview("50", "acme", 1_700_000_050);

        let mut candidates = vec![
            PreviewResourceCandidate::from_json_value(&protected_in_previews).unwrap(),
            PreviewResourceCandidate::from_json_value(&convention_named_repo).unwrap(),
            PreviewResourceCandidate::from_json_value(&convention_named_stack).unwrap(),
            PreviewResourceCandidate::from_json_value(&permanent_helper).unwrap(),
        ];
        candidates.extend(PreviewResourceCandidate::from_preview_bundle(&pr_50));

        // Reconcile against open PRs = ["50"]; PR #88 is orphaned and should be deleted, PR #77 is protected!
        let orphan_plan = plan_orphan_preview_gc(&candidates, &["#50"]);
        assert_eq!(orphan_plan.skipped_protected, 1);
        assert_eq!(orphan_plan.targets.len(), 2);
        assert_eq!(orphan_plan.targets[0].kind.kind_str(), "Stack");
        assert_eq!(orphan_plan.targets[0].name, "preview-stack-pr-88");
        assert_eq!(orphan_plan.targets[1].pr_id, "88");
        assert_eq!(orphan_plan.targets[1].name, "preview-source-pr-88");
        assert_eq!(orphan_plan.targets[1].kind.kind_str(), "GitRepository");
    }
}
