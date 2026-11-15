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

    async fn list_dynamic_statuses(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        plural: &str,
        is_stack: bool,
    ) -> HashMap<(String, String), LiveResourceStatus> {
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
            if item.metadata.deletion_timestamp.is_some() {
                out.insert((ns, name), LiveResourceStatus::Deleting);
            } else if let Some(status) = extract_ready_status(&item.data, is_stack) {
                out.insert((ns, name), status);
            }
        }
        out
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
}
