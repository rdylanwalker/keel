//! `keel-kube`
//!
//! Kubernetes integration layer using `kube-rs`:
//! - Stateless Kubernetes `Secret`-backed state store (`secret_store`),
//! - Per-tenant scoped SeaweedFS S3 identity provisioning via kube-rs exec (`s3_identity`),
//! - Multi-tenant namespace/RBAC/quota/network-policy & Flux `HelmRelease` synthesis (`tenant`),
//! - Per-tenant repository stubs, `keel-gitops-credentials` and PKO tenant-repo sources (`tenant_repo`),
//! - Kyverno admission policies & explicit local-dev Docker socket `PolicyException` (`kyverno`),
//! - Requests reconciliation of tenant PKO Stacks stalled on a since-restored Secret (`stack_recovery`),
//! - Real-time CRD condition normalization & SSE telemetry broadcaster (`telemetry`).

mod ca_bundle;
mod kyverno;
mod live;
mod preview_runner;
mod s3_identity;
mod secret_store;
mod stack_recovery;
mod telemetry;
mod tenant;
mod tenant_repo;

pub use ca_bundle::{
    KEEL_CA_SECRET_NAME, build_keel_ca_bundle_configmap, is_keel_ca_bundle_target_namespace,
    validate_public_ca_pem,
};
pub use kyverno::{
    DockerSocketExceptionConfig, KyvernoManifestBundle, KyvernoPolicyConfig,
    POLICY_PROTECT_PLATFORM_OWNED_TENANT_OBJECTS, POLICY_RESTRICT_PULUMI_PROVIDERS,
    build_protect_control_plane_policy, build_restrict_pulumi_providers_policy_for_registry,
    default_allowed_pulumi_providers, default_reserved_route_hostnames, pulumi_provider_allowlist,
};
pub use live::{
    KubeClusterStatusReader, LiveResourceInfo, LiveResourceStatus, LiveStatusSource,
    STATUS_MESSAGE_MAX_CHARS, StackDiagnostics, extract_not_ready_message,
    is_stack_manifest_protected,
};
#[cfg(any(test, feature = "test-support"))]
pub use preview_runner::InMemoryPreviewCluster;
pub use preview_runner::{
    KubePreviewCluster, PREVIEW_PROVIDER_SECRET_KIND, PreviewCluster, PreviewPrerequisites,
    TenantGitSource, preview_report_configmap_name, tenant_git_repository_manifest_path,
    tenant_git_source_from_manifest,
};
pub use s3_identity::{
    SEAWEEDFS_FILER_POD, SeaweedFsIdentityOps, SeaweedFsTenantS3Provisioner,
    TenantS3IdentityProvisioner, TenantS3ProvisionOutcome, provision_tenant_s3_identity_with,
};
pub use secret_store::{
    GITHUB_TOKEN_SECRET_NAME, InMemorySecretStore, K8sSecretStore, PULUMI_PASSPHRASE_SECRET_KEY,
    PULUMI_PASSPHRASE_SECRET_NAME, S3_CREDENTIALS_SECRET_NAME, SecretBackend, SecretStateStore,
    TENANT_SOPS_AGE_SECRET_NAME, TenantAgeKey, UserGitCredentials, build_pulumi_passphrase_secret,
    build_tenant_age_key_secret, generate_pulumi_passphrase, user_git_creds_secret_name,
};
pub use stack_recovery::{
    KubeStackRecovery, MAX_RECONCILE_ATTEMPTS, StackRecoveryCluster, StalledStackNudge,
    plan_stalled_stack_nudges, reconcile_request_attempt, reconcile_request_value,
    recover_stalled_stacks, secrets_named_in_stall_message, stack_stalled_on_secrets,
};
pub use telemetry::{CrdReconcileStatus, CrdWatchEvent, TelemetryMux, WatchedCrdKind};
pub use tenant::{
    ENVOY_OWNING_GATEWAY_LABEL, FlaggerCanarySpecInput, FluxDependencyRef, FluxHealthCheckRef,
    FluxKustomizationSpecInput, FluxManifestAppInput, HelmChartSourceKind, HelmChartSourceRef,
    HelmReleaseSpecInput, HelmValuesSecretRef, MANIFEST_APP_CONFIG_DIR, PLATFORM_OWNED_LABEL,
    ROUTE_HOSTNAMES_ANNOTATION, RemoteKubeConfigRef, TENANT_RECONCILER_SERVICE_ACCOUNT,
    TENANT_S3_CREDENTIALS_SECRET_NAME, TenantIsolationConfig, TenantManifestBundle,
    build_tenant_pko_workspace_network_policy, build_tenant_reconciler_rbac_yaml,
    default_tenant_service_account_name, manifest_app_config_kustomization_name,
    manifest_app_secrets_name, manifest_app_values_configmap_name,
    synthesize_canary_gateway_network_policy_yaml, synthesize_flagger_canary_yaml,
    synthesize_flux_kustomization_yaml, synthesize_flux_manifest_app_kustomizations,
    synthesize_helm_app_network_policy_yaml, synthesize_helm_release_yaml,
    synthesize_remote_target_namespace, synthesize_remote_target_namespace_yaml, tenant_namespace,
    tenant_s3_backend_url, validate_canary_host,
};
// PKO operator SSH host keys (`keel-system/keel-pko-ssh-known-hosts`, written by
// `keel_server::tenant_repos` for SSH tenant repositories, mounted statically into the operator).
pub use tenant_repo::{
    PKO_OPERATOR_SSH_KNOWN_HOSTS_CONFIGMAP, build_pko_operator_known_hosts_configmap,
    pko_operator_needs_ssh_known_hosts,
};
pub use tenant_repo::{
    SSH_IDENTITY_PUB_KEY, TENANT_GITOPS_CREDENTIALS_SECRET_NAME, TENANT_REPO_STUB_FILES,
    TENANT_ROOT_KUSTOMIZATION_NAME, TenantGitopsCredentialStatus, TenantRepoCredential,
    TenantRepoSource, TenantRepoStub, TenantRepoTransport, build_tenant_repo_git_repository,
    build_tenant_repo_kustomization_yaml, build_tenant_root_kustomization,
    ensure_git_safe_manifests, is_full_commit_sha, synthesize_tenant_repo_stub,
    tenant_gitops_credentials_secret, tenant_gitops_credentials_status,
    tenant_repo_preview_git_source, tenant_repo_stack_git_source, tenant_repo_workspace_template,
};

#[derive(Debug, thiserror::Error)]
pub enum KubeError {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML serialization error: {0}")]
    Yaml(#[from] noyalib::Error),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// The requested Kubernetes resource does not exist (maps to HTTP 404).
    #[error("Not found: {0}")]
    NotFound(String),

    /// The operation conflicts with resource state, e.g. a protected Stack (maps to HTTP 409).
    #[error("Conflict: {0}")]
    Conflict(String),

    /// A live cluster connection is required but unavailable (maps to HTTP 503).
    #[error("Service unavailable: {0}")]
    Unavailable(String),

    /// The tenant has no usable Git source for an ephemeral PKO preview (fail closed: previews
    /// never fall back to the platform `flux-system` whole-repository source).
    #[error("Preview source unavailable: {0}")]
    PreviewSourceUnavailable(String),

    /// The tenant has no usable platform-written Git source for its PKO `Stack`s (fail closed:
    /// Stacks clone only the tenant's own source, never the platform repository; maps to 409).
    #[error("Tenant Git source unavailable: {0}")]
    TenantSourceUnavailable(String),

    #[error("Cryptographic operation failed: {0}")]
    Crypto(String),

    #[error("Corrupted Kubernetes Secret `{namespace}/{name}`: {reason}")]
    CorruptedSecret {
        namespace: String,
        name: String,
        reason: String,
    },
}
