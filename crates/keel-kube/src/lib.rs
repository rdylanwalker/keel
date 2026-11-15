//! `keel-kube`
//!
//! Kubernetes integration layer using `kube-rs`:
//! - Stateless Kubernetes `Secret`-backed state store (`secret_store`),
//! - Multi-tenant namespace/RBAC/quota/network-policy & Flux `HelmRelease` synthesis (`tenant`),
//! - Kyverno admission policies & explicit local-dev Docker socket `PolicyException` (`kyverno`),
//! - Real-time CRD condition normalization & SSE telemetry broadcaster (`telemetry`).

pub mod kyverno;
pub mod live;
pub mod secret_store;
pub mod telemetry;
pub mod tenant;

use futures::Stream;
use k8s_openapi::api::core::v1::Secret;
use keel_core::EntityMeta;
use kube::Client;
use tokio_stream::wrappers::ReceiverStream;

pub use kyverno::{
    DockerSocketExceptionConfig, KyvernoManifestBundle, KyvernoPolicyConfig,
    build_disallow_privileged_containers_policy, build_local_kind_docker_socket_exception,
    build_protect_control_plane_policy, build_restrict_pulumi_providers_policy,
};
pub use live::{
    ClusterEventSummary, KubeClusterStatusReader, LiveResourceInfo, LiveResourceStatus,
    LiveStatusSource, ResourceCondition, ResourceMetadata, StackDiagnostics, extract_conditions,
    extract_last_synced, extract_ready_status, extract_resource_count,
};
pub use secret_store::{
    BOOTSTRAP_TOKEN_SECRET_NAME, InMemorySecretStore, K8sSecretStore, SESSION_KEY_LEN,
    SESSION_KEY_SECRET_NAME, SecretStateStore, TENANT_SOPS_AGE_SECRET_NAME, TenantAgeKey,
    USER_GIT_CREDS_SECRET_PREFIX, UserGitCredentials, generate_bootstrap_token,
    hash_bootstrap_token, user_git_creds_secret_name,
};
pub use telemetry::{
    CrdReconcileStatus, CrdWatchEvent, DEFAULT_TELEMETRY_BUFFER_CAPACITY, TelemetryMux,
    WatchedCrdKind,
};
pub use tenant::{
    FluxDependencyRef, FluxHealthCheckRef, FluxKustomizationSpecInput, HelmChartSourceKind,
    HelmChartSourceRef, HelmReleaseSpecInput, HelmValuesSecretRef, RemoteKubeConfigRef,
    TENANT_NAMESPACE_PREFIX, TenantIsolationConfig, TenantManifestBundle, TenantQuotaSpec,
    build_tenant_default_deny_network_policy, build_tenant_namespace, build_tenant_resource_quota,
    build_tenant_service_account, synthesize_flux_kustomization,
    synthesize_flux_kustomization_yaml, synthesize_helm_release, synthesize_helm_release_yaml,
    tenant_namespace, validate_tenant_id,
};

#[derive(Debug, thiserror::Error)]
pub enum KubeError {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML serialization error: {0}")]
    Yaml(#[from] serde_yml::Error),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Cryptographic operation failed: {0}")]
    Crypto(String),

    #[error("Corrupted Kubernetes Secret `{namespace}/{name}`: {reason}")]
    CorruptedSecret {
        namespace: String,
        name: String,
        reason: String,
    },
}

/// High-level Kubernetes integration handle wrapping a `kube::Client`,
/// stateless `K8sSecretStore`, and `TelemetryMux`.
#[derive(Clone)]
pub struct KubeManager {
    pub client: Client,
    pub telemetry: TelemetryMux,
}

impl KubeManager {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            telemetry: TelemetryMux::default(),
        }
    }

    pub fn with_telemetry(client: Client, telemetry: TelemetryMux) -> Self {
        Self { client, telemetry }
    }

    pub fn secret_store(&self) -> K8sSecretStore {
        K8sSecretStore::new(self.client.clone())
    }
}

pub fn create_secret_manifest(meta: &EntityMeta) -> Secret {
    let mut secret = Secret::default();
    secret.metadata.name = Some(meta.name.clone());
    secret.metadata.namespace = Some(meta.namespace.clone());
    secret
}

pub fn create_event_stream(rx: tokio::sync::mpsc::Receiver<String>) -> impl Stream<Item = String> {
    ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[test]
    fn test_create_secret_manifest() {
        let meta = EntityMeta::new("db-secret", "tenant-1");
        let secret = create_secret_manifest(&meta);
        assert_eq!(secret.metadata.name.as_deref(), Some("db-secret"));
        assert_eq!(secret.metadata.namespace.as_deref(), Some("tenant-1"));
    }

    #[tokio::test]
    async fn test_create_event_stream() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send("ready".to_string()).await.unwrap();
        drop(tx);
        let events: Vec<String> = create_event_stream(rx).collect().await;
        assert_eq!(events, vec!["ready".to_string()]);
    }
}
