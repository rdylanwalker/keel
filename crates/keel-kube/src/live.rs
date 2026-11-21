//! Live Kubernetes CRD status reader via `kube-rs` (`SDD.md` §5.3, §7.1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Event as K8sEvent, Pod};
use kube::Client;
use kube::api::{
    Api, AttachParams, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams,
};
use kube::discovery::ApiResource;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::KubeError;

/// Reconciled status of a Kubernetes GitOps or PKO resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveResourceStatus {
    Synced,
    Ready,
    Reconciling,
    Deleting,
    Error,
}

/// Live status plus reconciliation metadata (`last_synced`, `resource_count`) for topology views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveResourceInfo {
    pub status: LiveResourceStatus,
    pub last_synced: Option<String>,
    pub resource_count: Option<u32>,
    /// `true` only when `metadata.deletionTimestamp` is set on the live object.
    pub terminating: bool,
    /// Anti-lockout protection (`keel.io/protect` label or `spec.config.protect`), see [`is_stack_object_protected`].
    pub protected: bool,
    /// Tenant from the `keel.io/tenant` label, if present.
    pub tenant: Option<String>,
    /// Provider from the `keel.io/provider` label or `spec.config["keel:provider"]`, if present.
    pub provider: Option<String>,
}

impl LiveResourceInfo {
    /// Creates a minimal [`LiveResourceInfo`] carrying only a reconciliation status.
    pub fn from_status(status: LiveResourceStatus) -> Self {
        Self {
            terminating: status == LiveResourceStatus::Deleting,
            status,
            last_synced: None,
            resource_count: None,
            protected: false,
            tenant: None,
            provider: None,
        }
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Returns `true` if a PKO `Stack` is protected against deletion, mirroring
/// [`keel_pko::PkoStack::is_protected`]: the canonical `keel.io/protect: "true"` label, or
/// `spec.config.protect` / `spec.config["keel:protect"]` set to `true`. PKO `Stack`s have no
/// top-level `spec.protect` field, so it is deliberately ignored.
pub fn is_stack_object_protected(
    labels: Option<&std::collections::BTreeMap<String, String>>,
    data: &Value,
) -> bool {
    let label = labels
        .and_then(|l| l.get(keel_pko::stack::LABEL_PROTECT))
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    label
        || truthy(data.pointer("/spec/config/protect"))
        || truthy(data.pointer("/spec/config/keel:protect"))
}

/// Same as [`is_stack_object_protected`] for a full manifest document (`metadata` + `spec`).
pub fn is_stack_manifest_protected(doc: &Value) -> bool {
    let label = doc
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .and_then(|l| l.get(keel_pko::stack::LABEL_PROTECT));
    truthy(label) || is_stack_object_protected(None, doc)
}

/// Maps a Kubernetes API `404` into [`KubeError::NotFound`], passing other errors through.
fn not_found_or(err: kube::Error, msg: String) -> KubeError {
    match err {
        kube::Error::Api(status) if status.code == 404 => KubeError::NotFound(msg),
        other => KubeError::Kube(other),
    }
}

/// Extracts the provider for a Stack from the `keel.io/provider` label or `spec.config["keel:provider"]`.
fn extract_stack_provider(
    labels: Option<&std::collections::BTreeMap<String, String>>,
    data: &Value,
) -> Option<String> {
    labels
        .and_then(|l| l.get("keel.io/provider"))
        .cloned()
        .or_else(|| {
            data.pointer("/spec/config/keel:provider")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|p| !p.trim().is_empty())
}

/// Live status and traffic weight for a Flagger `Canary` resource.
#[derive(Debug, Clone, PartialEq)]
pub struct CanaryStatusInfo {
    pub status: LiveResourceStatus,
    pub last_synced: Option<String>,
    pub canary_weight: Option<u32>,
    pub canary_phase: Option<String>,
    pub primary_version: Option<String>,
    pub error_rate: Option<f64>,
    pub latency_ms: Option<f64>,
}

/// Normalized Kubernetes resource condition (`Ready`, `Reconciling`, `Stalled`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}

/// Summary of a Kubernetes `Event` associated with a Stack or its workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterEventSummary {
    pub event_type: String,
    pub reason: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

/// Resource metadata & spec summary surfaced in the Inspect modal.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub labels: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub annotations: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub spec_summary: std::collections::BTreeMap<String, String>,
}

/// Detailed live diagnostics for a PKO v2 `Stack` and its workspace pod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackDiagnostics {
    pub name: String,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ResourceMetadata>,
    pub conditions: Vec<ResourceCondition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_pod: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_phase: Option<String>,
    pub events: Vec<ClusterEventSummary>,
    pub logs: Vec<String>,
}

/// Abstract reader for live Flux and PKO v2 resource statuses.
#[async_trait]
pub trait LiveStatusSource: Send + Sync {
    /// Returns `(namespace, name) -> LiveResourceStatus` for PKO v2 `Stack` resources.
    async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus>;

    /// Returns `(namespace, name) -> LiveResourceStatus` for Flux `HelmRelease` and `Kustomization` resources.
    async fn release_statuses(&self) -> HashMap<(String, String), LiveResourceStatus>;

    /// Returns `(namespace, name) -> LiveResourceInfo` for PKO v2 `Stack` resources.
    async fn stack_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
        self.stack_statuses()
            .await
            .into_iter()
            .map(|(k, status)| (k, LiveResourceInfo::from_status(status)))
            .collect()
    }

    /// Returns `(namespace, name) -> LiveResourceInfo` for Flux `HelmRelease` and `Kustomization` resources.
    async fn release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
        self.release_statuses()
            .await
            .into_iter()
            .map(|(k, status)| (k, LiveResourceInfo::from_status(status)))
            .collect()
    }

    /// Returns `(namespace, name) -> CanaryStatusInfo` for Flagger `Canary` resources.
    async fn canary_statuses(&self) -> HashMap<(String, String), CanaryStatusInfo> {
        HashMap::new()
    }

    /// Fetches detailed conditions, workspace pod status, events, and logs for a `Stack`.
    async fn stack_diagnostics(&self, _namespace: &str, _name: &str) -> Option<StackDiagnostics> {
        None
    }

    /// Fetches detailed conditions, pod status, events, and logs for a Flux `HelmRelease` / `Kustomization`.
    async fn release_diagnostics(&self, _namespace: &str, _name: &str) -> Option<StackDiagnostics> {
        None
    }

    /// Triggers a Kubernetes `rollout restart` on `<name>-workspace` StatefulSet in `namespace`.
    async fn restart_stack_workspace(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<String, KubeError> {
        Err(KubeError::Unavailable(
            "Live Kubernetes cluster connection required to restart stack workspace".to_string(),
        ))
    }

    /// Annotates a PKO v2 `Stack` with `pulumi.com/reconcile-request`.
    async fn reconcile_stack(&self, _namespace: &str, _name: &str) -> Result<String, KubeError> {
        Err(KubeError::Unavailable(
            "Live Kubernetes cluster connection required to reconcile stack".to_string(),
        ))
    }

    /// Clears finalizers on a stuck/terminating PKO v2 `Stack` and deletes it if not already terminating.
    async fn force_delete_stack(&self, _namespace: &str, _name: &str) -> Result<String, KubeError> {
        Err(KubeError::Unavailable(
            "Live Kubernetes cluster connection required to force-delete stack".to_string(),
        ))
    }

    /// Cancels a stale Pulumi state lock for a PKO v2 `Stack` via `pulumi cancel` in its workspace pod
    /// and requests immediate reconciliation.
    async fn unlock_stack(&self, _namespace: &str, _name: &str) -> Result<String, KubeError> {
        Err(KubeError::Unavailable(
            "Live Kubernetes cluster connection required to unlock stack".to_string(),
        ))
    }

    /// Annotates a Flux `HelmRelease` (or `Kustomization`) with `reconcile.fluxcd.io/requestedAt`.
    async fn reconcile_release(&self, _namespace: &str, _name: &str) -> Result<String, KubeError> {
        Err(KubeError::Unavailable(
            "Live Kubernetes cluster connection required to reconcile release".to_string(),
        ))
    }

    /// Reads the `<name>-outputs` Kubernetes `Secret` (if present) for a PKO `Stack` in `namespace`.
    async fn stack_outputs(&self, _namespace: &str, _name: &str) -> HashMap<String, String> {
        HashMap::new()
    }

    /// Deletes a namespaced dynamic Kubernetes resource (e.g., ephemeral PKO `Stack` or Flux `GitRepository`).
    /// Returns `Ok(true)` if deleted, `Ok(false)` if not found (or in offline mode).
    async fn delete_dynamic_resource(
        &self,
        api_version: &str,
        kind: &str,
        namespace: &str,
        name: &str,
    ) -> Result<bool, KubeError> {
        let _ = (api_version, kind, namespace, name);
        Ok(false)
    }

    /// Server-side applies a cluster-scoped Kyverno CEL policy document (`policies.kyverno.io/v1`).
    async fn apply_cluster_policy(&self, name: &str, policy_doc: &Value) -> Result<(), KubeError> {
        let _ = (name, policy_doc);
        Ok(())
    }
}

/// Live `kube-rs` implementation querying PKO v2 `Stack`, Flux `HelmRelease`, and Flux `Kustomization` CRDs.
///
/// CRD list results are memoized for [`DEFAULT_STATUS_CACHE_TTL`] so concurrent UI/CLI polling of
/// `GET /api/v1/stacks` / `GET /api/v1/releases` does not fan out into repeated cluster-wide LISTs
/// (`NFR-PERF`). Mutating operations invalidate the cache immediately.
#[derive(Clone)]
pub struct KubeClusterStatusReader {
    client: Client,
    status_cache: Arc<StdMutex<StatusCache>>,
    cache_ttl: Duration,
}

/// Default time-to-live for memoized CRD status LIST results.
pub const DEFAULT_STATUS_CACHE_TTL: Duration = Duration::from_secs(2);

type StatusMap = HashMap<(String, String), LiveResourceInfo>;
type StatusCache = HashMap<String, (Instant, StatusMap)>;

impl KubeClusterStatusReader {
    /// Connects to the Kubernetes cluster using default `KUBECONFIG` or in-cluster configuration.
    pub async fn try_default() -> Result<Self, KubeError> {
        let client = Client::try_default().await?;
        Ok(Self::from_client(client))
    }

    /// Wraps an existing `kube::Client`.
    pub fn from_client(client: Client) -> Self {
        Self {
            client,
            status_cache: Arc::new(StdMutex::new(HashMap::new())),
            cache_ttl: DEFAULT_STATUS_CACHE_TTL,
        }
    }

    /// Overrides the CRD status LIST cache TTL (`Duration::ZERO` disables caching).
    pub fn with_status_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    fn cached_statuses(&self, key: &str) -> Option<StatusMap> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        let cache = self.status_cache.lock().ok()?;
        let (at, map) = cache.get(key)?;
        (at.elapsed() < self.cache_ttl).then(|| map.clone())
    }

    fn store_statuses(&self, key: String, map: &StatusMap) {
        if self.cache_ttl.is_zero() {
            return;
        }
        if let Ok(mut cache) = self.status_cache.lock() {
            cache.insert(key, (Instant::now(), map.clone()));
        }
    }

    /// Drops memoized CRD status LIST results (called after every mutating operation).
    pub fn invalidate_status_cache(&self) {
        if let Ok(mut cache) = self.status_cache.lock() {
            cache.clear();
        }
    }

    /// Returns a reference to the underlying `kube::Client`.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Reads a UTF-8 string value from `Secret.data[key]` in the given namespace.
    pub async fn read_secret_key(&self, namespace: &str, name: &str, key: &str) -> Option<String> {
        use k8s_openapi::api::core::v1::Secret;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let secret = secrets.get(name).await.ok()?;
        let data = secret.data?;
        let bytes = data.get(key)?;
        String::from_utf8(bytes.0.clone()).ok()
    }

    /// Reads all UTF-8 key-value pairs from a Kubernetes `Secret` (such as `<stack>-outputs`).
    pub async fn read_secret_map(&self, namespace: &str, name: &str) -> HashMap<String, String> {
        use k8s_openapi::api::core::v1::Secret;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let Ok(secret) = secrets.get(name).await else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        if let Some(data) = secret.data {
            for (k, bytes) in data {
                if let Ok(val) = String::from_utf8(bytes.0) {
                    out.insert(k, val);
                }
            }
        }
        out
    }

    async fn collect_canary_statuses_for_client(
        client: &Client,
    ) -> HashMap<(String, String), CanaryStatusInfo> {
        let ar = ApiResource {
            group: "flagger.app".to_string(),
            version: "v1beta1".to_string(),
            api_version: "flagger.app/v1beta1".to_string(),
            kind: "Canary".to_string(),
            plural: "canaries".to_string(),
        };
        let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
        let Ok(list) = api.list(&ListParams::default()).await else {
            return HashMap::new();
        };

        let deploy_api: Api<k8s_openapi::api::apps::v1::Deployment> = Api::all(client.clone());
        let mut primary_versions: HashMap<(String, String), String> = HashMap::new();
        if let Ok(deploys) = deploy_api.list(&ListParams::default()).await {
            for dep in deploys.items {
                let Some(dep_name) = dep.metadata.name.as_deref() else {
                    continue;
                };
                let Some(base_name) = dep_name.strip_suffix("-primary") else {
                    continue;
                };
                let ns = dep
                    .metadata
                    .namespace
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                let label_ver = dep
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("app.kubernetes.io/version"))
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty());
                let image_ver = dep
                    .spec
                    .as_ref()
                    .and_then(|s| s.template.spec.as_ref())
                    .and_then(|ps| ps.containers.first())
                    .and_then(|c| c.image.as_deref())
                    .and_then(|img| img.rsplit_once(':').map(|(_, tag)| tag.to_string()))
                    .filter(|v| !v.is_empty());
                if let Some(ver) = label_ver.or(image_ver) {
                    primary_versions.insert((ns, base_name.to_string()), ver);
                }
            }
        }

        let (error_rates, latencies_ms) = if list.items.is_empty() {
            (HashMap::new(), HashMap::new())
        } else {
            Self::collect_canary_prometheus_metrics(client).await
        };

        let mut out = HashMap::new();
        for item in list.items {
            let Some(name) = item.metadata.name.clone() else {
                continue;
            };
            let namespace = item
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let status_obj = item.data.get("status");
            let phase = status_obj
                .and_then(|s| s.get("phase"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let status = match phase.as_deref().unwrap_or_default() {
                "Succeeded" | "Initialized" => LiveResourceStatus::Ready,
                "Failed" => LiveResourceStatus::Error,
                _ => LiveResourceStatus::Reconciling,
            };
            let canary_weight = status_obj
                .and_then(|s| s.get("canaryWeight"))
                .and_then(Value::as_u64)
                .and_then(|w| u32::try_from(w).ok());
            let last_synced = status_obj
                .and_then(|s| s.get("lastTransitionTime"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| extract_last_synced(&item.data, None));
            let target_ref_name = item
                .data
                .pointer("/spec/targetRef/name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let primary_version = target_ref_name
                .as_ref()
                .and_then(|t| {
                    primary_versions
                        .get(&(namespace.clone(), t.clone()))
                        .cloned()
                })
                .or_else(|| {
                    primary_versions
                        .get(&(namespace.clone(), name.clone()))
                        .cloned()
                });
            let metric_key = (namespace.clone(), name.clone());
            let prefixed_key = (namespace.clone(), format!("{namespace}-{name}"));
            let target_key = target_ref_name.map(|t| (namespace.clone(), t));
            let error_rate = error_rates
                .get(&metric_key)
                .or_else(|| error_rates.get(&prefixed_key))
                .or_else(|| target_key.as_ref().and_then(|k| error_rates.get(k)))
                .map(|v| (v * 100.0).round() / 100.0);
            let latency_ms = latencies_ms
                .get(&metric_key)
                .or_else(|| latencies_ms.get(&prefixed_key))
                .or_else(|| target_key.as_ref().and_then(|k| latencies_ms.get(k)))
                .map(|v| (v * 10.0).round() / 10.0);
            out.insert(
                (namespace, name),
                CanaryStatusInfo {
                    status,
                    last_synced,
                    canary_weight,
                    canary_phase: phase,
                    primary_version,
                    error_rate,
                    latency_ms,
                },
            );
        }
        out
    }

    async fn discover_prometheus_service_name(client: &Client) -> String {
        let svc_api: Api<k8s_openapi::api::core::v1::Service> =
            Api::namespaced(client.clone(), "keel-system");
        let Ok(Ok(list)) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            svc_api.list(&ListParams::default()),
        )
        .await
        else {
            return "prometheus".to_string();
        };
        let mut fallback: Option<String> = None;
        for svc in list.items {
            if let Some(name) = svc.metadata.name {
                if name == "prometheus" {
                    return name;
                }
                if fallback.is_none() && name.ends_with("-prometheus-server") {
                    fallback = Some(name);
                }
            }
        }
        fallback.unwrap_or_else(|| "prometheus".to_string())
    }

    async fn collect_canary_prometheus_metrics(
        client: &Client,
    ) -> (
        HashMap<(String, String), f64>,
        HashMap<(String, String), f64>,
    ) {
        const ERROR_RATE_QUERY: &str = r#"(sum by (keel_route) (label_replace(rate(envoy_cluster_upstream_rq{envoy_cluster_name=~"httproute/[^/]+/[^/]+/.*",envoy_response_code=~"5.*"}[2m]), "keel_route", "$1/$2", "envoy_cluster_name", "httproute/([^/]+)/([^/]+)/.*")) or sum by (keel_route) (label_replace(rate(envoy_cluster_upstream_rq{envoy_cluster_name=~"httproute/[^/]+/[^/]+/.*"}[2m]), "keel_route", "$1/$2", "envoy_cluster_name", "httproute/([^/]+)/([^/]+)/.*")) * 0) / sum by (keel_route) (label_replace(rate(envoy_cluster_upstream_rq{envoy_cluster_name=~"httproute/[^/]+/[^/]+/.*"}[2m]), "keel_route", "$1/$2", "envoy_cluster_name", "httproute/([^/]+)/([^/]+)/.*")) * 100"#;
        const P99_LATENCY_QUERY: &str = r#"histogram_quantile(0.99, sum by (le, keel_route) (label_replace(rate(envoy_cluster_upstream_rq_time_bucket{envoy_cluster_name=~"httproute/[^/]+/[^/]+/.*"}[2m]), "keel_route", "$1/$2", "envoy_cluster_name", "httproute/([^/]+)/([^/]+)/.*")))"#;

        let prom_svc = Self::discover_prometheus_service_name(client).await;
        tokio::join!(
            Self::query_prometheus_vector(client, &prom_svc, ERROR_RATE_QUERY),
            Self::query_prometheus_vector(client, &prom_svc, P99_LATENCY_QUERY),
        )
    }

    async fn query_prometheus_vector(
        client: &Client,
        service_name: &str,
        promql: &str,
    ) -> HashMap<(String, String), f64> {
        let encoded = percent_encode_query(promql);
        let uri = format!(
            "/api/v1/namespaces/keel-system/services/http:{service_name}:80/proxy/api/v1/query?query={encoded}"
        );
        let Ok(req) = http::Request::get(&uri).body(Vec::new()) else {
            return HashMap::new();
        };
        let Ok(Ok(body)) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.request::<Value>(req),
        )
        .await
        else {
            return HashMap::new();
        };
        parse_prometheus_vector_response(&body)
    }

    /// Lists Secrets only inside tenant namespaces (`tenant-*` namespaces labeled
    /// `keel.io/tenant`). Falls back to a cluster-wide list only if namespace listing is
    /// forbidden, preserving existing behaviour on restricted RBAC setups; callers still filter
    /// on `tenant-*` namespaces.
    async fn list_tenant_namespace_secrets(&self) -> Vec<k8s_openapi::api::core::v1::Secret> {
        let ns_api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(self.client.clone());
        match ns_api
            .list(&ListParams::default().labels("keel.io/tenant"))
            .await
        {
            Ok(namespaces) => {
                let futures = namespaces
                    .items
                    .into_iter()
                    .filter_map(|n| n.metadata.name)
                    .filter(|n| n.starts_with("tenant-"))
                    .map(|ns| {
                        let api: Api<k8s_openapi::api::core::v1::Secret> =
                            Api::namespaced(self.client.clone(), &ns);
                        async move { api.list(&ListParams::default()).await.ok() }
                    });
                futures::future::join_all(futures)
                    .await
                    .into_iter()
                    .flatten()
                    .flat_map(|l| l.items)
                    .collect()
            }
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "Namespace list failed; falling back to cluster-wide Secret list for tenant kubeconfigs"
                );
                let api: Api<k8s_openapi::api::core::v1::Secret> = Api::all(self.client.clone());
                api.list(&ListParams::default())
                    .await
                    .map(|l| l.items)
                    .unwrap_or_default()
            }
        }
    }

    /// Builds a `kube::Client` from an **untrusted**, tenant-authored kubeconfig.
    ///
    /// The kubeconfig is rejected (returning `None`) unless it passes
    /// [`validate_untrusted_kubeconfig`]: `exec`/`auth-provider` plugins would execute
    /// arbitrary binaries inside `keel-server`, and file-path credentials (`tokenFile`,
    /// `client-certificate`, `client-key`, `certificate-authority`) would read server-local
    /// files such as the in-cluster ServiceAccount token. `secret_ref` is a
    /// `namespace/name` identifier used only for logging; secret content is never logged.
    async fn build_client_from_kubeconfig_yaml(yaml: &str, secret_ref: &str) -> Option<Client> {
        let kc = match kube::config::Kubeconfig::from_yaml(yaml) {
            Ok(kc) => kc,
            Err(_) => {
                tracing::warn!(
                    secret = %secret_ref,
                    "Skipping tenant kubeconfig Secret: failed to parse kubeconfig"
                );
                return None;
            }
        };
        if let Err(reason) = validate_untrusted_kubeconfig(&kc) {
            tracing::warn!(
                secret = %secret_ref,
                reason = %reason,
                "Skipping tenant kubeconfig Secret: unsafe kubeconfig rejected"
            );
            return None;
        }
        let mut cfg =
            kube::Config::from_custom_kubeconfig(kc, &kube::config::KubeConfigOptions::default())
                .await
                .ok()?;
        if !std::path::Path::new("/var/run/secrets/kubernetes.io/serviceaccount/token").exists()
            && let Some(host) = cfg.cluster_url.host()
            && host.ends_with("-control-plane")
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            && let Ok(Ok(out)) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                tokio::process::Command::new("docker")
                    .args(["port", host, "6443/tcp"])
                    .output(),
            )
            .await
            && out.status.success()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if let Some(port) = stdout
                .lines()
                .next()
                .and_then(|l| l.trim().rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok())
                && let Ok(uri) = format!("https://127.0.0.1:{port}").parse()
            {
                cfg.cluster_url = uri;
            }
        }
        cfg.connect_timeout = Some(std::time::Duration::from_secs(3));
        cfg.read_timeout = Some(std::time::Duration::from_secs(5));
        Client::try_from(cfg).ok()
    }

    async fn list_dynamic_status_details(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        is_stack: bool,
    ) -> HashMap<(String, String), LiveResourceInfo> {
        let ar = ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_string(),
            plural: plural.to_string(),
        };
        let cache_key = format!("{group}/{version}/{plural}/{is_stack}");
        if let Some(hit) = self.cached_statuses(&cache_key) {
            return hit;
        }
        let api: Api<DynamicObject> = Api::all_with(self.client.clone(), &ar);
        let Ok(list) = api.list(&ListParams::default()).await else {
            return HashMap::new();
        };

        let mut out = HashMap::new();
        for item in list.items {
            let ns = item
                .metadata
                .namespace
                .unwrap_or_else(|| "default".to_string());
            let name = item.metadata.name.unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            if !is_stack
                && kind == "Kustomization"
                && item
                    .data
                    .pointer("/spec/healthChecks")
                    .and_then(Value::as_array)
                    .is_some_and(|checks| {
                        checks
                            .iter()
                            .any(|hc| hc.get("kind").and_then(Value::as_str) == Some("Stack"))
                    })
            {
                continue;
            }
            let creation_ts = item
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.to_string());
            let last_synced = extract_last_synced(&item.data, creation_ts.as_deref());
            let resource_count = extract_resource_count(&item.data);

            let terminating = item.metadata.deletion_timestamp.is_some();
            let labels = item.metadata.labels.as_ref();
            let status = if terminating {
                Some(LiveResourceStatus::Deleting)
            } else {
                extract_ready_status(&item.data, is_stack)
            };
            if let Some(status) = status {
                out.insert(
                    (ns, name),
                    LiveResourceInfo {
                        status,
                        last_synced,
                        resource_count,
                        terminating,
                        protected: is_stack && is_stack_object_protected(labels, &item.data),
                        tenant: labels.and_then(|l| l.get("keel.io/tenant")).cloned(),
                        provider: extract_stack_provider(labels, &item.data),
                    },
                );
            }
        }
        self.store_statuses(cache_key, &out);
        out
    }

    async fn list_dynamic_statuses(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        is_stack: bool,
    ) -> HashMap<(String, String), LiveResourceStatus> {
        self.list_dynamic_status_details(group, version, kind, plural, is_stack)
            .await
            .into_iter()
            .map(|(k, info)| (k, info.status))
            .collect()
    }

    /// Reads `spec.backend` from a live PKO `Stack` resource, if present.
    pub async fn read_stack_backend(&self, namespace: &str, name: &str) -> Option<String> {
        let ar = ApiResource {
            group: "pulumi.com".to_string(),
            version: "v1".to_string(),
            api_version: "pulumi.com/v1".to_string(),
            kind: "Stack".to_string(),
            plural: "stacks".to_string(),
        };
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
        let obj = api.get(name).await.ok()?;
        obj.data
            .get("spec")
            .and_then(|s| s.get("backend"))
            .and_then(|b| b.as_str())
            .map(str::to_string)
    }

    /// Returns `true` if `Service/keel-docker-proxy` is deployed in `flux-system` for local `kind`-in-`kind` dev.
    pub async fn has_docker_proxy_service(&self) -> bool {
        let ar = ApiResource {
            group: String::new(),
            version: "v1".to_string(),
            api_version: "v1".to_string(),
            kind: "Service".to_string(),
            plural: "services".to_string(),
        };
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), "flux-system", &ar);
        api.get("keel-docker-proxy").await.is_ok()
    }
}

#[async_trait]
impl LiveStatusSource for KubeClusterStatusReader {
    async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
        let mut out = self
            .list_dynamic_statuses("pulumi.com", "v1", "Stack", "stacks", true)
            .await;

        // Also check Flux Kustomizations in `flux-system` so `management-cluster-core` reflects Flux readiness
        let kustomizations = self
            .list_dynamic_statuses(
                "kustomize.toolkit.fluxcd.io",
                "v1",
                "Kustomization",
                "kustomizations",
                true,
            )
            .await;
        for (key, st) in kustomizations {
            if key.0 == "flux-system" {
                out.entry(key).or_insert(st);
            }
        }
        out
    }

    async fn stack_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
        let mut out = self
            .list_dynamic_status_details("pulumi.com", "v1", "Stack", "stacks", true)
            .await;

        let kustomizations = self
            .list_dynamic_status_details(
                "kustomize.toolkit.fluxcd.io",
                "v1",
                "Kustomization",
                "kustomizations",
                true,
            )
            .await;
        for (key, info) in kustomizations {
            if key.0 == "flux-system" {
                out.entry(key).or_insert(info);
            }
        }
        out
    }

    async fn release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
        let mut out = self
            .list_dynamic_status_details(
                "helm.toolkit.fluxcd.io",
                "v2",
                "HelmRelease",
                "helmreleases",
                false,
            )
            .await;

        let kustomizations = self
            .list_dynamic_status_details(
                "kustomize.toolkit.fluxcd.io",
                "v1",
                "Kustomization",
                "kustomizations",
                false,
            )
            .await;
        for (key, info) in kustomizations {
            out.entry(key).or_insert(info);
        }
        out
    }

    async fn canary_statuses(&self) -> HashMap<(String, String), CanaryStatusInfo> {
        let mut out = Self::collect_canary_statuses_for_client(&self.client).await;

        // Also poll workload clusters referenced by `*-kubeconfig` Secrets in `tenant-*` namespaces.
        // Secrets are listed per tenant namespace (namespaces labeled `keel.io/tenant`) so the
        // control plane never pulls `keel-system`/`flux-system` Secrets into memory here. These
        // Secrets are written by tenant Pulumi programs and carry no Keel label, so the
        // `-kubeconfig` suffix filter below is still required.
        let secrets = self.list_tenant_namespace_secrets().await;
        {
            let mut kubeconfig_yamls = Vec::new();
            for sec in secrets {
                let Some(ns) = sec.metadata.namespace.as_deref() else {
                    continue;
                };
                let Some(name) = sec.metadata.name.as_deref() else {
                    continue;
                };
                if !ns.starts_with("tenant-") || !name.ends_with("-kubeconfig") {
                    continue;
                }
                let Some(data) = sec.data else {
                    continue;
                };
                let raw_bytes = data.get("value").or_else(|| data.get("kubeconfig"));
                if let Some(bytes) = raw_bytes
                    && let Ok(yaml) = String::from_utf8(bytes.0.clone())
                    && !yaml.trim().is_empty()
                {
                    kubeconfig_yamls.push((format!("{ns}/{name}"), yaml));
                }
            }

            let remote_futures =
                kubeconfig_yamls
                    .into_iter()
                    .map(|(secret_ref, yaml)| async move {
                        let client =
                            Self::build_client_from_kubeconfig_yaml(&yaml, &secret_ref).await?;
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            Self::collect_canary_statuses_for_client(&client),
                        )
                        .await
                        .ok()
                    });
            for remote_map in futures::future::join_all(remote_futures)
                .await
                .into_iter()
                .flatten()
            {
                for (k, v) in remote_map {
                    out.entry(k).or_insert(v);
                }
            }
        }

        out
    }

    async fn release_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
        let mut out = self
            .list_dynamic_statuses(
                "helm.toolkit.fluxcd.io",
                "v2",
                "HelmRelease",
                "helmreleases",
                false,
            )
            .await;

        let kustomizations = self
            .list_dynamic_statuses(
                "kustomize.toolkit.fluxcd.io",
                "v1",
                "Kustomization",
                "kustomizations",
                false,
            )
            .await;
        for (key, st) in kustomizations {
            out.entry(key).or_insert(st);
        }
        out
    }

    async fn stack_diagnostics(&self, namespace: &str, name: &str) -> Option<StackDiagnostics> {
        let ar = ApiResource {
            group: "pulumi.com".to_string(),
            version: "v1".to_string(),
            api_version: "pulumi.com/v1".to_string(),
            kind: "Stack".to_string(),
            plural: "stacks".to_string(),
        };
        let stack_api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &ar);
        let (conditions, metadata) = match stack_api.get(name).await {
            Ok(obj) => (
                extract_conditions(&obj.data),
                Some(extract_resource_metadata(&obj, "pulumi.com/v1", "Stack")),
            ),
            Err(_) => (Vec::new(), None),
        };

        let pod_name = format!("{name}-workspace-0");
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let (workspace_pod, workspace_phase, logs) = match pod_api.get(&pod_name).await {
            Ok(pod) => {
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.clone())
                    .or_else(|| Some("Pending".to_string()));
                let log_params = LogParams {
                    container: Some("pulumi".to_string()),
                    tail_lines: Some(100),
                    ..Default::default()
                };
                let log_lines = match pod_api.logs(&pod_name, &log_params).await {
                    Ok(raw) if !raw.trim().is_empty() => raw.lines().map(str::to_string).collect(),
                    _ => {
                        let prev_params = LogParams {
                            container: Some("pulumi".to_string()),
                            previous: true,
                            tail_lines: Some(100),
                            ..Default::default()
                        };
                        pod_api
                            .logs(&pod_name, &prev_params)
                            .await
                            .map(|raw| raw.lines().map(str::to_string).collect())
                            .unwrap_or_default()
                    }
                };
                (Some(pod_name.clone()), phase, log_lines)
            }
            Err(_) => (None, None, Vec::new()),
        };

        let workspace_sts = format!("{name}-workspace");
        let event_api: Api<K8sEvent> = Api::namespaced(self.client.clone(), namespace);
        let mut events: Vec<ClusterEventSummary> = event_api
            .list(&ListParams::default())
            .await
            .map(|list| {
                list.items
                    .into_iter()
                    .filter(|ev| {
                        let obj_name = ev.involved_object.name.as_deref().unwrap_or_default();
                        obj_name == name || obj_name == workspace_sts || obj_name == pod_name
                    })
                    .map(|ev| ClusterEventSummary {
                        event_type: ev.type_.unwrap_or_else(|| "Normal".to_string()),
                        reason: ev.reason.unwrap_or_default(),
                        message: ev.message.unwrap_or_default(),
                        count: ev.count,
                        last_seen: ev
                            .last_timestamp
                            .map(|t| t.0.to_string())
                            .or_else(|| ev.metadata.creation_timestamp.map(|t| t.0.to_string())),
                    })
                    .collect()
            })
            .unwrap_or_default();
        events.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        events.truncate(20);

        Some(StackDiagnostics {
            name: name.to_string(),
            namespace: namespace.to_string(),
            metadata,
            conditions,
            workspace_pod,
            workspace_phase,
            events,
            logs,
        })
    }

    async fn release_diagnostics(&self, namespace: &str, name: &str) -> Option<StackDiagnostics> {
        let hr_ar = ApiResource {
            group: "helm.toolkit.fluxcd.io".to_string(),
            version: "v2".to_string(),
            api_version: "helm.toolkit.fluxcd.io/v2".to_string(),
            kind: "HelmRelease".to_string(),
            plural: "helmreleases".to_string(),
        };
        let hr_api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &hr_ar);
        let (mut conditions, mut metadata) = match hr_api.get(name).await {
            Ok(obj) => (
                extract_conditions(&obj.data),
                Some(extract_resource_metadata(
                    &obj,
                    "helm.toolkit.fluxcd.io/v2",
                    "HelmRelease",
                )),
            ),
            Err(_) => (Vec::new(), None),
        };

        if conditions.is_empty() && metadata.is_none() {
            let ks_ar = ApiResource {
                group: "kustomize.toolkit.fluxcd.io".to_string(),
                version: "v1".to_string(),
                api_version: "kustomize.toolkit.fluxcd.io/v1".to_string(),
                kind: "Kustomization".to_string(),
                plural: "kustomizations".to_string(),
            };
            let ks_api: Api<DynamicObject> =
                Api::namespaced_with(self.client.clone(), namespace, &ks_ar);
            if let Ok(obj) = ks_api.get(name).await {
                conditions = extract_conditions(&obj.data);
                metadata = Some(extract_resource_metadata(
                    &obj,
                    "kustomize.toolkit.fluxcd.io/v1",
                    "Kustomization",
                ));
            }
        }

        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let matched_pod = pod_api
            .list(&ListParams::default())
            .await
            .ok()
            .and_then(|list| {
                list.items.into_iter().find(|p| {
                    let pname = p.metadata.name.as_deref().unwrap_or_default();
                    let labels = p.metadata.labels.as_ref();
                    let matches_label = labels
                        .and_then(|l| l.get("app.kubernetes.io/instance"))
                        .map(|v| v == name)
                        .unwrap_or(false)
                        || labels
                            .and_then(|l| l.get("app"))
                            .map(|v| v == name)
                            .unwrap_or(false);
                    matches_label || pname == name || pname.starts_with(&format!("{name}-"))
                })
            });

        let (workspace_pod, workspace_phase, logs) = match matched_pod {
            Some(pod) => {
                let pname = pod.metadata.name.clone().unwrap_or_default();
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.clone())
                    .or_else(|| Some("Pending".to_string()));
                let log_params = LogParams {
                    tail_lines: Some(100),
                    ..Default::default()
                };
                let log_lines = match pod_api.logs(&pname, &log_params).await {
                    Ok(raw) if !raw.trim().is_empty() => raw.lines().map(str::to_string).collect(),
                    _ => {
                        let prev_params = LogParams {
                            previous: true,
                            tail_lines: Some(100),
                            ..Default::default()
                        };
                        pod_api
                            .logs(&pname, &prev_params)
                            .await
                            .map(|raw| raw.lines().map(str::to_string).collect())
                            .unwrap_or_default()
                    }
                };
                (Some(pname), phase, log_lines)
            }
            None => (None, None, Vec::new()),
        };

        let event_api: Api<K8sEvent> = Api::namespaced(self.client.clone(), namespace);
        let mut events: Vec<ClusterEventSummary> = event_api
            .list(&ListParams::default())
            .await
            .map(|list| {
                list.items
                    .into_iter()
                    .filter(|ev| {
                        let obj_name = ev.involved_object.name.as_deref().unwrap_or_default();
                        obj_name == name
                            || obj_name.starts_with(&format!("{name}-"))
                            || workspace_pod.as_deref() == Some(obj_name)
                    })
                    .map(|ev| ClusterEventSummary {
                        event_type: ev.type_.unwrap_or_else(|| "Normal".to_string()),
                        reason: ev.reason.unwrap_or_default(),
                        message: ev.message.unwrap_or_default(),
                        count: ev.count,
                        last_seen: ev
                            .last_timestamp
                            .map(|t| t.0.to_string())
                            .or_else(|| ev.metadata.creation_timestamp.map(|t| t.0.to_string())),
                    })
                    .collect()
            })
            .unwrap_or_default();
        events.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        events.truncate(20);

        Some(StackDiagnostics {
            name: name.to_string(),
            namespace: namespace.to_string(),
            metadata,
            conditions,
            workspace_pod,
            workspace_phase,
            events,
            logs,
        })
    }

    async fn restart_stack_workspace(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<String, KubeError> {
        self.invalidate_status_cache();
        let sts_name = format!("{name}-workspace");
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), namespace);
        let restarted_at = Utc::now().to_rfc3339();
        let patch = json!({
            "spec": {
                "template": {
                    "metadata": {
                        "annotations": {
                            "kubectl.kubernetes.io/restartedAt": restarted_at
                        }
                    }
                }
            }
        });
        sts_api
            .patch(
                &sts_name,
                &PatchParams::default(),
                &Patch::Strategic(&patch),
            )
            .await
            .map_err(|e| {
                not_found_or(
                    e,
                    format!("Workspace StatefulSet {namespace}/{sts_name} not found"),
                )
            })?;
        Ok(format!(
            "Triggered rollout restart for StatefulSet {namespace}/{sts_name}"
        ))
    }

    async fn reconcile_stack(&self, namespace: &str, name: &str) -> Result<String, KubeError> {
        self.invalidate_status_cache();
        let ar = ApiResource {
            group: "pulumi.com".to_string(),
            version: "v1".to_string(),
            api_version: "pulumi.com/v1".to_string(),
            kind: "Stack".to_string(),
            plural: "stacks".to_string(),
        };
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
        let requested_at = Utc::now().to_rfc3339();
        let patch = json!({
            "metadata": {
                "annotations": {
                    "pulumi.com/reconcile-request": requested_at
                }
            }
        });
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| not_found_or(e, format!("Stack {namespace}/{name} not found")))?;
        Ok(format!(
            "Requested reconciliation for Stack {namespace}/{name}"
        ))
    }

    async fn force_delete_stack(&self, namespace: &str, name: &str) -> Result<String, KubeError> {
        self.invalidate_status_cache();
        if namespace == "flux-system" && name == keel_pko::MANAGEMENT_CORE_STACK_NAME {
            return Err(KubeError::Conflict(
                "Cannot force-delete protected management-cluster-core stack".to_string(),
            ));
        }

        let ar = ApiResource {
            group: "pulumi.com".to_string(),
            version: "v1".to_string(),
            api_version: "pulumi.com/v1".to_string(),
            kind: "Stack".to_string(),
            plural: "stacks".to_string(),
        };
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
        let existing = api
            .get_opt(name)
            .await?
            .ok_or_else(|| KubeError::NotFound(format!("Stack {namespace}/{name} not found")))?;

        if is_stack_object_protected(existing.metadata.labels.as_ref(), &existing.data) {
            return Err(KubeError::Conflict(format!(
                "Cannot force-delete protected Stack {namespace}/{name} (labeled `{}: \"true\"` or `spec.config.protect: true`); remove protection via a GitOps PR first",
                keel_pko::stack::LABEL_PROTECT
            )));
        }

        let was_terminating = existing.metadata.deletion_timestamp.is_some();
        let warning_suffix = force_delete_warning_suffix(&existing.data);
        let patch = json!({
            "metadata": {
                "finalizers": Value::Null
            }
        });
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;

        if !was_terminating {
            match api.delete(name, &DeleteParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(err)) if err.code == 404 => {}
                Err(err) => return Err(err.into()),
            }
        }

        let base_msg = if was_terminating {
            format!("Cleared finalizers on terminating Stack {namespace}/{name}")
        } else {
            format!("Cleared finalizers and force-deleted Stack {namespace}/{name}")
        };
        Ok(match warning_suffix {
            Some(warn) => format!("{base_msg}. {warn}"),
            None => base_msg,
        })
    }

    async fn unlock_stack(&self, namespace: &str, name: &str) -> Result<String, KubeError> {
        self.invalidate_status_cache();
        let ar = ApiResource {
            group: "pulumi.com".to_string(),
            version: "v1".to_string(),
            api_version: "pulumi.com/v1".to_string(),
            kind: "Stack".to_string(),
            plural: "stacks".to_string(),
        };
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
        let existing = api
            .get_opt(name)
            .await?
            .ok_or_else(|| KubeError::NotFound(format!("Stack {namespace}/{name} not found")))?;

        let (backend, stack_name, flux_dir) = extract_stack_unlock_params(&existing)?;

        let pod_name = format!("{name}-workspace-0");
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        if pods.get_opt(&pod_name).await?.is_none() {
            return Err(KubeError::InvalidInput(format!(
                "Workspace pod {namespace}/{pod_name} not found; restart the workspace or wait for it to be scheduled"
            )));
        }

        let (login_ok, login_out) = exec_pod_command(
            &pods,
            &pod_name,
            "pulumi",
            vec!["pulumi".to_string(), "login".to_string(), backend],
        )
        .await?;
        if !login_ok {
            return Err(KubeError::InvalidInput(format!(
                "Failed to log into Pulumi backend in {namespace}/{pod_name}: {login_out}"
            )));
        }

        let cancel_args = build_pulumi_cancel_args(&stack_name, flux_dir.as_deref());
        let (cancel_ok, cancel_out) =
            exec_pod_command(&pods, &pod_name, "pulumi", cancel_args).await?;
        if !cancel_ok {
            return Err(KubeError::InvalidInput(format!(
                "Failed to cancel lock for stack {namespace}/{name}: {cancel_out}"
            )));
        }

        let requested_at = Utc::now().to_rfc3339();
        let reconcile_patch = json!({
            "metadata": {
                "annotations": {
                    "pulumi.com/reconcile-request": requested_at
                }
            }
        });
        api.patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(&reconcile_patch),
        )
        .await?;

        let detail = cancel_out.trim();
        if detail.is_empty() {
            Ok(format!(
                "Unlocked stack {namespace}/{name} and requested reconciliation"
            ))
        } else {
            Ok(format!(
                "Unlocked stack {namespace}/{name} and requested reconciliation: {detail}"
            ))
        }
    }

    async fn reconcile_release(&self, namespace: &str, name: &str) -> Result<String, KubeError> {
        self.invalidate_status_cache();
        let requested_at = Utc::now().to_rfc3339();
        let patch = json!({
            "metadata": {
                "annotations": {
                    "reconcile.fluxcd.io/requestedAt": requested_at
                }
            }
        });

        let hr_ar = ApiResource {
            group: "helm.toolkit.fluxcd.io".to_string(),
            version: "v2".to_string(),
            api_version: "helm.toolkit.fluxcd.io/v2".to_string(),
            kind: "HelmRelease".to_string(),
            plural: "helmreleases".to_string(),
        };
        let hr_api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &hr_ar);
        match hr_api
            .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(_) => Ok(format!(
                "Requested immediate Flux reconciliation for HelmRelease {namespace}/{name}"
            )),
            Err(kube::Error::Api(err)) if err.code == 404 => {
                let ks_ar = ApiResource {
                    group: "kustomize.toolkit.fluxcd.io".to_string(),
                    version: "v1".to_string(),
                    api_version: "kustomize.toolkit.fluxcd.io/v1".to_string(),
                    kind: "Kustomization".to_string(),
                    plural: "kustomizations".to_string(),
                };
                let ks_api: Api<DynamicObject> =
                    Api::namespaced_with(self.client.clone(), namespace, &ks_ar);
                ks_api
                    .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await?;
                Ok(format!(
                    "Requested immediate Flux reconciliation for Kustomization {namespace}/{name}"
                ))
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn stack_outputs(&self, namespace: &str, name: &str) -> HashMap<String, String> {
        let secret_name = format!("{name}-outputs");
        let direct = self.read_secret_map(namespace, &secret_name).await;
        if !direct.is_empty() {
            return direct;
        }
        use k8s_openapi::api::core::v1::Secret;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().labels("auto.pulumi.com/component=update");
        let Ok(list) = secrets.list(&lp).await else {
            return HashMap::new();
        };
        let prefix = format!("{name}-");
        let suffix = "-stack-outputs";
        let latest = list
            .items
            .into_iter()
            .filter(|s| {
                s.metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(suffix))
            })
            .max_by_key(|s| {
                s.metadata
                    .resource_version
                    .as_deref()
                    .and_then(|rv| rv.parse::<u64>().ok())
                    .unwrap_or(0)
            });
        let mut out = HashMap::new();
        if let Some(secret) = latest
            && let Some(data) = secret.data
        {
            for (k, bytes) in data {
                if let Ok(raw) = String::from_utf8(bytes.0) {
                    let parsed = serde_json::from_str::<String>(&raw)
                        .unwrap_or_else(|_| raw.trim().trim_matches('"').to_string());
                    out.insert(k, parsed);
                }
            }
        }
        out
    }

    async fn delete_dynamic_resource(
        &self,
        api_version: &str,
        kind: &str,
        namespace: &str,
        name: &str,
    ) -> Result<bool, KubeError> {
        self.invalidate_status_cache();
        let ar = match (api_version, kind) {
            ("pulumi.com/v1", "Stack") => ApiResource {
                group: "pulumi.com".to_string(),
                version: "v1".to_string(),
                api_version: "pulumi.com/v1".to_string(),
                kind: "Stack".to_string(),
                plural: "stacks".to_string(),
            },
            ("source.toolkit.fluxcd.io/v1", "GitRepository") => ApiResource {
                group: "source.toolkit.fluxcd.io".to_string(),
                version: "v1".to_string(),
                api_version: "source.toolkit.fluxcd.io/v1".to_string(),
                kind: "GitRepository".to_string(),
                plural: "gitrepositories".to_string(),
            },
            _ => {
                return Err(KubeError::InvalidInput(format!(
                    "Unsupported dynamic resource for deletion: {api_version} {kind}"
                )));
            }
        };

        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    async fn apply_cluster_policy(&self, name: &str, policy_doc: &Value) -> Result<(), KubeError> {
        self.invalidate_status_cache();
        let api_version = policy_doc
            .get("apiVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("policies.kyverno.io/v1");
        let kind = policy_doc
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("ValidatingPolicy");
        let (group, version) = match api_version.split_once('/') {
            Some((g, v)) => (g.to_string(), v.to_string()),
            None => (String::new(), api_version.to_string()),
        };
        let plural = match kind {
            "ValidatingPolicy" => "validatingpolicies".to_string(),
            other => format!("{}s", other.to_ascii_lowercase()),
        };
        let ar = ApiResource {
            group,
            version,
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            plural,
        };
        let api: Api<DynamicObject> = Api::all_with(self.client.clone(), &ar);
        api.patch(
            name,
            &PatchParams::apply("keel-server").force(),
            &Patch::Apply(policy_doc),
        )
        .await?;
        Ok(())
    }
}

/// Validates an **untrusted** (tenant-authored) kubeconfig before it is handed to
/// `kube::Config::from_custom_kubeconfig`.
///
/// Only inline credential material is permitted: `token`, `username`/`password`,
/// `client-certificate-data`, `client-key-data`, and `certificate-authority-data`.
/// The following are rejected because they lead to code execution or server-local file reads
/// inside `keel-server`:
/// * `users[].user.exec` (credential plugin binaries) and `users[].user.auth-provider`,
/// * `users[].user.tokenFile`, `client-certificate`, `client-key` (file paths),
/// * `clusters[].cluster.certificate-authority` (file path).
///
/// All users and clusters are checked (not only the current context) so a later context switch
/// cannot smuggle an unsafe entry. Returned error strings never include credential values.
pub fn validate_untrusted_kubeconfig(kc: &kube::config::Kubeconfig) -> Result<(), String> {
    fn non_empty(v: &Option<String>) -> bool {
        v.as_deref().is_some_and(|s| !s.trim().is_empty())
    }
    for named in &kc.auth_infos {
        let Some(user) = named.auth_info.as_ref() else {
            continue;
        };
        let who = &named.name;
        if user.exec.is_some() {
            return Err(format!("user '{who}' uses an exec credential plugin"));
        }
        if user.auth_provider.is_some() {
            return Err(format!("user '{who}' uses an auth-provider plugin"));
        }
        if non_empty(&user.token_file) {
            return Err(format!("user '{who}' references a tokenFile path"));
        }
        if non_empty(&user.client_certificate) {
            return Err(format!(
                "user '{who}' references a client-certificate file path"
            ));
        }
        if non_empty(&user.client_key) {
            return Err(format!("user '{who}' references a client-key file path"));
        }
    }
    for named in &kc.clusters {
        let Some(cluster) = named.cluster.as_ref() else {
            continue;
        };
        if non_empty(&cluster.certificate_authority) {
            return Err(format!(
                "cluster '{}' references a certificate-authority file path",
                named.name
            ));
        }
    }
    Ok(())
}

/// Extracts all `status.conditions` entries from a Kubernetes resource's JSON payload.
pub fn extract_resource_metadata(
    obj: &DynamicObject,
    fallback_api_version: &str,
    fallback_kind: &str,
) -> ResourceMetadata {
    let mut spec_summary = std::collections::BTreeMap::new();
    let data = &obj.data;

    let mut insert_str = |key: &str, pointer: &str| {
        if let Some(s) = data.pointer(pointer).and_then(Value::as_str)
            && !s.is_empty()
        {
            spec_summary.insert(key.to_string(), s.to_string());
        }
    };
    insert_str("stack", "/spec/stack");
    insert_str("projectRepo", "/spec/projectRepo");
    insert_str("branch", "/spec/branch");
    insert_str("chart", "/spec/chart/spec/chart");
    insert_str("chartVersion", "/spec/chart/spec/version");
    insert_str("targetNamespace", "/spec/targetNamespace");
    insert_str("targetCluster", "/spec/kubeConfig/secretRef/name");
    insert_str("serviceAccount", "/spec/serviceAccountName");
    insert_str("interval", "/spec/interval");

    if let Some(deps) = data.pointer("/spec/dependsOn").and_then(Value::as_array) {
        let dep_names: Vec<&str> = deps
            .iter()
            .filter_map(|d| d.get("name").and_then(Value::as_str))
            .collect();
        if !dep_names.is_empty() {
            spec_summary.insert("dependsOn".to_string(), dep_names.join(", "));
        }
    }

    ResourceMetadata {
        api_version: obj
            .types
            .as_ref()
            .map(|t| t.api_version.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| Some(fallback_api_version.to_string())),
        kind: obj
            .types
            .as_ref()
            .map(|t| t.kind.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| Some(fallback_kind.to_string())),
        uid: obj.metadata.uid.clone(),
        creation_timestamp: obj
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.to_string()),
        deletion_timestamp: obj
            .metadata
            .deletion_timestamp
            .as_ref()
            .map(|t| t.0.to_string()),
        labels: obj.metadata.labels.clone().unwrap_or_default(),
        annotations: obj
            .metadata
            .annotations
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| k != "kubectl.kubernetes.io/last-applied-configuration")
            .collect(),
        spec_summary,
    }
}

pub fn extract_conditions(data: &Value) -> Vec<ResourceCondition> {
    let Some(arr) = data.pointer("/status/conditions").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|c| {
            let condition_type = c.get("type").and_then(Value::as_str)?.to_string();
            let status = c.get("status").and_then(Value::as_str)?.to_string();
            let reason = c
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let message = c
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let last_transition_time = c
                .get("lastTransitionTime")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(ResourceCondition {
                condition_type,
                status,
                reason,
                message,
                last_transition_time,
            })
        })
        .collect()
}

/// Extracts `Ready` condition from a Kubernetes resource's JSON payload.
pub fn extract_ready_status(data: &Value, is_stack: bool) -> Option<LiveResourceStatus> {
    let conditions = data.pointer("/status/conditions")?.as_array()?;
    let reconciling_true = conditions.iter().any(|c| {
        c.get("type").and_then(Value::as_str) == Some("Reconciling")
            && c.get("status").and_then(Value::as_str) == Some("True")
    });
    let ready = conditions
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))?;
    let status = ready.get("status").and_then(Value::as_str)?;
    let reason = ready
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match status {
        "True" => Some(if is_stack {
            LiveResourceStatus::Synced
        } else {
            LiveResourceStatus::Ready
        }),
        "False" => {
            if reconciling_true
                || reason.contains("Progressing")
                || reason.contains("InProgress")
                || reason.contains("Reconciling")
                || reason.contains("DependencyNotReady")
            {
                Some(LiveResourceStatus::Reconciling)
            } else {
                Some(LiveResourceStatus::Error)
            }
        }
        _ => Some(LiveResourceStatus::Reconciling),
    }
}

/// Extracts the most recent sync/reconciliation timestamp (RFC3339) from a Flux or PKO resource.
pub fn extract_last_synced(data: &Value, creation_timestamp: Option<&str>) -> Option<String> {
    if let Some(ts) = data
        .pointer("/status/lastUpdate/lastResyncTime")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Some(ts.to_string());
    }
    if let Some(ts) = data
        .pointer("/status/history/0/lastDeployed")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Some(ts.to_string());
    }
    if let Some(ts) = data
        .pointer("/status/lastHandledReconcileAt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Some(ts.to_string());
    }
    if let Some(conditions) = data.pointer("/status/conditions").and_then(Value::as_array)
        && let Some(ts) = conditions
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
            .and_then(|c| c.get("lastTransitionTime"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    {
        return Some(ts.to_string());
    }
    creation_timestamp
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Extracts the managed resource count from a Flux `Kustomization` or PKO v2 `Stack` status.
pub fn extract_resource_count(data: &Value) -> Option<u32> {
    if let Some(entries) = data
        .pointer("/status/inventory/entries")
        .and_then(Value::as_array)
    {
        return Some(entries.len() as u32);
    }
    if let Some(changes) = data
        .pointer("/status/lastUpdate/resourceChanges")
        .and_then(Value::as_object)
    {
        let total: u64 = changes
            .iter()
            .filter(|(op, _)| op.as_str() != "delete")
            .filter_map(|(_, v)| v.as_u64())
            .sum();
        return Some(total as u32);
    }
    if let Some(count) = data
        .pointer("/status/resourceCount")
        .and_then(Value::as_u64)
    {
        return Some(count as u32);
    }
    None
}

fn percent_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit((b >> 4) as u32, 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((b & 0x0f) as u32, 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

fn parse_prometheus_vector_response(body: &Value) -> HashMap<(String, String), f64> {
    let mut out = HashMap::new();
    if body.get("status").and_then(Value::as_str) != Some("success") {
        return out;
    }
    let Some(results) = body.pointer("/data/result").and_then(Value::as_array) else {
        return out;
    };
    for item in results {
        let Some(route) = item.pointer("/metric/keel_route").and_then(Value::as_str) else {
            continue;
        };
        let Some((ns, target)) = route.split_once('/') else {
            continue;
        };
        let Some(raw_val) = item
            .get("value")
            .and_then(Value::as_array)
            .and_then(|arr| arr.get(1))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if let Ok(val) = raw_val.parse::<f64>()
            && val.is_finite()
        {
            out.insert((ns.to_string(), target.to_string()), val);
        }
    }
    out
}

fn extract_stack_unlock_params(
    stack_obj: &DynamicObject,
) -> Result<(String, String, Option<String>), KubeError> {
    let backend = stack_obj
        .data
        .pointer("/spec/backend")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            KubeError::InvalidInput("Stack spec.backend is missing or empty".to_string())
        })?
        .to_string();

    let stack_name = stack_obj
        .data
        .pointer("/spec/stack")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| KubeError::InvalidInput("Stack spec.stack is missing or empty".to_string()))?
        .to_string();

    let flux_dir = stack_obj
        .data
        .pointer("/spec/fluxSource/dir")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(ref dir) = flux_dir
        && (dir.starts_with('/') || dir.split('/').any(|seg| seg == ".."))
    {
        return Err(KubeError::InvalidInput(format!(
            "Invalid spec.fluxSource.dir: {dir}"
        )));
    }

    Ok((backend, stack_name, flux_dir))
}

fn build_pulumi_cancel_args(stack_name: &str, flux_dir: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "pulumi".to_string(),
        "cancel".to_string(),
        "--stack".to_string(),
        stack_name.to_string(),
        "--yes".to_string(),
    ];
    if let Some(dir) = flux_dir {
        let trimmed = dir.trim_matches('/');
        if !trimmed.is_empty() {
            args.push("-C".to_string());
            args.push(format!("/share/source/{trimmed}"));
        }
    }
    args
}

async fn exec_pod_command(
    pods: &Api<Pod>,
    pod_name: &str,
    container: &str,
    command: Vec<String>,
) -> Result<(bool, String), KubeError> {
    let ap = AttachParams::default()
        .container(container)
        .stdout(true)
        .stderr(true)
        .stdin(false)
        .tty(false);
    let mut attached = pods.exec(pod_name, command, &ap).await?;

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    if let Some(mut stdout) = attached.stdout() {
        let _ = stdout.read_to_string(&mut stdout_buf).await;
    }
    if let Some(mut stderr) = attached.stderr() {
        let _ = stderr.read_to_string(&mut stderr_buf).await;
    }

    let ok = match attached.take_status() {
        Some(status_fut) => match status_fut.await {
            Some(status) => status.status.as_deref() == Some("Success"),
            None => true,
        },
        None => true,
    };

    let combined = match (stdout_buf.trim(), stderr_buf.trim()) {
        ("", "") => String::new(),
        (out, "") => out.to_string(),
        ("", err) => err.to_string(),
        (out, err) => format!("{out}\n{err}"),
    };
    Ok((ok, combined))
}

pub fn force_delete_warning_suffix(data: &Value) -> Option<String> {
    let resource_count = extract_resource_count(data).unwrap_or(0);
    let last_state = data
        .pointer("/status/lastUpdate/state")
        .and_then(Value::as_str);
    if resource_count > 0 {
        Some(format!(
            "Warning: {resource_count} tracked cloud resource(s) were not destroyed by Pulumi and may need manual cleanup."
        ))
    } else if last_state == Some("failed") {
        Some(
            "Warning: the last update failed; partially provisioned cloud resources were not destroyed by Pulumi and may need manual cleanup."
                .to_string(),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_kc(yaml: &str) -> kube::config::Kubeconfig {
        kube::config::Kubeconfig::from_yaml(yaml).expect("valid kubeconfig yaml")
    }

    const SAFE_KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: workload
  cluster:
    server: https://203.0.113.10:6443
    certificate-authority-data: Y2E=
users:
- name: admin
  user:
    client-certificate-data: Y2VydA==
    client-key-data: a2V5
- name: tok
  user:
    token: abc
- name: basic
  user:
    username: u
    password: p
contexts:
- name: workload
  context:
    cluster: workload
    user: admin
current-context: workload
"#;

    #[test]
    fn validate_untrusted_kubeconfig_accepts_inline_credentials() {
        assert!(validate_untrusted_kubeconfig(&parse_kc(SAFE_KUBECONFIG)).is_ok());
    }

    #[test]
    fn validate_untrusted_kubeconfig_rejects_exec_and_auth_provider() {
        let exec = SAFE_KUBECONFIG.replace(
            "    token: abc\n",
            "    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: /bin/sh\n      args: [\"-c\", \"id\"]\n",
        );
        let err = validate_untrusted_kubeconfig(&parse_kc(&exec)).unwrap_err();
        assert!(err.contains("exec"), "{err}");
        assert!(!err.contains("/bin/sh"));

        let ap = SAFE_KUBECONFIG.replace(
            "    token: abc\n",
            "    auth-provider:\n      name: gcp\n      config:\n        cmd-path: /bin/sh\n",
        );
        let err = validate_untrusted_kubeconfig(&parse_kc(&ap)).unwrap_err();
        assert!(err.contains("auth-provider"), "{err}");
    }

    #[test]
    fn validate_untrusted_kubeconfig_rejects_file_path_credentials() {
        for (needle, replacement, expect) in [
            (
                "    token: abc\n",
                "    tokenFile: /var/run/secrets/kubernetes.io/serviceaccount/token\n",
                "tokenFile",
            ),
            (
                "    client-certificate-data: Y2VydA==\n",
                "    client-certificate: /etc/keel/tls.crt\n",
                "client-certificate",
            ),
            (
                "    client-key-data: a2V5\n",
                "    client-key: /etc/keel/tls.key\n",
                "client-key",
            ),
            (
                "    certificate-authority-data: Y2E=\n",
                "    certificate-authority: /etc/ssl/ca.crt\n",
                "certificate-authority",
            ),
        ] {
            let yaml = SAFE_KUBECONFIG.replace(needle, replacement);
            assert_ne!(yaml, SAFE_KUBECONFIG, "replacement for {expect} must apply");
            let err = validate_untrusted_kubeconfig(&parse_kc(&yaml)).unwrap_err();
            assert!(err.contains(expect), "{err}");
            assert!(!err.contains("/etc/") && !err.contains("/var/"));
        }
    }

    #[test]
    fn validate_untrusted_kubeconfig_checks_non_current_users() {
        // Unsafe user is not referenced by current-context but must still be rejected.
        let yaml = SAFE_KUBECONFIG.replace(
            "- name: basic\n  user:\n    username: u\n    password: p\n",
            "- name: basic\n  user:\n    tokenFile: /etc/passwd\n",
        );
        assert_ne!(yaml, SAFE_KUBECONFIG);
        assert!(validate_untrusted_kubeconfig(&parse_kc(&yaml)).is_err());
    }
    use serde_json::json;

    #[test]
    fn extracts_stack_and_release_conditions() {
        let synced = json!({
            "status": {
                "conditions": [
                    { "type": "Ready", "status": "True", "reason": "Succeeded" }
                ]
            }
        });
        assert_eq!(
            extract_ready_status(&synced, true),
            Some(LiveResourceStatus::Synced)
        );
        assert_eq!(
            extract_ready_status(&synced, false),
            Some(LiveResourceStatus::Ready)
        );

        let reconciling = json!({
            "status": {
                "conditions": [
                    { "type": "Ready", "status": "False", "reason": "Reconciling" }
                ]
            }
        });
        assert_eq!(
            extract_ready_status(&reconciling, true),
            Some(LiveResourceStatus::Reconciling)
        );

        let failed = json!({
            "status": {
                "conditions": [
                    { "type": "Ready", "status": "False", "reason": "InstallFailed" }
                ]
            }
        });
        assert_eq!(
            extract_ready_status(&failed, false),
            Some(LiveResourceStatus::Error)
        );

        let no_status = json!({ "metadata": { "name": "foo" } });
        assert_eq!(extract_ready_status(&no_status, true), None);
    }

    #[test]
    fn extracts_last_synced_and_resource_count() {
        let pko_stack = json!({
            "status": {
                "lastUpdate": {
                    "lastResyncTime": "2026-11-15T10:15:00Z",
                    "resourceChanges": { "create": 2, "same": 4, "delete": 1 }
                }
            }
        });
        assert_eq!(
            extract_last_synced(&pko_stack, None).as_deref(),
            Some("2026-11-15T10:15:00Z")
        );
        assert_eq!(extract_resource_count(&pko_stack), Some(6));

        let flux_ks = json!({
            "status": {
                "inventory": {
                    "entries": [
                        { "id": "a" },
                        { "id": "b" },
                        { "id": "c" }
                    ]
                },
                "conditions": [
                    { "type": "Ready", "status": "True", "lastTransitionTime": "2026-11-15T09:00:00Z" }
                ]
            }
        });
        assert_eq!(
            extract_last_synced(&flux_ks, None).as_deref(),
            Some("2026-11-15T09:00:00Z")
        );
        assert_eq!(extract_resource_count(&flux_ks), Some(3));
    }

    #[test]
    fn percent_encodes_promql_queries() {
        let encoded = percent_encode_query(r#"rate(x{a="1/2"}[2m]) * 100"#);
        assert!(!encoded.contains(' '));
        assert!(!encoded.contains('{'));
        assert!(!encoded.contains('"'));
        assert!(encoded.contains("rate%28x%7Ba%3D%221%2F2%22%7D%5B2m%5D%29"));
    }

    #[test]
    fn parses_prometheus_vector_response_and_filters_non_finite() {
        let body = json!({
            "status": "success",
            "data": {
                "resultType": "vector",
                "result": [
                    {
                        "metric": { "keel_route": "tenant-acme/podinfo" },
                        "value": [1700000000.0, "0.25"]
                    },
                    {
                        "metric": { "keel_route": "tenant-acme/empty" },
                        "value": [1700000000.0, "NaN"]
                    },
                    {
                        "metric": { "keel_route": "tenant-acme/inf" },
                        "value": [1700000000.0, "+Inf"]
                    }
                ]
            }
        });
        let parsed = parse_prometheus_vector_response(&body);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed.get(&("tenant-acme".to_string(), "podinfo".to_string())),
            Some(&0.25)
        );

        let err_body = json!({ "status": "error", "error": "bad query" });
        assert!(parse_prometheus_vector_response(&err_body).is_empty());
    }

    #[test]
    fn extracts_stack_unlock_params_and_builds_cancel_args() {
        let stack: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": "azure-free-tier-vm", "namespace": "tenant-default" },
            "spec": {
                "backend": "s3://pulumi-state?endpoint=s3.local:8333",
                "stack": "organization/azure-free-tier-vm/default-azure-free-tier-vm",
                "fluxSource": {
                    "dir": "tenants/default/infra/azure-free-tier-vm"
                }
            }
        }))
        .unwrap();

        let (backend, stack_name, dir) = extract_stack_unlock_params(&stack).unwrap();
        assert_eq!(backend, "s3://pulumi-state?endpoint=s3.local:8333");
        assert_eq!(
            stack_name,
            "organization/azure-free-tier-vm/default-azure-free-tier-vm"
        );
        assert_eq!(
            dir.as_deref(),
            Some("tenants/default/infra/azure-free-tier-vm")
        );

        let args = build_pulumi_cancel_args(&stack_name, dir.as_deref());
        assert_eq!(
            args,
            vec![
                "pulumi",
                "cancel",
                "--stack",
                "organization/azure-free-tier-vm/default-azure-free-tier-vm",
                "--yes",
                "-C",
                "/share/source/tenants/default/infra/azure-free-tier-vm",
            ]
        );
    }

    #[test]
    fn rejects_invalid_stack_unlock_params() {
        let missing_backend: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": "test" },
            "spec": { "stack": "org/proj/dev" }
        }))
        .unwrap();
        assert!(extract_stack_unlock_params(&missing_backend).is_err());

        let traversal_dir: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": "test" },
            "spec": {
                "backend": "s3://pulumi-state",
                "stack": "org/proj/dev",
                "fluxSource": { "dir": "../../etc" }
            }
        }))
        .unwrap();
        assert!(extract_stack_unlock_params(&traversal_dir).is_err());
    }

    #[test]
    fn force_delete_warning_suffix_flags_tracked_or_failed_resources() {
        let with_resources = json!({
            "status": {
                "lastUpdate": { "state": "succeeded", "resourceChanges": { "create": 5 } }
            }
        });
        assert!(
            force_delete_warning_suffix(&with_resources)
                .unwrap()
                .contains("5 tracked cloud resource(s)")
        );

        let failed_partial = json!({
            "status": {
                "lastUpdate": { "state": "failed" }
            }
        });
        assert!(
            force_delete_warning_suffix(&failed_partial)
                .unwrap()
                .contains("partially provisioned cloud resources")
        );

        let clean = json!({
            "status": {
                "lastUpdate": { "state": "succeeded" }
            }
        });
        assert!(force_delete_warning_suffix(&clean).is_none());
    }

    #[test]
    fn extract_resource_metadata_includes_deletion_timestamp() {
        let obj: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": {
                "name": "azure-free-tier-vm",
                "deletionTimestamp": "2026-11-19T05:26:42Z"
            }
        }))
        .unwrap();
        let meta = extract_resource_metadata(&obj, "pulumi.com/v1", "Stack");
        assert!(meta.deletion_timestamp.is_some());
    }

    struct StubLiveStatus;

    #[async_trait]
    impl LiveStatusSource for StubLiveStatus {
        async fn stack_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::from([(
                ("tenant-a".to_string(), "s1".to_string()),
                LiveResourceStatus::Ready,
            )])
        }

        async fn release_statuses(&self) -> HashMap<(String, String), LiveResourceStatus> {
            HashMap::from([(
                ("tenant-a".to_string(), "r1".to_string()),
                LiveResourceStatus::Synced,
            )])
        }
    }

    #[tokio::test]
    async fn live_status_source_default_methods_cover_fallbacks() {
        let stub = StubLiveStatus;
        let s_details = stub.stack_status_details().await;
        assert_eq!(
            s_details
                .get(&("tenant-a".to_string(), "s1".to_string()))
                .unwrap()
                .status,
            LiveResourceStatus::Ready
        );
        let r_details = stub.release_status_details().await;
        assert_eq!(
            r_details
                .get(&("tenant-a".to_string(), "r1".to_string()))
                .unwrap()
                .status,
            LiveResourceStatus::Synced
        );
        assert!(stub.canary_statuses().await.is_empty());
        assert!(stub.stack_diagnostics("tenant-a", "s1").await.is_none());
        assert!(stub.release_diagnostics("tenant-a", "r1").await.is_none());
        assert!(
            stub.restart_stack_workspace("tenant-a", "s1")
                .await
                .is_err()
        );
        assert!(stub.reconcile_stack("tenant-a", "s1").await.is_err());
        assert!(stub.force_delete_stack("tenant-a", "s1").await.is_err());
        assert!(stub.unlock_stack("tenant-a", "s1").await.is_err());
        assert!(stub.reconcile_release("tenant-a", "r1").await.is_err());
        assert!(stub.stack_outputs("tenant-a", "s1").await.is_empty());
        assert!(
            !stub
                .delete_dynamic_resource("pulumi.com/v1", "Stack", "keel-previews", "s1")
                .await
                .unwrap()
        );
        assert!(
            stub.apply_cluster_policy("restrict-pulumi-providers", &json!({}))
                .await
                .is_ok()
        );
    }

    async fn spawn_mock_k8s_server<F>(handler: F) -> String
    where
        F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = std::sync::Arc::new(handler);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = std::sync::Arc::clone(&handler);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.split();
                    let mut reader = BufReader::new(read_half);
                    loop {
                        let mut request_line = String::new();
                        match reader.read_line(&mut request_line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let parts: Vec<&str> = request_line.split_whitespace().collect();
                        if parts.len() < 2 {
                            break;
                        }
                        let method = parts[0].to_string();
                        let path = parts[1].to_string();

                        let mut content_length: usize = 0;
                        loop {
                            let mut hdr = String::new();
                            if reader.read_line(&mut hdr).await.unwrap_or(0) == 0 {
                                break;
                            }
                            let trimmed = hdr.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = trimmed.split_once(':')
                                && k.eq_ignore_ascii_case("content-length")
                            {
                                content_length = v.trim().parse().unwrap_or(0);
                            }
                        }
                        if content_length > 0 {
                            let mut buf = vec![0u8; content_length];
                            let _ =
                                tokio::io::AsyncReadExt::read_exact(&mut reader, &mut buf).await;
                        }

                        let (status_code, body) = handler(&method, &path);
                        let reason = match status_code {
                            200 => "OK",
                            404 => "Not Found",
                            _ => "Internal Server Error",
                        };
                        let resp = format!(
                            "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                            body.len()
                        );
                        if write_half.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        format!("http://127.0.0.1:{}", addr.port())
    }

    fn mock_k8s_client(base_url: &str) -> Client {
        let cfg = kube::Config::new(base_url.parse().unwrap());
        Client::try_from(cfg).unwrap()
    }

    #[tokio::test]
    async fn kube_cluster_status_reader_queries_stacks_releases_canaries_secrets_and_actions() {
        let url = spawn_mock_k8s_server(|method, path| {
            let path_only = path.split('?').next().unwrap_or(path);
            match (method, path_only) {
                ("GET", "/api/v1/namespaces/tenant-a/secrets/my-stack-outputs") => (
                    200,
                    json!({
                        "apiVersion": "v1",
                        "kind": "Secret",
                        "metadata": { "name": "my-stack-outputs", "namespace": "tenant-a" },
                        "data": {
                            "endpoint": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"10.0.0.5")
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/apis/pulumi.com/v1/stacks") => (
                    200,
                    json!({
                        "apiVersion": "pulumi.com/v1",
                        "kind": "StackList",
                        "metadata": {},
                        "items": [
                            {
                                "apiVersion": "pulumi.com/v1",
                                "kind": "Stack",
                                "metadata": {
                                    "name": "my-stack",
                                    "namespace": "tenant-a",
                                    "creationTimestamp": "2026-11-19T00:00:00Z"
                                },
                                "spec": { "backend": "s3://keel-state" },
                                "status": {
                                    "conditions": [{ "type": "Ready", "status": "True", "reason": "Succeeded" }],
                                    "lastUpdate": { "lastResyncTime": "2026-11-19T01:00:00Z", "resourceChanges": { "create": 3 } }
                                }
                            },
                            {
                                "apiVersion": "pulumi.com/v1",
                                "kind": "Stack",
                                "metadata": {
                                    "name": "deleting-stack",
                                    "namespace": "tenant-a",
                                    "deletionTimestamp": "2026-11-19T02:00:00Z"
                                }
                            }
                        ]
                    })
                    .to_string(),
                ),
                ("GET", "/apis/kustomize.toolkit.fluxcd.io/v1/kustomizations") => (
                    200,
                    json!({
                        "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
                        "kind": "KustomizationList",
                        "metadata": {},
                        "items": [
                            {
                                "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
                                "kind": "Kustomization",
                                "metadata": { "name": "management-cluster-core", "namespace": "flux-system" },
                                "status": {
                                    "conditions": [{ "type": "Ready", "status": "True", "reason": "ReconciliationSucceeded" }]
                                }
                            },
                            {
                                "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
                                "kind": "Kustomization",
                                "metadata": { "name": "stack-wrapper", "namespace": "tenant-a" },
                                "spec": {
                                    "healthChecks": [{ "kind": "Stack", "name": "my-stack" }]
                                }
                            }
                        ]
                    })
                    .to_string(),
                ),
                ("GET", "/apis/helm.toolkit.fluxcd.io/v2/helmreleases") => (
                    200,
                    json!({
                        "apiVersion": "helm.toolkit.fluxcd.io/v2",
                        "kind": "HelmReleaseList",
                        "metadata": {},
                        "items": [
                            {
                                "apiVersion": "helm.toolkit.fluxcd.io/v2",
                                "kind": "HelmRelease",
                                "metadata": { "name": "podinfo", "namespace": "tenant-a" },
                                "status": {
                                    "conditions": [{ "type": "Ready", "status": "True", "reason": "InstallSucceeded" }]
                                }
                            }
                        ]
                    })
                    .to_string(),
                ),
                ("GET", "/apis/flagger.app/v1beta1/canaries") => (
                    200,
                    json!({
                        "apiVersion": "flagger.app/v1beta1",
                        "kind": "CanaryList",
                        "metadata": {},
                        "items": [
                            {
                                "apiVersion": "flagger.app/v1beta1",
                                "kind": "Canary",
                                "metadata": { "name": "podinfo", "namespace": "tenant-a" },
                                "spec": { "targetRef": { "name": "podinfo" } },
                                "status": {
                                    "phase": "Succeeded",
                                    "canaryWeight": 0,
                                    "lastTransitionTime": "2026-11-19T01:00:00Z"
                                }
                            }
                        ]
                    })
                    .to_string(),
                ),
                ("GET", "/apis/apps/v1/deployments") => (
                    200,
                    json!({
                        "apiVersion": "apps/v1",
                        "kind": "DeploymentList",
                        "metadata": {},
                        "items": [
                            {
                                "apiVersion": "apps/v1",
                                "kind": "Deployment",
                                "metadata": {
                                    "name": "podinfo-primary",
                                    "namespace": "tenant-a",
                                    "labels": { "app.kubernetes.io/version": "6.5.0" }
                                }
                            }
                        ]
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/namespaces/keel-system/services") => (
                    200,
                    json!({
                        "apiVersion": "v1",
                        "kind": "ServiceList",
                        "metadata": {},
                        "items": [{ "metadata": { "name": "prometheus" } }]
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/namespaces/keel-system/services/http:prometheus:80/proxy/api/v1/query") => (
                    200,
                    json!({
                        "status": "success",
                        "data": {
                            "resultType": "vector",
                            "result": [{
                                "metric": { "keel_route": "tenant-a/podinfo" },
                                "value": [1700000000, "1.25"]
                            }]
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/secrets") => (
                    200,
                    json!({
                        "apiVersion": "v1",
                        "kind": "SecretList",
                        "metadata": {},
                        "items": []
                    })
                    .to_string(),
                ),
                ("GET", "/apis/pulumi.com/v1/namespaces/tenant-a/stacks/my-stack") => (
                    200,
                    json!({
                        "apiVersion": "pulumi.com/v1",
                        "kind": "Stack",
                        "metadata": { "name": "my-stack", "namespace": "tenant-a" },
                        "spec": { "backend": "s3://keel-state" },
                        "status": {
                            "conditions": [{ "type": "Ready", "status": "True", "reason": "Succeeded" }],
                            "lastUpdate": { "state": "succeeded", "resourceChanges": { "create": 2 } }
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/namespaces/flux-system/services/keel-docker-proxy") => (
                    200,
                    json!({ "apiVersion": "v1", "kind": "Service", "metadata": { "name": "keel-docker-proxy" } }).to_string(),
                ),
                ("GET", "/api/v1/namespaces/tenant-a/pods/my-stack-workspace-0") => (
                    200,
                    json!({
                        "apiVersion": "v1",
                        "kind": "Pod",
                        "metadata": { "name": "my-stack-workspace-0", "namespace": "tenant-a" },
                        "status": { "phase": "Running" }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/namespaces/tenant-a/pods/my-stack-workspace-0/log") => (
                    200,
                    "Updating stack...\nSucceeded\n".to_string(),
                ),
                ("GET", "/api/v1/namespaces/tenant-a/events") => (
                    200,
                    json!({
                        "apiVersion": "v1",
                        "kind": "EventList",
                        "metadata": {},
                        "items": [
                            {
                                "metadata": { "name": "ev-1", "namespace": "tenant-a" },
                                "involvedObject": { "name": "my-stack" },
                                "type": "Normal",
                                "reason": "Updated",
                                "message": "Stack updated",
                                "count": 1
                            },
                            {
                                "metadata": { "name": "ev-2", "namespace": "tenant-a" },
                                "involvedObject": { "name": "podinfo" },
                                "type": "Normal",
                                "reason": "Synced",
                                "message": "HelmRelease synced",
                                "count": 1
                            }
                        ]
                    })
                    .to_string(),
                ),
                ("GET", "/apis/helm.toolkit.fluxcd.io/v2/namespaces/tenant-a/helmreleases/podinfo") => (
                    200,
                    json!({
                        "apiVersion": "helm.toolkit.fluxcd.io/v2",
                        "kind": "HelmRelease",
                        "metadata": { "name": "podinfo", "namespace": "tenant-a" },
                        "status": { "conditions": [{ "type": "Ready", "status": "True", "reason": "InstallSucceeded" }] }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/namespaces/tenant-a/pods") => (
                    200,
                    json!({
                        "apiVersion": "v1",
                        "kind": "PodList",
                        "metadata": {},
                        "items": [{
                            "apiVersion": "v1",
                            "kind": "Pod",
                            "metadata": {
                                "name": "podinfo-abc",
                                "namespace": "tenant-a",
                                "labels": { "app.kubernetes.io/instance": "podinfo" }
                            },
                            "status": { "phase": "Running" }
                        }]
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/namespaces/tenant-a/pods/podinfo-abc/log") => (
                    200,
                    "podinfo listening on 9898\n".to_string(),
                ),
                ("PATCH", "/apis/apps/v1/namespaces/tenant-a/statefulsets/my-stack-workspace") => (
                    200,
                    json!({
                        "apiVersion": "apps/v1",
                        "kind": "StatefulSet",
                        "metadata": { "name": "my-stack-workspace", "namespace": "tenant-a" }
                    })
                    .to_string(),
                ),
                ("PATCH", "/apis/pulumi.com/v1/namespaces/tenant-a/stacks/my-stack")
                | ("DELETE", "/apis/pulumi.com/v1/namespaces/tenant-a/stacks/my-stack")
                | ("PATCH", "/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/tenant-a/kustomizations/ks-only") => (
                    200,
                    json!({ "apiVersion": "v1", "kind": "Status", "status": "Success" }).to_string(),
                ),
                ("PATCH", "/apis/helm.toolkit.fluxcd.io/v2/namespaces/tenant-a/helmreleases/podinfo") => (
                    200,
                    json!({ "apiVersion": "helm.toolkit.fluxcd.io/v2", "kind": "HelmRelease" }).to_string(),
                ),
                ("PATCH", "/apis/helm.toolkit.fluxcd.io/v2/namespaces/tenant-a/helmreleases/ks-only") => (
                    404,
                    json!({ "kind": "Status", "status": "Failure", "code": 404, "message": "not found", "reason": "NotFound" }).to_string(),
                ),
                ("DELETE", "/apis/pulumi.com/v1/namespaces/keel-previews/stacks/pr-1-stack")
                | ("DELETE", "/apis/source.toolkit.fluxcd.io/v1/namespaces/keel-previews/gitrepositories/pr-1-repo") => (
                    200,
                    json!({ "apiVersion": "v1", "kind": "Status", "status": "Success" }).to_string(),
                ),
                ("PATCH", "/apis/policies.kyverno.io/v1/validatingpolicies/restrict-pulumi-providers") => (
                    200,
                    json!({
                        "apiVersion": "policies.kyverno.io/v1",
                        "kind": "ValidatingPolicy",
                        "metadata": { "name": "restrict-pulumi-providers" }
                    })
                    .to_string(),
                ),
                _ => (
                    404,
                    json!({ "kind": "Status", "status": "Failure", "code": 404, "message": "not found", "reason": "NotFound" }).to_string(),
                ),
            }
        })
        .await;

        let reader = KubeClusterStatusReader::from_client(mock_k8s_client(&url));
        let _ = reader.client();

        assert_eq!(
            reader
                .read_secret_key("tenant-a", "my-stack-outputs", "endpoint")
                .await
                .as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            reader
                .stack_outputs("tenant-a", "my-stack")
                .await
                .get("endpoint")
                .map(String::as_str),
            Some("10.0.0.5")
        );
        assert_eq!(
            reader
                .read_stack_backend("tenant-a", "my-stack")
                .await
                .as_deref(),
            Some("s3://keel-state")
        );
        assert!(reader.has_docker_proxy_service().await);

        let stacks = reader.stack_statuses().await;
        assert_eq!(
            stacks.get(&("tenant-a".to_string(), "my-stack".to_string())),
            Some(&LiveResourceStatus::Synced)
        );
        assert_eq!(
            stacks.get(&("tenant-a".to_string(), "deleting-stack".to_string())),
            Some(&LiveResourceStatus::Deleting)
        );
        assert_eq!(
            stacks.get(&(
                "flux-system".to_string(),
                "management-cluster-core".to_string()
            )),
            Some(&LiveResourceStatus::Synced)
        );

        let stack_details = reader.stack_status_details().await;
        assert!(stack_details.contains_key(&("tenant-a".to_string(), "my-stack".to_string())));

        let releases = reader.release_statuses().await;
        assert_eq!(
            releases.get(&("tenant-a".to_string(), "podinfo".to_string())),
            Some(&LiveResourceStatus::Ready)
        );
        let release_details = reader.release_status_details().await;
        assert!(release_details.contains_key(&("tenant-a".to_string(), "podinfo".to_string())));

        let canaries = reader.canary_statuses().await;
        let canary = canaries
            .get(&("tenant-a".to_string(), "podinfo".to_string()))
            .unwrap();
        assert_eq!(canary.status, LiveResourceStatus::Ready);
        assert_eq!(canary.primary_version.as_deref(), Some("6.5.0"));
        assert_eq!(canary.error_rate, Some(1.25));

        let stack_diag = reader
            .stack_diagnostics("tenant-a", "my-stack")
            .await
            .unwrap();
        assert_eq!(stack_diag.workspace_phase.as_deref(), Some("Running"));
        assert_eq!(stack_diag.events.len(), 1);
        assert_eq!(stack_diag.logs, vec!["Updating stack...", "Succeeded"]);

        let rel_diag = reader
            .release_diagnostics("tenant-a", "podinfo")
            .await
            .unwrap();
        assert_eq!(rel_diag.workspace_pod.as_deref(), Some("podinfo-abc"));
        assert_eq!(rel_diag.events.len(), 1);

        assert!(
            reader
                .restart_stack_workspace("tenant-a", "my-stack")
                .await
                .is_ok()
        );
        assert!(reader.reconcile_stack("tenant-a", "my-stack").await.is_ok());
        assert!(
            reader
                .reconcile_release("tenant-a", "podinfo")
                .await
                .is_ok()
        );
        assert!(
            reader
                .reconcile_release("tenant-a", "ks-only")
                .await
                .is_ok()
        );

        // Protected stack cannot be force-deleted
        assert!(
            reader
                .force_delete_stack("flux-system", "management-cluster-core")
                .await
                .is_err()
        );
        let force_msg = reader
            .force_delete_stack("tenant-a", "my-stack")
            .await
            .unwrap();
        assert!(force_msg.contains("Cleared finalizers and force-deleted Stack tenant-a/my-stack"));

        assert!(
            reader
                .delete_dynamic_resource("pulumi.com/v1", "Stack", "keel-previews", "pr-1-stack")
                .await
                .unwrap()
        );
        assert!(
            reader
                .delete_dynamic_resource(
                    "source.toolkit.fluxcd.io/v1",
                    "GitRepository",
                    "keel-previews",
                    "pr-1-repo"
                )
                .await
                .unwrap()
        );
        assert!(
            !reader
                .delete_dynamic_resource("pulumi.com/v1", "Stack", "keel-previews", "missing-stack")
                .await
                .unwrap()
        );
        assert!(
            reader
                .delete_dynamic_resource("v1", "ConfigMap", "keel-previews", "cm")
                .await
                .is_err()
        );

        let policy_doc = json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "ValidatingPolicy",
            "metadata": { "name": "restrict-pulumi-providers" }
        });
        assert!(
            reader
                .apply_cluster_policy("restrict-pulumi-providers", &policy_doc)
                .await
                .is_ok()
        );
    }

    #[test]
    fn stack_protection_matches_pko_is_protected_semantics() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert(
            keel_pko::stack::LABEL_PROTECT.to_string(),
            "true".to_string(),
        );
        assert!(is_stack_object_protected(Some(&labels), &json!({})));

        // The legacy/incorrect `keel.io/protected` label must NOT count.
        let mut wrong = std::collections::BTreeMap::new();
        wrong.insert("keel.io/protected".to_string(), "true".to_string());
        assert!(!is_stack_object_protected(Some(&wrong), &json!({})));

        assert!(is_stack_object_protected(
            None,
            &json!({ "spec": { "config": { "protect": "true" } } })
        ));
        assert!(is_stack_object_protected(
            None,
            &json!({ "spec": { "config": { "keel:protect": "true" } } })
        ));
        // Nonexistent top-level `spec.protect` must not count (PKO has no such field).
        assert!(!is_stack_object_protected(
            None,
            &json!({ "spec": { "protect": true } })
        ));
        assert!(!is_stack_object_protected(
            None,
            &json!({ "spec": { "config": { "protect": "false" } } })
        ));

        assert!(is_stack_manifest_protected(&json!({
            "metadata": { "labels": { "keel.io/protect": "true" } }
        })));
        assert!(!is_stack_manifest_protected(&json!({
            "metadata": { "labels": { "keel.io/protected": "true" } }
        })));

        // Agrees with keel_pko::PkoStack::is_protected for a synthesized management stack.
        let stack =
            keel_pko::synthesize_management_core_stack(&keel_pko::ManagementCoreStackParams::new(
                "s3://state",
                "flux-system",
                "clusters/management",
            ))
            .unwrap();
        let doc = serde_json::to_value(&stack).unwrap();
        assert!(stack.is_protected());
        assert_eq!(is_stack_manifest_protected(&doc), stack.is_protected());
    }

    #[tokio::test]
    async fn force_delete_refuses_protected_and_reports_not_found() {
        let url = spawn_mock_k8s_server(|method, path| {
            let path_only = path.split('?').next().unwrap_or(path);
            match (method, path_only) {
                ("GET", "/apis/pulumi.com/v1/namespaces/tenant-a/stacks/locked") => (
                    200,
                    json!({
                        "apiVersion": "pulumi.com/v1",
                        "kind": "Stack",
                        "metadata": {
                            "name": "locked",
                            "namespace": "tenant-a",
                            "labels": { "keel.io/protect": "true" }
                        },
                        "spec": {}
                    })
                    .to_string(),
                ),
                ("GET", "/apis/pulumi.com/v1/namespaces/tenant-a/stacks/cfg-locked") => (
                    200,
                    json!({
                        "apiVersion": "pulumi.com/v1",
                        "kind": "Stack",
                        "metadata": { "name": "cfg-locked", "namespace": "tenant-a" },
                        "spec": { "config": { "protect": "true" } }
                    })
                    .to_string(),
                ),
                _ => (
                    404,
                    json!({ "kind": "Status", "status": "Failure", "code": 404, "message": "not found", "reason": "NotFound" }).to_string(),
                ),
            }
        })
        .await;
        let reader = KubeClusterStatusReader::from_client(mock_k8s_client(&url));

        for name in ["locked", "cfg-locked"] {
            let err = reader
                .force_delete_stack("tenant-a", name)
                .await
                .unwrap_err();
            assert!(matches!(err, KubeError::Conflict(_)), "{name}: {err:?}");
        }
        let err = reader
            .force_delete_stack("flux-system", "management-cluster-core")
            .await
            .unwrap_err();
        assert!(matches!(err, KubeError::Conflict(_)));

        let err = reader
            .force_delete_stack("tenant-a", "missing")
            .await
            .unwrap_err();
        assert!(matches!(err, KubeError::NotFound(_)), "{err:?}");
        let err = reader
            .unlock_stack("tenant-a", "missing")
            .await
            .unwrap_err();
        assert!(matches!(err, KubeError::NotFound(_)), "{err:?}");
        let err = reader
            .restart_stack_workspace("tenant-a", "missing")
            .await
            .unwrap_err();
        assert!(matches!(err, KubeError::NotFound(_)), "{err:?}");
        let err = reader
            .reconcile_stack("tenant-a", "missing")
            .await
            .unwrap_err();
        assert!(matches!(err, KubeError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn status_details_carry_protection_provider_and_terminating_flags_and_cache() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_c = std::sync::Arc::clone(&hits);
        let url = spawn_mock_k8s_server(move |method, path| {
            let path_only = path.split('?').next().unwrap_or(path);
            match (method, path_only) {
                ("GET", "/apis/pulumi.com/v1/stacks") => {
                    hits_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        200,
                        json!({
                            "apiVersion": "pulumi.com/v1",
                            "kind": "StackList",
                            "metadata": {},
                            "items": [
                                {
                                    "apiVersion": "pulumi.com/v1", "kind": "Stack",
                                    "metadata": {
                                        "name": "vm1", "namespace": "tenant-a",
                                        "labels": { "keel.io/provider": "aws", "keel.io/tenant": "a", "keel.io/protect": "true" }
                                    },
                                    "spec": {},
                                    "status": { "conditions": [ { "type": "Ready", "status": "True", "reason": "ProcessingCompleted" } ] }
                                },
                                {
                                    "apiVersion": "pulumi.com/v1", "kind": "Stack",
                                    "metadata": {
                                        "name": "gone", "namespace": "tenant-a",
                                        "deletionTimestamp": "2026-01-01T00:00:00Z",
                                        "finalizers": ["finalizer.stack.pulumi.com"]
                                    },
                                    "spec": { "config": { "keel:provider": "proxmox" } }
                                }
                            ]
                        })
                        .to_string(),
                    )
                }
                _ => (
                    404,
                    json!({ "kind": "Status", "status": "Failure", "code": 404, "message": "not found", "reason": "NotFound" }).to_string(),
                ),
            }
        })
        .await;
        let reader = KubeClusterStatusReader::from_client(mock_k8s_client(&url))
            .with_status_cache_ttl(Duration::from_secs(60));
        let details = reader.stack_status_details().await;
        let vm1 = details
            .get(&("tenant-a".to_string(), "vm1".to_string()))
            .unwrap();
        assert!(vm1.protected && !vm1.terminating);
        assert_eq!(vm1.provider.as_deref(), Some("aws"));
        assert_eq!(vm1.tenant.as_deref(), Some("a"));
        let gone = details
            .get(&("tenant-a".to_string(), "gone".to_string()))
            .unwrap();
        assert!(gone.terminating);
        assert_eq!(gone.status, LiveResourceStatus::Deleting);
        assert_eq!(gone.provider.as_deref(), Some("proxmox"));

        let _ = reader.stack_status_details().await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "cached");
        reader.invalidate_status_cache();
        let _ = reader.stack_status_details().await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
