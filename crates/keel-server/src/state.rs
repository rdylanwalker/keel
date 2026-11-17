//! Stateless `AppState` and cluster/repository state providers (`SDD.md` §6.3).
//!
//! `keel-server` holds zero SQLite or external database connections. All state is
//! derived from Kubernetes `Secret`s (`SecretStateStore`), Git (`GitRunner` + `repo_workdir`),
//! `ForgeProvider`, and in-memory `kube-rs` CRD caches (`TelemetryMux`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use keel_core::{AuthPhase, OidcConfig};
use keel_git::{ForgeProvider, GitRunner, LocalGitProvider};
use keel_kube::{
    InMemorySecretStore, LiveResourceStatus, LiveStatusSource, SecretStateStore, StackDiagnostics,
    TelemetryMux,
};
use keel_pko::{PreviewReport, PreviewResourceCandidate, PreviewStack};
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

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
        Err(ServerError::BadRequest(
            "Live Kubernetes cluster connection required to restart stack workspace".to_string(),
        ))
    }

    async fn reconcile_release(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<String, ServerError> {
        Err(ServerError::BadRequest(
            "Live Kubernetes cluster connection required to reconcile release".to_string(),
        ))
    }
}

/// Default stateless [`ClusterStateProvider`] that scans `tenants/` and `deploy/`
/// YAML manifests inside `repo_workdir` (and optionally merges cached CRD summaries).
#[derive(Debug, Clone)]
pub struct RepoClusterStateProvider {
    repo_workdir: PathBuf,
}

impl RepoClusterStateProvider {
    pub fn new(repo_workdir: impl Into<PathBuf>) -> Self {
        Self {
            repo_workdir: repo_workdir.into(),
        }
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
            for doc in serde_yml::Deserializer::from_str(&content) {
                let Ok(val) = Value::deserialize(doc) else {
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
                let protect = val
                    .pointer("/spec/protect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
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

                stacks.push(StackSummary {
                    name,
                    namespace,
                    tenant: doc_tenant,
                    stack: stack_name,
                    provider: doc_provider,
                    stack_type,
                    preview,
                    protect,
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
                    tenant: None,
                    stack: "keel/management/core".to_string(),
                    provider: None,
                    stack_type: "management".to_string(),
                    preview: false,
                    protect: true,
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
            if path.file_name().and_then(|n| n.to_str()) == Some("flux-kustomization.yaml") {
                continue;
            }
            if let Some(parent) = path.parent()
                && parent != tenants_root.as_path()
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
                    && let Some(fname) = path.file_name().and_then(|n| n.to_str())
                    && fname != "kustomization.yaml"
                    && !set.contains(fname)
                {
                    continue;
                }
            }
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            for doc in serde_yml::Deserializer::from_str(&content) {
                let Ok(val) = Value::deserialize(doc) else {
                    continue;
                };
                let api_version = val
                    .get("apiVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let kind = val.get("kind").and_then(Value::as_str).unwrap_or_default();
                let is_flux_release = kind == "HelmRelease"
                    || (kind == "Kustomization"
                        && api_version.starts_with("kustomize.toolkit.fluxcd.io/"))
                    || kind == "Canary";
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
                    .and_then(Value::as_str)
                    .unwrap_or(&name)
                    .to_string();
                let version = val
                    .pointer("/spec/chart/spec/version")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let runtime = match kind {
                    "Kustomization" => "kustomize",
                    _ => "helm",
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
            if let Some(info) = live.get(&(stack.namespace.clone(), stack.name.clone())) {
                stack.status = Self::status_label(&info.status);
                stack.last_synced = info.last_synced.clone();
                if info.resource_count.is_some() {
                    stack.resource_count = info.resource_count;
                }
            } else if stack.name == "management-cluster-core"
                && let Some(info) = live
                    .get(&("flux-system".to_string(), "core-addons".to_string()))
                    .or_else(|| live.get(&("flux-system".to_string(), "flux-system".to_string())))
            {
                stack.status = Self::status_label(&info.status);
                stack.last_synced = info.last_synced.clone();
                if info.resource_count.is_some() {
                    stack.resource_count = info.resource_count;
                }
            }
        }
        // Keep any live tenant-* stack that is absent from Git `main` (awaiting Flux prune or mid-termination)
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
            if !stacks.iter().any(|s| s.namespace == *ns && s.name == *name) {
                stacks.push(StackSummary {
                    name: name.clone(),
                    namespace: ns.clone(),
                    tenant: inferred_tenant.map(str::to_owned),
                    stack: name.clone(),
                    provider: None,
                    stack_type: infer_stack_type(name, ns, None, None),
                    preview: false,
                    protect: false,
                    bundle: None,
                    bundle_item: None,
                    resource_count: info.resource_count,
                    last_synced: info.last_synced.clone(),
                    status: "Deleting".to_string(),
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
        // Deduplicate paired HelmRelease + Canary manifests into a single ReleaseSummary with `canary = true`
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
                });
            }
        }
        Ok(releases)
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
            .map_err(|e| ServerError::Internal(format!("Rollout restart failed: {e}")))
    }

    async fn reconcile_release(&self, namespace: &str, name: &str) -> Result<String, ServerError> {
        self.live
            .reconcile_release(namespace, name)
            .await
            .map_err(|e| ServerError::Internal(format!("Flux reconcile failed: {e}")))
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
    pub docker_proxy_url: Option<String>,
    pub git_lock: Arc<Mutex<()>>,
    pub preview_cache: Arc<RwLock<HashMap<String, CachedPreview>>>,
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
            docker_proxy_url: None,
            git_lock: Arc::new(Mutex::new(())),
            preview_cache: Arc::new(RwLock::new(HashMap::new())),
        }
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
}
