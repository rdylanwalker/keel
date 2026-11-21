//! Stateless `AppState` and cluster/repository state providers (`SDD.md` §6.3).
//!
//! `keel-server` holds zero SQLite or external database connections. All state is
//! derived from Kubernetes `Secret`s (`SecretStateStore`), Git (`GitRunner` + `repo_workdir`),
//! `ForgeProvider`, and in-memory `kube-rs` CRD caches (`TelemetryMux`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use keel_core::{
    AuthPhase, InfraResourceKind, ManagementAddonKind, OidcConfig, split_yaml_documents,
};
use keel_git::{ForgeProvider, GitRunner, LocalGitProvider};
use keel_kube::{
    InMemorySecretStore, LiveResourceInfo, LiveResourceStatus, LiveStatusSource, SecretStateStore,
    StackDiagnostics, TelemetryMux,
};
use keel_pko::{GcPlan, PreviewReport, PreviewResourceCandidate, PreviewStack};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::error::ServerError;

/// Summary of a PKO v2 `Stack` resource returned by `GET /api/v1/stacks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackSummary {
    pub name: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub stack: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(rename = "type", default = "default_stack_type")]
    pub stack_type: String,
    pub preview: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_kind: Option<InfraResourceKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    pub protect: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<String>,
    pub status: String,
}

fn default_stack_type() -> String {
    "workload".to_string()
}

fn infer_stack_type(
    name: &str,
    namespace: &str,
    provider: Option<&str>,
    label_type: Option<&str>,
) -> String {
    if let Some(t) = label_type.map(str::trim).filter(|s| !s.is_empty()) {
        return t.to_string();
    }
    if name == "management-cluster-core" || namespace == "flux-system" {
        return "management".to_string();
    }
    if matches!(provider, Some("aws" | "gcp" | "azure" | "cloudflare")) {
        return "cloud".to_string();
    }
    "workload".to_string()
}

/// Summary of a Flux `HelmRelease` / `Kustomization` / Flagger `Canary` returned by `GET /api/v1/releases`.
///
/// Management-plane add-ons (`clusters/management/core-addons/`) are reported to cluster admins as
/// releases with `managementAddon: true`, `protected: true`, `tenant: "management"` and the extra
/// `catalogId` / `addonKind` / `desiredVersion` / `conflict` fields; those fields are omitted for
/// tenant releases so tenant payloads are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseSummary {
    pub name: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub chart: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub runtime: String,
    pub canary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canary_weight: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canary_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<String>,
    pub status: String,
    /// `true` for a protected management-plane core add-on (never for tenant releases).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub management_addon: bool,
    /// `true` when the release cannot be deleted through the API (management add-ons).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub protected: bool,
    /// `catalog/core` application id backing a management add-on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    /// Flux object kind reconciling a management add-on (`HelmRelease` or `Kustomization`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addon_kind: Option<ManagementAddonKind>,
    /// Desired chart version / image tag declared in Git for a management add-on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_version: Option<String>,
    /// `true` when the management add-on id is defined by more than one Flux object in Git.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflict: bool,
}

/// Live management add-on key: Flux object `(kind, namespace, name)`.
pub type ManagementAddonKey = (ManagementAddonKind, String, String);

/// Stateless provider reading PKO `Stack` and Flux/Flagger release summaries
/// from Git manifests and/or in-memory CRD caches.
#[async_trait]
pub trait ClusterStateProvider: Send + Sync {
    async fn list_stacks(
        &self,
        tenant: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError>;

    async fn list_releases(&self, tenant: Option<&str>)
    -> Result<Vec<ReleaseSummary>, ServerError>;

    /// Management-plane core add-ons (`clusters/management/core-addons/`) as protected
    /// releases, with live Flux status where available. Callers must restrict this to cluster
    /// admins; it never includes the infrastructure root Kustomizations (`flux-system`,
    /// `core-addons`, `keel-tenants`).
    async fn list_management_addon_releases(&self) -> Result<Vec<ReleaseSummary>, ServerError> {
        Ok(Vec::new())
    }

    async fn stack_diagnostics(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<Option<StackDiagnostics>, ServerError> {
        Ok(None)
    }

    async fn release_diagnostics(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<Option<StackDiagnostics>, ServerError> {
        Ok(None)
    }

    async fn restart_stack_workspace(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<String, ServerError> {
        Err(ServerError::ServiceUnavailable(
            "Live Kubernetes cluster connection required to restart stack workspace".to_string(),
        ))
    }

    async fn reconcile_stack(&self, _namespace: &str, _name: &str) -> Result<String, ServerError> {
        Err(ServerError::ServiceUnavailable(
            "Live Kubernetes cluster connection required to reconcile stack".to_string(),
        ))
    }

    async fn force_delete_stack(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<String, ServerError> {
        Err(ServerError::ServiceUnavailable(
            "Live Kubernetes cluster connection required to force-delete stack".to_string(),
        ))
    }

    async fn unlock_stack(&self, _namespace: &str, _name: &str) -> Result<String, ServerError> {
        Err(ServerError::ServiceUnavailable(
            "Live Kubernetes cluster connection required to unlock stack".to_string(),
        ))
    }

    async fn reconcile_release(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<String, ServerError> {
        Err(ServerError::ServiceUnavailable(
            "Live Kubernetes cluster connection required to reconcile release".to_string(),
        ))
    }

    async fn stack_outputs(&self, _namespace: &str, _name: &str) -> HashMap<String, String> {
        HashMap::new()
    }

    /// Live Flux `HelmRelease`/`Kustomization` status for management-plane namespaces
    /// (`keel-system`, `flux-system`) keyed by `(kind, namespace, name)`; empty without a live
    /// cluster.
    async fn management_addon_statuses(&self) -> HashMap<ManagementAddonKey, LiveResourceInfo> {
        HashMap::new()
    }

    /// Whether live Kubernetes status is available (false for the offline Git-only provider).
    fn has_live_status(&self) -> bool {
        false
    }

    async fn execute_preview_gc(&self, _plan: &GcPlan) -> Result<usize, ServerError> {
        Ok(0)
    }

    async fn apply_cluster_policy(
        &self,
        _name: &str,
        _policy_doc: &Value,
    ) -> Result<(), ServerError> {
        Ok(())
    }

    async fn delete_tenant_stack(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<bool, ServerError> {
        Ok(false)
    }
}

/// Default stateless [`ClusterStateProvider`] that scans `tenants/` and `deploy/`
/// YAML manifests inside `repo_workdir` (and optionally merges cached CRD summaries).
#[derive(Debug, Clone)]
pub struct RepoClusterStateProvider {
    repo_workdir: PathBuf,
}

fn infer_stack_resource_kind(
    name: &str,
    namespace: &str,
    provider: Option<&str>,
    label_resource_kind: Option<&str>,
    catalog_id: Option<&str>,
    has_kubeconfig_secret: bool,
    is_manifest_runtime: bool,
) -> InfraResourceKind {
    if let Some(r) = label_resource_kind.and_then(|r| r.parse::<InfraResourceKind>().ok()) {
        return r;
    }

    if name == "management-cluster-core"
        || namespace == "flux-system"
        || provider == Some("kind")
        || has_kubeconfig_secret
        || catalog_id.is_some_and(|c| {
            c.contains("cluster")
                || c.contains("eks")
                || c.contains("gke")
                || c.contains("aks")
                || c.contains("k3s")
        })
        || name.contains("cluster")
        || name.contains("eks")
        || name.contains("gke")
        || name.contains("aks")
        || name.contains("k3s")
    {
        InfraResourceKind::Cluster
    } else if is_manifest_runtime
        || provider == Some("kubernetes")
        || provider == Some("cloudflare")
        || catalog_id == Some("postgres-managed")
        || catalog_id == Some("canary-gateway-config")
        || name.contains("postgres")
        || name.contains("db")
        || name.contains("gateway")
        || name.ends_with("-net")
        || name == "net"
    {
        InfraResourceKind::Service
    } else {
        InfraResourceKind::Vm
    }
}

/// Pseudo-tenant label reported for the protected management control-plane stack (`flux-system`).
pub const MANAGEMENT_TENANT: &str = "management";

/// Status reported for a live tenant `Stack` that is absent from Git `main` but not terminating.
pub const STACK_STATUS_ORPHANED: &str = "Orphaned";

impl RepoClusterStateProvider {
    pub fn new(repo_workdir: impl Into<PathBuf>) -> Self {
        Self {
            repo_workdir: repo_workdir.into(),
        }
    }

    /// Reads the Stage 0 `bootstrapTarget` (e.g. `kind`, `aws`) from `.keel/config.yaml`, used as
    /// the provider of the management control-plane stack.
    fn bootstrap_target(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.repo_workdir.join(".keel/config.yaml")).ok()?;
        let doc = serde_yml::from_str::<Value>(&raw).ok()?;
        doc.get("bootstrapTarget")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::collect_yaml_files(&path, out);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && (ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
            {
                out.push(path);
            }
        }
    }

    fn infer_tenant_from_path(repo_root: &Path, file_path: &Path) -> Option<String> {
        let rel = file_path.strip_prefix(repo_root).ok()?;
        let mut comps = rel.components();
        let first = comps.next()?.as_os_str().to_str()?;
        if first == "tenants" {
            let tenant = comps.next()?.as_os_str().to_str()?;
            return Some(tenant.to_string());
        }
        None
    }

    fn infer_bundle_from_path(repo_root: &Path, file_path: &Path) -> Option<String> {
        let rel = file_path.strip_prefix(repo_root).ok()?;
        let mut comps = rel.components();
        if comps.next()?.as_os_str().to_str()? != "tenants" {
            return None;
        }
        let _tenant = comps.next()?;
        if comps.next()?.as_os_str().to_str()? != "bundles" {
            return None;
        }
        comps
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .map(str::to_owned)
    }

    fn infer_bundle_item_from_path(repo_root: &Path, file_path: &Path) -> Option<String> {
        let rel = file_path.strip_prefix(repo_root).ok()?;
        let mut comps = rel.components();
        if comps.next()?.as_os_str().to_str()? != "tenants" {
            return None;
        }
        let _tenant = comps.next()?;
        if comps.next()?.as_os_str().to_str()? != "bundles" {
            return None;
        }
        let _bundle = comps.next()?;
        comps
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .map(str::to_owned)
    }
}

#[async_trait]
impl ClusterStateProvider for RepoClusterStateProvider {
    async fn list_stacks(
        &self,
        tenant: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let mut files = Vec::new();
        Self::collect_yaml_files(&self.repo_workdir.join("tenants"), &mut files);
        Self::collect_yaml_files(&self.repo_workdir.join("clusters"), &mut files);
        files.sort();

        let mut stacks = Vec::new();
        for path in files {
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            for doc in split_yaml_documents(&content) {
                let Ok(val) = serde_yml::from_str::<Value>(&doc) else {
                    continue;
                };
                if val.get("kind").and_then(Value::as_str) != Some("Stack") {
                    continue;
                }
                let name = val
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let namespace = val
                    .pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_string();
                let stack_name = val
                    .pointer("/spec/stack")
                    .and_then(Value::as_str)
                    .unwrap_or(&name)
                    .to_string();
                let preview = val
                    .pointer("/spec/preview")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Same semantics as `keel_pko::PkoStack::is_protected` (`keel.io/protect` label or
                // `spec.config.protect`), shared with the force-delete guard (`NFR-REL-01`).
                let protect = keel_kube::is_stack_manifest_protected(&val);
                let doc_tenant = val
                    .pointer("/metadata/labels/keel.io~1tenant")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Self::infer_tenant_from_path(&self.repo_workdir, &path));
                let doc_provider = val
                    .pointer("/metadata/labels/keel.io~1provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let doc_bundle = val
                    .pointer("/metadata/labels/keel.io~1bundle")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Self::infer_bundle_from_path(&self.repo_workdir, &path));
                let doc_bundle_item = val
                    .pointer("/metadata/labels/keel.io~1bundle-item")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Self::infer_bundle_item_from_path(&self.repo_workdir, &path));

                if let Some(t_filter) = tenant
                    && doc_tenant.as_deref() != Some(t_filter)
                {
                    continue;
                }
                if let Some(p_filter) = provider
                    && doc_provider.as_deref() != Some(p_filter)
                {
                    continue;
                }

                let label_type = val
                    .pointer("/metadata/labels/keel.io~1type")
                    .and_then(Value::as_str);
                let stack_type =
                    infer_stack_type(&name, &namespace, doc_provider.as_deref(), label_type);

                let catalog_id = val
                    .pointer("/metadata/labels/keel.io~1catalog-id")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let is_manifest_runtime = val.pointer("/spec/manifests").is_some();
                let has_kubeconfig_secret =
                    val.pointer("/spec/config/kubeconfigSecretName").is_some()
                        && doc_provider.as_deref() != Some("kubernetes")
                        && catalog_id.as_deref() != Some("canary-gateway-config")
                        && !name.contains("gateway");

                let label_resource_kind = val
                    .pointer("/metadata/labels/keel.io~1resource-kind")
                    .and_then(Value::as_str);
                let resource_kind = infer_stack_resource_kind(
                    &name,
                    &namespace,
                    doc_provider.as_deref(),
                    label_resource_kind,
                    catalog_id.as_deref(),
                    has_kubeconfig_secret,
                    is_manifest_runtime,
                );

                let doc_tenant = if stack_type == "management" {
                    doc_tenant.or_else(|| Some(MANAGEMENT_TENANT.to_string()))
                } else {
                    doc_tenant
                };
                let doc_provider = if stack_type == "management" {
                    doc_provider.or_else(|| self.bootstrap_target())
                } else {
                    doc_provider
                };

                stacks.push(StackSummary {
                    name,
                    namespace,
                    tenant: doc_tenant,
                    stack: stack_name,
                    provider: doc_provider,
                    stack_type,
                    preview,
                    protect,
                    resource_kind: Some(resource_kind),
                    catalog_id,
                    bundle: doc_bundle,
                    bundle_item: doc_bundle_item,
                    resource_count: None,
                    last_synced: None,
                    status: "Pending".to_string(),
                });
            }
        }

        if tenant.is_none()
            && provider.is_none()
            && !stacks.iter().any(|s| s.name == "management-cluster-core")
        {
            stacks.insert(
                0,
                StackSummary {
                    name: "management-cluster-core".to_string(),
                    namespace: "flux-system".to_string(),
                    tenant: Some(MANAGEMENT_TENANT.to_string()),
                    stack: "keel/management/core".to_string(),
                    provider: self.bootstrap_target(),
                    stack_type: "management".to_string(),
                    preview: false,
                    protect: true,
                    resource_kind: Some(InfraResourceKind::Cluster),
                    catalog_id: None,
                    bundle: None,
                    bundle_item: None,
                    resource_count: None,
                    last_synced: None,
                    status: "Pending".to_string(),
                },
            );
        }

        Ok(stacks)
    }

    async fn list_releases(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let mut files = Vec::new();
        let tenants_root = self.repo_workdir.join("tenants");
        Self::collect_yaml_files(&tenants_root, &mut files);
        files.sort();

        // Cache item-level kustomization.yaml resource allowlists so stale unreferenced
        // manifests (e.g. a leftover helmrelease.yaml beside helmrelease-canary.yaml) are ignored.
        let mut kustomize_allowlists: HashMap<PathBuf, Option<std::collections::HashSet<String>>> =
            HashMap::new();

        let mut releases = Vec::new();
        for path in files {
            let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let has_manifests_yaml = path
                .parent()
                .is_some_and(|p| p.join("manifests.yaml").is_file());
            let has_stack_yaml = path
                .parent()
                .is_some_and(|p| p.join("stack.yaml").is_file());
            if fname == "flux-kustomization.yaml" && (!has_manifests_yaml || has_stack_yaml) {
                continue;
            }
            if let Some(parent) = path.parent()
                && parent != tenants_root.as_path()
                && fname != "flux-kustomization.yaml"
            {
                let allowed = kustomize_allowlists
                    .entry(parent.to_path_buf())
                    .or_insert_with(|| {
                        let k_path = parent.join("kustomization.yaml");
                        let raw = std::fs::read_to_string(k_path).ok()?;
                        let doc = serde_yml::from_str::<Value>(&raw).ok()?;
                        let arr = doc.get("resources")?.as_array()?;
                        Some(
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .map(|s| s.trim_start_matches("./").to_string())
                                .collect(),
                        )
                    });
                if let Some(set) = allowed
                    && !fname.is_empty()
                    && fname != "kustomization.yaml"
                    && !set.contains(fname)
                {
                    continue;
                }
            }
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            for doc in split_yaml_documents(&content) {
                let Ok(val) = serde_yml::from_str::<Value>(&doc) else {
                    continue;
                };
                let api_version = val
                    .get("apiVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let kind = val.get("kind").and_then(Value::as_str).unwrap_or_default();
                let is_manifest_pulumi_stack = kind == "Stack" && has_manifests_yaml;
                let is_flux_release = kind == "HelmRelease"
                    || (kind == "Kustomization"
                        && api_version.starts_with("kustomize.toolkit.fluxcd.io/"))
                    || kind == "Canary"
                    || is_manifest_pulumi_stack;
                if !is_flux_release {
                    continue;
                }
                let name = val
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let namespace = val
                    .pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_string();
                let doc_tenant = val
                    .pointer("/metadata/labels/keel.io~1tenant")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Self::infer_tenant_from_path(&self.repo_workdir, &path));

                if let Some(t_filter) = tenant
                    && doc_tenant.as_deref() != Some(t_filter)
                {
                    continue;
                }

                let chart = val
                    .pointer("/spec/chart/spec/chart")
                    .or_else(|| val.pointer("/metadata/labels/keel.io~1catalog-id"))
                    .and_then(Value::as_str)
                    .unwrap_or(&name)
                    .to_string();
                let version = val
                    .pointer("/spec/chart/spec/version")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let label_runtime = val
                    .pointer("/metadata/labels/keel.io~1runtime")
                    .and_then(Value::as_str);
                let runtime = if label_runtime == Some("manifest") || has_manifests_yaml {
                    "manifest"
                } else {
                    match kind {
                        "Kustomization" => "kustomize",
                        _ => "helm",
                    }
                }
                .to_string();

                let target_cluster = val
                    .pointer("/spec/kubeConfig/secretRef/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let doc_bundle = val
                    .pointer("/metadata/labels/keel.io~1bundle")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Self::infer_bundle_from_path(&self.repo_workdir, &path));
                let doc_bundle_item = val
                    .pointer("/metadata/labels/keel.io~1bundle-item")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Self::infer_bundle_item_from_path(&self.repo_workdir, &path));

                releases.push(ReleaseSummary {
                    name,
                    namespace,
                    tenant: doc_tenant,
                    chart,
                    version,
                    runtime,
                    canary: kind == "Canary",
                    canary_weight: None,
                    canary_phase: None,
                    primary_version: None,
                    error_rate: None,
                    latency_ms: None,
                    target_cluster,
                    bundle: doc_bundle,
                    bundle_item: doc_bundle_item,
                    last_synced: None,
                    status: "Pending".to_string(),
                    ..ReleaseSummary::default()
                });
            }
        }

        // Deduplicate paired HelmRelease + Canary (and any duplicate (namespace, name) manifests)
        let canary_keys: std::collections::HashSet<(String, String)> = releases
            .iter()
            .filter(|r| r.canary)
            .map(|r| (r.namespace.clone(), r.name.clone()))
            .collect();
        let has_paired_hr: std::collections::HashSet<(String, String)> = releases
            .iter()
            .filter(|r| !r.canary && canary_keys.contains(&(r.namespace.clone(), r.name.clone())))
            .map(|r| (r.namespace.clone(), r.name.clone()))
            .collect();
        releases.retain(|r| {
            !(r.canary && has_paired_hr.contains(&(r.namespace.clone(), r.name.clone())))
        });
        for rel in &mut releases {
            if canary_keys.contains(&(rel.namespace.clone(), rel.name.clone())) {
                rel.canary = true;
            }
        }
        let mut seen = std::collections::HashSet::new();
        releases.retain(|r| seen.insert((r.namespace.clone(), r.name.clone())));

        Ok(releases)
    }

    async fn list_management_addon_releases(&self) -> Result<Vec<ReleaseSummary>, ServerError> {
        Ok(
            crate::routes::management::read_desired_core_addons(&self.repo_workdir)
                .await
                .into_iter()
                .map(crate::routes::management::management_addon_release)
                .collect(),
        )
    }
}

/// Hybrid `ClusterStateProvider` that reads desired manifests from Git via [`RepoClusterStateProvider`]
/// and overlays live Kubernetes CRD reconciliation status from a [`LiveStatusSource`].
pub struct KubeClusterStateProvider {
    repo: RepoClusterStateProvider,
    live: Arc<dyn LiveStatusSource>,
}

impl KubeClusterStateProvider {
    pub fn new(repo: RepoClusterStateProvider, live: Arc<dyn LiveStatusSource>) -> Self {
        Self { repo, live }
    }

    fn status_label(status: &LiveResourceStatus) -> String {
        match status {
            LiveResourceStatus::Synced => "Synced".to_string(),
            LiveResourceStatus::Ready => "Ready".to_string(),
            LiveResourceStatus::Reconciling => "Reconciling".to_string(),
            LiveResourceStatus::Deleting => "Deleting".to_string(),
            LiveResourceStatus::Error => "Error".to_string(),
        }
    }
}

#[async_trait]
impl ClusterStateProvider for KubeClusterStateProvider {
    async fn list_stacks(
        &self,
        tenant: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let mut stacks = self.repo.list_stacks(tenant, provider).await?;
        let live = self.live.stack_status_details().await;
        for stack in &mut stacks {
            let is_management = stack.stack_type == "management";
            let info = live
                .get(&(stack.namespace.clone(), stack.name.clone()))
                .or_else(|| {
                    is_management
                        .then(|| {
                            live.get(&("flux-system".to_string(), "core-addons".to_string()))
                                .or_else(|| {
                                    live.get(&(
                                        "flux-system".to_string(),
                                        "flux-system".to_string(),
                                    ))
                                })
                        })
                        .flatten()
                });
            if let Some(info) = info {
                stack.status = Self::status_label(&info.status);
                stack.last_synced = info.last_synced.clone();
                if info.resource_count.is_some() {
                    stack.resource_count = info.resource_count;
                }
                stack.protect = stack.protect || info.protected;
                if stack.provider.is_none() {
                    stack.provider = info.provider.clone();
                }
            }
        }
        // Keep any live tenant-* stack that is absent from Git `main`. Such a stack is only
        // "Deleting" when the API server reports a `deletionTimestamp`; otherwise it is reported
        // honestly as `Orphaned` (live, but not declared in Git / awaiting Flux prune).
        for ((ns, name), info) in &live {
            if !ns.starts_with("tenant-") {
                continue;
            }
            let inferred_tenant = info
                .tenant
                .as_deref()
                .or_else(|| ns.strip_prefix("tenant-"));
            if let Some(t_filter) = tenant
                && inferred_tenant != Some(t_filter)
            {
                continue;
            }
            if let Some(p_filter) = provider
                && info.provider.as_deref() != Some(p_filter)
            {
                continue;
            }
            if !stacks.iter().any(|s| s.namespace == *ns && s.name == *name) {
                let stack_type = infer_stack_type(name, ns, info.provider.as_deref(), None);
                let resource_kind = infer_stack_resource_kind(
                    name,
                    ns,
                    info.provider.as_deref(),
                    None,
                    None,
                    false,
                    false,
                );

                stacks.push(StackSummary {
                    name: name.clone(),
                    namespace: ns.clone(),
                    tenant: inferred_tenant.map(str::to_owned),
                    stack: name.clone(),
                    provider: info.provider.clone(),
                    stack_type,
                    preview: false,
                    protect: info.protected,
                    resource_kind: Some(resource_kind),
                    catalog_id: None,
                    bundle: None,
                    bundle_item: None,
                    resource_count: info.resource_count,
                    last_synced: info.last_synced.clone(),
                    status: if info.terminating {
                        "Deleting".to_string()
                    } else {
                        STACK_STATUS_ORPHANED.to_string()
                    },
                });
            }
        }
        Ok(stacks)
    }

    async fn list_releases(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let mut releases = self.repo.list_releases(tenant).await?;
        let known_stacks = self
            .repo
            .list_stacks(tenant, None)
            .await
            .unwrap_or_default();

        let live_stacks = self.live.stack_statuses().await;
        let live = self.live.release_status_details().await;
        let live_canaries = self.live.canary_statuses().await;
        for rel in &mut releases {
            if let Some(info) = live.get(&(rel.namespace.clone(), rel.name.clone())) {
                rel.status = Self::status_label(&info.status);
                rel.last_synced = info.last_synced.clone();
            }
            if rel.canary
                && let Some(cinfo) = live_canaries.get(&(rel.namespace.clone(), rel.name.clone()))
            {
                rel.status = Self::status_label(&cinfo.status);
                rel.last_synced = cinfo
                    .last_synced
                    .clone()
                    .or_else(|| rel.last_synced.clone());
                rel.canary_weight = cinfo.canary_weight;
                rel.canary_phase = cinfo.canary_phase.clone();
                rel.primary_version = cinfo.primary_version.clone();
                rel.error_rate = cinfo.error_rate;
                rel.latency_ms = cinfo.latency_ms;
            }
        }
        // Keep any live tenant-* release that is absent from Git `main` (awaiting Flux prune or mid-termination),
        // but ignore Flux `Kustomization` wrappers that back PKO `Stack`s (e.g. bundle infra steps).
        for ((ns, name), info) in &live {
            if !ns.starts_with("tenant-") {
                continue;
            }
            let inferred_tenant = ns.strip_prefix("tenant-");
            if let Some(t_filter) = tenant
                && inferred_tenant != Some(t_filter)
            {
                continue;
            }
            if live_stacks.contains_key(&(ns.clone(), name.clone()))
                || known_stacks
                    .iter()
                    .any(|s| s.namespace == *ns && s.name == *name)
            {
                continue;
            }
            if !releases
                .iter()
                .any(|r| r.namespace == *ns && r.name == *name)
            {
                releases.push(ReleaseSummary {
                    name: name.clone(),
                    namespace: ns.clone(),
                    tenant: inferred_tenant.map(str::to_owned),
                    chart: name.clone(),
                    version: None,
                    runtime: "helm".to_string(),
                    canary: false,
                    canary_weight: None,
                    canary_phase: None,
                    primary_version: None,
                    error_rate: None,
                    latency_ms: None,
                    target_cluster: None,
                    bundle: None,
                    bundle_item: None,
                    last_synced: info.last_synced.clone(),
                    status: "Deleting".to_string(),
                    ..ReleaseSummary::default()
                });
            }
        }
        Ok(releases)
    }

    async fn list_management_addon_releases(&self) -> Result<Vec<ReleaseSummary>, ServerError> {
        let mut addons =
            crate::routes::management::read_desired_core_addons(&self.repo.repo_workdir).await;
        let live = self.management_addon_statuses().await;
        crate::routes::management::overlay_management_addon_statuses(&mut addons, &live);
        Ok(addons
            .into_iter()
            .map(crate::routes::management::management_addon_release)
            .collect())
    }

    async fn stack_diagnostics(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<StackDiagnostics>, ServerError> {
        Ok(self.live.stack_diagnostics(namespace, name).await)
    }

    async fn release_diagnostics(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<StackDiagnostics>, ServerError> {
        Ok(self.live.release_diagnostics(namespace, name).await)
    }

    async fn restart_stack_workspace(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<String, ServerError> {
        self.live
            .restart_stack_workspace(namespace, name)
            .await
            .map_err(|e| map_live_action_error(e, "Rollout restart failed"))
    }

    async fn reconcile_stack(&self, namespace: &str, name: &str) -> Result<String, ServerError> {
        self.live
            .reconcile_stack(namespace, name)
            .await
            .map_err(|e| map_live_action_error(e, "Stack reconcile failed"))
    }

    async fn force_delete_stack(&self, namespace: &str, name: &str) -> Result<String, ServerError> {
        self.live
            .force_delete_stack(namespace, name)
            .await
            .map_err(|e| map_live_action_error(e, "Force-delete stack failed"))
    }

    async fn unlock_stack(&self, namespace: &str, name: &str) -> Result<String, ServerError> {
        self.live
            .unlock_stack(namespace, name)
            .await
            .map_err(|e| map_live_action_error(e, "Unlock stack failed"))
    }

    async fn reconcile_release(&self, namespace: &str, name: &str) -> Result<String, ServerError> {
        self.live
            .reconcile_release(namespace, name)
            .await
            .map_err(|e| ServerError::Internal(format!("Flux reconcile failed: {e}")))
    }

    async fn stack_outputs(&self, namespace: &str, name: &str) -> HashMap<String, String> {
        self.live.stack_outputs(namespace, name).await
    }

    async fn management_addon_statuses(&self) -> HashMap<ManagementAddonKey, LiveResourceInfo> {
        let is_mgmt_ns = |ns: &str| {
            ns == keel_core::KEEL_SYSTEM_NAMESPACE || ns == keel_core::FLUX_SYSTEM_NAMESPACE
        };
        let helm = self
            .live
            .helm_release_status_details()
            .await
            .into_iter()
            .map(|(k, v)| (ManagementAddonKind::HelmRelease, k, v));
        let kustomizations = self
            .live
            .kustomization_status_details()
            .await
            .into_iter()
            .map(|(k, v)| (ManagementAddonKind::Kustomization, k, v));
        helm.chain(kustomizations)
            .filter(|(_, (ns, _), _)| is_mgmt_ns(ns))
            .map(|(kind, (ns, name), info)| ((kind, ns, name), info))
            .collect()
    }

    fn has_live_status(&self) -> bool {
        true
    }

    async fn execute_preview_gc(&self, plan: &GcPlan) -> Result<usize, ServerError> {
        let mut deleted = 0usize;
        for target in &plan.targets {
            if self
                .live
                .delete_dynamic_resource(
                    &target.api_version,
                    target.kind.kind_str(),
                    &target.namespace,
                    &target.name,
                )
                .await
                .map_err(|e| {
                    ServerError::Internal(format!(
                        "Failed to garbage-collect preview resource {}/{}: {e}",
                        target.namespace, target.name
                    ))
                })?
            {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    async fn apply_cluster_policy(
        &self,
        name: &str,
        policy_doc: &Value,
    ) -> Result<(), ServerError> {
        self.live
            .apply_cluster_policy(name, policy_doc)
            .await
            .map_err(|e| {
                ServerError::Internal(format!(
                    "Failed to apply Kyverno ClusterPolicy '{name}': {e}"
                ))
            })
    }

    async fn delete_tenant_stack(&self, namespace: &str, name: &str) -> Result<bool, ServerError> {
        self.live
            .delete_dynamic_resource("pulumi.com/v1", "Stack", namespace, name)
            .await
            .map_err(ServerError::Kube)
    }
}

/// Maps typed [`keel_kube::KubeError`]s from live Stack actions onto HTTP-facing [`ServerError`]s:
/// `NotFound` -> 404, `Conflict` (e.g. protected Stack) -> 409, `InvalidInput` -> 400, else 500.
pub(crate) fn map_live_action_error(err: keel_kube::KubeError, context: &str) -> ServerError {
    match err {
        keel_kube::KubeError::NotFound(msg) => ServerError::NotFound(msg),
        keel_kube::KubeError::Conflict(msg) => ServerError::Conflict(msg),
        keel_kube::KubeError::InvalidInput(msg) => ServerError::BadRequest(msg),
        other => ServerError::Internal(format!("{context}: {other}")),
    }
}

/// Cached ephemeral preview state for a PR (in-memory view of `keel-previews` CRs & diff report).
#[derive(Debug, Clone)]
pub struct CachedPreview {
    pub report: PreviewReport,
    pub preview_stack: PreviewStack,
    pub candidates: Vec<PreviewResourceCandidate>,
}

/// Stateless Axum application state shared across handlers (`SDD.md` §6.3, §7.3).
#[derive(Clone)]
pub struct AppState {
    pub forge: Arc<dyn ForgeProvider>,
    pub secret_store: Arc<dyn SecretStateStore>,
    pub telemetry: TelemetryMux,
    pub git_runner: GitRunner,
    pub repo_workdir: PathBuf,
    pub remote_url: String,
    pub auth_phase: Arc<RwLock<AuthPhase>>,
    pub oidc_config: Arc<RwLock<Option<OidcConfig>>>,
    pub oidc_ca_pem: Option<Vec<u8>>,
    pub state_provider: Arc<dyn ClusterStateProvider>,
    pub backend_url: String,
    pub s3_credentials_secret: Option<String>,
    /// Provisions per-tenant scoped SeaweedFS S3 identities (`None` without a live cluster).
    pub tenant_s3_provisioner: Option<Arc<dyn keel_kube::TenantS3IdentityProvisioner>>,
    pub docker_proxy_url: Option<String>,
    pub git_lock: Arc<Mutex<()>>,
    /// Optional read-through cache of preview reports; the cluster (`keel-previews` preview
    /// `Stack` labels/annotations + report `ConfigMap`) is the source of truth (OPEN-10).
    pub preview_cache: Arc<RwLock<HashMap<String, CachedPreview>>>,
    /// Live ephemeral PKO preview runner (`None` without a cluster ⇒ synthesized previews only).
    pub preview_cluster: Option<Arc<dyn keel_kube::PreviewCluster>>,
    /// Fail closed (`409`) on Pulumi Stack proposals whose tenant has no
    /// `keel-pulumi-passphrase` Secret. Defaults to `true`; only cluster-less local dev (in-memory
    /// Secret store, Stacks are never applied) turns it off.
    pub require_pulumi_passphrase: bool,
}

impl AppState {
    /// Constructs a new [`AppState`] with explicit dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        forge: Arc<dyn ForgeProvider>,
        secret_store: Arc<dyn SecretStateStore>,
        telemetry: TelemetryMux,
        git_runner: GitRunner,
        repo_workdir: impl Into<PathBuf>,
        remote_url: impl Into<String>,
        auth_phase: AuthPhase,
    ) -> Self {
        let repo_workdir = repo_workdir.into();
        let state_provider = Arc::new(RepoClusterStateProvider::new(&repo_workdir));
        Self {
            forge,
            secret_store,
            telemetry,
            git_runner,
            repo_workdir,
            remote_url: remote_url.into(),
            auth_phase: Arc::new(RwLock::new(auth_phase)),
            oidc_config: Arc::new(RwLock::new(None)),
            oidc_ca_pem: None,
            state_provider,
            backend_url: "s3://pulumi-state".to_string(),
            s3_credentials_secret: None,
            tenant_s3_provisioner: None,
            docker_proxy_url: None,
            git_lock: Arc::new(Mutex::new(())),
            preview_cache: Arc::new(RwLock::new(HashMap::new())),
            preview_cluster: None,
            require_pulumi_passphrase: true,
        }
    }

    /// Configures the live ephemeral PKO preview runner (`keel-previews`).
    pub fn with_preview_cluster(
        mut self,
        cluster: Option<Arc<dyn keel_kube::PreviewCluster>>,
    ) -> Self {
        self.preview_cluster = cluster;
        self
    }

    /// Enables/disables the tenant Pulumi state passphrase gate on Stack proposals.
    pub fn with_require_pulumi_passphrase(mut self, required: bool) -> Self {
        self.require_pulumi_passphrase = required;
        self
    }

    /// Configures an optional `DOCKER_HOST` proxy endpoint for local `kind`-in-`kind` PKO workspaces.
    pub fn with_docker_proxy_url(mut self, url: Option<String>) -> Self {
        self.docker_proxy_url = url;
        self
    }

    /// Configures an optional PEM-encoded root CA certificate for OIDC discovery/token/JWKS calls.
    pub fn with_oidc_ca_pem(mut self, pem: Option<Vec<u8>>) -> Self {
        self.oidc_ca_pem = pem;
        self
    }

    pub fn with_s3_credentials_secret(mut self, secret_name: Option<String>) -> Self {
        self.s3_credentials_secret = secret_name;
        self
    }

    /// Configures the per-tenant scoped S3 identity provisioner (`SDD.md` §6.3).
    pub fn with_tenant_s3_provisioner(
        mut self,
        provisioner: Option<Arc<dyn keel_kube::TenantS3IdentityProvisioner>>,
    ) -> Self {
        self.tenant_s3_provisioner = provisioner;
        self
    }

    /// Overrides the OIDC configuration used by `/api/v1/auth/login` and `/callback`.
    pub fn with_oidc_config(self, oidc: Option<OidcConfig>) -> Self {
        Self {
            oidc_config: Arc::new(RwLock::new(oidc)),
            ..self
        }
    }

    /// Overrides the `ClusterStateProvider` (e.g. with a live `kube-rs` CRD cache).
    pub fn with_state_provider(mut self, provider: Arc<dyn ClusterStateProvider>) -> Self {
        self.state_provider = provider;
        self
    }

    /// Creates a minimal default `AppState` for CLI smoke tests or unconfigured startup.
    pub fn default_ephemeral() -> Self {
        let tmp_dir = std::env::temp_dir().join("keel-server-default");
        let forge = Arc::new(LocalGitProvider::new(&tmp_dir));
        let secret_store = Arc::new(InMemorySecretStore::new());
        Self::new(
            forge,
            secret_store,
            TelemetryMux::default(),
            GitRunner::new(),
            &tmp_dir,
            tmp_dir.to_string_lossy().to_string(),
            AuthPhase::OidcActive,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockLiveStatus {
        stacks: HashMap<(String, String), LiveResourceStatus>,
        releases: HashMap<(String, String), LiveResourceStatus>,
    }

    #[async_trait]
    impl LiveStatusSource for MockLiveStatus {
        async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            self.stacks.clone()
        }

        async fn release_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            self.releases.clone()
        }
    }

    struct KindSplitLiveStatus;

    #[async_trait]
    impl LiveStatusSource for KindSplitLiveStatus {
        async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::new()
        }

        async fn release_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::new()
        }

        async fn helm_release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            HashMap::from([
                (
                    ("keel-system".to_string(), "kyverno".to_string()),
                    LiveResourceInfo::from_status(LiveResourceStatus::Ready),
                ),
                (
                    ("tenant-a".to_string(), "flagger".to_string()),
                    LiveResourceInfo::from_status(LiveResourceStatus::Ready),
                ),
            ])
        }

        async fn kustomization_status_details(
            &self,
        ) -> HashMap<(String, String), LiveResourceInfo> {
            HashMap::from([
                (
                    ("flux-system".to_string(), "rauthy".to_string()),
                    LiveResourceInfo::from_status(LiveResourceStatus::Reconciling),
                ),
                (
                    ("tenant-a".to_string(), "apps".to_string()),
                    LiveResourceInfo::from_status(LiveResourceStatus::Ready),
                ),
            ])
        }
    }

    fn write_core_addons(root: &Path) {
        let dir = root.join(keel_core::MANAGEMENT_CORE_ADDONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("kyverno.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: kyverno\n  namespace: keel-system\nspec:\n  chart:\n    spec:\n      chart: kyverno\n      version: \"3.4.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("rauthy.yaml"),
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: rauthy\n  namespace: flux-system\nspec:\n  path: ./x\n  images:\n    - name: ghcr.io/sebadob/rauthy\n      newTag: 0.37.2\n",
        )
        .unwrap();
        // Duplicate id across kinds -> conflict; root Kustomization -> never listed.
        std::fs::write(
            dir.join("extra.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: forgejo\n  namespace: flux-system\nspec:\n  chart:\n    spec:\n      chart: forgejo\n---\napiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: forgejo\n  namespace: flux-system\nspec:\n  path: ./y\n---\napiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: flux-system\n  namespace: flux-system\nspec:\n  path: ./clusters/management\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn repo_provider_lists_management_addons_as_pending_releases() {
        let tmp = tempfile::tempdir().unwrap();
        write_core_addons(tmp.path());
        let provider = RepoClusterStateProvider::new(tmp.path());
        let releases = provider.list_management_addon_releases().await.unwrap();
        let names: Vec<(&str, Option<ManagementAddonKind>)> = releases
            .iter()
            .map(|r| (r.name.as_str(), r.addon_kind))
            .collect();
        assert_eq!(releases.len(), 4, "{names:?}");
        assert!(releases.iter().all(|r| r.management_addon
            && r.protected
            && r.status == "Pending"
            && r.tenant.as_deref() == Some(MANAGEMENT_TENANT)));
        assert!(!releases.iter().any(|r| r.name == "flux-system"));
        let kyverno = releases.iter().find(|r| r.name == "kyverno").unwrap();
        assert_eq!(kyverno.chart, "kyverno");
        assert_eq!(kyverno.version.as_deref(), Some("3.4.0"));
        assert_eq!(kyverno.desired_version.as_deref(), Some("3.4.0"));
        assert_eq!(kyverno.runtime, "helm");
        assert!(!kyverno.conflict);
        assert!(
            releases
                .iter()
                .filter(|r| r.name == "forgejo")
                .all(|r| r.conflict)
        );
        // Tenant listing is unaffected by core add-ons.
        assert!(provider.list_releases(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn kube_provider_overlays_live_status_on_management_addon_releases() {
        let tmp = tempfile::tempdir().unwrap();
        write_core_addons(tmp.path());
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(KindSplitLiveStatus),
        );
        let releases = provider.list_management_addon_releases().await.unwrap();
        let status = |name: &str| {
            releases
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.status.clone())
                .unwrap()
        };
        assert_eq!(status("kyverno"), "Ready");
        assert_eq!(status("rauthy"), "Reconciling");
        assert_eq!(status("forgejo"), "Pending");
        // The `tenant-a/flagger` HelmRelease never masquerades as a management add-on.
        assert!(!releases.iter().any(|r| r.namespace == "tenant-a"));
    }

    #[tokio::test]
    async fn management_addon_statuses_are_kind_aware_and_management_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(KindSplitLiveStatus),
        );
        let live = provider.management_addon_statuses().await;
        assert_eq!(live.len(), 2);
        assert_eq!(
            live[&(
                ManagementAddonKind::HelmRelease,
                "keel-system".to_string(),
                "kyverno".to_string()
            )]
                .status,
            LiveResourceStatus::Ready
        );
        assert_eq!(
            live[&(
                ManagementAddonKind::Kustomization,
                "flux-system".to_string(),
                "rauthy".to_string()
            )]
                .status,
            LiveResourceStatus::Reconciling
        );
        assert!(!live.contains_key(&(
            ManagementAddonKind::HelmRelease,
            "flux-system".to_string(),
            "rauthy".to_string()
        )));
    }

    #[tokio::test]
    async fn kube_cluster_state_provider_overlays_live_statuses_and_falls_back_to_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant_infra = tmp.path().join("tenants/acme/infra");
        let tenant_apps = tmp.path().join("tenants/acme/apps");
        std::fs::create_dir_all(&tenant_infra).unwrap();
        std::fs::create_dir_all(&tenant_apps).unwrap();
        std::fs::write(
            tenant_infra.join("dev-kind.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: dev-kind\n  namespace: tenant-acme\nspec:\n  stack: acme/kind-cluster/dev-kind\n",
        )
        .unwrap();
        std::fs::write(
            tenant_apps.join("podinfo.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: podinfo\n  namespace: tenant-acme\nspec:\n  chart:\n    spec:\n      chart: podinfo\n",
        )
        .unwrap();

        let mut stacks = HashMap::new();
        stacks.insert(
            ("flux-system".to_string(), "core-addons".to_string()),
            LiveResourceStatus::Synced,
        );
        stacks.insert(
            ("tenant-acme".to_string(), "dev-kind".to_string()),
            LiveResourceStatus::Reconciling,
        );

        let mut releases = HashMap::new();
        releases.insert(
            ("tenant-acme".to_string(), "podinfo".to_string()),
            LiveResourceStatus::Ready,
        );
        // Simulate a Flux Kustomization wrapper for the `dev-kind` Stack appearing in release_statuses()
        releases.insert(
            ("tenant-acme".to_string(), "dev-kind".to_string()),
            LiveResourceStatus::Ready,
        );

        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(MockLiveStatus { stacks, releases }),
        );

        let out_stacks = provider.list_stacks(None, None).await.unwrap();
        assert_eq!(out_stacks[0].name, "management-cluster-core");
        assert_eq!(out_stacks[0].stack_type, "management");
        assert_eq!(out_stacks[0].status, "Synced");
        assert_eq!(out_stacks[1].name, "dev-kind");
        assert_eq!(out_stacks[1].stack_type, "workload");
        assert_eq!(out_stacks[1].status, "Reconciling");

        let out_releases = provider.list_releases(None).await.unwrap();
        assert_eq!(out_releases.len(), 1);
        assert_eq!(out_releases[0].name, "podinfo");
        assert_eq!(out_releases[0].status, "Ready");
    }

    #[tokio::test]
    async fn list_releases_deduplicates_stale_helmrelease_beside_canary() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle_item_dir = tmp.path().join("tenants/default/bundles/test/step-2");
        let standalone_app_dir = tmp.path().join("tenants/default/apps/podinfo");
        std::fs::create_dir_all(&bundle_item_dir).unwrap();
        std::fs::create_dir_all(&standalone_app_dir).unwrap();

        // Bundle item directory with stale helmrelease.yaml + active helmrelease-canary.yaml + canary.yaml
        std::fs::write(
            bundle_item_dir.join("helmrelease.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: test-step-2\n  namespace: tenant-default\nspec:\n  chart:\n    spec:\n      chart: podinfo\n",
        )
        .unwrap();
        std::fs::write(
            bundle_item_dir.join("helmrelease-canary.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: test-step-2\n  namespace: tenant-default\nspec:\n  chart:\n    spec:\n      chart: podinfo\n",
        )
        .unwrap();
        std::fs::write(
            bundle_item_dir.join("canary.yaml"),
            "apiVersion: flagger.app/v1beta1\nkind: Canary\nmetadata:\n  name: test-step-2\n  namespace: tenant-default\nspec:\n  targetRef:\n    name: test-step-2\n",
        )
        .unwrap();
        std::fs::write(
            bundle_item_dir.join("kustomization.yaml"),
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - helmrelease-canary.yaml\n  - canary.yaml\n",
        )
        .unwrap();

        // Standalone app directory without item-level kustomization.yaml, also containing both manifests
        std::fs::write(
            standalone_app_dir.join("helmrelease.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: podinfo\n  namespace: tenant-default\nspec:\n  chart:\n    spec:\n      chart: podinfo\n",
        )
        .unwrap();
        std::fs::write(
            standalone_app_dir.join("helmrelease-canary.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: podinfo\n  namespace: tenant-default\nspec:\n  chart:\n    spec:\n      chart: podinfo\n",
        )
        .unwrap();
        std::fs::write(
            standalone_app_dir.join("canary.yaml"),
            "apiVersion: flagger.app/v1beta1\nkind: Canary\nmetadata:\n  name: podinfo\n  namespace: tenant-default\nspec:\n  targetRef:\n    name: podinfo\n",
        )
        .unwrap();

        let repo_provider = RepoClusterStateProvider::new(tmp.path());
        let repo_releases = repo_provider.list_releases(None).await.unwrap();
        assert_eq!(repo_releases.len(), 2);
        assert!(repo_releases.iter().all(|r| r.canary));

        let kube_provider = KubeClusterStateProvider::new(
            repo_provider,
            Arc::new(MockLiveStatus {
                stacks: HashMap::new(),
                releases: HashMap::new(),
            }),
        );
        let kube_releases = kube_provider.list_releases(None).await.unwrap();
        assert_eq!(kube_releases.len(), 2);
        assert!(
            kube_releases
                .iter()
                .any(|r| r.name == "test-step-2" && r.canary)
        );
        assert!(
            kube_releases
                .iter()
                .any(|r| r.name == "podinfo" && r.canary)
        );
    }

    #[test]
    fn infers_stack_type_from_labels_and_providers() {
        assert_eq!(
            infer_stack_type("management-cluster-core", "flux-system", None, None),
            "management"
        );
        assert_eq!(
            infer_stack_type("eks-prod", "tenant-acme", Some("aws"), None),
            "cloud"
        );
        assert_eq!(
            infer_stack_type("pve-nodes", "tenant-acme", Some("proxmox"), None),
            "workload"
        );
        assert_eq!(
            infer_stack_type("custom", "tenant-acme", Some("aws"), Some("workload")),
            "workload"
        );
    }

    struct DetailLiveStatus {
        stacks: HashMap<(String, String), keel_kube::LiveResourceInfo>,
    }

    #[async_trait]
    impl LiveStatusSource for DetailLiveStatus {
        async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            self.stacks
                .iter()
                .map(|(k, v)| (k.clone(), v.status.clone()))
                .collect()
        }
        async fn release_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::new()
        }
        async fn stack_status_details(
            &self,
        ) -> HashMap<(String, String), keel_kube::LiveResourceInfo> {
            self.stacks.clone()
        }
    }

    fn live_info(
        status: LiveResourceStatus,
        provider: Option<&str>,
        terminating: bool,
        protected: bool,
    ) -> keel_kube::LiveResourceInfo {
        let mut info = keel_kube::LiveResourceInfo::from_status(status);
        info.provider = provider.map(str::to_owned);
        info.terminating = terminating;
        info.protected = protected;
        info.resource_count = Some(7);
        info
    }

    #[tokio::test]
    async fn protect_column_uses_canonical_label_and_spec_config() {
        let tmp = tempfile::tempdir().unwrap();
        let infra = tmp.path().join("tenants/acme/infra");
        std::fs::create_dir_all(&infra).unwrap();
        std::fs::write(
            infra.join("a.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: a\n  namespace: tenant-acme\n  labels:\n    keel.io/protect: \"true\"\nspec:\n  stack: a\n",
        )
        .unwrap();
        std::fs::write(
            infra.join("b.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: b\n  namespace: tenant-acme\nspec:\n  stack: b\n  config:\n    protect: \"true\"\n",
        )
        .unwrap();
        std::fs::write(
            infra.join("c.yaml"),
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: c\n  namespace: tenant-acme\n  labels:\n    keel.io/protected: \"true\"\nspec:\n  stack: c\n",
        )
        .unwrap();
        let stacks = RepoClusterStateProvider::new(tmp.path())
            .list_stacks(Some("acme"), None)
            .await
            .unwrap();
        let by = |n: &str| stacks.iter().find(|s| s.name == n).unwrap().protect;
        assert!(by("a"));
        assert!(by("b"));
        assert!(!by("c"));
    }

    #[tokio::test]
    async fn live_only_stacks_respect_provider_filter_and_report_orphaned_vs_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".keel")).unwrap();
        std::fs::write(
            tmp.path().join(".keel/config.yaml"),
            "apiVersion: keel.io/v1alpha1\nkind: KeelConfig\nbootstrapTarget: kind\n",
        )
        .unwrap();
        let mut stacks = HashMap::new();
        stacks.insert(
            ("tenant-acme".to_string(), "aws-vm".to_string()),
            live_info(LiveResourceStatus::Synced, Some("aws"), false, true),
        );
        stacks.insert(
            ("tenant-acme".to_string(), "pve-vm".to_string()),
            live_info(LiveResourceStatus::Deleting, Some("proxmox"), true, false),
        );
        stacks.insert(
            ("tenant-other".to_string(), "aws-other".to_string()),
            live_info(LiveResourceStatus::Synced, Some("aws"), false, false),
        );
        stacks.insert(
            (
                "flux-system".to_string(),
                "management-cluster-core".to_string(),
            ),
            live_info(LiveResourceStatus::Synced, None, false, true),
        );
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(DetailLiveStatus { stacks }),
        );

        let aws = provider.list_stacks(None, Some("aws")).await.unwrap();
        let names: Vec<&str> = aws.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"aws-vm") && names.contains(&"aws-other"));
        assert!(!names.contains(&"pve-vm"), "{names:?}");
        let aws_vm = aws.iter().find(|s| s.name == "aws-vm").unwrap();
        assert_eq!(aws_vm.status, STACK_STATUS_ORPHANED);
        assert!(aws_vm.protect);
        assert_eq!(aws_vm.provider.as_deref(), Some("aws"));

        let acme_aws = provider
            .list_stacks(Some("acme"), Some("aws"))
            .await
            .unwrap();
        assert_eq!(acme_aws.len(), 1);
        assert_eq!(acme_aws[0].name, "aws-vm");

        let all = provider.list_stacks(None, None).await.unwrap();
        let pve = all.iter().find(|s| s.name == "pve-vm").unwrap();
        assert_eq!(pve.status, "Deleting");
        let mgmt = all
            .iter()
            .find(|s| s.name == "management-cluster-core")
            .unwrap();
        assert_eq!(mgmt.tenant.as_deref(), Some(MANAGEMENT_TENANT));
        assert_eq!(mgmt.provider.as_deref(), Some("kind"));
        assert_eq!(mgmt.resource_count, Some(7));
        assert!(mgmt.protect);
        assert_eq!(mgmt.status, "Synced");
    }

    #[test]
    fn live_action_errors_map_to_http_semantics() {
        use keel_kube::KubeError;
        assert!(matches!(
            map_live_action_error(KubeError::NotFound("x".into()), "c"),
            ServerError::NotFound(_)
        ));
        assert!(matches!(
            map_live_action_error(KubeError::Conflict("x".into()), "c"),
            ServerError::Conflict(_)
        ));
        assert!(matches!(
            map_live_action_error(KubeError::InvalidInput("x".into()), "c"),
            ServerError::BadRequest(_)
        ));
        assert!(matches!(
            map_live_action_error(KubeError::Crypto("x".into()), "c"),
            ServerError::Internal(_)
        ));
    }
}
