//! `keel-kube`
//!
//! Kubernetes integration layer using `kube-rs`:
//! - Stateless Kubernetes `Secret`-backed state store (`secret_store`),
//! - Per-tenant scoped SeaweedFS S3 identity provisioning via kube-rs exec (`s3_identity`),
//! - Multi-tenant namespace/RBAC/quota/network-policy & Flux `HelmRelease` synthesis (`tenant`),
//! - Per-tenant repository stubs, `keel-gitops-credentials` and PKO tenant-repo sources (`tenant_repo`),
//! - Kyverno admission policies & explicit local-dev Docker socket `PolicyException` (`kyverno`),
//! - Real-time CRD condition normalization & SSE telemetry broadcaster (`telemetry`).

pub mod ca_bundle;
pub mod kyverno;
pub mod live;
pub mod preview_runner;
pub mod s3_identity;
pub mod secret_store;
pub mod telemetry;
pub mod tenant;
pub mod tenant_repo;

pub use ca_bundle::{
    KEEL_CA_BUNDLE_LABEL, KEEL_CA_SECRET_NAME, build_keel_ca_bundle_configmap,
    is_keel_ca_bundle_target_namespace, validate_public_ca_pem,
};
pub use kyverno::{
    DockerSocketExceptionConfig, KyvernoManifestBundle, KyvernoPolicyConfig, PlatformOwnedWriter,
    build_disallow_privileged_containers_policy, build_local_kind_docker_socket_exception,
    build_protect_control_plane_policy, build_protect_platform_owned_tenant_objects_policy,
    build_restrict_pulumi_providers_policy, build_restrict_pulumi_providers_policy_for_registry,
    build_restrict_tenant_flux_source_status_policy, build_restrict_tenant_flux_sources_policy,
    build_restrict_tenant_pulumi_objects_policy, build_sync_keel_ca_bundle_manifests,
    is_tenant_subject, platform_owned_writers, pulumi_provider_allowlist,
};
pub use live::{
    CanaryStatusInfo, ClusterEventSummary, KubeClusterStatusReader, LiveResourceInfo,
    LiveResourceStatus, LiveStatusSource, ResourceCondition, ResourceMetadata, StackDiagnostics,
    extract_conditions, extract_last_synced, extract_ready_status, extract_resource_count,
    is_stack_manifest_protected, is_stack_object_protected,
};
pub use preview_runner::{
    DEFAULT_TENANT_GIT_BRANCH, InMemoryPreviewCluster, InMemoryPreviewSecret, KubePreviewCluster,
    PREVIEW_PROVIDER_SECRET_KIND, PreviewCluster, PreviewPrerequisites, TenantGitSource,
    legacy_flux_source_stacks, legacy_flux_source_tenant_stacks, preview_git_secret_name,
    preview_report_configmap_name, preview_s3_secret_name, tenant_git_repository_manifest_path,
    tenant_git_source_from_manifest, tenant_git_source_from_repository,
};
pub use s3_identity::{
    SeaweedFsIdentityOps, SeaweedFsTenantS3Provisioner, TenantS3IdentityProvisioner,
    TenantS3ProvisionOutcome, generate_tenant_s3_keys, provision_tenant_s3_identity_with,
};
pub use secret_store::{
    GITHUB_TOKEN_SECRET_NAME, InMemorySecretStore, K8sSecretStore, PULUMI_PASSPHRASE_SECRET_KEY,
    PULUMI_PASSPHRASE_SECRET_KIND, PULUMI_PASSPHRASE_SECRET_NAME, PulumiPassphraseOutcome,
    S3_CREDENTIALS_SECRET_NAME, SESSION_KEY_LEN, SESSION_KEY_SECRET_NAME, SecretStateStore,
    TENANT_SOPS_AGE_SECRET_NAME, TenantAgeKey, USER_GIT_CREDS_SECRET_PREFIX, UserGitCredentials,
    build_pulumi_passphrase_secret, generate_pulumi_passphrase, user_git_creds_secret_name,
};
pub use telemetry::{
    CrdReconcileStatus, CrdWatchEvent, DEFAULT_TELEMETRY_BUFFER_CAPACITY, TelemetryMux,
    WatchedCrdKind,
};
pub use tenant::{
    ENVOY_OWNING_GATEWAY_LABEL, FlaggerCanarySpecInput, FluxDependencyRef, FluxHealthCheckRef,
    FluxKustomizationSpecInput, FluxManifestAppInput, FluxSubstituteReference, HelmChartSourceKind,
    HelmChartSourceRef, HelmReleaseSpecInput, HelmValuesSecretRef, KEEL_SERVER_TENANT_CLUSTER_ROLE,
    MANIFEST_APP_CONFIG_DIR, PLATFORM_OWNED_KINDS, PLATFORM_OWNED_LABEL,
    PLATFORM_OWNED_TENANT_FILES, ROUTE_HOSTNAMES_ANNOTATION, RemoteKubeConfigRef,
    TENANT_NAMESPACE_PREFIX, TENANT_PKO_WORKSPACE_NETWORK_POLICY, TenantIsolationConfig,
    TenantManifestBundle, TenantQuotaSpec, build_keel_server_tenant_role_binding,
    build_tenant_default_deny_network_policy, build_tenant_namespace,
    build_tenant_pko_workspace_network_policy, build_tenant_resource_quota,
    build_tenant_service_account, ensure_tenant_pko_workspace_network_policy,
    generate_flux_kustomization_manifest, label_platform_owned_manifests,
    manifest_app_config_kustomization_name, manifest_app_secrets_name,
    manifest_app_values_configmap_name, platform_owned_labels_missing,
    synthesize_canary_gateway_network_policy, synthesize_canary_gateway_network_policy_yaml,
    synthesize_flagger_canary, synthesize_flagger_canary_yaml, synthesize_flux_kustomization,
    synthesize_flux_kustomization_yaml, synthesize_flux_manifest_app_kustomizations,
    synthesize_helm_app_network_policy_yaml, synthesize_helm_release, synthesize_helm_release_yaml,
    synthesize_remote_target_namespace, synthesize_remote_target_namespace_yaml, tenant_namespace,
    validate_tenant_id,
};
pub use tenant_repo::{
    PKO_OPERATOR_SSH_KNOWN_HOSTS_CONFIGMAP, TENANT_GITOPS_CREDENTIALS_SECRET_KIND,
    TENANT_GITOPS_CREDENTIALS_SECRET_NAME, TENANT_REPO_STUB_FILES, TENANT_ROOT_KUSTOMIZATION_NAME,
    TenantRepoCredential, TenantRepoSource, TenantRepoStub, TenantRepoTransport,
    build_pko_operator_known_hosts_configmap, build_tenant_gitops_credentials_secret,
    build_tenant_repo_git_repository, build_tenant_repo_kustomization_yaml,
    build_tenant_root_kustomization, ensure_git_safe_manifests, is_full_commit_sha,
    merge_pko_operator_ssh_known_hosts_values, pko_operator_needs_ssh_known_hosts,
    synthesize_tenant_repo_stub, tenant_repo_git_auth, tenant_repo_preview_git_source,
    tenant_repo_preview_secret_keys, tenant_repo_preview_secret_payload,
    tenant_repo_resources_from_monorepo, tenant_repo_stack_git_source,
    tenant_repo_stub_kustomization_entries, tenant_repo_workspace_template, validate_known_hosts,
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
