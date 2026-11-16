//! Live Kubernetes CRD status reader via `kube-rs` (`SDD.md` §5.3, §7.1).

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Event as K8sEvent, Pod};
use kube::Client;
use kube::api::{Api, DynamicObject, ListParams, LogParams, Patch, PatchParams};
use kube::discovery::ApiResource;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
            .map(|(k, status)| {
                (
                    k,
                    LiveResourceInfo {
                        status,
                        last_synced: None,
                        resource_count: None,
                    },
                )
            })
            .collect()
    }

    /// Returns `(namespace, name) -> LiveResourceInfo` for Flux `HelmRelease` and `Kustomization` resources.
    async fn release_status_details(&self) -> HashMap<(String, String), LiveResourceInfo> {
        self.release_statuses()
            .await
            .into_iter()
            .map(|(k, status)| {
                (
                    k,
                    LiveResourceInfo {
                        status,
                        last_synced: None,
                        resource_count: None,
                    },
                )
            })
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
        Err(KubeError::InvalidInput(
            "Live Kubernetes cluster connection required to restart stack workspace".to_string(),
        ))
    }

    /// Annotates a Flux `HelmRelease` (or `Kustomization`) with `reconcile.fluxcd.io/requestedAt`.
    async fn reconcile_release(&self, _namespace: &str, _name: &str) -> Result<String, KubeError> {
        Err(KubeError::InvalidInput(
            "Live Kubernetes cluster connection required to reconcile release".to_string(),
        ))
    }
}

/// Live `kube-rs` implementation querying PKO v2 `Stack`, Flux `HelmRelease`, and Flux `Kustomization` CRDs.
#[derive(Clone)]
pub struct KubeClusterStatusReader {
    client: Client,
}

impl KubeClusterStatusReader {
    /// Connects to the Kubernetes cluster using default `KUBECONFIG` or in-cluster configuration.
    pub async fn try_default() -> Result<Self, KubeError> {
        let client = Client::try_default().await?;
        Ok(Self { client })
    }

    /// Wraps an existing `kube::Client`.
    pub fn from_client(client: Client) -> Self {
        Self { client }
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

    async fn build_client_from_kubeconfig_yaml(yaml: &str) -> Option<Client> {
        let kc = kube::config::Kubeconfig::from_yaml(yaml).ok()?;
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
                .map(|t| t.0.to_rfc3339());
            let last_synced = extract_last_synced(&item.data, creation_ts.as_deref());
            let resource_count = extract_resource_count(&item.data);

            if item.metadata.deletion_timestamp.is_some() {
                out.insert(
                    (ns, name),
                    LiveResourceInfo {
                        status: LiveResourceStatus::Deleting,
                        last_synced,
                        resource_count,
                    },
                );
            } else if let Some(status) = extract_ready_status(&item.data, is_stack) {
                out.insert(
                    (ns, name),
                    LiveResourceInfo {
                        status,
                        last_synced,
                        resource_count,
                    },
                );
            }
        }
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

        // Also poll workload clusters referenced by `*-kubeconfig` Secrets in `tenant-*` namespaces
        let secret_api: Api<k8s_openapi::api::core::v1::Secret> = Api::all(self.client.clone());
        if let Ok(secrets) = secret_api.list(&ListParams::default()).await {
            let mut kubeconfig_yamls = Vec::new();
            for sec in secrets.items {
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
                    kubeconfig_yamls.push(yaml);
                }
            }

            let remote_futures = kubeconfig_yamls.into_iter().map(|yaml| async move {
                let client = Self::build_client_from_kubeconfig_yaml(&yaml).await?;
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
                            .map(|t| t.0.to_rfc3339())
                            .or_else(|| ev.metadata.creation_timestamp.map(|t| t.0.to_rfc3339())),
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
                            .map(|t| t.0.to_rfc3339())
                            .or_else(|| ev.metadata.creation_timestamp.map(|t| t.0.to_rfc3339())),
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
            .await?;
        Ok(format!(
            "Triggered rollout restart for StatefulSet {namespace}/{sts_name}"
        ))
    }

    async fn reconcile_release(&self, namespace: &str, name: &str) -> Result<String, KubeError> {
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
            .map(|t| t.0.to_rfc3339()),
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
