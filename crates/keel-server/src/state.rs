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
use keel_git::{ForgeProvider, GitRunner};
use keel_kube::{
    LiveResourceInfo, LiveResourceStatus, LiveStatusSource, SecretStateStore, StackDiagnostics,
    TelemetryMux,
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
    /// Why the release is not Ready: its live Flux object's `Ready` condition (`Reason:
    /// message`), a failed canary's analysis message, or the tenant root Kustomization failure
    /// that keeps it from being applied (for a release with a live object: its latest Git
    /// change is not applied, its own status is kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
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

    /// [`Self::list_stacks`] for several tenants (a non-admin caller's tenants). Providers that
    /// read shared state override this to read it once instead of once per tenant.
    async fn list_stacks_for_tenants(
        &self,
        tenants: &[String],
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let mut out = Vec::new();
        for tenant in tenants {
            out.extend(self.list_stacks(Some(tenant), provider).await?);
        }
        Ok(out)
    }

    /// [`Self::list_releases`] for several tenants (see [`Self::list_stacks_for_tenants`]).
    async fn list_releases_for_tenants(
        &self,
        tenants: &[String],
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let mut out = Vec::new();
        for tenant in tenants {
            out.extend(self.list_releases(Some(tenant)).await?);
        }
        Ok(out)
    }

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

/// Committed YAML files of a repository subtree: repository-relative path (`/`-separated) →
/// contents.
pub type ManifestTree = std::collections::BTreeMap<String, String>;

/// Desired manifests of tenants that have their own repositories (every remote forge): the
/// committed `tenants/<id>/` YAML of each tenant repository's `main`. Implemented over
/// disposable read-only mirrors by [`crate::tenant_repos::TenantRepoManifests`].
#[async_trait]
pub trait TenantManifestSource: Send + Sync {
    /// The `tenants/<id>/` tree of each of `tenants` (repository-relative paths, only files under
    /// `tenants/<id>/`). A tenant whose repository cannot be read is served from its last
    /// snapshot, or omitted (logged) when there is none.
    async fn tenant_manifests(&self, tenants: &[String]) -> Vec<(String, Arc<ManifestTree>)>;
}

/// Default stateless [`ClusterStateProvider`] that scans `tenants/` and `clusters/` YAML
/// manifests inside `repo_workdir` and, when tenants have their own repositories, each tenant
/// repository's committed `tenants/<id>/` (`TenantManifestSource`).
///
/// Content read from tenant `<id>`'s own repository is confined to it: documents outside the
/// `tenant-<id>` namespace are ignored and the tenant is always `<id>` (never a `keel.io/tenant`
/// label), so one tenant's repository can never inject entries into another tenant's listings.
/// Platform-owned objects (`keel.io/platform-owned`, e.g. the `keel-tenant-root` stub
/// Kustomization) are never reported as tenant workloads.
#[derive(Clone)]
pub struct RepoClusterStateProvider {
    repo_workdir: PathBuf,
    tenant_source: Option<Arc<dyn TenantManifestSource>>,
}

impl std::fmt::Debug for RepoClusterStateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoClusterStateProvider")
            .field("repo_workdir", &self.repo_workdir)
            .field("tenant_source", &self.tenant_source.is_some())
            .finish()
    }
}

/// One desired-state tree: the platform working tree (`confine: None`) or tenant `<id>`'s own
/// repository (`confine: Some(id)`).
struct DesiredTree {
    confine: Option<String>,
    files: Arc<ManifestTree>,
}

/// `true` for an object the platform owns inside a tenant namespace (stub source/root
/// Kustomization, baseline RBAC); never a tenant workload.
fn is_platform_owned(val: &Value) -> bool {
    val.pointer(&format!(
        "/metadata/labels/{}",
        keel_kube::PLATFORM_OWNED_LABEL.replace('/', "~1")
    ))
    .is_some_and(|v| !v.is_null())
}

/// Repository-relative paths of `tree` in path-component order (`a/b` before `a-b/...`).
fn sorted_tree_paths(tree: &ManifestTree) -> Vec<&String> {
    let mut paths: Vec<&String> = tree.keys().collect();
    paths.sort_by(|a, b| a.split('/').cmp(b.split('/')));
    paths
}

/// Parent directory of a repository-relative path (`""` at the root).
fn rel_parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Applies tenant-repository confinement to a document: `None` (skip) when its namespace is not
/// `tenant-<id>` (an absent namespace is the root Kustomization's `targetNamespace`), else the
/// namespace and the forced tenant id.
fn confine_document(confine: &str, val: &Value) -> Option<(String, String)> {
    let expected = format!("tenant-{confine}");
    match val.pointer("/metadata/namespace").and_then(Value::as_str) {
        None => Some((expected, confine.to_string())),
        Some(ns) if ns == expected => Some((expected, confine.to_string())),
        Some(_) => None,
    }
}

/// Tenants a listing covers: `None` for every tenant, else only the listed ones.
type TenantScope<'a> = Option<&'a [String]>;

/// `true` when a document of `doc_tenant` belongs to `scope`.
fn in_scope(scope: TenantScope<'_>, doc_tenant: Option<&str>) -> bool {
    scope.is_none_or(|ids| doc_tenant.is_some_and(|t| ids.iter().any(|id| id == t)))
}

/// One-tenant [`TenantScope`] storage for the `tenant: Option<&str>` entry points.
fn single_scope(tenant: Option<&str>) -> Option<[String; 1]> {
    tenant.map(|t| [t.to_string()])
}

/// Resource kind of a Stack: the `keel.io/resource-kind` label, which proposals stamp from the
/// blueprint's `metadata.resourceKind`, else a name/provider heuristic for unlabelled Stacks.
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
            tenant_source: None,
        }
    }

    /// Also lists tenant workloads from each tenant's own repository through `source`.
    pub fn with_tenant_source(mut self, source: Arc<dyn TenantManifestSource>) -> Self {
        self.tenant_source = Some(source);
        self
    }

    /// Reads the Stage 0 `bootstrapTarget` (e.g. `kind`, `aws`) from `.keel/config.yaml`, used as
    /// the provider of the management control-plane stack.
    fn bootstrap_target(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.repo_workdir.join(".keel/config.yaml")).ok()?;
        let doc = noyalib::from_str::<Value>(&raw).ok()?;
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

    /// YAML files under `subdirs` of the platform working tree.
    async fn worktree_tree(&self, subdirs: &[&str]) -> ManifestTree {
        let mut files = Vec::new();
        for sub in subdirs {
            Self::collect_yaml_files(&self.repo_workdir.join(sub), &mut files);
        }
        let mut tree = ManifestTree::new();
        for path in files {
            let Some(rel) = path
                .strip_prefix(&self.repo_workdir)
                .ok()
                .and_then(|r| r.to_str())
                .map(|r| r.replace('\\', "/"))
            else {
                continue;
            };
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                tree.insert(rel, content);
            }
        }
        tree
    }

    /// Tenant ids with a directory under the platform `tenants/` (their stubs).
    fn platform_tenant_ids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.repo_workdir.join("tenants")) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|id| {
                id != MANAGEMENT_TENANT && keel_core::validate_dns_label(id, "tenant").is_ok()
            })
            .collect();
        ids.sort();
        ids
    }

    /// The platform working tree (`subdirs`) followed by the tenant repositories (all tenants,
    /// or only those in `scope`) when tenants have their own repositories. With tenant
    /// repositories the platform `tenants/` holds only platform-owned stubs, so it is never read
    /// for tenant workloads (a stray document there is not reconciled by Flux).
    async fn desired_trees(&self, scope: TenantScope<'_>, subdirs: &[&str]) -> Vec<DesiredTree> {
        let platform_subdirs: Vec<&str> = subdirs
            .iter()
            .copied()
            .filter(|sub| self.tenant_source.is_none() || *sub != "tenants")
            .collect();
        let mut trees = vec![DesiredTree {
            confine: None,
            files: Arc::new(self.worktree_tree(&platform_subdirs).await),
        }];
        if let Some(source) = &self.tenant_source {
            let mut ids = self.platform_tenant_ids();
            if let Some(scope) = scope {
                ids.retain(|id| scope.contains(id));
            }
            if !ids.is_empty() {
                for (id, files) in source.tenant_manifests(&ids).await {
                    trees.push(DesiredTree {
                        confine: Some(id),
                        files,
                    });
                }
            }
        }
        trees
    }

    fn infer_tenant_from_path(rel: &str) -> Option<String> {
        let mut comps = rel.split('/');
        (comps.next()? == "tenants")
            .then(|| comps.next())
            .flatten()
            .map(str::to_owned)
    }

    /// `(bundle, item)` of a `tenants/<id>/bundles/<bundle>/<item>/...` path.
    fn infer_bundle_from_path(rel: &str) -> (Option<String>, Option<String>) {
        let comps: Vec<&str> = rel.split('/').collect();
        if comps.len() < 4 || comps[0] != "tenants" || comps[2] != "bundles" {
            return (None, None);
        }
        (
            Some(comps[3].to_string()),
            comps.get(4).map(|c| (*c).to_string()),
        )
    }

    fn stacks_from_tree(
        &self,
        tree: &DesiredTree,
        scope: TenantScope<'_>,
        provider: Option<&str>,
        out: &mut Vec<StackSummary>,
    ) {
        for rel in sorted_tree_paths(&tree.files) {
            let content = &tree.files[rel];
            for doc in split_yaml_documents(content) {
                let Ok(val) = noyalib::from_str::<Value>(&doc) else {
                    continue;
                };
                if val.get("kind").and_then(Value::as_str) != Some("Stack")
                    || is_platform_owned(&val)
                {
                    continue;
                }
                let confined = match tree.confine.as_deref() {
                    Some(id) => match confine_document(id, &val) {
                        Some(c) => Some(c),
                        None => continue,
                    },
                    None => None,
                };
                let name = val
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let namespace = match &confined {
                    Some((ns, _)) => ns.clone(),
                    None => val
                        .pointer("/metadata/namespace")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_string(),
                };
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
                let doc_tenant = match &confined {
                    Some((_, id)) => Some(id.clone()),
                    None => val
                        .pointer("/metadata/labels/keel.io~1tenant")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| Self::infer_tenant_from_path(rel)),
                };
                let doc_provider = val
                    .pointer("/metadata/labels/keel.io~1provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let (path_bundle, path_item) = Self::infer_bundle_from_path(rel);
                let doc_bundle = val
                    .pointer("/metadata/labels/keel.io~1bundle")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(path_bundle);
                let doc_bundle_item = val
                    .pointer("/metadata/labels/keel.io~1bundle-item")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(path_item);

                if !in_scope(scope, doc_tenant.as_deref()) {
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
                // A tenant repository never declares the management control plane, whatever its
                // `keel.io/type` label says.
                if tree.confine.is_some()
                    && (stack_type == "management" || name == "management-cluster-core")
                {
                    continue;
                }

                let catalog_id = val
                    .pointer("/metadata/labels/keel.io~1catalog-id")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let is_manifest_runtime = val.pointer("/spec/manifests").is_some();
                let has_kubeconfig_secret =
                    val.pointer("/spec/config/kubeconfigSecretName").is_some()
                        && doc_provider.as_deref() != Some("kubernetes")
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

                out.push(StackSummary {
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
    }

    fn releases_from_tree(
        tree: &DesiredTree,
        scope: TenantScope<'_>,
        out: &mut Vec<ReleaseSummary>,
    ) {
        let files = &tree.files;
        // Cache item-level kustomization.yaml resource allowlists so stale unreferenced
        // manifests (e.g. a leftover helmrelease.yaml beside helmrelease-canary.yaml) are ignored.
        let mut kustomize_allowlists: HashMap<String, Option<std::collections::HashSet<String>>> =
            HashMap::new();

        for rel in sorted_tree_paths(files) {
            if !rel.starts_with("tenants/") {
                continue;
            }
            let parent = rel_parent(rel);
            let fname = rel.rsplit('/').next().unwrap_or("");
            let has_manifests_yaml = files.contains_key(&format!("{parent}/manifests.yaml"));
            let has_stack_yaml = files.contains_key(&format!("{parent}/stack.yaml"));
            if fname == "flux-kustomization.yaml" && (!has_manifests_yaml || has_stack_yaml) {
                continue;
            }
            if parent != "tenants" && fname != "flux-kustomization.yaml" {
                let allowed = kustomize_allowlists
                    .entry(parent.to_string())
                    .or_insert_with(|| {
                        let raw = files.get(&format!("{parent}/kustomization.yaml"))?;
                        let doc = noyalib::from_str::<Value>(raw).ok()?;
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
            let content = &files[rel];
            for doc in split_yaml_documents(content) {
                let Ok(val) = noyalib::from_str::<Value>(&doc) else {
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
                if !is_flux_release || is_platform_owned(&val) {
                    continue;
                }
                // `<instance>-config` Kustomizations (SOPS secrets of an app) are part of the
                // app they configure, not releases of their own. Only that exact shape counts,
                // as for live objects (`live_config_parent`).
                if kind == "Kustomization"
                    && let Some(parent) = val
                        .pointer("/metadata/labels/keel.io~1config-for")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                    && val.pointer("/metadata/name").and_then(Value::as_str)
                        == Some(keel_kube::manifest_app_config_kustomization_name(parent).as_str())
                {
                    continue;
                }
                let confined = match tree.confine.as_deref() {
                    Some(id) => match confine_document(id, &val) {
                        Some(c) => Some(c),
                        None => continue,
                    },
                    None => None,
                };
                let name = val
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let namespace = match &confined {
                    Some((ns, _)) => ns.clone(),
                    None => val
                        .pointer("/metadata/namespace")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_string(),
                };
                let doc_tenant = match &confined {
                    Some((_, id)) => Some(id.clone()),
                    None => val
                        .pointer("/metadata/labels/keel.io~1tenant")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| Self::infer_tenant_from_path(rel)),
                };

                if !in_scope(scope, doc_tenant.as_deref()) {
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
                let (path_bundle, path_item) = Self::infer_bundle_from_path(rel);
                let doc_bundle = val
                    .pointer("/metadata/labels/keel.io~1bundle")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(path_bundle);
                let doc_bundle_item = val
                    .pointer("/metadata/labels/keel.io~1bundle-item")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(path_item);

                out.push(ReleaseSummary {
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
    }
}

impl RepoClusterStateProvider {
    async fn stacks_in(
        &self,
        scope: TenantScope<'_>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let mut stacks = Vec::new();
        for tree in self.desired_trees(scope, &["tenants", "clusters"]).await {
            self.stacks_from_tree(&tree, scope, provider, &mut stacks);
        }

        if scope.is_none()
            && provider.is_none()
            && !stacks
                .iter()
                .any(|s| s.namespace == "flux-system" && s.name == "management-cluster-core")
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

    async fn releases_in(
        &self,
        scope: TenantScope<'_>,
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let mut releases = Vec::new();
        for tree in self.desired_trees(scope, &["tenants"]).await {
            Self::releases_from_tree(&tree, scope, &mut releases);
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
}

#[async_trait]
impl ClusterStateProvider for RepoClusterStateProvider {
    async fn list_stacks(
        &self,
        tenant: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let one = single_scope(tenant);
        self.stacks_in(one.as_ref().map(|a| a.as_slice()), provider)
            .await
    }

    async fn list_releases(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let one = single_scope(tenant);
        self.releases_in(one.as_ref().map(|a| a.as_slice())).await
    }

    async fn list_stacks_for_tenants(
        &self,
        tenants: &[String],
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        self.stacks_in(Some(tenants), provider).await
    }

    async fn list_releases_for_tenants(
        &self,
        tenants: &[String],
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        self.releases_in(Some(tenants)).await
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

    /// Why a release's `<name>-config` Kustomization (labelled `keel.io/config-for: <name>`)
    /// keeps its configuration (SOPS secrets / values) from being applied, if it failed.
    fn config_failure_note(
        live: &HashMap<(String, String), LiveResourceInfo>,
        rel: &ReleaseSummary,
    ) -> Option<String> {
        let config_name = format!("{}-config", rel.name);
        let info = live.get(&(rel.namespace.clone(), config_name.clone()))?;
        if info.config_for.as_deref() != Some(rel.name.as_str()) {
            return None;
        }
        if info.status != LiveResourceStatus::Error {
            return None;
        }
        let what = format!(
            "Latest configuration not applied: Kustomization {}/{config_name} failed",
            rel.namespace
        );
        Some(match &info.message {
            Some(m) => format!("{what}: {m}"),
            None => what,
        })
    }

    /// Parent app of a live `<instance>-config` Kustomization: only a `Kustomization` named
    /// `<config-for>-config` (the shape Keel generates) counts, so any other object carrying a
    /// `keel.io/config-for` label is still listed like any other release.
    fn live_config_parent<'a>(name: &str, info: &'a LiveResourceInfo) -> Option<&'a str> {
        let parent = info.config_for.as_deref()?;
        (info.kind.as_deref() == Some("Kustomization")
            && name == keel_kube::manifest_app_config_kustomization_name(parent))
        .then_some(parent)
    }

    /// Row for a live tenant object no longer declared in Git `main`: `Deleting` with a
    /// `deletionTimestamp`, else `Orphaned` (awaiting Flux prune). The runtime is the object's
    /// `keel.io/runtime` label (what its Git row reported), else derived from its kind.
    fn live_only_release(ns: &str, name: &str, info: &LiveResourceInfo) -> ReleaseSummary {
        let runtime = info.runtime.clone().unwrap_or_else(|| {
            match info.kind.as_deref() {
                Some("Kustomization") => "kustomize",
                _ => "helm",
            }
            .to_string()
        });
        ReleaseSummary {
            name: name.to_string(),
            namespace: ns.to_string(),
            tenant: ns.strip_prefix("tenant-").map(str::to_owned),
            chart: name.to_string(),
            version: None,
            runtime,
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
            status: if info.terminating {
                "Deleting".to_string()
            } else {
                STACK_STATUS_ORPHANED.to_string()
            },
            status_message: info.message.clone(),
            ..ReleaseSummary::default()
        }
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

impl KubeClusterStateProvider {
    async fn stacks_in(
        &self,
        scope: TenantScope<'_>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let mut stacks = self.repo.stacks_in(scope, provider).await?;
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
            // The namespace decides the tenant: a `keel.io/tenant` label on a live object is
            // tenant-controlled and must never list it under another tenant.
            let inferred_tenant = ns.strip_prefix("tenant-");
            if !in_scope(scope, inferred_tenant) {
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

    async fn releases_in(
        &self,
        scope: TenantScope<'_>,
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let mut releases = self.repo.releases_in(scope).await?;
        let known_stacks = self.repo.stacks_in(scope, None).await.unwrap_or_default();

        let live_stacks = self.live.stack_statuses().await;
        let live = self.live.release_status_details().await;
        let live_canaries = self.live.canary_statuses().await;
        for rel in &mut releases {
            // The tenant root Kustomization applies every app of the tenant repository: while it
            // fails, the release's latest Git change is not applied (a new release stays
            // Pending, an applied one keeps its previous live object and status).
            let root_failure = rel
                .namespace
                .starts_with("tenant-")
                .then(|| {
                    live.get(&(
                        rel.namespace.clone(),
                        keel_kube::TENANT_ROOT_KUSTOMIZATION_NAME.to_string(),
                    ))
                })
                .flatten()
                .and_then(|root| root.message.as_ref().map(|m| (root.status.clone(), m)));
            if let Some(info) = live.get(&(rel.namespace.clone(), rel.name.clone())) {
                rel.status = Self::status_label(&info.status);
                rel.last_synced = info.last_synced.clone();
                rel.status_message = info.message.clone();
                if let Some((LiveResourceStatus::Error, m)) = &root_failure {
                    let note = format!(
                        "Latest Git changes not applied: Kustomization {}/{} failed: {m}",
                        rel.namespace,
                        keel_kube::TENANT_ROOT_KUSTOMIZATION_NAME
                    );
                    rel.status_message = Some(match rel.status_message.take() {
                        Some(own) => format!("{own}; {note}"),
                        None => note,
                    });
                }
            } else if let Some((root_status, m)) = &root_failure {
                // Not applied yet and the root Kustomization is not ready: that is why it stays
                // Pending (or fails).
                if *root_status == LiveResourceStatus::Error {
                    rel.status = Self::status_label(root_status);
                }
                rel.status_message = Some(format!(
                    "Not applied: Kustomization {}/{} is not ready: {m}",
                    rel.namespace,
                    keel_kube::TENANT_ROOT_KUSTOMIZATION_NAME
                ));
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
                if cinfo.message.is_some() {
                    rel.status_message = cinfo.message.clone();
                }
            }
            // An app's `<instance>-config` Kustomization (its SOPS secrets / values) is folded
            // into the app row: when it fails the app's latest configuration is not applied,
            // so say why on the parent row instead of listing it separately.
            if let Some(note) = Self::config_failure_note(&live, rel) {
                rel.status_message = Some(match rel.status_message.take() {
                    Some(own) => format!("{own}; {note}"),
                    None => note,
                });
            }
        }
        // Keep any live tenant-* release that is absent from Git `main` (awaiting Flux prune or mid-termination),
        // but ignore Flux `Kustomization` wrappers that back PKO `Stack`s (e.g. bundle infra steps).
        let mut config_objects = Vec::new();
        for ((ns, name), info) in &live {
            if !ns.starts_with("tenant-") {
                continue;
            }
            let inferred_tenant = ns.strip_prefix("tenant-");
            if !in_scope(scope, inferred_tenant) {
                continue;
            }
            // The platform-owned root Kustomization of a tenant repository is not a release.
            if name == keel_kube::TENANT_ROOT_KUSTOMIZATION_NAME {
                continue;
            }
            // An app's `<instance>-config` Kustomization belongs to its parent app row; decided
            // once every parent row is known.
            if let Some(parent) = Self::live_config_parent(name, info) {
                config_objects.push((ns, name, info, parent));
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
                let mut rel = Self::live_only_release(ns, name, info);
                if let Some(note) = Self::config_failure_note(&live, &rel) {
                    rel.status_message = Some(match rel.status_message.take() {
                        Some(own) => format!("{own}; {note}"),
                        None => note,
                    });
                }
                releases.push(rel);
            }
        }
        // A config object whose parent app has no row (removed from Git and already pruned) is
        // listed on its own so a stuck one (finalizer, failed prune) stays visible.
        for (ns, name, info, parent) in config_objects {
            if !releases
                .iter()
                .any(|r| r.namespace == *ns && (r.name == parent || r.name == *name))
            {
                releases.push(Self::live_only_release(ns, name, info));
            }
        }
        Ok(releases)
    }
}

#[async_trait]
impl ClusterStateProvider for KubeClusterStateProvider {
    async fn list_stacks(
        &self,
        tenant: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        let one = single_scope(tenant);
        self.stacks_in(one.as_ref().map(|a| a.as_slice()), provider)
            .await
    }

    async fn list_releases(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        let one = single_scope(tenant);
        self.releases_in(one.as_ref().map(|a| a.as_slice())).await
    }

    async fn list_stacks_for_tenants(
        &self,
        tenants: &[String],
        provider: Option<&str>,
    ) -> Result<Vec<StackSummary>, ServerError> {
        self.stacks_in(Some(tenants), provider).await
    }

    async fn list_releases_for_tenants(
        &self,
        tenants: &[String],
    ) -> Result<Vec<ReleaseSummary>, ServerError> {
        self.releases_in(Some(tenants)).await
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
    /// Confidential OIDC client secret (`KEEL_OIDC_CLIENT_SECRET`), sent only in the token
    /// exchange (`client_secret_post`). Never serialized, logged or returned; `None` for a public
    /// PKCE client.
    pub oidc_client_secret: Option<Arc<str>>,
    pub state_provider: Arc<dyn ClusterStateProvider>,
    pub backend_url: String,
    pub s3_credentials_secret: Option<String>,
    /// Provisions per-tenant scoped SeaweedFS S3 identities (`None` without a live cluster).
    pub tenant_s3_provisioner: Option<Arc<dyn keel_kube::TenantS3IdentityProvisioner>>,
    pub docker_proxy_url: Option<String>,
    /// Forge HTTPS origin (`.keel/config.yaml` `forge.publicUrl`, e.g.
    /// `https://forgejo.keel.local:8888`). Tenant and preview PKO Stacks clone the in-cluster
    /// Forgejo through it because PKO rejects the `http://` Service URL Flux uses. Platform
    /// configuration only; `None` makes such Stacks fail closed with an actionable `409`.
    pub forge_public_url: Option<String>,
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
    /// Per-tenant repository registry (layout, naming convention, clone cache, per-repository
    /// locks). Defaults to the monorepo layout of the development-only Local Git provider.
    pub tenant_repos: crate::tenant_repos::TenantRepos,
    /// Disposable TTL cache of per-repository PR listings (with forge rate-limit back-off).
    pub proposal_cache: Arc<crate::proposal_repo::ProposalListCache>,
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
        let tenant_repos = crate::tenant_repos::TenantRepos::monorepo(
            crate::tenant_repos::TenantRepos::default_cache_root(&repo_workdir),
        );
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
            oidc_client_secret: None,
            state_provider,
            backend_url: "s3://pulumi-state".to_string(),
            s3_credentials_secret: None,
            tenant_s3_provisioner: None,
            docker_proxy_url: None,
            forge_public_url: None,
            git_lock: Arc::new(Mutex::new(())),
            preview_cache: Arc::new(RwLock::new(HashMap::new())),
            preview_cluster: None,
            require_pulumi_passphrase: true,
            tenant_repos,
            proposal_cache: Arc::default(),
        }
    }

    /// Configures the per-tenant repository registry (dedicated repositories for remote forges).
    pub fn with_tenant_repos(mut self, repos: crate::tenant_repos::TenantRepos) -> Self {
        self.tenant_repos = repos;
        self
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

    /// Configures the forge HTTPS origin PKO Stacks clone the in-cluster Forgejo through
    /// (`forge.publicUrl`). Blank values are ignored; a trailing `/` is dropped.
    pub fn with_forge_public_url(mut self, url: Option<String>) -> Self {
        self.forge_public_url = url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());
        self
    }

    /// Configures an optional PEM-encoded root CA certificate for OIDC discovery/token/JWKS calls.
    pub fn with_oidc_ca_pem(mut self, pem: Option<Vec<u8>>) -> Self {
        self.oidc_ca_pem = pem;
        self
    }

    /// Configures the confidential OIDC client secret (blank values mean a public PKCE client).
    pub fn with_oidc_client_secret(mut self, secret: Option<String>) -> Self {
        self.oidc_client_secret = secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(Arc::from);
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

    /// Creates a minimal default `AppState` for unit tests.
    #[cfg(test)]
    pub(crate) fn default_ephemeral() -> Self {
        let tmp_dir = std::env::temp_dir().join("keel-server-default");
        let forge = Arc::new(keel_git::LocalGitProvider::new(&tmp_dir));
        let secret_store = Arc::new(keel_kube::InMemorySecretStore::new());
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

        async fn stack_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            infos(&self.stacks)
        }

        async fn release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            infos(&self.releases)
        }

        async fn helm_release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            infos(&self.releases)
        }

        async fn kustomization_status_details(
            &self,
        ) -> HashMap<(String, String), LiveResourceInfo> {
            HashMap::new()
        }
    }

    fn infos(
        statuses: &HashMap<(String, String), LiveResourceStatus>,
    ) -> HashMap<(String, String), LiveResourceInfo> {
        statuses
            .iter()
            .map(|(k, status)| (k.clone(), LiveResourceInfo::from_status(status.clone())))
            .collect()
    }

    struct KindSplitLiveStatus;

    #[async_trait]
    impl LiveStatusSource for KindSplitLiveStatus {
        async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::new()
        }

        async fn stack_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            HashMap::new()
        }

        async fn release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
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
        // Simulate a Flux Kustomization wrapper for the `dev-kind` Stack appearing in the release statuses
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

    /// A Helm app with secret fields commits a `<instance>-config` Flux Kustomization (labelled
    /// `keel.io/config-for`) beside its HelmRelease: it must not show up as a second release.
    #[tokio::test]
    async fn list_releases_skips_helm_app_config_kustomization() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tenants/acme/apps/git");
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("helmrelease.yaml"),
            "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: git\n  namespace: tenant-acme\n  labels:\n    keel.io/tenant: acme\nspec:\n  chart:\n    spec:\n      chart: forgejo\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("config-kustomization.yaml"),
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: git-config\n  namespace: tenant-acme\n  labels:\n    keel.io/tenant: acme\n    keel.io/catalog-id: forgejo\n    keel.io/config-for: git\nspec:\n  path: ./tenants/acme/apps/git/config\n",
        )
        .unwrap();
        let releases = RepoClusterStateProvider::new(tmp.path())
            .list_releases(Some("acme"))
            .await
            .unwrap();
        assert_eq!(
            releases.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["git"]
        );
        assert_eq!(releases[0].runtime, "helm");
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

    struct ReleaseDetailLive(HashMap<(String, String), LiveResourceInfo>);

    #[async_trait]
    impl LiveStatusSource for ReleaseDetailLive {
        async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::new()
        }
        async fn stack_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            HashMap::new()
        }
        async fn release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            self.0.clone()
        }
        async fn helm_release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
            self.0.clone()
        }
        async fn kustomization_status_details(
            &self,
        ) -> HashMap<(String, String), LiveResourceInfo> {
            HashMap::new()
        }
    }

    /// Snag #5 (fresh-user dry run): a release reports why it is not Ready, including the
    /// tenant root Kustomization failure that keeps it from being applied at all.
    #[tokio::test]
    async fn list_releases_surfaces_not_ready_reasons() {
        let tmp = tempfile::tempdir().unwrap();
        for (tenant, name) in [
            ("acme", "web"),
            ("acme", "api"),
            ("acme", "live"),
            ("beta", "db"),
            ("beta", "cache"),
            ("gamma", "ui"),
        ] {
            let dir = tmp.path().join(format!("tenants/{tenant}/apps/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("helmrelease.yaml"),
                format!(
                    "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: {name}\n  namespace: tenant-{tenant}\nspec:\n  chart:\n    spec:\n      chart: {name}\n"
                ),
            )
            .unwrap();
        }
        let info = |status: LiveResourceStatus, message: Option<&str>| LiveResourceInfo {
            message: message.map(str::to_owned),
            ..LiveResourceInfo::from_status(status)
        };
        let root = keel_kube::TENANT_ROOT_KUSTOMIZATION_NAME.to_string();
        let live = HashMap::from([
            (
                ("tenant-acme".to_string(), "web".to_string()),
                info(
                    LiveResourceStatus::Error,
                    Some("InstallFailed: admission webhook denied the request"),
                ),
            ),
            (
                ("tenant-acme".to_string(), root.clone()),
                info(
                    LiveResourceStatus::Error,
                    Some("ReconciliationFailed: canaries.flagger.app \"web\" is forbidden"),
                ),
            ),
            (
                ("tenant-acme".to_string(), "live".to_string()),
                info(LiveResourceStatus::Ready, None),
            ),
            (
                ("tenant-beta".to_string(), "cache".to_string()),
                info(LiveResourceStatus::Ready, None),
            ),
            (
                ("tenant-beta".to_string(), root.clone()),
                info(
                    LiveResourceStatus::Reconciling,
                    Some("Progressing: reconciliation in progress"),
                ),
            ),
            (
                ("tenant-gamma".to_string(), root.clone()),
                info(LiveResourceStatus::Ready, None),
            ),
        ]);
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(ReleaseDetailLive(live)),
        );
        let releases = provider.list_releases(None).await.unwrap();
        let by_name = |n: &str| releases.iter().find(|r| r.name == n).unwrap();
        assert_eq!(releases.len(), 6, "the root Kustomization is not a release");

        let root_failed = "Latest Git changes not applied: Kustomization \
                           tenant-acme/keel-tenant-root failed: ReconciliationFailed: \
                           canaries.flagger.app \"web\" is forbidden";
        let web = by_name("web");
        assert_eq!(web.status, "Error");
        assert_eq!(
            web.status_message.as_deref(),
            Some(
                format!("InstallFailed: admission webhook denied the request; {root_failed}")
                    .as_str()
            )
        );
        // An applied, Ready release keeps its status but says its latest change is not applied.
        let live_rel = by_name("live");
        assert_eq!(live_rel.status, "Ready");
        assert_eq!(live_rel.status_message.as_deref(), Some(root_failed));
        // A root Kustomization that is only progressing says nothing about applied releases.
        let cache = by_name("cache");
        assert_eq!(cache.status, "Ready");
        assert_eq!(cache.status_message, None);
        let api = by_name("api");
        assert_eq!(api.status, "Error");
        assert_eq!(
            api.status_message.as_deref(),
            Some(
                "Not applied: Kustomization tenant-acme/keel-tenant-root is not ready: \
                 ReconciliationFailed: canaries.flagger.app \"web\" is forbidden"
            )
        );
        let db = by_name("db");
        assert_eq!(db.status, "Pending");
        assert!(
            db.status_message
                .as_deref()
                .unwrap()
                .ends_with("Progressing: reconciliation in progress")
        );
        let ui = by_name("ui");
        assert_eq!(ui.status, "Pending");
        assert_eq!(ui.status_message, None);
        let json = serde_json::to_value(by_name("api")).unwrap();
        assert_eq!(
            json["statusMessage"],
            api.status_message.as_deref().unwrap()
        );
        assert!(
            serde_json::to_value(ui)
                .unwrap()
                .get("statusMessage")
                .is_none()
        );
    }

    /// FU-5 (fresh-user dry run): an app's live `<instance>-config` Kustomization is folded into
    /// the app row (from Git or live-only) when that row exists and listed on its own otherwise;
    /// live-only releases report their `keel.io/runtime` (else kind-derived runtime) and
    /// `Orphaned` unless they carry a `deletionTimestamp`.
    #[tokio::test]
    async fn list_releases_folds_config_kustomizations_and_reports_orphans_honestly() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tenants/acme/apps/echo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("flux.yaml"),
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: echo\n  namespace: tenant-acme\n  labels:\n    keel.io/runtime: manifest\nspec:\n  path: ./apps/echo/manifests\n---\napiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: echo-config\n  namespace: tenant-acme\n  labels:\n    keel.io/config-for: echo\nspec:\n  path: ./apps/echo/config\n",
        )
        .unwrap();
        let mut ready_config = LiveResourceInfo::from_status(LiveResourceStatus::Ready);
        ready_config.kind = Some("Kustomization".to_string());
        ready_config.config_for = Some("echo".to_string());
        let mut failed_config = LiveResourceInfo::from_status(LiveResourceStatus::Error);
        failed_config.kind = Some("Kustomization".to_string());
        failed_config.config_for = Some("web".to_string());
        failed_config.message = Some("BuildFailed: secrets.enc.yaml: sops decrypt".to_string());
        let mut orphan_kustomization = LiveResourceInfo::from_status(LiveResourceStatus::Ready);
        orphan_kustomization.kind = Some("Kustomization".to_string());
        // A removed manifest app keeps the runtime its Git row reported.
        let mut orphan_manifest = LiveResourceInfo::from_status(LiveResourceStatus::Ready);
        orphan_manifest.kind = Some("Kustomization".to_string());
        orphan_manifest.runtime = Some("manifest".to_string());
        // Only a Kustomization named `<config-for>-config` is folded: any other labelled object
        // is listed like every other orphan.
        let mut labelled_helm = LiveResourceInfo::from_status(LiveResourceStatus::Ready);
        labelled_helm.kind = Some("HelmRelease".to_string());
        labelled_helm.config_for = Some("echo".to_string());
        let mut misnamed_config = LiveResourceInfo::from_status(LiveResourceStatus::Ready);
        misnamed_config.kind = Some("Kustomization".to_string());
        misnamed_config.config_for = Some("echo".to_string());
        // A live-only app whose config failed: the config is folded into the orphaned parent.
        let mut api_config = failed_config.clone();
        api_config.config_for = Some("api".to_string());
        let mut deleting_helm = LiveResourceInfo::from_status(LiveResourceStatus::Deleting);
        deleting_helm.kind = Some("HelmRelease".to_string());
        let live = HashMap::from([
            (
                ("tenant-acme".to_string(), "echo".to_string()),
                LiveResourceInfo::from_status(LiveResourceStatus::Ready),
            ),
            (
                ("tenant-acme".to_string(), "echo-config".to_string()),
                ready_config,
            ),
            // A failed config whose parent app is gone is listed on its own (FU-5).
            (
                ("tenant-acme".to_string(), "web-config".to_string()),
                failed_config.clone(),
            ),
            (
                ("tenant-acme".to_string(), "api".to_string()),
                orphan_manifest.clone(),
            ),
            (
                ("tenant-acme".to_string(), "api-config".to_string()),
                api_config,
            ),
            (
                ("tenant-acme".to_string(), "landing".to_string()),
                orphan_manifest,
            ),
            (
                ("tenant-acme".to_string(), "sneaky".to_string()),
                labelled_helm,
            ),
            (
                ("tenant-acme".to_string(), "sneaky-config".to_string()),
                misnamed_config,
            ),
            (
                ("tenant-acme".to_string(), "old".to_string()),
                orphan_kustomization,
            ),
            (
                ("tenant-acme".to_string(), "gone".to_string()),
                deleting_helm,
            ),
        ]);
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(ReleaseDetailLive(live.clone())),
        );
        let releases = provider.list_releases(None).await.unwrap();
        let names: Vec<&str> = releases.iter().map(|r| r.name.as_str()).collect();
        assert!(!names.contains(&"echo-config"), "{names:?}");
        assert!(!names.contains(&"api-config"), "{names:?}");
        let by_name = |n: &str| releases.iter().find(|r| r.name == n).unwrap();
        let web_config = by_name("web-config");
        assert_eq!(web_config.runtime, "kustomize");
        assert_eq!(web_config.status, STACK_STATUS_ORPHANED);
        assert_eq!(
            web_config.status_message.as_deref(),
            Some("BuildFailed: secrets.enc.yaml: sops decrypt")
        );
        let api = by_name("api");
        assert_eq!(api.runtime, "manifest");
        assert_eq!(api.status, STACK_STATUS_ORPHANED);
        assert_eq!(
            api.status_message.as_deref(),
            Some(
                "Latest configuration not applied: Kustomization tenant-acme/api-config \
                 failed: BuildFailed: secrets.enc.yaml: sops decrypt"
            )
        );
        let landing = by_name("landing");
        assert_eq!(landing.runtime, "manifest");
        assert_eq!(landing.status_message, None);
        let sneaky = by_name("sneaky");
        assert_eq!(sneaky.runtime, "helm");
        assert_eq!(sneaky.status, STACK_STATUS_ORPHANED);
        assert_eq!(by_name("sneaky-config").runtime, "kustomize");
        let echo = by_name("echo");
        assert_eq!(echo.runtime, "manifest");
        assert_eq!(echo.status, "Ready");
        assert_eq!(echo.status_message, None);
        let old = by_name("old");
        assert_eq!(old.runtime, "kustomize");
        assert_eq!(old.status, STACK_STATUS_ORPHANED);
        let gone = by_name("gone");
        assert_eq!(gone.runtime, "helm");
        assert_eq!(gone.status, "Deleting");

        // A failing config Kustomization is reported on its parent app row.
        let mut live = live;
        live.insert(
            ("tenant-acme".to_string(), "echo-config".to_string()),
            LiveResourceInfo {
                config_for: Some("echo".to_string()),
                ..failed_config
            },
        );
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(ReleaseDetailLive(live)),
        );
        let releases = provider.list_releases(Some("acme")).await.unwrap();
        let echo = releases.iter().find(|r| r.name == "echo").unwrap();
        assert_eq!(echo.status, "Ready");
        assert_eq!(
            echo.status_message.as_deref(),
            Some(
                "Latest configuration not applied: Kustomization tenant-acme/echo-config \
                 failed: BuildFailed: secrets.enc.yaml: sops decrypt"
            )
        );
        assert!(!releases.iter().any(|r| r.name == "echo-config"));
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
        async fn stack_status_details(
            &self,
        ) -> HashMap<(String, String), keel_kube::LiveResourceInfo> {
            self.stacks.clone()
        }
        async fn release_status_details(
            &self,
        ) -> HashMap<(String, String), keel_kube::LiveResourceInfo> {
            HashMap::new()
        }
        async fn helm_release_status_details(
            &self,
        ) -> HashMap<(String, String), keel_kube::LiveResourceInfo> {
            HashMap::new()
        }
        async fn kustomization_status_details(
            &self,
        ) -> HashMap<(String, String), keel_kube::LiveResourceInfo> {
            HashMap::new()
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

    /// Fixed tenant trees (paths relative to the repository root).
    struct FixedTenantSource(Vec<(String, Vec<(String, String)>)>);

    #[async_trait]
    impl TenantManifestSource for FixedTenantSource {
        async fn tenant_manifests(&self, tenants: &[String]) -> Vec<(String, Arc<ManifestTree>)> {
            self.0
                .iter()
                .filter(|(t, _)| tenants.contains(t))
                .map(|(t, files)| (t.clone(), Arc::new(files.iter().cloned().collect())))
                .collect()
        }
    }

    #[tokio::test]
    async fn tenant_repository_content_is_confined_to_its_tenant() {
        let tmp = tempfile::tempdir().unwrap();
        // Platform: the stubs (with the platform-owned root Kustomization) of acme and beta.
        for t in ["acme", "beta"] {
            let dir = tmp.path().join(format!("tenants/{t}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("sync.yaml"),
                keel_core::yaml::to_string(&keel_kube::build_tenant_root_kustomization(t).unwrap())
                    .unwrap(),
            )
            .unwrap();
        }
        let hr = |name: &str, ns: &str, labels: &str| {
            format!(
                "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: {name}\n  namespace: {ns}\n  labels:\n    x: y\n{labels}spec:\n  chart:\n    spec:\n      chart: c\n"
            )
        };
        let st = |name: &str, ns: Option<&str>, labels: &str| {
            let ns = ns
                .map(|n| format!("  namespace: {n}\n"))
                .unwrap_or_default();
            format!(
                "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: {name}\n{ns}  labels:\n    x: y\n{labels}spec:\n  stack: {name}\n"
            )
        };
        let source = FixedTenantSource(vec![
            (
                "acme".to_string(),
                vec![
                    (
                        "tenants/acme/apps/web/helmrelease.yaml".into(),
                        hr("web", "tenant-acme", ""),
                    ),
                    // Into beta's namespace: dropped.
                    (
                        "tenants/acme/apps/x/helmrelease.yaml".into(),
                        hr("x", "tenant-beta", ""),
                    ),
                    // Labelled as beta's: still acme's.
                    (
                        "tenants/acme/infra/vm/stack.yaml".into(),
                        st("vm", Some("tenant-acme"), "    keel.io/tenant: beta\n"),
                    ),
                    // No namespace: the root Kustomization's `targetNamespace`.
                    (
                        "tenants/acme/infra/bare/stack.yaml".into(),
                        st("bare", None, ""),
                    ),
                    // A tenant repository never declares the management plane.
                    (
                        "tenants/acme/infra/mgmt/stack.yaml".into(),
                        st(
                            "management-cluster-core",
                            Some("tenant-acme"),
                            "    keel.io/type: management\n",
                        ),
                    ),
                    // ... not even disguised as a workload (would hide the real one).
                    (
                        "tenants/acme/infra/mgmt2/stack.yaml".into(),
                        st(
                            "management-cluster-core",
                            Some("tenant-acme"),
                            "    keel.io/type: workload\n",
                        ),
                    ),
                ],
            ),
            (
                "beta".to_string(),
                vec![(
                    "tenants/beta/apps/api/helmrelease.yaml".into(),
                    hr("api", "tenant-beta", ""),
                )],
            ),
            // Not a platform tenant: never requested.
            (
                "ghost".to_string(),
                vec![(
                    "tenants/ghost/apps/g/helmrelease.yaml".into(),
                    hr("g", "tenant-ghost", ""),
                )],
            ),
        ]);
        // A stray workload document in the platform repository's `tenants/` is never reconciled
        // (the platform applies only the stubs): not listed.
        let stray = tmp.path().join("tenants/acme/apps/stray");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(
            stray.join("helmrelease.yaml"),
            hr("stray", "tenant-acme", ""),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("tenants/acme/stack.yaml"),
            st("stray-stack", Some("tenant-acme"), ""),
        )
        .unwrap();
        let provider =
            RepoClusterStateProvider::new(tmp.path()).with_tenant_source(Arc::new(source));

        // The management stack stays listed (from the platform) for cluster administrators.
        let all_stacks = provider.list_stacks(None, None).await.unwrap();
        assert!(
            all_stacks
                .iter()
                .any(|s| s.namespace == "flux-system" && s.name == "management-cluster-core"),
            "{all_stacks:?}"
        );
        assert!(
            !all_stacks
                .iter()
                .any(|s| s.name == "management-cluster-core" && s.namespace != "flux-system"),
            "{all_stacks:?}"
        );
        assert!(!all_stacks.iter().any(|s| s.name == "stray-stack"));

        // Several tenants at once (a member of both).
        let both = ["acme".to_string(), "beta".to_string()];
        let mut both_releases: Vec<String> = provider
            .list_releases_for_tenants(&both)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        both_releases.sort();
        assert_eq!(both_releases, vec!["api", "web"]);
        assert_eq!(
            provider
                .list_releases_for_tenants(&both[1..])
                .await
                .unwrap()
                .len(),
            1
        );

        let releases = provider.list_releases(None).await.unwrap();
        let mut keys: Vec<(String, String, Option<String>)> = releases
            .iter()
            .map(|r| (r.namespace.clone(), r.name.clone(), r.tenant.clone()))
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ("tenant-acme".into(), "web".into(), Some("acme".into())),
                ("tenant-beta".into(), "api".into(), Some("beta".into())),
            ]
        );
        let beta = provider.list_releases(Some("beta")).await.unwrap();
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].name, "api");

        let stacks = provider.list_stacks(Some("beta"), None).await.unwrap();
        assert!(stacks.is_empty(), "{stacks:?}");
        let mut acme: Vec<(String, String)> = provider
            .list_stacks(Some("acme"), None)
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.namespace, s.name))
            .collect();
        acme.sort();
        assert_eq!(
            acme,
            vec![
                ("tenant-acme".into(), "bare".into()),
                ("tenant-acme".into(), "vm".into()),
            ]
        );
    }

    #[tokio::test]
    async fn live_objects_are_listed_by_namespace_never_by_label() {
        let tmp = tempfile::tempdir().unwrap();
        let mut info = live_info(LiveResourceStatus::Synced, Some("aws"), false, false);
        info.tenant = Some("acme".to_string());
        let stacks = HashMap::from([(("tenant-beta".to_string(), "spoof".to_string()), info)]);
        let provider = KubeClusterStateProvider::new(
            RepoClusterStateProvider::new(tmp.path()),
            Arc::new(DetailLiveStatus { stacks }),
        );
        assert!(
            provider
                .list_stacks(Some("acme"), None)
                .await
                .unwrap()
                .is_empty()
        );
        let beta = provider.list_stacks(Some("beta"), None).await.unwrap();
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].tenant.as_deref(), Some("beta"));
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
