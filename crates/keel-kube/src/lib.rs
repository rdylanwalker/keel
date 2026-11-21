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

pub use kyverno::{
    DockerSocketExceptionConfig, KyvernoManifestBundle, KyvernoPolicyConfig,
    build_disallow_privileged_containers_policy, build_local_kind_docker_socket_exception,
    build_protect_control_plane_policy, build_restrict_pulumi_providers_policy,
};
pub use live::{
    CanaryStatusInfo, ClusterEventSummary, KubeClusterStatusReader, LiveResourceInfo,
    LiveResourceStatus, LiveStatusSource, ResourceCondition, ResourceMetadata, StackDiagnostics,
    extract_conditions, extract_last_synced, extract_ready_status, extract_resource_count,
};
pub use secret_store::{
    GITHUB_TOKEN_SECRET_NAME, InMemorySecretStore, K8sSecretStore, S3_CREDENTIALS_SECRET_NAME,
    SESSION_KEY_LEN, SESSION_KEY_SECRET_NAME, SecretStateStore, TENANT_SOPS_AGE_SECRET_NAME,
    TenantAgeKey, USER_GIT_CREDS_SECRET_PREFIX, UserGitCredentials, user_git_creds_secret_name,
};
pub use telemetry::{
    CrdReconcileStatus, CrdWatchEvent, DEFAULT_TELEMETRY_BUFFER_CAPACITY, TelemetryMux,
    WatchedCrdKind,
};
pub use tenant::{
    FlaggerCanarySpecInput, FluxDependencyRef, FluxHealthCheckRef, FluxKustomizationSpecInput,
    FluxSubstituteReference, HelmChartSourceKind, HelmChartSourceRef, HelmReleaseSpecInput,
    HelmValuesSecretRef, RemoteKubeConfigRef, TENANT_NAMESPACE_PREFIX, TenantIsolationConfig,
    TenantManifestBundle, TenantQuotaSpec, build_tenant_default_deny_network_policy,
    build_tenant_namespace, build_tenant_resource_quota, build_tenant_service_account,
    generate_flux_kustomization_manifest, synthesize_canary_gateway_network_policy,
    synthesize_canary_gateway_network_policy_yaml, synthesize_flagger_canary,
    synthesize_flagger_canary_yaml, synthesize_flux_kustomization,
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
