//! Real-time Kubernetes CRD watch event normalization & SSE telemetry multiplexer (`SDD.md` §7.3, `NFR-PERF-02`).
//!
//! Normalizes condition transitions across Flux CD (`GitRepository`, `Kustomization`,
//! `HelmRelease`), Pulumi Kubernetes Operator v2 (`Stack`), and Flagger (`Canary`) into
//! [`CrdWatchEvent`] payloads broadcast to connected SSE subscribers via [`TelemetryMux`].

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt, stream};
use kube::Client;
use kube::api::{Api, DynamicObject};
use kube::discovery::ApiResource;
use kube::runtime::watcher::{self, Event as WatcherEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::KubeError;

/// Default `tokio::sync::broadcast` ring-buffer capacity for SSE telemetry events.
pub const DEFAULT_TELEMETRY_BUFFER_CAPACITY: usize = 1024;

/// Maximum number of recent [`CrdWatchEvent`]s replayed to newly connected SSE subscribers.
const MAX_RECENT_TELEMETRY_EVENTS: usize = 100;

/// Watched GitOps, infrastructure, and progressive delivery CRD kinds (`SDD.md` §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WatchedCrdKind {
    GitRepository,
    Kustomization,
    HelmRelease,
    Stack,
    Canary,
}

impl WatchedCrdKind {
    /// Complete set of CRDs streamed over `/api/v1/events/stream`.
    pub const ALL: [Self; 5] = [
        Self::GitRepository,
        Self::Kustomization,
        Self::HelmRelease,
        Self::Stack,
        Self::Canary,
    ];

    /// Returns the dynamic Kubernetes `ApiResource` descriptor for `kube-rs` watchers.
    pub fn api_resource(self) -> ApiResource {
        match self {
            Self::GitRepository => ApiResource {
                group: "source.toolkit.fluxcd.io".to_string(),
                version: "v1".to_string(),
                api_version: "source.toolkit.fluxcd.io/v1".to_string(),
                kind: "GitRepository".to_string(),
                plural: "gitrepositories".to_string(),
            },
            Self::Kustomization => ApiResource {
                group: "kustomize.toolkit.fluxcd.io".to_string(),
                version: "v1".to_string(),
                api_version: "kustomize.toolkit.fluxcd.io/v1".to_string(),
                kind: "Kustomization".to_string(),
                plural: "kustomizations".to_string(),
            },
            Self::HelmRelease => ApiResource {
                group: "helm.toolkit.fluxcd.io".to_string(),
                version: "v2".to_string(),
                api_version: "helm.toolkit.fluxcd.io/v2".to_string(),
                kind: "HelmRelease".to_string(),
                plural: "helmreleases".to_string(),
            },
            Self::Stack => ApiResource {
                group: "pulumi.com".to_string(),
                version: "v1".to_string(),
                api_version: "pulumi.com/v1".to_string(),
                kind: "Stack".to_string(),
                plural: "stacks".to_string(),
            },
            Self::Canary => ApiResource {
                group: "flagger.app".to_string(),
                version: "v1beta1".to_string(),
                api_version: "flagger.app/v1beta1".to_string(),
                kind: "Canary".to_string(),
                plural: "canaries".to_string(),
            },
        }
    }
}

impl fmt::Display for WatchedCrdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitRepository => f.write_str("GitRepository"),
            Self::Kustomization => f.write_str("Kustomization"),
            Self::HelmRelease => f.write_str("HelmRelease"),
            Self::Stack => f.write_str("Stack"),
            Self::Canary => f.write_str("Canary"),
        }
    }
}

impl FromStr for WatchedCrdKind {
    type Err = KubeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GitRepository" => Ok(Self::GitRepository),
            "Kustomization" => Ok(Self::Kustomization),
            "HelmRelease" => Ok(Self::HelmRelease),
            "Stack" => Ok(Self::Stack),
            "Canary" => Ok(Self::Canary),
            other => Err(KubeError::InvalidInput(format!(
                "unsupported watched CRD kind `{other}`"
            ))),
        }
    }
}

/// Normalized reconciliation status across Flux, PKO v2, and Flagger CRDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CrdReconcileStatus {
    Ready,
    Progressing,
    Previewing,
    Suspended,
    Failed,
    Deleted,
    Unknown,
}

impl fmt::Display for CrdReconcileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => f.write_str("Ready"),
            Self::Progressing => f.write_str("Progressing"),
            Self::Previewing => f.write_str("Previewing"),
            Self::Suspended => f.write_str("Suspended"),
            Self::Failed => f.write_str("Failed"),
            Self::Deleted => f.write_str("Deleted"),
            Self::Unknown => f.write_str("Unknown"),
        }
    }
}

/// Real-time CRD telemetry event pushed to SSE clients (`/api/v1/events/stream`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrdWatchEvent {
    pub kind: WatchedCrdKind,
    pub namespace: String,
    pub name: String,
    pub status: CrdReconcileStatus,
    pub message: String,
    pub revision: Option<String>,
    pub timestamp: DateTime<Utc>,
}

impl CrdWatchEvent {
    pub fn new(
        kind: WatchedCrdKind,
        namespace: impl Into<String>,
        name: impl Into<String>,
        status: CrdReconcileStatus,
        message: impl Into<String>,
        revision: Option<String>,
    ) -> Self {
        Self {
            kind,
            namespace: namespace.into(),
            name: name.into(),
            status,
            message: message.into(),
            revision,
            timestamp: Utc::now(),
        }
    }

    /// Serializes the event into a compact JSON string suitable for an SSE `data:` frame.
    pub fn to_sse_json(&self) -> Result<String, KubeError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Extracts a normalized [`CrdWatchEvent`] from an applied/reconciled `kube::api::DynamicObject`.
    pub fn from_dynamic_object(kind: WatchedCrdKind, obj: &DynamicObject) -> Option<Self> {
        let name = obj.metadata.name.clone()?;
        let namespace = obj
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let spec = obj.data.get("spec");
        let status_obj = obj.data.get("status");

        let is_suspended = spec
            .and_then(|s| s.get("suspend"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_preview_stack = kind == WatchedCrdKind::Stack
            && spec
                .and_then(|s| s.get("preview"))
                .and_then(Value::as_bool)
                .unwrap_or(false);

        let revision = extract_revision(status_obj);
        let (mut status, message) = extract_status_and_message(kind, status_obj, is_preview_stack);

        if is_suspended {
            status = CrdReconcileStatus::Suspended;
        }

        Some(Self {
            kind,
            namespace,
            name,
            status,
            message,
            revision,
            timestamp: Utc::now(),
        })
    }

    /// Creates a `Deleted` [`CrdWatchEvent`] when a watched CRD instance is garbage-collected.
    pub fn deleted_from_dynamic_object(kind: WatchedCrdKind, obj: &DynamicObject) -> Option<Self> {
        let name = obj.metadata.name.clone()?;
        let namespace = obj
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let revision = extract_revision(obj.data.get("status"));

        Some(Self {
            kind,
            namespace,
            name,
            status: CrdReconcileStatus::Deleted,
            message: format!("{kind} deleted"),
            revision,
            timestamp: Utc::now(),
        })
    }
}

fn extract_revision(status_obj: Option<&Value>) -> Option<String> {
    let status = status_obj?;
    status
        .pointer("/artifact/revision")
        .or_else(|| status.get("lastAppliedRevision"))
        .or_else(|| status.get("lastAttemptedRevision"))
        .or_else(|| status.pointer("/lastUpdate/revision"))
        .or_else(|| status.get("trackedConfig"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn extract_status_and_message(
    kind: WatchedCrdKind,
    status_obj: Option<&Value>,
    is_preview_stack: bool,
) -> (CrdReconcileStatus, String) {
    let Some(status) = status_obj else {
        return (
            CrdReconcileStatus::Progressing,
            format!("Awaiting initial {kind} reconciliation"),
        );
    };

    // Flagger Canary sets `status.phase` explicitly (`Initializing`, `Initialized`, `Progressing`, `Promoting`, `Finalising`, `Succeeded`, `Failed`)
    if kind == WatchedCrdKind::Canary
        && let Some(phase) = status.get("phase").and_then(Value::as_str)
    {
        let weight = status
            .get("canaryWeight")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mapped = match phase {
            "Initialized" | "Succeeded" => CrdReconcileStatus::Ready,
            "Progressing" | "Promoting" | "Finalising" | "Initializing" | "Waiting" => {
                CrdReconcileStatus::Progressing
            }
            "Failed" => CrdReconcileStatus::Failed,
            _ => CrdReconcileStatus::Unknown,
        };
        let msg = status
            .get("conditions")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Canary phase={phase} (weight={weight}%)"));
        return (mapped, msg);
    }

    // Standard Kubernetes `status.conditions` (`Ready` or `Reconciling`) used by Flux CD & PKO v2
    if let Some(conditions) = status.get("conditions").and_then(Value::as_array) {
        let reconciling_true = conditions.iter().any(|c| {
            c.get("type").and_then(Value::as_str) == Some("Reconciling")
                && c.get("status").and_then(Value::as_str) == Some("True")
        });

        if let Some(ready_cond) = conditions
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
        {
            let cond_status = ready_cond
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            let reason = ready_cond
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("");
            let message = ready_cond
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let mapped = match cond_status {
                "True" => CrdReconcileStatus::Ready,
                "False" if reconciling_true || reason.contains("Progressing") => {
                    if is_preview_stack {
                        CrdReconcileStatus::Previewing
                    } else {
                        CrdReconcileStatus::Progressing
                    }
                }
                "False" => CrdReconcileStatus::Failed,
                _ => {
                    if is_preview_stack {
                        CrdReconcileStatus::Previewing
                    } else {
                        CrdReconcileStatus::Progressing
                    }
                }
            };
            return (mapped, message);
        }
    }

    // Fallback for PKO v2 `status.lastUpdate.state` (`succeeded`, `updating`, `failed`)
    if let Some(last_state) = status.pointer("/lastUpdate/state").and_then(Value::as_str) {
        let msg = status
            .pointer("/lastUpdate/message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mapped = match last_state {
            "succeeded" => CrdReconcileStatus::Ready,
            "updating" | "running" => {
                if is_preview_stack {
                    CrdReconcileStatus::Previewing
                } else {
                    CrdReconcileStatus::Progressing
                }
            }
            "failed" => CrdReconcileStatus::Failed,
            _ => CrdReconcileStatus::Unknown,
        };
        return (mapped, msg);
    }

    (CrdReconcileStatus::Unknown, String::new())
}

/// Broadcast-backed SSE telemetry multiplexer wrapping `tokio::sync::broadcast::Sender<CrdWatchEvent>`.
#[derive(Clone, Debug)]
pub struct TelemetryMux {
    sender: broadcast::Sender<CrdWatchEvent>,
    recent: Arc<Mutex<VecDeque<CrdWatchEvent>>>,
}

impl Default for TelemetryMux {
    fn default() -> Self {
        Self::new(DEFAULT_TELEMETRY_BUFFER_CAPACITY)
    }
}

impl TelemetryMux {
    /// Creates a new [`TelemetryMux`] with the requested ring-buffer capacity (clamped to `>= 1`).
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            sender,
            recent: Arc::new(Mutex::new(VecDeque::with_capacity(
                MAX_RECENT_TELEMETRY_EVENTS,
            ))),
        }
    }

    /// Publishes a [`CrdWatchEvent`] to all active subscribers and records it in the replay buffer.
    ///
    /// Returns the number of active subscribers that received the event (`0` if nobody is currently subscribed).
    ///
    /// The replay buffer lock is held across the broadcast `send` so that [`Self::subscribe_stream`]
    /// observes every event exactly once: either in its replay snapshot or on its live receiver.
    pub fn publish(&self, event: CrdWatchEvent) -> usize {
        let mut guard = self.recent.lock().ok();
        if let Some(buf) = guard.as_mut() {
            if buf.len() >= MAX_RECENT_TELEMETRY_EVENTS {
                buf.pop_front();
            }
            buf.push_back(event.clone());
        }
        let delivered = self.sender.send(event).unwrap_or(0);
        drop(guard);
        delivered
    }

    /// Subscribes a new `broadcast::Receiver<CrdWatchEvent>`.
    pub fn subscribe(&self) -> broadcast::Receiver<CrdWatchEvent> {
        self.sender.subscribe()
    }

    /// Returns an async `futures::Stream` over a new subscription for Axum SSE endpoints,
    /// replaying recent events first and gracefully continuing past lagged ring-buffer slots until the broadcaster closes.
    pub fn subscribe_stream(&self) -> impl Stream<Item = CrdWatchEvent> + Send + 'static {
        // Snapshot the replay buffer and subscribe atomically under the same lock `publish` holds
        // while sending, so no event is both replayed and received live (duplicate) or lost.
        let (rx, snapshot): (_, Vec<CrdWatchEvent>) = match self.recent.lock() {
            Ok(buf) => (self.subscribe(), buf.iter().cloned().collect()),
            Err(_) => (self.subscribe(), Vec::new()),
        };

        stream::iter(snapshot).chain(stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => return Some((ev, rx)),
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "SSE subscriber lagged behind telemetry buffer");
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        }))
    }

    /// Subscribes to events restricted to a single Kubernetes namespace (e.g. `tenant-<id>`),
    /// skipping lagged frames gracefully while enforcing multi-tenant stream isolation (`NFR-SEC-02`).
    pub fn subscribe_namespace_stream(
        &self,
        namespace: impl Into<String>,
    ) -> impl Stream<Item = CrdWatchEvent> + Send + 'static {
        let allowed_ns = namespace.into();
        self.subscribe_stream().filter(move |ev| {
            let matches = ev.namespace == allowed_ns;
            async move { matches }
        })
    }

    /// Returns the current number of active SSE/CLI subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// Spawns a background `kube::runtime::watcher` task streaming live CRD changes into this [`TelemetryMux`].
    pub fn spawn_crd_watcher(
        &self,
        client: Client,
        kind: WatchedCrdKind,
        namespace: Option<String>,
    ) -> tokio::task::JoinHandle<()> {
        let mux = self.clone();
        let ar = kind.api_resource();
        let api: Api<DynamicObject> = match namespace {
            Some(ns) => Api::namespaced_with(client, &ns, &ar),
            None => Api::all_with(client, &ar),
        };

        tokio::spawn(async move {
            let stream = watcher::watcher(api, watcher::Config::default());
            futures::pin_mut!(stream);
            let mut consecutive_errors: u32 = 0;
            let mut last_published: std::collections::HashMap<
                (String, String),
                (CrdReconcileStatus, String, Option<String>),
            > = std::collections::HashMap::new();

            while let Some(item) = stream.next().await {
                if item.is_ok() {
                    consecutive_errors = 0;
                }
                match item {
                    Ok(WatcherEvent::Apply(obj) | WatcherEvent::InitApply(obj)) => {
                        if let Some(ev) = CrdWatchEvent::from_dynamic_object(kind, &obj) {
                            let key = (ev.namespace.clone(), ev.name.clone());
                            let sig = (ev.status, ev.message.clone(), ev.revision.clone());
                            if last_published.get(&key) != Some(&sig) {
                                last_published.insert(key, sig);
                                mux.publish(ev);
                            }
                        }
                    }
                    Ok(WatcherEvent::Delete(obj)) => {
                        if let Some(ev) = CrdWatchEvent::deleted_from_dynamic_object(kind, &obj) {
                            last_published.remove(&(ev.namespace.clone(), ev.name.clone()));
                            mux.publish(ev);
                        }
                    }
                    Ok(WatcherEvent::Init | WatcherEvent::InitDone) => {}
                    Err(err) => {
                        let not_installed = is_crd_not_installed(&err);
                        let delay = watcher_backoff_delay(
                            consecutive_errors,
                            not_installed,
                            rand::random::<f64>(),
                        );
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        if not_installed {
                            tracing::debug!(%kind, delay_ms = delay.as_millis() as u64, "CRD not installed in cluster yet; backing off");
                        } else {
                            tracing::warn!(%kind, error = %err, delay_ms = delay.as_millis() as u64, "CRD watcher stream error; backing off");
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        })
    }
}

/// Initial CRD watcher retry delay.
const WATCHER_BACKOFF_BASE: Duration = Duration::from_millis(500);
/// Maximum CRD watcher retry delay for transient errors.
const WATCHER_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Maximum CRD watcher retry delay while a CRD is not installed (`404`).
const WATCHER_BACKOFF_MAX_NOT_INSTALLED: Duration = Duration::from_secs(300);

/// Returns `true` if a watcher error is a typed Kubernetes `404 NotFound` (CRD not installed yet).
pub fn is_crd_not_installed(err: &watcher::Error) -> bool {
    match err {
        watcher::Error::InitialListFailed(e)
        | watcher::Error::WatchStartFailed(e)
        | watcher::Error::WatchFailed(e) => {
            matches!(e, kube::Error::Api(status) if status.code == 404)
        }
        watcher::Error::WatchError(status) => status.code == 404,
        watcher::Error::NoResourceVersion => false,
    }
}

/// Computes exponential backoff with "equal jitter" for the `attempt`-th consecutive watcher error.
///
/// `jitter` is a uniform sample in `[0, 1)`; the result lies in `[cap/2, cap)` where
/// `cap = min(base * 2^attempt, max)`. CRDs that are not installed back off to a longer ceiling.
pub fn watcher_backoff_delay(attempt: u32, not_installed: bool, jitter: f64) -> Duration {
    let max = if not_installed {
        WATCHER_BACKOFF_MAX_NOT_INSTALLED
    } else {
        WATCHER_BACKOFF_MAX
    };
    let exp = WATCHER_BACKOFF_BASE
        .checked_mul(1u32.checked_shl(attempt.min(20)).unwrap_or(u32::MAX))
        .unwrap_or(max);
    let cap = exp.min(max);
    let half = cap / 2;
    half + half.mul_f64(jitter.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn telemetry_mux_fanout_and_zero_subscriber_publish() {
        let mux = TelemetryMux::new(32);
        assert_eq!(mux.subscriber_count(), 0);

        // Publishing with 0 subscribers succeeds and returns 0
        let initial_event = CrdWatchEvent::new(
            WatchedCrdKind::GitRepository,
            "flux-system",
            "infra-gitops",
            CrdReconcileStatus::Ready,
            "stored artifact for revision 'main@sha1:abc1234'",
            Some("main@sha1:abc1234".to_string()),
        );
        assert_eq!(mux.publish(initial_event), 0);

        let mut rx1 = mux.subscribe();
        let mut rx2 = mux.subscribe();
        assert_eq!(mux.subscriber_count(), 2);

        let ev = CrdWatchEvent::new(
            WatchedCrdKind::HelmRelease,
            "tenant-acme",
            "authentik",
            CrdReconcileStatus::Progressing,
            "Running 'upgrade' action with timeout of 5m0s",
            Some("2026.10.1".to_string()),
        );

        let delivered = mux.publish(ev.clone());
        assert_eq!(delivered, 2);

        let got1 = rx1.recv().await.unwrap();
        let got2 = rx2.recv().await.unwrap();
        assert_eq!(got1, ev);
        assert_eq!(got2, ev);

        // Verify JSON SSE serialization contains all required fields
        let sse_json = got1.to_sse_json().unwrap();
        let parsed: Value = serde_json::from_str(&sse_json).unwrap();
        assert_eq!(parsed["kind"], "HelmRelease");
        assert_eq!(parsed["namespace"], "tenant-acme");
        assert_eq!(parsed["name"], "authentik");
        assert_eq!(parsed["status"], "Progressing");
        assert_eq!(parsed["revision"], "2026.10.1");
        assert!(parsed["timestamp"].is_string());
    }

    #[tokio::test]
    async fn subscribe_stream_replays_recent_events_to_late_subscribers() {
        let mux = TelemetryMux::new(16);
        let early = CrdWatchEvent::new(
            WatchedCrdKind::GitRepository,
            "flux-system",
            "flux-system",
            CrdReconcileStatus::Ready,
            "Fetched main",
            Some("main@sha1:deadbeef".to_string()),
        );
        mux.publish(early.clone());

        let mut stream = Box::pin(mux.subscribe_stream());
        let replayed = stream.next().await.expect("recent event should replay");
        assert_eq!(replayed, early);
    }

    #[tokio::test]
    async fn namespace_filtered_stream_isolates_tenants() {
        let mux = TelemetryMux::default();
        let mut tenant_a_stream = Box::pin(mux.subscribe_namespace_stream("tenant-alpha"));

        mux.publish(CrdWatchEvent::new(
            WatchedCrdKind::Stack,
            "tenant-beta",
            "db-beta",
            CrdReconcileStatus::Ready,
            "Beta stack ready",
            None,
        ));
        mux.publish(CrdWatchEvent::new(
            WatchedCrdKind::Stack,
            "tenant-alpha",
            "db-alpha",
            CrdReconcileStatus::Ready,
            "Alpha stack ready",
            Some("sha256:111".to_string()),
        ));

        let received = tenant_a_stream.next().await.unwrap();
        assert_eq!(received.namespace, "tenant-alpha");
        assert_eq!(received.name, "db-alpha");
    }

    #[test]
    fn watched_crd_kinds_and_dynamic_object_normalization_cover_all_five_crds() {
        assert_eq!(WatchedCrdKind::ALL.len(), 5);
        for kind in WatchedCrdKind::ALL {
            let s = kind.to_string();
            let parsed = WatchedCrdKind::from_str(&s).unwrap();
            assert_eq!(parsed, kind);
            let ar = kind.api_resource();
            assert_eq!(ar.kind, s);
        }
        assert!(WatchedCrdKind::from_str("UnsupportedKind").is_err());

        // 1. Flux GitRepository (Ready)
        let git_repo: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": { "name": "keel-root", "namespace": "flux-system" },
            "status": {
                "artifact": { "revision": "main@sha1:deadbeef" },
                "conditions": [
                    { "type": "Ready", "status": "True", "reason": "Succeeded", "message": "Fetched revision" }
                ]
            }
        }))
        .unwrap();
        let ev_git =
            CrdWatchEvent::from_dynamic_object(WatchedCrdKind::GitRepository, &git_repo).unwrap();
        assert_eq!(ev_git.status, CrdReconcileStatus::Ready);
        assert_eq!(ev_git.revision.as_deref(), Some("main@sha1:deadbeef"));

        // 2. Flux Kustomization (Suspended)
        let kustomization: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
            "kind": "Kustomization",
            "metadata": { "name": "apps", "namespace": "tenant-1" },
            "spec": { "suspend": true },
            "status": {
                "lastAppliedRevision": "main@sha1:1234",
                "conditions": [
                    { "type": "Ready", "status": "True", "reason": "ReconciliationSucceeded", "message": "Applied" }
                ]
            }
        }))
        .unwrap();
        let ev_ks =
            CrdWatchEvent::from_dynamic_object(WatchedCrdKind::Kustomization, &kustomization)
                .unwrap();
        assert_eq!(ev_ks.status, CrdReconcileStatus::Suspended);

        // 3. Flux HelmRelease (Failed)
        let hr: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": { "name": "redis", "namespace": "tenant-1" },
            "status": {
                "lastAttemptedRevision": "19.0.0",
                "conditions": [
                    { "type": "Ready", "status": "False", "reason": "UpgradeFailed", "message": "Helm upgrade failed" }
                ]
            }
        }))
        .unwrap();
        let ev_hr = CrdWatchEvent::from_dynamic_object(WatchedCrdKind::HelmRelease, &hr).unwrap();
        assert_eq!(ev_hr.status, CrdReconcileStatus::Failed);
        assert_eq!(ev_hr.revision.as_deref(), Some("19.0.0"));

        // 4. PKO Stack (Previewing in `keel-previews`)
        let pko_stack: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": "pr-42-preview", "namespace": "keel-previews" },
            "spec": { "preview": true },
            "status": {
                "lastUpdate": {
                    "state": "updating",
                    "revision": "keel/acme/db-170000",
                    "message": "Running pulumi preview --json"
                }
            }
        }))
        .unwrap();
        let ev_stack =
            CrdWatchEvent::from_dynamic_object(WatchedCrdKind::Stack, &pko_stack).unwrap();
        assert_eq!(ev_stack.status, CrdReconcileStatus::Previewing);
        assert_eq!(ev_stack.revision.as_deref(), Some("keel/acme/db-170000"));

        // 5. Flagger Canary (Progressing -> Deleted)
        let canary: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "flagger.app/v1beta1",
            "kind": "Canary",
            "metadata": { "name": "api-gateway", "namespace": "tenant-1" },
            "status": {
                "phase": "Progressing",
                "canaryWeight": 25,
                "trackedConfig": "cfg-v2"
            }
        }))
        .unwrap();
        let ev_canary =
            CrdWatchEvent::from_dynamic_object(WatchedCrdKind::Canary, &canary).unwrap();
        assert_eq!(ev_canary.status, CrdReconcileStatus::Progressing);
        assert!(ev_canary.message.contains("weight=25%"));
        assert_eq!(ev_canary.revision.as_deref(), Some("cfg-v2"));

        let deleted =
            CrdWatchEvent::deleted_from_dynamic_object(WatchedCrdKind::Canary, &canary).unwrap();
        assert_eq!(deleted.status, CrdReconcileStatus::Deleted);

        // Status Display coverage & uninitialized / reconciling / failed branches
        let statuses = [
            CrdReconcileStatus::Ready,
            CrdReconcileStatus::Progressing,
            CrdReconcileStatus::Previewing,
            CrdReconcileStatus::Suspended,
            CrdReconcileStatus::Failed,
            CrdReconcileStatus::Deleted,
            CrdReconcileStatus::Unknown,
        ];
        for st in statuses {
            assert!(!st.to_string().is_empty());
        }

        let no_status: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": "bootstrap-stack", "namespace": "tenant-1" }
        }))
        .unwrap();
        assert_eq!(
            CrdWatchEvent::from_dynamic_object(WatchedCrdKind::Stack, &no_status)
                .unwrap()
                .status,
            CrdReconcileStatus::Progressing
        );

        let preview_reconciling: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": "pr-99", "namespace": "keel-previews" },
            "spec": { "preview": true },
            "status": {
                "conditions": [
                    { "type": "Reconciling", "status": "True" },
                    { "type": "Ready", "status": "False", "reason": "Progressing", "message": "Preview running" }
                ]
            }
        }))
        .unwrap();
        assert_eq!(
            CrdWatchEvent::from_dynamic_object(WatchedCrdKind::Stack, &preview_reconciling)
                .unwrap()
                .status,
            CrdReconcileStatus::Previewing
        );
    }

    #[test]
    fn watcher_backoff_grows_exponentially_with_jitter_and_caps() {
        let d0 = watcher_backoff_delay(0, false, 0.0);
        assert_eq!(d0, WATCHER_BACKOFF_BASE / 2);
        let d0_hi = watcher_backoff_delay(0, false, 0.999);
        assert!(d0_hi < WATCHER_BACKOFF_BASE && d0_hi > d0);
        let d3 = watcher_backoff_delay(3, false, 0.0);
        assert_eq!(d3, WATCHER_BACKOFF_BASE * 8 / 2);
        // Capped for large attempts (no overflow).
        let big = watcher_backoff_delay(u32::MAX, false, 0.5);
        assert!(big <= WATCHER_BACKOFF_MAX && big >= WATCHER_BACKOFF_MAX / 2);
        let big_ni = watcher_backoff_delay(40, true, 0.0);
        assert_eq!(big_ni, WATCHER_BACKOFF_MAX_NOT_INSTALLED / 2);
        // Out-of-range jitter is clamped.
        assert!(watcher_backoff_delay(1, false, 7.0) <= WATCHER_BACKOFF_BASE * 2);
    }

    #[test]
    fn crd_not_installed_detection_is_typed() {
        let status_404 = kube::core::Status {
            code: 404,
            reason: "NotFound".to_string(),
            ..Default::default()
        };
        let status_500 = kube::core::Status {
            code: 500,
            message: "contains 404 in text".to_string(),
            ..Default::default()
        };
        assert!(is_crd_not_installed(&watcher::Error::InitialListFailed(
            kube::Error::Api(Box::new(status_404.clone()))
        )));
        assert!(is_crd_not_installed(&watcher::Error::WatchError(Box::new(
            status_404
        ))));
        assert!(!is_crd_not_installed(&watcher::Error::WatchFailed(
            kube::Error::Api(Box::new(status_500.clone()))
        )));
        assert!(!is_crd_not_installed(&watcher::Error::WatchError(
            Box::new(status_500)
        )));
        assert!(!is_crd_not_installed(&watcher::Error::NoResourceVersion));
    }

    #[tokio::test]
    async fn subscribe_stream_replays_then_streams_without_duplicates() {
        let mux = TelemetryMux::new(16);
        let mk = |n: &str| {
            CrdWatchEvent::new(
                WatchedCrdKind::Stack,
                "tenant-a",
                n,
                CrdReconcileStatus::Ready,
                "ok",
                None,
            )
        };
        mux.publish(mk("before"));
        let stream = Box::pin(mux.subscribe_stream());
        mux.publish(mk("after"));
        drop(mux);
        let names: Vec<String> = stream.map(|e| e.name).collect().await;
        assert_eq!(names, vec!["before".to_string(), "after".to_string()]);
    }

    #[tokio::test]
    async fn subscribe_stream_recovers_from_lag_and_closes_cleanly() {
        let mux = TelemetryMux::new(2);
        let stream = Box::pin(mux.subscribe_stream());

        // Overflow small capacity=2 buffer to trigger RecvError::Lagged recovery
        for idx in 0..5 {
            mux.publish(CrdWatchEvent::new(
                WatchedCrdKind::Kustomization,
                "tenant-lag",
                format!("ks-{idx}"),
                CrdReconcileStatus::Ready,
                "Applied",
                None,
            ));
        }

        // Drop mux so stream terminates after draining remaining buffered items
        drop(mux);

        let collected: Vec<CrdWatchEvent> = stream.collect().await;
        assert!(!collected.is_empty());
        assert_eq!(collected.last().unwrap().name, "ks-4");
    }
}
