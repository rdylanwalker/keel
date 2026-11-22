//! Multi-tenant boundary isolation & Flux `HelmRelease` manifest synthesis (`SDD.md` §5.1, §6.2).
//!
//! Synthesizes tenant `Namespace` (enforcing `restricted` Pod Security Standards),
//! `ServiceAccount`, `ResourceQuota`, default-deny `NetworkPolicy`, and Flux CD
//! `HelmRelease` manifests with `spec.valuesFrom` (`kind: Secret`) bindings
//! and optional `spec.kubeConfig.secretRef` for remote workload clusters.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    LimitRange, LimitRangeItem, LimitRangeSpec, Namespace, ResourceQuota, ResourceQuotaSpec,
    ServiceAccount,
};
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicySpec};
use k8s_openapi::api::rbac::v1::{ClusterRoleBinding, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KubeError;

/// Prefix applied to every tenant namespace (`tenant-<tenant-id>`).
pub const TENANT_NAMESPACE_PREFIX: &str = "tenant-";

/// Maximum length for `<tenant-id>` so `tenant-<tenant-id>` fits RFC 1123 DNS label (63 chars).
const MAX_TENANT_ID_LEN: usize = 63 - TENANT_NAMESPACE_PREFIX.len();

/// Validates that a `<tenant-id>` is a safe, lowercase RFC 1123 DNS label segment.
pub fn validate_tenant_id(tenant_id: &str) -> Result<(), KubeError> {
    if tenant_id.is_empty() || tenant_id.len() > MAX_TENANT_ID_LEN {
        return Err(KubeError::InvalidInput(format!(
            "tenant ID `{tenant_id}` must be 1..={MAX_TENANT_ID_LEN} characters"
        )));
    }
    keel_core::validate_dns_label(tenant_id, "tenant_id")
        .map_err(|e| KubeError::InvalidInput(e.to_string()))
}

/// Validates a Kubernetes resource name (RFC 1123 DNS subdomain segment).
pub fn validate_resource_name(field: &str, name: &str) -> Result<(), KubeError> {
    keel_core::validate_dns_label(name, field).map_err(|e| KubeError::InvalidInput(e.to_string()))
}

/// Maps a validated `<tenant-id>` to its isolated Kubernetes namespace `tenant-<tenant-id>`.
pub fn tenant_namespace(tenant_id: &str) -> Result<String, KubeError> {
    validate_tenant_id(tenant_id)?;
    keel_core::tenant_namespace(tenant_id).map_err(|e| KubeError::InvalidInput(e.to_string()))
}

/// Returns the default Flux/PKO reconciliation `ServiceAccount` name for a tenant.
pub fn default_tenant_service_account_name(tenant_id: &str) -> Result<String, KubeError> {
    validate_tenant_id(tenant_id)?;
    Ok(format!("tenant-{tenant_id}-sa"))
}

/// Resource quota limits applied to a tenant namespace (`NFR-SEC-04`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantQuotaSpec {
    pub requests_cpu: String,
    pub requests_memory: String,
    pub limits_cpu: String,
    pub limits_memory: String,
    pub pods: String,
    pub persistent_volume_claims: String,
    pub services_loadbalancers: String,
}

impl Default for TenantQuotaSpec {
    fn default() -> Self {
        Self {
            requests_cpu: "4".to_string(),
            requests_memory: "8Gi".to_string(),
            limits_cpu: "8".to_string(),
            limits_memory: "16Gi".to_string(),
            pods: "50".to_string(),
            persistent_volume_claims: "20".to_string(),
            services_loadbalancers: "0".to_string(),
        }
    }
}

/// Configuration for synthesizing tenant namespace isolation manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantIsolationConfig {
    pub tenant_id: String,
    pub service_account_name: Option<String>,
    pub quota_name: String,
    pub quota: TenantQuotaSpec,
}

impl TenantIsolationConfig {
    pub fn new(tenant_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            service_account_name: None,
            quota_name: "tenant-resource-quota".to_string(),
            quota: TenantQuotaSpec::default(),
        }
    }

    pub fn resolved_service_account_name(&self) -> Result<String, KubeError> {
        match &self.service_account_name {
            Some(sa) => {
                validate_resource_name("service_account_name", sa)?;
                Ok(sa.clone())
            }
            None => default_tenant_service_account_name(&self.tenant_id),
        }
    }
}

/// Label marking objects the platform writes for a tenant (`tenants/<id>/rbac.yaml`, the
/// `keel-gitops` GitRepository stub and its NetworkPolicy). Kyverno
/// (`protect-platform-owned-tenant-objects`) reserves changes to them for `keel-tenant-reconciler`
/// and `keel-server`, so tenant identities (namespace `admin`) cannot edit or delete them.
pub const PLATFORM_OWNED_LABEL: &str = "keel.io/platform-owned";

/// [`tenant_common_labels`] plus `keel.io/platform-owned: "true"`, for the platform-written tenant
/// baseline objects only (never tenant app objects, which tenant identities apply).
pub(crate) fn platform_owned_labels(tenant_id: &str) -> BTreeMap<String, String> {
    let mut labels = tenant_common_labels(tenant_id);
    labels.insert(PLATFORM_OWNED_LABEL.to_string(), "true".to_string());
    labels
}

fn tenant_common_labels(tenant_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "keel".to_string(),
        ),
        ("keel.io/tenant".to_string(), tenant_id.to_string()),
    ])
}

/// `restricted` Pod Security Standards admission labels (`enforce`/`audit`/`warn`) for every
/// Keel-created tenant workload namespace (`NFR-SEC-03`).
fn restricted_pod_security_labels() -> [(String, String); 4] {
    [
        ("pod-security.kubernetes.io/enforce", "restricted"),
        ("pod-security.kubernetes.io/enforce-version", "latest"),
        ("pod-security.kubernetes.io/audit", "restricted"),
        ("pod-security.kubernetes.io/warn", "restricted"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()))
}

/// Generates the tenant `Namespace` (`tenant-<id>`) enforcing Kubernetes `restricted`
/// Pod Security Standards (`NFR-SEC-02`, `NFR-SEC-03`).
pub fn build_tenant_namespace(tenant_id: &str) -> Result<Namespace, KubeError> {
    let ns_name = tenant_namespace(tenant_id)?;
    let mut labels = platform_owned_labels(tenant_id);
    labels.extend(restricted_pod_security_labels());

    Ok(Namespace {
        metadata: ObjectMeta {
            name: Some(ns_name),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        ..Namespace::default()
    })
}

/// Generates the tenant-scoped `ServiceAccount` used for Flux `--no-cross-namespace-refs=true`
/// impersonation and PKO workspace execution.
pub fn build_tenant_service_account(
    config: &TenantIsolationConfig,
) -> Result<ServiceAccount, KubeError> {
    let ns_name = tenant_namespace(&config.tenant_id)?;
    let sa_name = config.resolved_service_account_name()?;

    Ok(ServiceAccount {
        metadata: ObjectMeta {
            name: Some(sa_name),
            namespace: Some(ns_name),
            labels: Some(platform_owned_labels(&config.tenant_id)),
            ..ObjectMeta::default()
        },
        automount_service_account_token: Some(true),
        ..ServiceAccount::default()
    })
}

/// Generates the tenant `ResourceQuota` manifest.
pub fn build_tenant_resource_quota(
    config: &TenantIsolationConfig,
) -> Result<ResourceQuota, KubeError> {
    let ns_name = tenant_namespace(&config.tenant_id)?;
    validate_resource_name("quota_name", &config.quota_name)?;

    let hard = BTreeMap::from([
        (
            "requests.cpu".to_string(),
            Quantity(config.quota.requests_cpu.clone()),
        ),
        (
            "requests.memory".to_string(),
            Quantity(config.quota.requests_memory.clone()),
        ),
        (
            "limits.cpu".to_string(),
            Quantity(config.quota.limits_cpu.clone()),
        ),
        (
            "limits.memory".to_string(),
            Quantity(config.quota.limits_memory.clone()),
        ),
        ("pods".to_string(), Quantity(config.quota.pods.clone())),
        (
            "persistentvolumeclaims".to_string(),
            Quantity(config.quota.persistent_volume_claims.clone()),
        ),
        (
            "services.loadbalancers".to_string(),
            Quantity(config.quota.services_loadbalancers.clone()),
        ),
    ]);

    Ok(ResourceQuota {
        metadata: ObjectMeta {
            name: Some(config.quota_name.clone()),
            namespace: Some(ns_name),
            labels: Some(platform_owned_labels(&config.tenant_id)),
            ..ObjectMeta::default()
        },
        spec: Some(ResourceQuotaSpec {
            hard: Some(hard),
            ..ResourceQuotaSpec::default()
        }),
        ..ResourceQuota::default()
    })
}

/// Generates a default-deny `NetworkPolicy` (`default-deny-all`) isolating all pods in `tenant-<id>`
/// across both `Ingress` and `Egress`.
pub fn build_tenant_default_deny_network_policy(
    tenant_id: &str,
) -> Result<NetworkPolicy, KubeError> {
    let ns_name = tenant_namespace(tenant_id)?;

    Ok(NetworkPolicy {
        metadata: ObjectMeta {
            name: Some("default-deny-all".to_string()),
            namespace: Some(ns_name),
            labels: Some(platform_owned_labels(tenant_id)),
            ..ObjectMeta::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector::default()),
            policy_types: Some(vec!["Ingress".to_string(), "Egress".to_string()]),
            ingress: None,
            egress: None,
        }),
    })
}

/// Name of the per-tenant `NetworkPolicy` that admits PKO workspace pods through
/// `default-deny-all`.
pub const TENANT_PKO_WORKSPACE_NETWORK_POLICY: &str = "allow-pko-workspace";

/// TCP port of the in-cluster SeaweedFS S3 state backend
/// (`keel-system/s3-state-backend-seaweedfs-s3`, `keel_pko::IN_CLUSTER_S3_BACKEND_URL`).
pub const IN_CLUSTER_S3_STATE_BACKEND_PORT: u16 = 8333;

/// `app` label of the local-dev `flux-system/keel-docker-proxy` (`local-docker-proxy.yaml`).
pub const LOCAL_DOCKER_PROXY_APP_LABEL: &str = "keel-docker-proxy";

/// Port of the local-dev Docker socket proxy (`DOCKER_HOST=tcp://keel-docker-proxy.flux-system:2375`).
pub const LOCAL_DOCKER_PROXY_PORT: u16 = 2375;

/// Private IPv4 ranges (RFC 1918 and CGNAT) PKO workspaces reach only on
/// [`PKO_WORKSPACE_PRIVATE_EGRESS_PORTS`]. Link-local `169.254.0.0/16` (cloud instance metadata
/// `169.254.169.254` on aws/gcp/azure/hetzner) is never allowed.
pub const PKO_WORKSPACE_PRIVATE_EGRESS_CIDRS: [&str; 4] = [
    "10.0.0.0/8",
    "100.64.0.0/10",
    "172.16.0.0/12",
    "192.168.0.0/16",
];

/// TCP ports PKO workspaces may use towards private addresses: Git over SSH (22), HTTPS forges and
/// the Kubernetes API Service (443), the API server endpoint after Service DNAT (6443) and the
/// Proxmox VE API (8006).
pub const PKO_WORKSPACE_PRIVATE_EGRESS_PORTS: [u16; 4] = [22, 443, 6443, 8006];

/// Generates the `allow-pko-workspace` `NetworkPolicy` every tenant needs to run PKO `Stack`s
/// through `default-deny-all`. Workspace pods (`auto.pulumi.com/component: workspace`) accept the
/// PKO operator (`keel-system`) on the agent gRPC port `50051`. Egress is an allowlist:
///
/// * cluster DNS (`kube-system`, 53);
/// * the `keel-ingress` Envoy proxy pods on `KEEL_INGRESS_PROXY_PORTS` (the `fetch` init
///   container clones `projectRepo` from the in-cluster Forgejo HTTPS ingress), never other
///   `keel-system` workloads (Forgejo's HTTP Service, the IdP, keel-server, SeaweedFS admin/filer);
/// * `keel-system` on `IN_CLUSTER_S3_STATE_BACKEND_PORT` (the default Pulumi state backend);
/// * the local-dev `flux-system/keel-docker-proxy` pods on `LOCAL_DOCKER_PROXY_PORT` (kind-in-kind
///   blueprints; present only with `--local-dev-docker-socket-exception`);
/// * `PKO_WORKSPACE_PRIVATE_EGRESS_CIDRS` on `PKO_WORKSPACE_PRIVATE_EGRESS_PORTS` (the
///   Kubernetes API server, private forges over HTTPS/SSH, the Proxmox VE API);
/// * public IPv4 on any port (`PUBLIC_EGRESS_EXCEPT_CIDRS` excluded: private, link-local,
///   loopback, multicast and reserved ranges), for cloud provider APIs and external forges.
///
/// Cloud instance metadata (`169.254.0.0/16`) is never reachable. Whether an `ipBlock` also
/// matches in-cluster pod IPs is CNI-specific, so `keel-system` workloads listening on 443/22 are
/// not guaranteed to be unreachable; this is defense in depth, not a hard boundary.
///
/// The selector label is not privileged: a tenant can put it on any pod, and tenant identities
/// can create their own `NetworkPolicy` objects, so default-deny for other tenant pods holds only
/// against workloads that do neither.
pub fn build_tenant_pko_workspace_network_policy(
    tenant_id: &str,
) -> Result<NetworkPolicy, KubeError> {
    let ns_name = tenant_namespace(tenant_id)?;
    let keel_system_selector = json!({
        "matchLabels": { "kubernetes.io/metadata.name": "keel-system" }
    });
    let egress = json!([
        {
            "to": [{
                "namespaceSelector": {
                    "matchLabels": { "kubernetes.io/metadata.name": "kube-system" }
                }
            }],
            "ports": [
                { "protocol": "UDP", "port": 53 },
                { "protocol": "TCP", "port": 53 }
            ]
        },
        {
            "to": [{
                "namespaceSelector": keel_system_selector.clone(),
                "podSelector": {
                    "matchLabels": { ENVOY_OWNING_GATEWAY_LABEL: "keel-ingress" }
                }
            }],
            "ports": tcp_ports(&KEEL_INGRESS_PROXY_PORTS)?,
        },
        {
            "to": [{ "namespaceSelector": keel_system_selector }],
            "ports": tcp_ports(&[IN_CLUSTER_S3_STATE_BACKEND_PORT])?,
        },
        {
            // Local-dev only: `--local-dev-docker-socket-exception` runs the `keel-docker-proxy`
            // in flux-system for kind-in-kind blueprints (`DOCKER_HOST`). Matches nothing when
            // the proxy is not deployed.
            "to": [{
                "namespaceSelector": {
                    "matchLabels": { "kubernetes.io/metadata.name": "flux-system" }
                },
                "podSelector": { "matchLabels": { "app": LOCAL_DOCKER_PROXY_APP_LABEL } }
            }],
            "ports": tcp_ports(&[LOCAL_DOCKER_PROXY_PORT])?,
        },
        {
            "to": PKO_WORKSPACE_PRIVATE_EGRESS_CIDRS
                .iter()
                .map(|cidr| json!({ "ipBlock": { "cidr": cidr } }))
                .collect::<Vec<_>>(),
            "ports": tcp_ports(&PKO_WORKSPACE_PRIVATE_EGRESS_PORTS)?,
        },
        {
            "to": [{
                "ipBlock": { "cidr": "0.0.0.0/0", "except": PUBLIC_EGRESS_EXCEPT_CIDRS }
            }]
        }
    ]);
    let value = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": TENANT_PKO_WORKSPACE_NETWORK_POLICY,
            "namespace": ns_name,
            "labels": platform_owned_labels(tenant_id),
        },
        "spec": {
            "podSelector": { "matchLabels": { "auto.pulumi.com/component": "workspace" } },
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": {
                        "matchLabels": { "kubernetes.io/metadata.name": "keel-system" }
                    }
                }],
                "ports": [{ "protocol": "TCP", "port": 50051 }]
            }],
            "egress": egress
        }
    });
    Ok(serde_json::from_value(value)?)
}

/// Generates a default `LimitRange` (`default-container-resources`) so upstream Helm charts
/// without explicit `resources.limits` satisfy the tenant `ResourceQuota`.
pub fn build_tenant_limit_range(tenant_id: &str) -> Result<LimitRange, KubeError> {
    let ns_name = tenant_namespace(tenant_id)?;

    let mut default_limits = BTreeMap::new();
    default_limits.insert("cpu".to_string(), Quantity("500m".to_string()));
    default_limits.insert("memory".to_string(), Quantity("1Gi".to_string()));

    let mut default_requests = BTreeMap::new();
    default_requests.insert("cpu".to_string(), Quantity("100m".to_string()));
    default_requests.insert("memory".to_string(), Quantity("128Mi".to_string()));

    Ok(LimitRange {
        metadata: ObjectMeta {
            name: Some("default-container-resources".to_string()),
            namespace: Some(ns_name),
            labels: Some(platform_owned_labels(tenant_id)),
            ..ObjectMeta::default()
        },
        spec: Some(LimitRangeSpec {
            limits: vec![LimitRangeItem {
                type_: "Container".to_string(),
                default: Some(default_limits),
                default_request: Some(default_requests),
                ..LimitRangeItem::default()
            }],
        }),
    })
}

/// Generates the namespace-scoped `RoleBinding` granting `ClusterRole/admin` to the tenant's
/// reconciliation `ServiceAccount` inside `tenant-<tenant-id>`.
pub fn build_tenant_admin_role_binding(
    config: &TenantIsolationConfig,
) -> Result<RoleBinding, KubeError> {
    let ns_name = tenant_namespace(&config.tenant_id)?;
    let sa_name = config.resolved_service_account_name()?;
    let binding_name = format!("{ns_name}-admin");

    Ok(RoleBinding {
        metadata: ObjectMeta {
            name: Some(binding_name),
            namespace: Some(ns_name.clone()),
            labels: Some(platform_owned_labels(&config.tenant_id)),
            ..ObjectMeta::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: "admin".to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: sa_name,
            namespace: Some(ns_name),
            api_group: None,
        }]),
    })
}

/// `ClusterRole` (shipped in `deploy/bootstrap/keel-server/rbac.yaml`) granting `keel-server` its
/// per-tenant operational access (scoped secrets, Stack/HelmRelease out-of-band operations).
pub const KEEL_SERVER_TENANT_CLUSTER_ROLE: &str = "keel-server-tenant";
/// `ServiceAccount` / `RoleBinding` name of the control-plane API server.
pub const KEEL_SERVER_SERVICE_ACCOUNT: &str = "keel-server";
/// Namespace hosting the `keel-server` `ServiceAccount`.
pub const KEEL_SERVER_NAMESPACE: &str = "keel-system";

/// Generates the `RoleBinding` (`keel-server`) inside `tenant-<tenant-id>` that binds the
/// `keel-server-tenant` `ClusterRole` to `keel-system/keel-server`. Tenant namespaces are dynamic,
/// so every tenant bundle must carry this binding or the server loses access to the tenant.
pub fn build_keel_server_tenant_role_binding(tenant_id: &str) -> Result<RoleBinding, KubeError> {
    let ns_name = tenant_namespace(tenant_id)?;
    Ok(RoleBinding {
        metadata: ObjectMeta {
            name: Some(KEEL_SERVER_SERVICE_ACCOUNT.to_string()),
            namespace: Some(ns_name),
            labels: Some(platform_owned_labels(tenant_id)),
            ..ObjectMeta::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: KEEL_SERVER_TENANT_CLUSTER_ROLE.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: KEEL_SERVER_SERVICE_ACCOUNT.to_string(),
            namespace: Some(KEEL_SERVER_NAMESPACE.to_string()),
            api_group: None,
        }]),
    })
}

/// Generates the `ClusterRoleBinding` granting `system:auth-delegator` to the tenant's
/// `ServiceAccount` so PKO v2 workspace pods can authenticate Kubernetes gRPC calls.
pub fn build_tenant_auth_delegator_binding(
    config: &TenantIsolationConfig,
) -> Result<ClusterRoleBinding, KubeError> {
    let ns_name = tenant_namespace(&config.tenant_id)?;
    let sa_name = config.resolved_service_account_name()?;
    let binding_name = format!("{ns_name}:{sa_name}:system:auth-delegator");

    Ok(ClusterRoleBinding {
        metadata: ObjectMeta {
            name: Some(binding_name),
            labels: Some(platform_owned_labels(&config.tenant_id)),
            ..ObjectMeta::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: "system:auth-delegator".to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: sa_name,
            namespace: Some(ns_name),
            api_group: None,
        }]),
    })
}

/// Synthesized bundle of baseline multi-tenant isolation manifests (`tenants/<tenant-id>/rbac.yaml`).
#[derive(Debug, Clone, PartialEq)]
pub struct TenantManifestBundle {
    pub namespace: Namespace,
    pub service_account: ServiceAccount,
    pub resource_quota: ResourceQuota,
    pub limit_range: LimitRange,
    pub network_policy: NetworkPolicy,
    /// `allow-pko-workspace`: PKO workspace pods through `default-deny-all`.
    pub pko_workspace_network_policy: NetworkPolicy,
    pub role_binding: RoleBinding,
    pub keel_server_role_binding: RoleBinding,
    pub auth_delegator_binding: ClusterRoleBinding,
}

impl TenantManifestBundle {
    /// Synthesizes all baseline isolation and reconciliation RBAC resources for a tenant.
    pub fn synthesize(config: &TenantIsolationConfig) -> Result<Self, KubeError> {
        Ok(Self {
            namespace: build_tenant_namespace(&config.tenant_id)?,
            service_account: build_tenant_service_account(config)?,
            resource_quota: build_tenant_resource_quota(config)?,
            limit_range: build_tenant_limit_range(&config.tenant_id)?,
            network_policy: build_tenant_default_deny_network_policy(&config.tenant_id)?,
            pko_workspace_network_policy: build_tenant_pko_workspace_network_policy(
                &config.tenant_id,
            )?,
            role_binding: build_tenant_admin_role_binding(config)?,
            keel_server_role_binding: build_keel_server_tenant_role_binding(&config.tenant_id)?,
            auth_delegator_binding: build_tenant_auth_delegator_binding(config)?,
        })
    }

    /// Serializes all tenant isolation manifests to JSON `Value` documents.
    pub fn to_json_values(&self) -> Result<Vec<Value>, KubeError> {
        Ok(vec![
            serde_json::to_value(&self.namespace)?,
            serde_json::to_value(&self.service_account)?,
            serde_json::to_value(&self.resource_quota)?,
            serde_json::to_value(&self.limit_range)?,
            serde_json::to_value(&self.network_policy)?,
            serde_json::to_value(&self.pko_workspace_network_policy)?,
            serde_json::to_value(&self.role_binding)?,
            serde_json::to_value(&self.keel_server_role_binding)?,
            serde_json::to_value(&self.auth_delegator_binding)?,
        ])
    }

    /// Serializes all tenant isolation manifests into a multi-document YAML string (`---` separated).
    pub fn to_multi_doc_yaml(&self) -> Result<String, KubeError> {
        Ok(keel_core::to_yaml_documents(self.to_json_values()?)?)
    }
}

/// Flux source kinds supported by `HelmRelease.spec.chart.spec.sourceRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelmChartSourceKind {
    HelmRepository,
    GitRepository,
    OCIRepository,
}

/// Source reference for a Flux `HelmRelease` chart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelmChartSourceRef {
    pub kind: HelmChartSourceKind,
    pub name: String,
    /// Optional explicit namespace. When set, must match `tenant-<id>` to comply with
    /// Flux `--no-cross-namespace-refs=true` (`SDD.md` §6.2).
    pub namespace: Option<String>,
}

/// Secret binding rendered into `HelmRelease.spec.valuesFrom` (`kind: Secret`): SOPS app secrets
/// or bundle item outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelmValuesSecretRef {
    /// Name of the tenant-scoped Kubernetes `Secret` holding bundle item outputs or SOPS secrets.
    pub name: String,
    /// Optional key inside the `Secret` (defaults to `values.yaml` in Flux when omitted).
    pub values_key: Option<String>,
    /// Optional dot-delimited Helm value target path (e.g. `authentik.postgresql.host`).
    pub target_path: Option<String>,
    /// Whether Flux may proceed if the `Secret` is optional.
    pub optional: Option<bool>,
}

impl HelmValuesSecretRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            values_key: None,
            target_path: None,
            optional: None,
        }
    }
}

/// Remote workload cluster `kubeconfig` reference (`HelmRelease.spec.kubeConfig.secretRef`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteKubeConfigRef {
    pub secret_name: String,
    pub key: Option<String>,
}

impl RemoteKubeConfigRef {
    pub fn new(secret_name: impl Into<String>) -> Self {
        Self {
            secret_name: secret_name.into(),
            key: None,
        }
    }

    pub fn with_key(secret_name: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            secret_name: secret_name.into(),
            key: Some(key.into()),
        }
    }
}

/// Same-namespace dependency entry for `HelmRelease.spec.dependsOn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluxDependencyRef {
    pub name: String,
}

/// Input specification for synthesizing an unmodified upstream Flux `HelmRelease` (`helm.toolkit.fluxcd.io/v2`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelmReleaseSpecInput {
    pub tenant_id: String,
    pub release_name: String,
    pub service_account_name: Option<String>,
    pub interval: String,
    pub chart: String,
    pub chart_version: Option<String>,
    pub source_ref: HelmChartSourceRef,
    pub values: Option<Value>,
    pub values_from_secrets: Vec<HelmValuesSecretRef>,
    pub kube_config: Option<RemoteKubeConfigRef>,
    pub target_namespace: Option<String>,
    pub depends_on: Vec<FluxDependencyRef>,
    #[serde(default)]
    pub extra_labels: BTreeMap<String, String>,
    /// `metadata.annotations` (e.g. `keel.io/route-hostnames`).
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// `spec.install.crds` / `spec.upgrade.crds` (catalog `spec.crdPolicy`); `None` keeps
    /// Flux's defaults (`Create` on install, `Skip` on upgrade).
    #[serde(default)]
    pub crds: Option<keel_core::HelmCrdPolicy>,
}

/// `HelmRelease` annotation listing (comma-separated) the Gateway route hostnames a Helm catalog
/// app's chart renders (`spec.gatewayRoutes`), so later proposals can detect hostname claims
/// without rendering the chart.
pub const ROUTE_HOSTNAMES_ANNOTATION: &str = "keel.io/route-hostnames";

impl HelmReleaseSpecInput {
    pub fn new(
        tenant_id: impl Into<String>,
        release_name: impl Into<String>,
        chart: impl Into<String>,
        source_ref: HelmChartSourceRef,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            release_name: release_name.into(),
            service_account_name: None,
            interval: "5m".to_string(),
            chart: chart.into(),
            chart_version: None,
            source_ref,
            values: None,
            values_from_secrets: Vec::new(),
            kube_config: None,
            target_namespace: None,
            depends_on: Vec::new(),
            extra_labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            crds: None,
        }
    }

    /// Helm release name Flux derives for this `HelmRelease` (`[<targetNamespace>-]<name>`,
    /// no `spec.releaseName` is set), i.e. the pods' `app.kubernetes.io/instance` label.
    pub fn helm_release_name(&self) -> String {
        match &self.target_namespace {
            Some(ns) => format!("{ns}-{}", self.release_name),
            None => self.release_name.clone(),
        }
    }
}

/// Synthesizes a Flux CD `HelmRelease` (`helm.toolkit.fluxcd.io/v2`) JSON manifest enforcing:
/// - `tenant-<id>` namespace confinement (`--no-cross-namespace-refs=true`),
/// - mandatory `spec.serviceAccountName` impersonation,
/// - `spec.valuesFrom` (`kind: Secret`) bindings (SOPS app secrets, bundle item outputs), and
/// - optional `spec.kubeConfig.secretRef` for remote workload clusters.
pub fn synthesize_helm_release(input: &HelmReleaseSpecInput) -> Result<Value, KubeError> {
    let namespace = tenant_namespace(&input.tenant_id)?;
    validate_resource_name("release_name", &input.release_name)?;
    validate_resource_name("source_ref.name", &input.source_ref.name)?;

    if input.chart.trim().is_empty() {
        return Err(KubeError::InvalidInput(
            "HelmRelease chart name must not be empty".to_string(),
        ));
    }

    if let Some(src_ns) = &input.source_ref.namespace
        && src_ns != &namespace
    {
        return Err(KubeError::InvalidInput(format!(
            "cross-namespace sourceRef (`{src_ns}`) is forbidden; expected `{namespace}`"
        )));
    }

    let sa_name = match &input.service_account_name {
        Some(sa) => {
            validate_resource_name("service_account_name", sa)?;
            sa.clone()
        }
        None => default_tenant_service_account_name(&input.tenant_id)?,
    };

    let mut chart_spec = serde_json::Map::new();
    chart_spec.insert("chart".to_string(), Value::String(input.chart.clone()));
    if let Some(version) = &input.chart_version {
        chart_spec.insert("version".to_string(), Value::String(version.clone()));
    }
    chart_spec.insert(
        "sourceRef".to_string(),
        json!({
            "kind": input.source_ref.kind,
            "name": input.source_ref.name,
            "namespace": namespace,
        }),
    );

    let mut spec = serde_json::Map::new();
    spec.insert(
        "interval".to_string(),
        Value::String(input.interval.clone()),
    );
    // Always explicit: under Flux multi-tenancy lockdown (`--default-service-account`) an empty
    // value makes the controller impersonate `default`, and for remote targets (`kubeConfig`)
    // that impersonation happens on the workload cluster, whose blueprint binds exactly this
    // tenant ServiceAccount identity.
    spec.insert("serviceAccountName".to_string(), Value::String(sa_name));
    spec.insert("chart".to_string(), json!({ "spec": chart_spec }));

    if !input.depends_on.is_empty() {
        let mut deps = Vec::with_capacity(input.depends_on.len());
        for dep in &input.depends_on {
            validate_resource_name("depends_on.name", &dep.name)?;
            deps.push(json!({
                "name": dep.name,
                "namespace": namespace,
            }));
        }
        spec.insert("dependsOn".to_string(), Value::Array(deps));
    }

    if let Some(kc) = &input.kube_config {
        validate_resource_name("kube_config.secret_name", &kc.secret_name)?;
        let mut secret_ref = serde_json::Map::new();
        secret_ref.insert("name".to_string(), Value::String(kc.secret_name.clone()));
        if let Some(key) = &kc.key {
            secret_ref.insert("key".to_string(), Value::String(key.clone()));
        }
        spec.insert("kubeConfig".to_string(), json!({ "secretRef": secret_ref }));
        spec.insert("install".to_string(), json!({ "createNamespace": true }));
        if input.target_namespace.is_none() {
            spec.insert(
                "targetNamespace".to_string(),
                Value::String(namespace.clone()),
            );
        }
    }

    if let Some(target_ns) = &input.target_namespace {
        validate_resource_name("target_namespace", target_ns)?;
        if input.kube_config.is_none() && target_ns != &namespace {
            return Err(KubeError::InvalidInput(format!(
                "target_namespace (`{target_ns}`) requires remote kube_config; local releases are confined to `{namespace}`"
            )));
        }
        spec.insert(
            "targetNamespace".to_string(),
            Value::String(target_ns.clone()),
        );
        spec.insert(
            "storageNamespace".to_string(),
            Value::String(target_ns.clone()),
        );
        spec.insert("install".to_string(), json!({ "createNamespace": true }));
    }

    if let Some(policy) = input.crds {
        let install = spec
            .entry("install".to_string())
            .or_insert_with(|| json!({}));
        install["crds"] = Value::String(policy.as_str().to_string());
        spec.insert("upgrade".to_string(), json!({ "crds": policy.as_str() }));
    }

    if let Some(values) = &input.values {
        spec.insert("values".to_string(), values.clone());
    }

    if !input.values_from_secrets.is_empty() {
        let mut values_from = Vec::with_capacity(input.values_from_secrets.len());
        for item in &input.values_from_secrets {
            validate_resource_name("values_from_secrets.name", &item.name)?;
            let mut entry = serde_json::Map::new();
            entry.insert("kind".to_string(), Value::String("Secret".to_string()));
            entry.insert("name".to_string(), Value::String(item.name.clone()));
            if let Some(values_key) = &item.values_key {
                entry.insert("valuesKey".to_string(), Value::String(values_key.clone()));
            }
            if let Some(target_path) = &item.target_path {
                entry.insert("targetPath".to_string(), Value::String(target_path.clone()));
            }
            if let Some(optional) = item.optional {
                entry.insert("optional".to_string(), Value::Bool(optional));
            }
            values_from.push(Value::Object(entry));
        }
        spec.insert("valuesFrom".to_string(), Value::Array(values_from));
    }

    let mut labels = tenant_common_labels(&input.tenant_id);
    for (k, v) in &input.extra_labels {
        labels.insert(k.clone(), v.clone());
    }

    let mut metadata = json!({
        "name": input.release_name,
        "namespace": namespace,
        "labels": labels,
    });
    if !input.annotations.is_empty() {
        metadata["annotations"] = json!(input.annotations);
    }

    Ok(json!({
        "apiVersion": "helm.toolkit.fluxcd.io/v2",
        "kind": "HelmRelease",
        "metadata": metadata,
        "spec": spec,
    }))
}

/// Synthesizes a Flux CD `HelmRelease` manifest directly as YAML.
pub fn synthesize_helm_release_yaml(input: &HelmReleaseSpecInput) -> Result<String, KubeError> {
    let manifest = synthesize_helm_release(input)?;
    Ok(keel_core::yaml::to_string(&manifest)?)
}

/// Name of the shared platform Gateway (`keel-system/keel-ingress`) that tenant routes and
/// Flagger `Canary`s attach to.
pub const KEEL_INGRESS_GATEWAY_NAME: &str = "keel-ingress";

/// Flagger mesh provider of every Keel `Canary` (Envoy Gateway through the Gateway API).
pub const FLAGGER_CANARY_PROVIDER: &str = "gatewayapi:v1";

/// Input specification for synthesizing a Flagger `Canary` (`flagger.app/v1beta1`) targeting
/// Envoy Gateway (`gatewayapi:v1`) and Prometheus (`SDD.md` §5.2, `NFR-REL-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlaggerCanarySpecInput {
    pub tenant_id: String,
    pub release_name: String,
    pub target_deployment_name: Option<String>,
    pub service_port: u16,
    pub gateway_name: String,
    pub gateway_namespace: String,
    pub hosts: Vec<String>,
    pub extra_labels: BTreeMap<String, String>,
}

impl FlaggerCanarySpecInput {
    pub fn new(tenant_id: impl Into<String>, release_name: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            release_name: release_name.into(),
            target_deployment_name: None,
            service_port: 80,
            gateway_name: KEEL_INGRESS_GATEWAY_NAME.to_string(),
            gateway_namespace: keel_core::KEEL_SYSTEM_NAMESPACE.to_string(),
            hosts: Vec::new(),
            extra_labels: BTreeMap::new(),
        }
    }
}

/// Synthesizes a Flagger `Canary` (`flagger.app/v1beta1`) manifest for Envoy Gateway + Prometheus.
pub fn synthesize_flagger_canary(input: &FlaggerCanarySpecInput) -> Result<Value, KubeError> {
    let namespace = tenant_namespace(&input.tenant_id)?;
    validate_resource_name("release_name", &input.release_name)?;
    let target_name = input
        .target_deployment_name
        .as_deref()
        .unwrap_or(&input.release_name);
    validate_resource_name("target_deployment_name", target_name)?;
    validate_resource_name("gateway_name", &input.gateway_name)?;
    validate_resource_name("gateway_namespace", &input.gateway_namespace)?;

    if input.service_port == 0 {
        return Err(KubeError::InvalidInput(
            "Flagger Canary service_port must be >= 1".to_string(),
        ));
    }

    let mut service = serde_json::Map::new();
    service.insert("port".to_string(), json!(input.service_port));
    service.insert("targetPort".to_string(), json!(input.service_port));
    service.insert(
        "gatewayRefs".to_string(),
        json!([
            {
                "name": input.gateway_name,
                "namespace": input.gateway_namespace,
                "group": "gateway.networking.k8s.io",
                "kind": "Gateway"
            }
        ]),
    );
    let clean_hosts: Vec<String> = input
        .hosts
        .iter()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    if !clean_hosts.is_empty() {
        service.insert("hosts".to_string(), json!(clean_hosts));
    }

    let mut labels = tenant_common_labels(&input.tenant_id);
    for (k, v) in &input.extra_labels {
        labels.insert(k.clone(), v.clone());
    }

    Ok(json!({
        "apiVersion": "flagger.app/v1beta1",
        "kind": "Canary",
        "metadata": {
            "name": input.release_name,
            "namespace": namespace,
            "labels": labels,
        },
        "spec": {
            "provider": FLAGGER_CANARY_PROVIDER,
            "targetRef": {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "name": target_name,
            },
            "service": Value::Object(service),
            "analysis": {
                "interval": "30s",
                "threshold": 5,
                "maxWeight": 50,
                "stepWeight": 10,
                "metrics": [
                    {
                        "name": "envoy-request-success-rate",
                        "templateRef": {
                            "name": "envoy-gateway-request-success-rate",
                            "namespace": input.gateway_namespace
                        },
                        "thresholdRange": { "min": 99 },
                        "interval": "2m"
                    },
                    {
                        "name": "envoy-request-duration",
                        "templateRef": {
                            "name": "envoy-gateway-request-duration",
                            "namespace": input.gateway_namespace
                        },
                        "thresholdRange": { "max": 500 },
                        "interval": "2m"
                    }
                ]
            }
        }
    }))
}

/// Synthesizes a Flagger `Canary` manifest directly as YAML.
pub fn synthesize_flagger_canary_yaml(input: &FlaggerCanarySpecInput) -> Result<String, KubeError> {
    let manifest = synthesize_flagger_canary(input)?;
    Ok(keel_core::yaml::to_string(&manifest)?)
}

/// Synthesizes a tenant `NetworkPolicy` allowing Envoy Gateway proxy pods (`keel-system`)
/// to reach the application's service port through `default-deny-all` (`SDD.md` §5.2).
pub fn synthesize_canary_gateway_network_policy(
    tenant_id: &str,
    release_name: &str,
    service_port: u16,
) -> Result<Value, KubeError> {
    let namespace = tenant_namespace(tenant_id)?;
    validate_resource_name("release_name", release_name)?;
    if service_port == 0 {
        return Err(KubeError::InvalidInput(
            "NetworkPolicy service_port must be >= 1".to_string(),
        ));
    }

    Ok(json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": format!("allow-{release_name}-envoy-ingress"),
            "namespace": namespace,
            "labels": tenant_common_labels(tenant_id),
        },
        "spec": {
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [
                {
                    "from": [
                        {
                            "namespaceSelector": {
                                "matchLabels": {
                                    "kubernetes.io/metadata.name": "keel-system"
                                }
                            }
                        }
                    ],
                    "ports": [
                        {
                            "protocol": "TCP",
                            "port": service_port
                        }
                    ]
                }
            ]
        }
    }))
}

/// Synthesizes a tenant Envoy Gateway ingress `NetworkPolicy` directly as YAML.
pub fn synthesize_canary_gateway_network_policy_yaml(
    tenant_id: &str,
    release_name: &str,
    service_port: u16,
) -> Result<String, KubeError> {
    let manifest = synthesize_canary_gateway_network_policy(tenant_id, release_name, service_port)?;
    Ok(keel_core::yaml::to_string(&manifest)?)
}

/// Synthesizes a `Namespace` for a tenant workload's target namespace on a **remote** cluster.
///
/// Remote Flux `Kustomization`s (`spec.kubeConfig` + `spec.targetNamespace`) that apply namespaced
/// objects next to a sibling `HelmRelease` would otherwise race Helm's `install.createNamespace`.
/// kustomize-controller applies `Namespace` objects before namespaced ones, so including this
/// manifest removes the race. It is annotated `prune: disabled` (never delete a namespace Helm
/// may also own) and `ssa: IfNotPresent` (never fight an existing namespace). Like
/// `build_tenant_namespace` it enforces the `restricted` Pod Security Standard (`NFR-SEC-03`):
/// workload clusters carry no guaranteed Kyverno baseline. Must not be used on
/// management-cluster tenant paths, where the tenant reconciler cannot create namespaces.
pub fn synthesize_remote_target_namespace(
    tenant_id: &str,
    namespace: &str,
) -> Result<Value, KubeError> {
    validate_tenant_id(tenant_id)?;
    validate_resource_name("namespace", namespace)?;
    let mut labels = tenant_common_labels(tenant_id);
    labels.extend(restricted_pod_security_labels());
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": namespace,
            "labels": labels,
            "annotations": {
                "kustomize.toolkit.fluxcd.io/prune": "disabled",
                "kustomize.toolkit.fluxcd.io/ssa": "IfNotPresent",
            },
        },
    }))
}

/// Synthesizes a remote target `Namespace` (see [`synthesize_remote_target_namespace`]) as YAML.
pub fn synthesize_remote_target_namespace_yaml(
    tenant_id: &str,
    namespace: &str,
) -> Result<String, KubeError> {
    let manifest = synthesize_remote_target_namespace(tenant_id, namespace)?;
    Ok(keel_core::yaml::to_string(&manifest)?)
}

/// Health-check target for `Kustomization.spec.healthChecks` (e.g., waiting on a PKO `Stack` CR).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluxHealthCheckRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
}

/// Reference to a `ConfigMap` or `Secret` for Flux `Kustomization.spec.postBuild.substituteFrom`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluxSubstituteReference {
    pub kind: String, // "ConfigMap" or "Secret"
    pub name: String,
    #[serde(default)]
    pub optional: bool,
}

/// Input specification for synthesizing a tenant-scoped Flux `Kustomization` (`kustomize.toolkit.fluxcd.io/v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluxKustomizationSpecInput {
    pub tenant_id: String,
    pub name: String,
    pub service_account_name: Option<String>,
    pub interval: String,
    pub source_name: String,
    pub path: String,
    pub depends_on: Vec<FluxDependencyRef>,
    pub health_checks: Vec<FluxHealthCheckRef>,
    pub prune: bool,
    pub wait: bool,
    #[serde(default)]
    pub kube_config: Option<RemoteKubeConfigRef>,
    #[serde(default)]
    pub target_namespace: Option<String>,
    #[serde(default)]
    pub extra_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub post_build_substitute: BTreeMap<String, String>,
    #[serde(default)]
    pub post_build_substitute_from: Vec<FluxSubstituteReference>,
    /// Same-namespace Secret holding the tenant SOPS `age` key (`*.agekey`). When set, the
    /// Kustomization decrypts SOPS-encrypted resources (e.g. a manifest app's `secrets.enc.yaml`)
    /// with `spec.decryption` (`provider: sops`) before applying them.
    #[serde(default)]
    pub decryption_secret_name: Option<String>,
}

impl FluxKustomizationSpecInput {
    pub fn new(
        tenant_id: impl Into<String>,
        name: impl Into<String>,
        source_name: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            name: name.into(),
            service_account_name: None,
            interval: "2m".to_string(),
            source_name: source_name.into(),
            path: path.into(),
            depends_on: Vec::new(),
            health_checks: Vec::new(),
            prune: true,
            wait: true,
            kube_config: None,
            target_namespace: None,
            extra_labels: BTreeMap::new(),
            post_build_substitute: BTreeMap::new(),
            post_build_substitute_from: Vec::new(),
            decryption_secret_name: None,
        }
    }
}

/// Synthesizes a tenant-scoped Flux `Kustomization` (`kustomize.toolkit.fluxcd.io/v1`) enforcing
/// `--no-cross-namespace-refs=true` (`sourceRef` & `dependsOn` confined to `tenant-<id>`) and `serviceAccountName`.
pub fn synthesize_flux_kustomization(
    input: &FluxKustomizationSpecInput,
) -> Result<Value, KubeError> {
    let namespace = tenant_namespace(&input.tenant_id)?;
    validate_resource_name("name", &input.name)?;
    validate_resource_name("source_name", &input.source_name)?;

    if input.path.trim().is_empty() {
        return Err(KubeError::InvalidInput(
            "Kustomization path must not be empty".to_string(),
        ));
    }

    let sa_name = match &input.service_account_name {
        Some(sa) => {
            validate_resource_name("service_account_name", sa)?;
            sa.clone()
        }
        None => default_tenant_service_account_name(&input.tenant_id)?,
    };

    let mut spec = serde_json::Map::new();
    spec.insert(
        "interval".to_string(),
        Value::String(input.interval.clone()),
    );
    spec.insert("path".to_string(), Value::String(input.path.clone()));
    spec.insert("prune".to_string(), Value::Bool(input.prune));
    spec.insert("wait".to_string(), Value::Bool(input.wait));
    // Always explicit: under Flux multi-tenancy lockdown (`--default-service-account`) an empty
    // value makes the controller impersonate `default`, and for remote targets (`kubeConfig`)
    // that impersonation happens on the workload cluster, whose blueprint binds exactly this
    // tenant ServiceAccount identity.
    spec.insert("serviceAccountName".to_string(), Value::String(sa_name));
    if let Some(kc) = &input.kube_config {
        validate_resource_name("kube_config.secret_name", &kc.secret_name)?;
        let mut secret_ref = serde_json::Map::new();
        secret_ref.insert("name".to_string(), Value::String(kc.secret_name.clone()));
        if let Some(key) = &kc.key {
            secret_ref.insert("key".to_string(), Value::String(key.clone()));
        }
        spec.insert("kubeConfig".to_string(), json!({ "secretRef": secret_ref }));
    }
    if let Some(target_ns) = &input.target_namespace {
        validate_resource_name("target_namespace", target_ns)?;
        if input.kube_config.is_none() && target_ns != &namespace {
            return Err(KubeError::InvalidInput(format!(
                "target_namespace (`{target_ns}`) requires remote kube_config; local Kustomizations are confined to `{namespace}`"
            )));
        }
        spec.insert(
            "targetNamespace".to_string(),
            Value::String(target_ns.clone()),
        );
    } else if input.kube_config.is_none() {
        // Flux multi-tenancy lockdown: local tenant Kustomizations impersonate the tenant SA and
        // force every namespaced object into `tenant-<id>`.
        spec.insert(
            "targetNamespace".to_string(),
            Value::String(namespace.clone()),
        );
    }
    spec.insert(
        "sourceRef".to_string(),
        json!({
            "kind": "GitRepository",
            "name": input.source_name,
            "namespace": namespace,
        }),
    );

    if !input.depends_on.is_empty() {
        let mut deps = Vec::with_capacity(input.depends_on.len());
        for dep in &input.depends_on {
            validate_resource_name("depends_on.name", &dep.name)?;
            deps.push(json!({
                "name": dep.name,
                "namespace": namespace,
            }));
        }
        spec.insert("dependsOn".to_string(), Value::Array(deps));
    }

    if !input.health_checks.is_empty() {
        let mut checks = Vec::with_capacity(input.health_checks.len());
        for hc in &input.health_checks {
            validate_resource_name("health_checks.name", &hc.name)?;
            let hc_ns = hc.namespace.as_deref().unwrap_or(&namespace);
            if hc_ns != namespace {
                return Err(KubeError::InvalidInput(format!(
                    "cross-namespace healthCheck (`{hc_ns}`) is forbidden; expected `{namespace}`"
                )));
            }
            checks.push(json!({
                "apiVersion": hc.api_version,
                "kind": hc.kind,
                "name": hc.name,
                "namespace": hc_ns,
            }));
        }
        spec.insert("healthChecks".to_string(), Value::Array(checks));
    }

    if let Some(secret_name) = &input.decryption_secret_name {
        validate_resource_name("decryption_secret_name", secret_name)?;
        spec.insert(
            "decryption".to_string(),
            json!({
                "provider": "sops",
                "secretRef": { "name": secret_name },
            }),
        );
    }

    if !input.post_build_substitute.is_empty() || !input.post_build_substitute_from.is_empty() {
        let mut post_build = serde_json::Map::new();
        if !input.post_build_substitute.is_empty() {
            post_build.insert("substitute".to_string(), json!(input.post_build_substitute));
        }
        if !input.post_build_substitute_from.is_empty() {
            let mut sub_from = Vec::with_capacity(input.post_build_substitute_from.len());
            for sr in &input.post_build_substitute_from {
                if sr.kind != "ConfigMap" && sr.kind != "Secret" {
                    return Err(KubeError::InvalidInput(format!(
                        "post_build_substitute_from.kind must be 'ConfigMap' or 'Secret', got '{}'",
                        sr.kind
                    )));
                }
                validate_resource_name("post_build_substitute_from.name", &sr.name)?;
                sub_from.push(json!({
                    "kind": sr.kind,
                    "name": sr.name,
                    "optional": sr.optional,
                }));
            }
            post_build.insert("substituteFrom".to_string(), Value::Array(sub_from));
        }
        spec.insert("postBuild".to_string(), Value::Object(post_build));
    }

    let mut labels = tenant_common_labels(&input.tenant_id);
    for (k, v) in &input.extra_labels {
        labels.insert(k.clone(), v.clone());
    }

    Ok(json!({
        "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
        "kind": "Kustomization",
        "metadata": {
            "name": input.name,
            "namespace": namespace,
            "labels": labels,
        },
        "spec": spec,
    }))
}

/// Synthesizes a tenant-scoped Flux `Kustomization` manifest directly as YAML.
pub fn synthesize_flux_kustomization_yaml(
    input: &FluxKustomizationSpecInput,
) -> Result<String, KubeError> {
    let manifest = synthesize_flux_kustomization(input)?;
    Ok(keel_core::yaml::to_string(&manifest)?)
}

/// Sub-directory (relative to a manifest app's directory) holding the app's substitution
/// sources: the values `ConfigMap` and the SOPS-encrypted `<app>-secrets` `Secret`.
pub const MANIFEST_APP_CONFIG_DIR: &str = "config";

/// Name of the Flux `Kustomization` that applies a manifest app's substitution sources.
pub fn manifest_app_config_kustomization_name(app_name: &str) -> String {
    format!("{app_name}-config")
}

/// Name of a manifest app's non-secret values `ConfigMap` (`postBuild.substituteFrom`).
pub fn manifest_app_values_configmap_name(app_name: &str) -> String {
    format!("{app_name}-values")
}

/// Name of a manifest app's SOPS-decrypted secret values `Secret` (`postBuild.substituteFrom`).
pub fn manifest_app_secrets_name(app_name: &str) -> String {
    format!("{app_name}-secrets")
}

/// Input for [`synthesize_flux_manifest_app_kustomizations`] (a `runtime: manifest` app on the
/// Flux engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FluxManifestAppInput {
    pub tenant_id: String,
    /// App (and app `Kustomization`) name; also prefixes the config objects.
    pub name: String,
    pub source_name: String,
    /// Repo-relative app directory (e.g. `tenants/acme/apps/rauthy`), without a leading `./`.
    pub app_dir: String,
    /// Remote workload cluster for the app manifests. Never applied to the config `Kustomization`.
    pub kube_config: Option<RemoteKubeConfigRef>,
    pub target_namespace: Option<String>,
    /// `true` when the app has SOPS-encrypted secret values (`config/secrets.enc.yaml`).
    pub has_secrets: bool,
    /// Additional app `Kustomization` dependencies (e.g. bundle items).
    pub depends_on: Vec<FluxDependencyRef>,
    pub extra_labels: BTreeMap<String, String>,
}

/// Synthesizes the two Flux `Kustomization`s of a Flux-engine manifest app as YAML:
/// `(config, app)`.
///
/// * `<name>-config` (path `<app_dir>/config`) applies the values `ConfigMap` and the
///   SOPS-decrypted `<name>-secrets` `Secret` in `tenant-<id>` of the **management** cluster: it
///   never carries `kubeConfig`, because Flux reads `postBuild.substituteFrom` sources from the
///   app `Kustomization`'s own namespace on the cluster where kustomize-controller runs.
/// * `<name>` (path `<app_dir>`) applies the rendered manifests (optionally to a remote cluster)
///   and `dependsOn` `<name>-config`, so the substitution sources exist before the first build;
///   `substituteFrom` is therefore `optional: false` (a missing source fails the reconcile instead
///   of silently rendering empty values).
pub fn synthesize_flux_manifest_app_kustomizations(
    input: &FluxManifestAppInput,
) -> Result<(String, String), KubeError> {
    let app_dir = input.app_dir.trim_start_matches("./").trim_end_matches('/');
    let config_name = manifest_app_config_kustomization_name(&input.name);

    let mut config = FluxKustomizationSpecInput::new(
        &input.tenant_id,
        &config_name,
        &input.source_name,
        format!("./{app_dir}/{MANIFEST_APP_CONFIG_DIR}"),
    );
    config.extra_labels = input.extra_labels.clone();
    config.extra_labels.remove("keel.io/runtime");
    config.extra_labels.remove("keel.io/engine");
    config
        .extra_labels
        .insert("keel.io/config-for".to_string(), input.name.clone());
    if input.has_secrets {
        config.decryption_secret_name = Some(crate::TENANT_SOPS_AGE_SECRET_NAME.to_string());
    }

    let mut app = FluxKustomizationSpecInput::new(
        &input.tenant_id,
        &input.name,
        &input.source_name,
        format!("./{app_dir}"),
    );
    app.extra_labels = input.extra_labels.clone();
    app.kube_config = input.kube_config.clone();
    app.target_namespace = input.target_namespace.clone();
    app.depends_on = input.depends_on.clone();
    if !app.depends_on.iter().any(|d| d.name == config_name) {
        app.depends_on.push(FluxDependencyRef {
            name: config_name.clone(),
        });
    }
    app.post_build_substitute_from = vec![FluxSubstituteReference {
        kind: "ConfigMap".to_string(),
        name: manifest_app_values_configmap_name(&input.name),
        optional: false,
    }];
    if input.has_secrets {
        app.post_build_substitute_from
            .push(FluxSubstituteReference {
                kind: "Secret".to_string(),
                name: manifest_app_secrets_name(&input.name),
                optional: false,
            });
    }

    Ok((
        synthesize_flux_kustomization_yaml(&config)?,
        synthesize_flux_kustomization_yaml(&app)?,
    ))
}

/// Label Envoy Gateway puts on the proxy pods of a Gateway (`keel-ingress`).
pub const ENVOY_OWNING_GATEWAY_LABEL: &str = "gateway.envoyproxy.io/owning-gateway-name";

/// Container ports of the `keel-ingress` Envoy proxy pods: Envoy Gateway maps listener ports
/// below 1024 to `port + 10000` (HTTP 80 -> 10080, HTTPS 443 -> 10443); the local HTTPS
/// listener keeps 8888. NetworkPolicies match the pod port after Service DNAT.
pub const KEEL_INGRESS_PROXY_PORTS: [u16; 3] = [10080, 10443, 8888];

/// Public IPv4 egress exclusions: RFC 1918, CGNAT, link-local (cloud metadata), loopback,
/// "this network", multicast and reserved ranges.
pub const PUBLIC_EGRESS_EXCEPT_CIDRS: [&str; 9] = [
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

fn tcp_ports(ports: &[u16]) -> Result<Vec<Value>, KubeError> {
    if ports.contains(&0) {
        return Err(KubeError::InvalidInput(
            "NetworkPolicy ports must be >= 1".to_string(),
        ));
    }
    Ok(ports
        .iter()
        .map(|p| json!({ "protocol": "TCP", "port": p }))
        .collect())
}

/// Synthesizes the Keel-generated tenant `NetworkPolicy` (`allow-<name>-app`) of a Helm app
/// declaring `spec.networkPolicy` (YAML). It selects the release's pods
/// (`app.kubernetes.io/instance: <helm_release_name>`) through the tenant `default-deny-all`:
///
/// * cluster DNS egress, always;
/// * ingress from the Envoy Gateway proxies (`keel-system`) on `gateway_ingress_ports`;
/// * ingress from any pod of the tenant namespace on `namespace_ingress_ports`;
/// * optionally release-internal traffic;
/// * optionally egress to the `keel-ingress` proxy pods only (never other `keel-system`
///   workloads such as the state backend or the management forge), on their listener ports;
/// * optionally public IPv4 egress on TCP 80/443 (private, link-local and reserved ranges
///   excluded).
pub fn synthesize_helm_app_network_policy_yaml(
    tenant_id: &str,
    name: &str,
    helm_release_name: &str,
    spec: &keel_core::HelmNetworkPolicySpec,
) -> Result<String, KubeError> {
    let namespace = tenant_namespace(tenant_id)?;
    validate_resource_name("name", name)?;
    if helm_release_name.is_empty() || helm_release_name.len() > 63 {
        return Err(KubeError::InvalidInput(format!(
            "Helm release name `{helm_release_name}` must be 1-63 characters"
        )));
    }
    let keel_system_selector = json!({
        "matchLabels": { "kubernetes.io/metadata.name": "keel-system" }
    });
    let release_pods = json!({
        "podSelector": {
            "matchLabels": { "app.kubernetes.io/instance": helm_release_name }
        }
    });

    let mut ingress = Vec::new();
    if !spec.gateway_ingress_ports.is_empty() {
        ingress.push(json!({
            "from": [{ "namespaceSelector": keel_system_selector.clone() }],
            "ports": tcp_ports(&spec.gateway_ingress_ports)?,
        }));
    }
    if !spec.namespace_ingress_ports.is_empty() {
        ingress.push(json!({
            "from": [{ "podSelector": {} }],
            "ports": tcp_ports(&spec.namespace_ingress_ports)?,
        }));
    }
    let mut egress = vec![json!({
        "to": [{
            "namespaceSelector": {
                "matchLabels": { "kubernetes.io/metadata.name": "kube-system" }
            }
        }],
        "ports": [
            { "protocol": "UDP", "port": 53 },
            { "protocol": "TCP", "port": 53 }
        ]
    })];
    if spec.release_traffic {
        ingress.push(json!({ "from": [release_pods.clone()] }));
        egress.push(json!({ "to": [release_pods.clone()] }));
    }
    if spec.gateway_egress {
        egress.push(json!({
            "to": [{
                "namespaceSelector": keel_system_selector,
                "podSelector": {
                    "matchLabels": { ENVOY_OWNING_GATEWAY_LABEL: "keel-ingress" }
                }
            }],
            "ports": tcp_ports(&KEEL_INGRESS_PROXY_PORTS)?,
        }));
    }
    if spec.public_egress {
        egress.push(json!({
            "to": [{
                "ipBlock": {
                    "cidr": "0.0.0.0/0",
                    "except": PUBLIC_EGRESS_EXCEPT_CIDRS,
                }
            }],
            "ports": tcp_ports(&[80, 443])?,
        }));
    }

    let mut labels = tenant_common_labels(tenant_id);
    labels.insert("keel.io/app".to_string(), name.to_string());
    let doc = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": format!("allow-{name}-app"),
            "namespace": namespace,
            "labels": labels,
        },
        "spec": {
            "podSelector": release_pods["podSelector"],
            "policyTypes": ["Ingress", "Egress"],
            "ingress": ingress,
            "egress": egress,
        }
    });
    Ok(keel_core::yaml::to_string(&doc)?)
}

// ---------------------------------------------------------------------------------------------
// Least-privilege tenant credentials (`NFR-SEC-02`, `NFR-SEC-05`)
// ---------------------------------------------------------------------------------------------

/// Secret name (inside `tenant-<id>`) holding the tenant-scoped S3 state backend identity
/// (`accessKey`, `secretKey`, `stateBackendUrl`). Unlike the control-plane
/// `keel-s3-credentials` (SeaweedFS admin keys, confined to `keel-system`/`flux-system`/
/// `keel-previews`), this identity can only read/write `<bucket>/tenants/<id>/*`.
pub const TENANT_S3_CREDENTIALS_SECRET_NAME: &str = "keel-tenant-s3-credentials";

/// Secret name (inside `tenant-<id>`) holding a read-only Git credential (`username`, `password`)
/// for the tenant's Flux `GitRepository`. Never a copy of the control-plane/forge admin token.
pub const TENANT_GIT_CREDENTIALS_SECRET_NAME: &str = "git-credentials";

/// `keel.io/secret-kind` label value for tenant-scoped S3 credentials.
pub const TENANT_S3_CREDENTIALS_SECRET_KIND: &str = "tenant-s3-credentials";

/// `keel.io/secret-kind` label value for tenant read-only Git credentials.
pub const TENANT_GIT_CREDENTIALS_SECRET_KIND: &str = "tenant-git-credentials-readonly";

/// Name of the Flux reconciler `ServiceAccount` (in `flux-system`) impersonated by the
/// `keel-tenants` Kustomization instead of cluster-admin `kustomize-controller`.
pub const TENANT_RECONCILER_SERVICE_ACCOUNT: &str = "keel-tenant-reconciler";

/// Returns the S3 object-key prefix (`tenants/<id>`) owning a tenant's Pulumi DIY state.
pub fn tenant_s3_state_prefix(tenant_id: &str) -> Result<String, KubeError> {
    validate_tenant_id(tenant_id)?;
    Ok(format!("tenants/{tenant_id}"))
}

/// Returns the SeaweedFS IAM identity name (`keel-tenant-<id>`) for a tenant.
pub fn tenant_s3_identity_name(tenant_id: &str) -> Result<String, KubeError> {
    validate_tenant_id(tenant_id)?;
    Ok(format!("keel-tenant-{tenant_id}"))
}

/// Rewrites a Pulumi DIY S3 backend URL (`s3://<bucket>[/<path>][?query]`) so the tenant's state
/// lives under `s3://<bucket>/tenants/<id>?query`, matching the scoped SeaweedFS identity.
pub fn tenant_s3_backend_url(base_backend_url: &str, tenant_id: &str) -> Result<String, KubeError> {
    let prefix = tenant_s3_state_prefix(tenant_id)?;
    let (bucket, query) = split_s3_backend_url(base_backend_url)?;
    let mut url = format!("s3://{bucket}/{prefix}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    Ok(url)
}

/// Extracts the bucket name from a Pulumi DIY S3 backend URL (`s3://<bucket>[/<path>][?query]`).
pub fn s3_bucket_from_backend_url(base_backend_url: &str) -> Result<String, KubeError> {
    let (bucket, _) = split_s3_backend_url(base_backend_url)?;
    validate_s3_bucket_name(bucket)?;
    Ok(bucket.to_string())
}

fn split_s3_backend_url(base_backend_url: &str) -> Result<(&str, Option<&str>), KubeError> {
    let rest = base_backend_url.strip_prefix("s3://").ok_or_else(|| {
        KubeError::InvalidInput(format!(
            "backend URL `{base_backend_url}` must use the `s3://` scheme"
        ))
    })?;
    let (location, query) = match rest.split_once('?') {
        Some((loc, q)) => (loc, Some(q)),
        None => (rest, None),
    };
    let bucket = location.split('/').next().unwrap_or_default();
    if bucket.is_empty() {
        return Err(KubeError::InvalidInput(format!(
            "backend URL `{base_backend_url}` is missing a bucket name"
        )));
    }
    Ok((bucket, query))
}

/// Returns the SeaweedFS identity actions granting a tenant access to its own state prefix only.
///
/// SeaweedFS matches `<Action>:<bucket>/<objectKey>` against wildcard actions; bucket listings
/// with a `prefix` query are authorized against that prefix, so Pulumi's prefixed `ListObjects`
/// calls are covered without granting bucket-wide `List`.
pub fn seaweedfs_tenant_identity_actions(
    bucket: &str,
    tenant_id: &str,
) -> Result<Vec<String>, KubeError> {
    validate_s3_bucket_name(bucket)?;
    let prefix = tenant_s3_state_prefix(tenant_id)?;
    Ok(["Read", "Write", "List"]
        .iter()
        .map(|action| format!("{action}:{bucket}/{prefix}/*"))
        .collect())
}

/// Builds the `weed shell` script (fed via **stdin**, never argv, so keys are not exposed in the
/// process table) that creates or updates a tenant's scoped SeaweedFS S3 identity.
pub fn seaweedfs_configure_tenant_identity_script(
    bucket: &str,
    tenant_id: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<String, KubeError> {
    for (field, value) in [("access_key", access_key), ("secret_key", secret_key)] {
        if value.len() < 16 || !value.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(KubeError::InvalidInput(format!(
                "{field} must be at least 16 ASCII alphanumeric characters"
            )));
        }
    }
    let user = tenant_s3_identity_name(tenant_id)?;
    let actions = seaweedfs_tenant_identity_actions(bucket, tenant_id)?.join(",");
    Ok(format!(
        "s3.configure -user={user} -access_key={access_key} -secret_key={secret_key} -actions={actions} -apply\n"
    ))
}

/// Synthesizes the `tenant-<id>/keel-tenant-s3-credentials` `Secret` for a scoped identity.
pub fn build_tenant_s3_credentials_secret(
    tenant_id: &str,
    access_key: &str,
    secret_key: &str,
    base_backend_url: &str,
) -> Result<Value, KubeError> {
    let ns = tenant_namespace(tenant_id)?;
    let backend_url = tenant_s3_backend_url(base_backend_url, tenant_id)?;
    let mut labels = tenant_common_labels(tenant_id);
    labels.insert(
        "keel.io/secret-kind".to_string(),
        TENANT_S3_CREDENTIALS_SECRET_KIND.to_string(),
    );
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": TENANT_S3_CREDENTIALS_SECRET_NAME,
            "namespace": ns,
            "labels": labels,
            "annotations": {
                "keel.io/s3-identity": tenant_s3_identity_name(tenant_id)?,
                "keel.io/s3-state-prefix": tenant_s3_state_prefix(tenant_id)?,
            },
        },
        "type": "Opaque",
        "stringData": {
            "accessKey": access_key,
            "secretKey": secret_key,
            "stateBackendUrl": backend_url,
        },
    }))
}

fn validate_s3_bucket_name(bucket: &str) -> Result<(), KubeError> {
    let valid = (3..=63).contains(&bucket.len())
        && bucket
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && !bucket.starts_with(['-', '.'])
        && !bucket.ends_with(['-', '.']);
    if valid {
        Ok(())
    } else {
        Err(KubeError::InvalidInput(format!(
            "invalid S3 bucket name `{bucket}`"
        )))
    }
}

/// Synthesizes the `flux-system/keel-tenant-reconciler` `ServiceAccount`, `ClusterRole`, and
/// `ClusterRoleBinding` impersonated by the root tenants `Kustomization`.
///
/// The role only covers the kinds Keel writes under `tenants/` (tenant onboarding + tenant
/// workloads). It may bind only `ClusterRole/admin` and `ClusterRole/keel-server-tenant`
/// (namespaced `RoleBinding`s) and
/// `ClusterRole/system:auth-delegator`, so it cannot mint cluster-admin. The companion Kyverno
/// `ValidatingPolicy/restrict-tenant-reconciler` confines its writes to `tenant-*` namespaces.
pub fn build_tenant_reconciler_rbac() -> [Value; 3] {
    let labels = json!({
        "app.kubernetes.io/managed-by": "keel",
        "app.kubernetes.io/part-of": "keel-control-plane"
    });
    let all = json!([
        "get", "list", "watch", "create", "update", "patch", "delete"
    ]);
    let service_account = json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {
            "name": TENANT_RECONCILER_SERVICE_ACCOUNT,
            "namespace": "flux-system",
            "labels": labels,
        },
        "automountServiceAccountToken": false
    });
    let cluster_role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": TENANT_RECONCILER_SERVICE_ACCOUNT,
            "labels": labels,
        },
        "rules": [
            {
                "apiGroups": [""],
                "resources": [
                    "namespaces", "serviceaccounts", "secrets", "configmaps", "services",
                    "resourcequotas", "limitranges", "persistentvolumeclaims"
                ],
                "verbs": all
            },
            { "apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": all },
            {
                "apiGroups": ["rbac.authorization.k8s.io"],
                "resources": ["rolebindings", "clusterrolebindings"],
                "verbs": all
            },
            {
                "apiGroups": ["rbac.authorization.k8s.io"],
                "resources": ["clusterroles"],
                "resourceNames": ["admin", KEEL_SERVER_TENANT_CLUSTER_ROLE, "system:auth-delegator"],
                "verbs": ["bind"]
            },
            {
                "apiGroups": ["source.toolkit.fluxcd.io"],
                "resources": ["gitrepositories", "helmrepositories", "ocirepositories", "helmcharts", "buckets"],
                "verbs": all
            },
            { "apiGroups": ["kustomize.toolkit.fluxcd.io"], "resources": ["kustomizations"], "verbs": all },
            { "apiGroups": ["helm.toolkit.fluxcd.io"], "resources": ["helmreleases"], "verbs": all },
            { "apiGroups": ["pulumi.com"], "resources": ["stacks", "programs"], "verbs": all },
            { "apiGroups": ["flagger.app"], "resources": ["canaries", "metrictemplates", "alertproviders"], "verbs": all },
            {
                "apiGroups": ["gateway.networking.k8s.io"],
                "resources": ["httproutes", "grpcroutes", "referencegrants"],
                "verbs": all
            },
            {
                "apiGroups": ["apps"],
                "resources": ["deployments", "statefulsets", "daemonsets"],
                "verbs": all
            },
            { "apiGroups": ["batch"], "resources": ["jobs", "cronjobs"], "verbs": all },
            { "apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": all },
            { "apiGroups": ["autoscaling"], "resources": ["horizontalpodautoscalers"], "verbs": all }
        ]
    });
    let binding = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": TENANT_RECONCILER_SERVICE_ACCOUNT,
            "labels": labels,
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": TENANT_RECONCILER_SERVICE_ACCOUNT
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": TENANT_RECONCILER_SERVICE_ACCOUNT,
            "namespace": "flux-system"
        }]
    });
    [service_account, cluster_role, binding]
}

/// Name of the `ClusterRole` aggregated into `ClusterRole/admin` that lets tenant
/// `ServiceAccount`s (bound to `admin` in `tenant-<id>`) manage Gateway API routes.
pub const TENANT_GATEWAY_ROUTES_CLUSTER_ROLE: &str = "keel-tenant-gateway-routes";

/// Synthesizes `ClusterRole/keel-tenant-gateway-routes`, aggregated into the built-in
/// `ClusterRole/admin` (`rbac.authorization.k8s.io/aggregate-to-admin`).
///
/// Neither the Gateway API CRDs nor Envoy Gateway aggregate route permissions into `admin`, so
/// without it a tenant Flux `Kustomization` (impersonating `tenant-<id>-sa`) cannot apply a
/// manifest app's `HTTPRoute`. Only `httproutes`/`grpcroutes` are granted (never
/// `referencegrants`, which would open cross-namespace backend references); the Kyverno
/// `restrict-tenant-route-hostnames` policy keeps tenant routes off reserved platform hosts.
pub fn build_tenant_gateway_routes_cluster_role() -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": TENANT_GATEWAY_ROUTES_CLUSTER_ROLE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "app.kubernetes.io/part-of": "keel-control-plane",
                "rbac.authorization.k8s.io/aggregate-to-admin": "true"
            }
        },
        "rules": [{
            "apiGroups": ["gateway.networking.k8s.io"],
            "resources": ["httproutes", "grpcroutes"],
            "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"]
        }]
    })
}

/// Name of the `ClusterRole` aggregated into `ClusterRole/admin` that lets tenant
/// `ServiceAccount`s (bound to `admin` in `tenant-<id>`) manage Flagger `Canary` objects.
pub const TENANT_FLAGGER_CANARIES_CLUSTER_ROLE: &str = "keel-tenant-flagger-canaries";

/// Synthesizes `ClusterRole/keel-tenant-flagger-canaries`, aggregated into the built-in
/// `ClusterRole/admin` (`rbac.authorization.k8s.io/aggregate-to-admin`).
///
/// The Flagger chart does not aggregate its CRDs into `admin`, so without it a tenant Flux
/// `Kustomization` (impersonating `tenant-<id>-sa`) cannot apply the `Canary` Keel synthesizes for
/// a canary app (`synthesize_flagger_canary`), and the whole tenant root `Kustomization` fails.
/// Only `canaries` are granted: the synthesized `Canary` references the platform-owned
/// `MetricTemplate`s in `keel-system`, so tenants never need `metrictemplates` (which carry an
/// arbitrary metrics-provider address Flagger would query) or `alertproviders`. The `Canary`
/// spec itself can still steer Flagger (webhooks, inline queries, foreign templates or
/// Gateways), so the Kyverno `restrict-tenant-flagger-canaries` policy
/// (`crate::kyverno::build_restrict_tenant_flagger_canaries_policy`) admits only the shape
/// `synthesize_flagger_canary` produces.
pub fn build_tenant_flagger_canaries_cluster_role() -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": TENANT_FLAGGER_CANARIES_CLUSTER_ROLE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "app.kubernetes.io/part-of": "keel-control-plane",
                "rbac.authorization.k8s.io/aggregate-to-admin": "true"
            }
        },
        "rules": [{
            "apiGroups": ["flagger.app"],
            "resources": ["canaries"],
            "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"]
        }]
    })
}

/// Serializes `build_tenant_reconciler_rbac` plus the `admin`-aggregated tenant roles
/// (`build_tenant_gateway_routes_cluster_role`, `build_tenant_flagger_canaries_cluster_role`)
/// into multi-document YAML.
pub fn build_tenant_reconciler_rbac_yaml() -> Result<String, KubeError> {
    let docs = build_tenant_reconciler_rbac().into_iter().chain([
        build_tenant_gateway_routes_cluster_role(),
        build_tenant_flagger_canaries_cluster_role(),
    ]);
    Ok(keel_core::to_yaml_documents(docs)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_tenant_baselines_are_labelled_platform_owned() {
        let current = TenantManifestBundle::synthesize(&TenantIsolationConfig::new("acme"))
            .unwrap()
            .to_multi_doc_yaml()
            .unwrap();
        for doc in keel_core::split_yaml_documents(&current) {
            let v: Value = noyalib::from_str(&doc).unwrap();
            assert_eq!(
                v["metadata"]["labels"][PLATFORM_OWNED_LABEL], "true",
                "{doc}"
            );
        }
    }

    #[test]
    fn remote_target_namespace_never_prunes_or_overwrites() {
        let ns = synthesize_remote_target_namespace("default", "staging-obs").unwrap();
        assert_eq!(ns["apiVersion"], "v1");
        assert_eq!(ns["kind"], "Namespace");
        assert_eq!(ns["metadata"]["name"], "staging-obs");
        assert!(ns["metadata"].get("namespace").is_none());
        assert_eq!(ns["metadata"]["labels"]["keel.io/tenant"], "default");
        // Same `restricted` PSS enforcement as the management-cluster tenant namespace.
        let tenant_ns = build_tenant_namespace("default").unwrap();
        for (key, value) in tenant_ns.metadata.labels.unwrap() {
            if key == PLATFORM_OWNED_LABEL {
                continue;
            }
            assert_eq!(ns["metadata"]["labels"][&key], value.as_str(), "{key}");
        }
        // Applied by the tenant's own Kustomization on the workload cluster: never platform-owned.
        assert!(ns["metadata"]["labels"].get(PLATFORM_OWNED_LABEL).is_none());
        assert_eq!(
            ns["metadata"]["labels"]["pod-security.kubernetes.io/enforce"],
            "restricted"
        );
        assert_eq!(
            ns["metadata"]["labels"]["pod-security.kubernetes.io/enforce-version"],
            "latest"
        );
        assert_eq!(
            ns["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/prune"],
            "disabled"
        );
        assert_eq!(
            ns["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/ssa"],
            "IfNotPresent"
        );
        let yaml = synthesize_remote_target_namespace_yaml("default", "tenant-default").unwrap();
        assert!(yaml.contains("kind: Namespace"));
        assert!(yaml.contains("name: tenant-default"));
        assert!(synthesize_remote_target_namespace("default", "Bad_NS").is_err());
        assert!(synthesize_remote_target_namespace("Bad Tenant", "ns").is_err());
    }

    #[test]
    fn tenant_s3_backend_url_scopes_state_under_tenant_prefix() {
        let base = "s3://pulumi-state?endpoint=s3.local:8333&s3ForcePathStyle=true";
        assert_eq!(
            tenant_s3_backend_url(base, "acme").unwrap(),
            "s3://pulumi-state/tenants/acme?endpoint=s3.local:8333&s3ForcePathStyle=true"
        );
        assert_eq!(
            tenant_s3_backend_url("s3://pulumi-state", "default").unwrap(),
            "s3://pulumi-state/tenants/default"
        );
        // An existing path is replaced (never nested) so tenants cannot escape their prefix.
        assert_eq!(
            tenant_s3_backend_url("s3://b-1/other/path?x=1", "acme").unwrap(),
            "s3://b-1/tenants/acme?x=1"
        );
        assert!(tenant_s3_backend_url("file:///tmp/state", "acme").is_err());
        assert!(tenant_s3_backend_url("s3://", "acme").is_err());
        assert!(tenant_s3_backend_url("s3://bucket", "../etc").is_err());
    }

    #[test]
    fn s3_bucket_from_backend_url_extracts_and_validates_bucket() {
        assert_eq!(
            s3_bucket_from_backend_url("s3://pulumi-state?endpoint=x:8333").unwrap(),
            "pulumi-state"
        );
        assert_eq!(
            s3_bucket_from_backend_url("s3://b-1/some/path").unwrap(),
            "b-1"
        );
        assert!(s3_bucket_from_backend_url("file:///tmp").is_err());
        assert!(s3_bucket_from_backend_url("s3://Bad_Bucket").is_err());
    }

    #[test]
    fn seaweedfs_identity_is_scoped_to_tenant_prefix_without_admin() {
        let actions = seaweedfs_tenant_identity_actions("pulumi-state", "acme").unwrap();
        assert_eq!(
            actions,
            vec![
                "Read:pulumi-state/tenants/acme/*",
                "Write:pulumi-state/tenants/acme/*",
                "List:pulumi-state/tenants/acme/*",
            ]
        );
        assert!(actions.iter().all(|a| !a.starts_with("Admin")));
        assert!(seaweedfs_tenant_identity_actions("Bad_Bucket", "acme").is_err());
        assert!(seaweedfs_tenant_identity_actions("pulumi-state", "ACME").is_err());

        let access = "AKIAABCDEFGHIJKLMNOP";
        let secret = "s3cretS3cretS3cretS3cretS3cret";
        let script =
            seaweedfs_configure_tenant_identity_script("pulumi-state", "acme", access, secret)
                .unwrap();
        assert!(script.starts_with("s3.configure -user=keel-tenant-acme "));
        assert!(script.contains(&format!("-access_key={access}")));
        assert!(script.contains("-actions=Read:pulumi-state/tenants/acme/*,"));
        assert!(script.trim_end().ends_with("-apply"));
        assert_eq!(script.lines().count(), 1);

        // Injection attempts into the weed shell script are rejected.
        assert!(
            seaweedfs_configure_tenant_identity_script(
                "pulumi-state",
                "acme",
                "AKIAABCDEFGHIJKL -apply\ns3.bucket.delete",
                secret
            )
            .is_err()
        );
        assert!(
            seaweedfs_configure_tenant_identity_script("pulumi-state", "acme", "short", secret)
                .is_err()
        );
    }

    #[test]
    fn tenant_credential_secrets_live_in_tenant_namespace() {
        let s3 = build_tenant_s3_credentials_secret(
            "acme",
            "AKIAABCDEFGHIJKLMNOP",
            "s3cretS3cretS3cretS3cret",
            "s3://pulumi-state?region=us-east-1",
        )
        .unwrap();
        assert_eq!(s3["metadata"]["name"], TENANT_S3_CREDENTIALS_SECRET_NAME);
        assert_eq!(s3["metadata"]["namespace"], "tenant-acme");
        assert_eq!(
            s3["metadata"]["labels"]["keel.io/secret-kind"],
            TENANT_S3_CREDENTIALS_SECRET_KIND
        );
        assert_eq!(s3["metadata"]["labels"]["keel.io/tenant"], "acme");
        assert_eq!(
            s3["stringData"]["stateBackendUrl"],
            "s3://pulumi-state/tenants/acme?region=us-east-1"
        );
        assert_eq!(
            s3["metadata"]["annotations"]["keel.io/s3-identity"],
            "keel-tenant-acme"
        );
    }

    #[test]
    fn tenant_reconciler_rbac_matches_seed_manifest() {
        let seed = include_str!("../../../deploy/seed/flux-system/tenant-reconciler-rbac.yaml");
        let docs: Vec<Value> = seed
            .split("\n---\n")
            .map(|d| noyalib::from_str(d).unwrap())
            .collect();
        let [_, role, _] = build_tenant_reconciler_rbac();
        let seed_role = docs
            .iter()
            .find(|d| {
                d["kind"] == "ClusterRole"
                    && d["metadata"]["name"] == TENANT_RECONCILER_SERVICE_ACCOUNT
            })
            .expect("seed ClusterRole");
        assert_eq!(seed_role["rules"], role["rules"]);
    }

    #[test]
    fn keel_server_tenant_role_binding_targets_server_service_account() {
        let rb = build_keel_server_tenant_role_binding("acme").unwrap();
        assert_eq!(rb.metadata.name.as_deref(), Some("keel-server"));
        assert_eq!(rb.metadata.namespace.as_deref(), Some("tenant-acme"));
        assert_eq!(rb.role_ref.kind, "ClusterRole");
        assert_eq!(rb.role_ref.name, KEEL_SERVER_TENANT_CLUSTER_ROLE);
        let subjects = rb.subjects.unwrap();
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].kind, "ServiceAccount");
        assert_eq!(subjects[0].name, "keel-server");
        assert_eq!(subjects[0].namespace.as_deref(), Some("keel-system"));
        assert!(build_keel_server_tenant_role_binding("Bad_ID").is_err());

        // The shared bundle never binds a tenant itself (the tenant RBAC bundle owns those).
        let shipped = include_str!("../../../deploy/bootstrap/keel-server/rbac.yaml");
        assert!(shipped.contains(&format!("name: {KEEL_SERVER_TENANT_CLUSTER_ROLE}")));
    }

    #[test]
    fn keel_server_install_bundle_ships_no_tenant_objects() {
        // `tenant-<id>` objects are `keel.io/platform-owned` and owned by `keel-tenants`; the
        // manual `kubectl apply -k deploy/bootstrap/keel-server` install must never touch one, or
        // Kyverno `protect-platform-owned-tenant-objects` denies it for managed-cluster admins.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/bootstrap/keel-server");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                !content.contains("tenant-default-rolebinding"),
                "{} references the removed tenant-default binding",
                path.display()
            );
            for doc in content.split("\n---\n") {
                let Ok(value) = noyalib::from_str::<Value>(doc) else {
                    continue;
                };
                let ns = value["metadata"]["namespace"].as_str().unwrap_or_default();
                assert!(
                    !ns.starts_with("tenant-"),
                    "{} ships {ns}/{}",
                    path.display(),
                    value["metadata"]["name"]
                );
                assert!(
                    value["metadata"]["labels"]["keel.io/platform-owned"].is_null(),
                    "{} ships a platform-owned object",
                    path.display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 3, "only {checked} documents checked");
    }

    #[test]
    fn tenant_reconciler_rbac_cannot_bind_cluster_admin() {
        let [sa, role, binding] = build_tenant_reconciler_rbac();
        assert_eq!(sa["metadata"]["namespace"], "flux-system");
        assert_eq!(sa["metadata"]["name"], TENANT_RECONCILER_SERVICE_ACCOUNT);
        let rules = role["rules"].as_array().unwrap();
        for rule in rules {
            assert_ne!(rule["apiGroups"], json!(["*"]));
            assert_ne!(rule["resources"], json!(["*"]));
            assert_ne!(rule["verbs"], json!(["*"]));
            if rule["verbs"] == json!(["bind"]) {
                assert_eq!(
                    rule["resourceNames"],
                    json!(["admin", "keel-server-tenant", "system:auth-delegator"])
                );
            }
            // No escalate verb and no ability to author ClusterRoles/Roles.
            let resources = rule["resources"].as_array().unwrap();
            if rule["verbs"] != json!(["bind"]) {
                assert!(!resources.contains(&json!("clusterroles")));
                assert!(!resources.contains(&json!("roles")));
            }
            assert!(
                !rule["verbs"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("escalate"))
            );
            assert!(
                !rule["verbs"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("impersonate"))
            );
        }
        assert_eq!(
            binding["roleRef"]["name"],
            TENANT_RECONCILER_SERVICE_ACCOUNT
        );
        assert_eq!(binding["subjects"][0]["namespace"], "flux-system");

        let yaml = build_tenant_reconciler_rbac_yaml().unwrap();
        assert_eq!(yaml.matches("\n---\n").count(), 4);
    }

    #[test]
    fn tenant_flagger_canaries_role_aggregates_into_admin_and_matches_seed() {
        let role = build_tenant_flagger_canaries_cluster_role();
        assert_eq!(
            role["metadata"]["labels"]["rbac.authorization.k8s.io/aggregate-to-admin"],
            "true"
        );
        let rules = role["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["apiGroups"], json!(["flagger.app"]));
        // Least privilege: tenants reference platform MetricTemplates, never create them.
        assert_eq!(rules[0]["resources"], json!(["canaries"]));
        for verb in ["bind", "escalate", "impersonate", "*"] {
            assert!(!rules[0]["verbs"].as_array().unwrap().contains(&json!(verb)));
        }

        // Every kind the synthesized Canary needs is covered by this role.
        let canary =
            synthesize_flagger_canary(&FlaggerCanarySpecInput::new("acme", "web")).unwrap();
        let api_version = canary["apiVersion"].as_str().unwrap();
        assert_eq!(api_version.split('/').next(), Some("flagger.app"));
        assert_eq!(canary["kind"], "Canary");
        for metric in canary["spec"]["analysis"]["metrics"].as_array().unwrap() {
            assert_eq!(metric["templateRef"]["namespace"], "keel-system");
        }

        let seed = include_str!("../../../deploy/seed/flux-system/tenant-reconciler-rbac.yaml");
        let seed_role = seed
            .split("\n---\n")
            .map(|d| noyalib::from_str::<Value>(d).unwrap())
            .find(|d| d["metadata"]["name"] == TENANT_FLAGGER_CANARIES_CLUSTER_ROLE)
            .expect("seed keel-tenant-flagger-canaries ClusterRole");
        assert_eq!(seed_role["metadata"]["labels"], role["metadata"]["labels"]);
        assert_eq!(seed_role["rules"], role["rules"]);

        let yaml = build_tenant_reconciler_rbac_yaml().unwrap();
        assert!(yaml.contains(TENANT_FLAGGER_CANARIES_CLUSTER_ROLE));
    }

    #[test]
    fn tenant_gateway_routes_role_aggregates_into_admin_and_matches_seed() {
        let role = build_tenant_gateway_routes_cluster_role();
        assert_eq!(
            role["metadata"]["labels"]["rbac.authorization.k8s.io/aggregate-to-admin"],
            "true"
        );
        let rules = role["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["apiGroups"], json!(["gateway.networking.k8s.io"]));
        assert_eq!(rules[0]["resources"], json!(["httproutes", "grpcroutes"]));

        let seed = include_str!("../../../deploy/seed/flux-system/tenant-reconciler-rbac.yaml");
        let seed_role = seed
            .split("\n---\n")
            .map(|d| noyalib::from_str::<Value>(d).unwrap())
            .find(|d| d["metadata"]["name"] == TENANT_GATEWAY_ROUTES_CLUSTER_ROLE)
            .expect("seed keel-tenant-gateway-routes ClusterRole");
        assert_eq!(seed_role["metadata"]["labels"], role["metadata"]["labels"]);
        assert_eq!(seed_role["rules"], role["rules"]);
    }

    #[test]
    fn local_flux_kustomization_defaults_target_namespace_to_tenant() {
        let input = FluxKustomizationSpecInput::new("acme", "web", "keel-gitops", "./tenants/acme");
        let doc = synthesize_flux_kustomization(&input).unwrap();
        assert_eq!(doc["spec"]["serviceAccountName"], "tenant-acme-sa");
        assert_eq!(doc["spec"]["targetNamespace"], "tenant-acme");
        assert_eq!(doc["metadata"]["namespace"], "tenant-acme");

        let mut remote = input.clone();
        remote.kube_config = Some(RemoteKubeConfigRef::new("remote-kubeconfig"));
        let doc = synthesize_flux_kustomization(&remote).unwrap();
        // Remote targets still impersonate the tenant SA (bound on the workload cluster).
        assert_eq!(doc["spec"]["serviceAccountName"], "tenant-acme-sa");
        assert!(doc["spec"].get("targetNamespace").is_none());
    }

    #[test]
    fn tenant_isolation_bundle_enforces_restricted_pss_quota_and_default_deny() {
        let cfg = TenantIsolationConfig::new("acme-prod");
        let bundle = TenantManifestBundle::synthesize(&cfg).unwrap();

        // 1. Namespace: `tenant-acme-prod` + `pod-security.kubernetes.io/enforce: restricted`
        assert_eq!(
            bundle.namespace.metadata.name.as_deref(),
            Some("tenant-acme-prod")
        );
        let ns_labels = bundle.namespace.metadata.labels.as_ref().unwrap();
        assert_eq!(
            ns_labels
                .get("pod-security.kubernetes.io/enforce")
                .map(String::as_str),
            Some("restricted")
        );
        assert_eq!(
            ns_labels
                .get("pod-security.kubernetes.io/audit")
                .map(String::as_str),
            Some("restricted")
        );
        assert_eq!(
            ns_labels
                .get("pod-security.kubernetes.io/warn")
                .map(String::as_str),
            Some("restricted")
        );

        // 2. ServiceAccount: isolated in `tenant-acme-prod` with automount disabled
        assert_eq!(
            bundle.service_account.metadata.namespace.as_deref(),
            Some("tenant-acme-prod")
        );
        assert_eq!(
            bundle.service_account.metadata.name.as_deref(),
            Some("tenant-acme-prod-sa")
        );
        assert_eq!(
            bundle.service_account.automount_service_account_token,
            Some(true)
        );

        // 3. ResourceQuota
        let hard = bundle
            .resource_quota
            .spec
            .as_ref()
            .unwrap()
            .hard
            .as_ref()
            .unwrap();
        assert_eq!(hard.get("requests.cpu").unwrap().0, "4");
        assert_eq!(hard.get("limits.memory").unwrap().0, "16Gi");
        assert_eq!(hard.get("services.loadbalancers").unwrap().0, "0");

        // 4. Default-deny NetworkPolicy
        let np_spec = bundle.network_policy.spec.as_ref().unwrap();
        assert_eq!(
            np_spec.policy_types.as_deref(),
            Some(&["Ingress".to_string(), "Egress".to_string()][..])
        );
        assert!(np_spec.ingress.is_none());
        assert!(np_spec.egress.is_none());

        // JSON & multi-doc YAML serialization
        let json_docs = bundle.to_json_values().unwrap();
        assert_eq!(json_docs.len(), 9);
        assert_eq!(json_docs[0]["kind"], "Namespace");
        assert_eq!(json_docs[1]["kind"], "ServiceAccount");
        assert_eq!(json_docs[2]["kind"], "ResourceQuota");
        assert_eq!(json_docs[3]["kind"], "LimitRange");
        assert_eq!(json_docs[4]["kind"], "NetworkPolicy");
        // 5. allow-pko-workspace: PKO workspace pods through default-deny-all.
        assert_eq!(json_docs[5]["kind"], "NetworkPolicy");
        assert_eq!(json_docs[5]["metadata"]["name"], "allow-pko-workspace");
        assert_eq!(json_docs[5]["metadata"]["namespace"], "tenant-acme-prod");
        assert_eq!(
            json_docs[5]["metadata"]["labels"]["keel.io/platform-owned"],
            "true"
        );
        assert_eq!(
            json_docs[5]["spec"]["podSelector"],
            json!({"matchLabels": {"auto.pulumi.com/component": "workspace"}})
        );
        let egress = json_docs[5]["spec"]["egress"].as_array().unwrap();
        assert_eq!(egress.len(), 6);
        // Every rule names destinations; no allow-all `{}`.
        assert!(
            egress
                .iter()
                .all(|rule| rule["to"].as_array().is_some_and(|to| !to.is_empty()))
        );
        assert_eq!(
            egress[1]["to"][0]["podSelector"]["matchLabels"][ENVOY_OWNING_GATEWAY_LABEL],
            "keel-ingress"
        );
        assert_eq!(
            egress[2]["ports"],
            json!([{"protocol": "TCP", "port": 8333}])
        );
        assert_eq!(
            egress[3]["ports"],
            json!([{"protocol": "TCP", "port": LOCAL_DOCKER_PROXY_PORT}])
        );
        assert!(egress[4]["ports"].as_array().is_some_and(|p| !p.is_empty()));
        assert_eq!(
            json_docs[5]["spec"]["ingress"],
            json!([{
                "from": [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "keel-system"}}}],
                "ports": [{"protocol": "TCP", "port": 50051}]
            }])
        );
        assert_eq!(json_docs[6]["kind"], "RoleBinding");
        assert_eq!(json_docs[6]["metadata"]["name"], "tenant-acme-prod-admin");
        assert_eq!(json_docs[7]["kind"], "RoleBinding");
        assert_eq!(json_docs[7]["metadata"]["name"], "keel-server");
        assert_eq!(json_docs[7]["metadata"]["namespace"], "tenant-acme-prod");
        assert_eq!(json_docs[7]["roleRef"]["kind"], "ClusterRole");
        assert_eq!(json_docs[7]["roleRef"]["name"], "keel-server-tenant");
        assert_eq!(json_docs[7]["subjects"][0]["name"], "keel-server");
        assert_eq!(json_docs[7]["subjects"][0]["namespace"], "keel-system");
        assert_eq!(json_docs[8]["kind"], "ClusterRoleBinding");
        assert_eq!(json_docs[8]["roleRef"]["name"], "system:auth-delegator");

        let yaml = bundle.to_multi_doc_yaml().unwrap();
        assert!(yaml.contains("\n---\n"));
        for doc in yaml.split("\n---\n") {
            let parsed: serde_json::Value =
                noyalib::from_str(doc).expect("each multi-doc YAML part should be valid YAML");
            assert!(parsed.get("kind").is_some());
        }
        assert!(yaml.contains("pod-security.kubernetes.io/enforce: restricted"));
        assert!(yaml.contains("kind: NetworkPolicy"));
        assert!(yaml.contains("default-deny-all"));
    }

    #[test]
    fn tenant_id_validation_rejects_traversal_and_uppercase() {
        assert!(tenant_namespace("").is_err());
        assert!(tenant_namespace("Acme").is_err());
        assert!(tenant_namespace("-acme").is_err());
        assert!(tenant_namespace("acme-").is_err());
        assert!(tenant_namespace("../flux-system").is_err());
        assert!(tenant_namespace(&"a".repeat(60)).is_err());
    }

    #[test]
    fn helm_app_network_policy() {
        let full = keel_core::HelmNetworkPolicySpec {
            gateway_ingress_ports: vec![3000, 8333],
            namespace_ingress_ports: vec![8333],
            release_traffic: true,
            gateway_egress: true,
            public_egress: true,
        };
        let np: Value = noyalib::from_str(
            &synthesize_helm_app_network_policy_yaml("acme", "git", "git", &full).unwrap(),
        )
        .unwrap();
        assert_eq!(np["metadata"]["name"], "allow-git-app");
        assert_eq!(np["metadata"]["namespace"], "tenant-acme");
        assert_eq!(
            np["spec"]["podSelector"]["matchLabels"]["app.kubernetes.io/instance"],
            "git"
        );
        assert_eq!(np["spec"]["policyTypes"], json!(["Ingress", "Egress"]));
        assert_eq!(
            np["spec"]["ingress"][0]["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "keel-system"
        );
        assert_eq!(np["spec"]["ingress"][0]["ports"][1]["port"], 8333);
        // Any pod of the tenant namespace, on the declared ports only.
        assert_eq!(
            np["spec"]["ingress"][1]["from"],
            json!([{ "podSelector": {} }])
        );
        assert_eq!(
            np["spec"]["ingress"][1]["ports"],
            json!([{ "protocol": "TCP", "port": 8333 }])
        );
        assert_eq!(
            np["spec"]["ingress"][2]["from"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/instance"],
            "git"
        );
        let egress = np["spec"]["egress"].as_array().unwrap();
        assert_eq!(egress.len(), 4);
        assert_eq!(egress[0]["ports"][0]["port"], 53);
        // Gateway egress: only the keel-ingress Envoy proxy pods, only their listener ports.
        let gw = &egress[2]["to"][0];
        assert_eq!(
            gw["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "keel-system"
        );
        assert_eq!(
            gw["podSelector"]["matchLabels"]["gateway.envoyproxy.io/owning-gateway-name"],
            "keel-ingress"
        );
        assert_eq!(
            egress[2]["ports"],
            json!([
                { "protocol": "TCP", "port": 10080 },
                { "protocol": "TCP", "port": 10443 },
                { "protocol": "TCP", "port": 8888 }
            ])
        );
        // Public egress: HTTP(S) only, private/link-local/reserved ranges excluded.
        assert_eq!(egress[3]["to"][0]["ipBlock"]["cidr"], "0.0.0.0/0");
        let except = egress[3]["to"][0]["ipBlock"]["except"].as_array().unwrap();
        for cidr in [
            "10.0.0.0/8",
            "169.254.0.0/16",
            "0.0.0.0/8",
            "224.0.0.0/4",
            "240.0.0.0/4",
        ] {
            assert!(except.contains(&json!(cidr)), "{cidr}");
        }
        assert_eq!(
            egress[3]["ports"],
            json!([
                { "protocol": "TCP", "port": 80 },
                { "protocol": "TCP", "port": 443 }
            ])
        );
        let bad_port = keel_core::HelmNetworkPolicySpec {
            namespace_ingress_ports: vec![0],
            ..keel_core::HelmNetworkPolicySpec::default()
        };
        assert!(synthesize_helm_app_network_policy_yaml("acme", "git", "git", &bad_port).is_err());

        // Minimal: DNS egress only, no ingress at all.
        let minimal: Value = noyalib::from_str(
            &synthesize_helm_app_network_policy_yaml(
                "acme",
                "s3",
                "tenant-acme-s3",
                &keel_core::HelmNetworkPolicySpec::default(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(minimal["spec"]["ingress"], json!([]));
        assert_eq!(minimal["spec"]["egress"].as_array().unwrap().len(), 1);
        assert_eq!(
            minimal["spec"]["podSelector"]["matchLabels"]["app.kubernetes.io/instance"],
            "tenant-acme-s3"
        );
        assert!(
            synthesize_helm_app_network_policy_yaml(
                "acme",
                "s3",
                "",
                &keel_core::HelmNetworkPolicySpec::default()
            )
            .is_err()
        );
    }

    #[test]
    fn helm_release_annotations_and_release_name() {
        let mut input = HelmReleaseSpecInput::new(
            "acme",
            "git",
            "forgejo",
            HelmChartSourceRef {
                kind: HelmChartSourceKind::HelmRepository,
                name: "git-repo".to_string(),
                namespace: None,
            },
        );
        assert_eq!(input.helm_release_name(), "git");
        let hr = synthesize_helm_release(&input).unwrap();
        assert!(hr["metadata"].get("annotations").is_none());
        input.annotations.insert(
            ROUTE_HOSTNAMES_ANNOTATION.to_string(),
            "git.acme.example".to_string(),
        );
        input.target_namespace = Some("tenant-acme".to_string());
        assert_eq!(input.helm_release_name(), "tenant-acme-git");
        let hr = synthesize_helm_release(&input).unwrap();
        assert_eq!(
            hr["metadata"]["annotations"][ROUTE_HOSTNAMES_ANNOTATION],
            "git.acme.example"
        );
    }

    #[test]
    fn helm_release_crd_policy_sets_install_and_upgrade_crds() {
        let mut input = HelmReleaseSpecInput::new(
            "acme",
            "pko",
            "pulumi-kubernetes-operator",
            HelmChartSourceRef {
                kind: HelmChartSourceKind::HelmRepository,
                name: "pko-repo".to_string(),
                namespace: None,
            },
        );
        // Unset: Flux defaults (no `install`/`upgrade` blocks on a local release).
        let hr = synthesize_helm_release(&input).unwrap();
        assert!(hr["spec"].get("install").is_none());
        assert!(hr["spec"].get("upgrade").is_none());

        input.crds = Some(keel_core::HelmCrdPolicy::CreateReplace);
        let hr = synthesize_helm_release(&input).unwrap();
        assert_eq!(hr["spec"]["install"], json!({ "crds": "CreateReplace" }));
        assert_eq!(hr["spec"]["upgrade"], json!({ "crds": "CreateReplace" }));

        // Remote targets keep `createNamespace` next to the CRD policy.
        input.kube_config = Some(RemoteKubeConfigRef::with_key("workload", "kubeconfig"));
        input.target_namespace = Some("pulumi-system".to_string());
        let hr = synthesize_helm_release(&input).unwrap();
        assert_eq!(
            hr["spec"]["install"],
            json!({ "createNamespace": true, "crds": "CreateReplace" })
        );
        assert_eq!(hr["spec"]["upgrade"]["crds"], "CreateReplace");
        assert_eq!(hr["spec"]["targetNamespace"], "pulumi-system");
    }

    #[test]
    fn helm_release_supports_values_from_secrets_and_remote_kubeconfig() {
        let mut input = HelmReleaseSpecInput::new(
            "fintech",
            "authentik",
            "authentik",
            HelmChartSourceRef {
                kind: HelmChartSourceKind::HelmRepository,
                name: "authentik-repo".to_string(),
                namespace: None,
            },
        );
        input.chart_version = Some("2026.10.1".to_string());
        input.values = Some(json!({
            "domain": "auth.fintech.example.com",
            "postgresql": { "enabled": false }
        }));
        input.values_from_secrets = vec![
            HelmValuesSecretRef {
                name: "postgres-managed-outputs".to_string(),
                values_key: Some("db_host".to_string()),
                target_path: Some("authentik.postgresql.host".to_string()),
                optional: None,
            },
            HelmValuesSecretRef {
                name: "authentik-sops-secrets".to_string(),
                values_key: Some("db_password".to_string()),
                target_path: Some("authentik.postgresql.password".to_string()),
                optional: Some(false),
            },
        ];
        input.kube_config = Some(RemoteKubeConfigRef::with_key(
            "workload-cluster-eu-kubeconfig",
            "value.yaml",
        ));
        input.depends_on = vec![FluxDependencyRef {
            name: "postgres-managed".to_string(),
        }];

        let manifest = synthesize_helm_release(&input).unwrap();
        assert_eq!(manifest["apiVersion"], "helm.toolkit.fluxcd.io/v2");
        assert_eq!(manifest["kind"], "HelmRelease");
        assert_eq!(manifest["metadata"]["namespace"], "tenant-fintech");
        assert_eq!(manifest["spec"]["serviceAccountName"], "tenant-fintech-sa");
        assert_eq!(
            manifest["spec"]["chart"]["spec"]["sourceRef"]["namespace"],
            "tenant-fintech"
        );
        assert_eq!(
            manifest["spec"]["kubeConfig"]["secretRef"]["name"],
            "workload-cluster-eu-kubeconfig"
        );
        assert_eq!(
            manifest["spec"]["kubeConfig"]["secretRef"]["key"],
            "value.yaml"
        );
        assert_eq!(manifest["spec"]["valuesFrom"][0]["kind"], "Secret");
        assert_eq!(
            manifest["spec"]["valuesFrom"][0]["name"],
            "postgres-managed-outputs"
        );
        assert_eq!(
            manifest["spec"]["valuesFrom"][0]["targetPath"],
            "authentik.postgresql.host"
        );
        assert_eq!(manifest["spec"]["dependsOn"][0]["name"], "postgres-managed");

        let yaml = synthesize_helm_release_yaml(&input).unwrap();
        assert!(yaml.contains("helm.toolkit.fluxcd.io/v2"));
        assert!(yaml.contains("workload-cluster-eu-kubeconfig"));
    }

    #[test]
    fn helm_release_rejects_cross_namespace_source_ref() {
        let input = HelmReleaseSpecInput::new(
            "tenant-a",
            "demo-app",
            "nginx",
            HelmChartSourceRef {
                kind: HelmChartSourceKind::GitRepository,
                name: "flux-system".to_string(),
                namespace: Some("flux-system".to_string()),
            },
        );
        let err = synthesize_helm_release(&input).unwrap_err();
        assert!(matches!(err, KubeError::InvalidInput(_)));

        // Custom service account + minimal secret ref + validation checks
        let mut custom_cfg = TenantIsolationConfig::new("ops");
        custom_cfg.service_account_name = Some("custom-ops-reconciler".to_string());
        let sa = build_tenant_service_account(&custom_cfg).unwrap();
        assert_eq!(sa.metadata.name.as_deref(), Some("custom-ops-reconciler"));

        let mut minimal = HelmReleaseSpecInput::new(
            "ops",
            "grafana",
            "grafana",
            HelmChartSourceRef {
                kind: HelmChartSourceKind::OCIRepository,
                name: "grafana-oci".to_string(),
                namespace: Some("tenant-ops".to_string()),
            },
        );
        minimal.service_account_name = Some("custom-ops-reconciler".to_string());
        minimal.values_from_secrets = vec![HelmValuesSecretRef::new("grafana-companion-secret")];
        let local_hr = synthesize_helm_release(&minimal).unwrap();
        assert_eq!(
            local_hr["spec"]["serviceAccountName"],
            "custom-ops-reconciler"
        );
        minimal.kube_config = Some(RemoteKubeConfigRef::new("remote-k3s"));
        let hr = synthesize_helm_release(&minimal).unwrap();
        // The explicit SA is kept for remote targets too (impersonated on the workload cluster).
        assert_eq!(hr["spec"]["serviceAccountName"], "custom-ops-reconciler");
        assert_eq!(
            hr["spec"]["valuesFrom"][0]["name"],
            "grafana-companion-secret"
        );
        assert_eq!(hr["spec"]["kubeConfig"]["secretRef"]["name"], "remote-k3s");

        minimal.chart = "   ".to_string();
        assert!(synthesize_helm_release(&minimal).is_err());
    }

    #[test]
    fn helm_release_target_namespace_requires_remote_kubeconfig() {
        let mut input = HelmReleaseSpecInput::new(
            "acme",
            "podinfo-staging",
            "podinfo",
            HelmChartSourceRef {
                kind: HelmChartSourceKind::HelmRepository,
                name: "podinfo-staging-repo".to_string(),
                namespace: Some("tenant-acme".to_string()),
            },
        );
        input.target_namespace = Some("staging-apps".to_string());
        let err = synthesize_helm_release(&input).unwrap_err();
        assert!(matches!(err, KubeError::InvalidInput(_)));

        input.kube_config = Some(RemoteKubeConfigRef::new("workload-kubeconfig"));
        let hr = synthesize_helm_release(&input).unwrap();
        assert_eq!(hr["spec"]["targetNamespace"], "staging-apps");
        assert_eq!(hr["spec"]["install"]["createNamespace"], true);
    }

    #[test]
    fn flagger_canary_synthesizes_gateway_api_and_prometheus_metrics() {
        let mut input = FlaggerCanarySpecInput::new("default", "podinfo");
        input.service_port = 9898;
        input.hosts = vec!["podinfo.keel.local".to_string()];

        let canary = synthesize_flagger_canary(&input).unwrap();
        assert_eq!(canary["apiVersion"], "flagger.app/v1beta1");
        assert_eq!(canary["kind"], "Canary");
        assert_eq!(canary["metadata"]["name"], "podinfo");
        assert_eq!(canary["metadata"]["namespace"], "tenant-default");
        assert_eq!(canary["spec"]["provider"], "gatewayapi:v1");
        assert_eq!(canary["spec"]["targetRef"]["kind"], "Deployment");
        assert_eq!(canary["spec"]["targetRef"]["name"], "podinfo");
        assert_eq!(canary["spec"]["service"]["port"], 9898);
        assert_eq!(
            canary["spec"]["service"]["gatewayRefs"][0]["name"],
            "keel-ingress"
        );
        assert_eq!(
            canary["spec"]["service"]["gatewayRefs"][0]["namespace"],
            "keel-system"
        );
        assert_eq!(canary["spec"]["service"]["hosts"][0], "podinfo.keel.local");
        assert_eq!(
            canary["spec"]["analysis"]["metrics"][0]["name"],
            "envoy-request-success-rate"
        );

        let np = synthesize_canary_gateway_network_policy("default", "podinfo", 9898).unwrap();
        assert_eq!(np["kind"], "NetworkPolicy");
        assert_eq!(np["metadata"]["name"], "allow-podinfo-envoy-ingress");
        assert_eq!(np["metadata"]["namespace"], "tenant-default");
        assert_eq!(
            np["spec"]["ingress"][0]["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "keel-system"
        );
        assert_eq!(np["spec"]["ingress"][0]["ports"][0]["port"], 9898);
    }

    #[test]
    fn flux_kustomization_synthesizes_post_build_substitute_and_substitute_from() {
        let mut input = FluxKustomizationSpecInput::new(
            "default",
            "raw-nginx",
            "keel-gitops",
            "./tenants/default/apps/raw-nginx",
        );
        input
            .post_build_substitute
            .insert("ENV".to_string(), "production".to_string());
        input.post_build_substitute_from = vec![
            FluxSubstituteReference {
                kind: "ConfigMap".to_string(),
                name: "raw-nginx-values".to_string(),
                optional: true,
            },
            FluxSubstituteReference {
                kind: "Secret".to_string(),
                name: "raw-nginx-secrets".to_string(),
                optional: true,
            },
        ];

        let yaml = synthesize_flux_kustomization_yaml(&input).unwrap();
        let doc: Value = noyalib::from_str(&yaml).unwrap();
        assert_eq!(doc["apiVersion"], "kustomize.toolkit.fluxcd.io/v1");
        assert_eq!(doc["kind"], "Kustomization");
        assert_eq!(doc["spec"]["postBuild"]["substitute"]["ENV"], "production");
        let sub_from = doc["spec"]["postBuild"]["substituteFrom"]
            .as_array()
            .unwrap();
        assert_eq!(sub_from.len(), 2);
        assert_eq!(sub_from[0]["kind"], "ConfigMap");
        assert_eq!(sub_from[0]["name"], "raw-nginx-values");
        assert_eq!(sub_from[0]["optional"], true);
        assert_eq!(sub_from[1]["kind"], "Secret");
        assert_eq!(sub_from[1]["name"], "raw-nginx-secrets");
        assert_eq!(sub_from[1]["optional"], true);
    }

    #[test]
    fn flux_kustomization_decryption_is_opt_in_and_same_namespace() {
        let mut input = FluxKustomizationSpecInput::new(
            "default",
            "rauthy",
            "keel-gitops",
            "./tenants/default/apps/rauthy",
        );
        let doc = synthesize_flux_kustomization(&input).unwrap();
        assert!(doc["spec"].get("decryption").is_none());

        input.decryption_secret_name = Some("sops-age".to_string());
        let doc = synthesize_flux_kustomization(&input).unwrap();
        assert_eq!(doc["spec"]["decryption"]["provider"], "sops");
        assert_eq!(doc["spec"]["decryption"]["secretRef"]["name"], "sops-age");
        assert!(
            doc["spec"]["decryption"]["secretRef"]
                .get("namespace")
                .is_none()
        );

        input.decryption_secret_name = Some("Bad_Name".to_string());
        assert!(synthesize_flux_kustomization(&input).is_err());
    }

    #[test]
    fn flux_manifest_app_config_kustomization_orders_substitution_sources_first() {
        let mut labels = BTreeMap::new();
        labels.insert("keel.io/runtime".to_string(), "manifest".to_string());
        labels.insert("keel.io/catalog-id".to_string(), "rauthy".to_string());
        let input = FluxManifestAppInput {
            tenant_id: "acme".to_string(),
            name: "rauthy".to_string(),
            source_name: "keel-gitops".to_string(),
            app_dir: "./tenants/acme/apps/rauthy/".to_string(),
            kube_config: None,
            target_namespace: None,
            has_secrets: true,
            depends_on: vec![FluxDependencyRef {
                name: "bundle-cluster".to_string(),
            }],
            extra_labels: labels,
        };
        let (config_yaml, app_yaml) = synthesize_flux_manifest_app_kustomizations(&input).unwrap();
        let config: Value = noyalib::from_str(&config_yaml).unwrap();
        let app: Value = noyalib::from_str(&app_yaml).unwrap();

        assert_eq!(config["metadata"]["name"], "rauthy-config");
        assert_eq!(config["spec"]["path"], "./tenants/acme/apps/rauthy/config");
        assert_eq!(config["spec"]["targetNamespace"], "tenant-acme");
        assert!(config["spec"].get("kubeConfig").is_none());
        assert!(config["spec"].get("postBuild").is_none());
        assert_eq!(
            config["spec"]["decryption"]["secretRef"]["name"],
            "sops-age"
        );
        assert_eq!(config["metadata"]["labels"]["keel.io/config-for"], "rauthy");
        assert!(
            config["metadata"]["labels"]
                .get("keel.io/runtime")
                .is_none()
        );

        assert_eq!(app["metadata"]["name"], "rauthy");
        assert_eq!(app["spec"]["path"], "./tenants/acme/apps/rauthy");
        // `wait: true`: Ready (and so any `dependsOn` from later bundle items) only once every
        // applied workload is healthy; the config sources must be applied first likewise.
        assert_eq!(app["spec"]["wait"], true);
        assert_eq!(config["spec"]["wait"], true);
        assert!(app["spec"].get("decryption").is_none());
        assert_eq!(app["metadata"]["labels"]["keel.io/runtime"], "manifest");
        let deps: Vec<&str> = app["spec"]["dependsOn"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(deps, ["bundle-cluster", "rauthy-config"]);
        let sub_from = app["spec"]["postBuild"]["substituteFrom"]
            .as_array()
            .unwrap();
        assert_eq!(sub_from.len(), 2);
        assert_eq!(sub_from[0]["kind"], "ConfigMap");
        assert_eq!(sub_from[0]["name"], "rauthy-values");
        assert_eq!(sub_from[1]["kind"], "Secret");
        assert_eq!(sub_from[1]["name"], "rauthy-secrets");
        assert!(sub_from.iter().all(|s| s["optional"] == false));
    }

    #[test]
    fn flux_manifest_app_remote_target_keeps_config_on_management_cluster() {
        let input = FluxManifestAppInput {
            tenant_id: "acme".to_string(),
            name: "web".to_string(),
            source_name: "keel-gitops".to_string(),
            app_dir: "tenants/acme/apps/web".to_string(),
            kube_config: Some(RemoteKubeConfigRef::with_key("edge", "kubeconfig")),
            target_namespace: Some("apps".to_string()),
            has_secrets: false,
            depends_on: Vec::new(),
            extra_labels: BTreeMap::new(),
        };
        let (config_yaml, app_yaml) = synthesize_flux_manifest_app_kustomizations(&input).unwrap();
        let config: Value = noyalib::from_str(&config_yaml).unwrap();
        let app: Value = noyalib::from_str(&app_yaml).unwrap();

        assert!(config["spec"].get("kubeConfig").is_none());
        assert_eq!(config["spec"]["targetNamespace"], "tenant-acme");
        assert!(config["spec"].get("decryption").is_none());
        assert_eq!(app["spec"]["kubeConfig"]["secretRef"]["name"], "edge");
        assert_eq!(app["spec"]["targetNamespace"], "apps");
        assert_eq!(
            app["spec"]["dependsOn"],
            json!([{ "name": "web-config", "namespace": "tenant-acme" }])
        );
        let sub_from = app["spec"]["postBuild"]["substituteFrom"]
            .as_array()
            .unwrap();
        assert_eq!(sub_from.len(), 1, "no Secret source without secret values");
        assert_eq!(sub_from[0]["optional"], false);
    }

    #[test]
    fn pko_workspace_policy_reaches_only_the_local_docker_proxy_in_flux_system() {
        let np = serde_json::to_value(build_tenant_pko_workspace_network_policy("acme").unwrap())
            .unwrap();
        let rules = np["spec"]["egress"].as_array().unwrap();
        let docker: Vec<_> = rules
            .iter()
            .filter(|r| {
                r["ports"]
                    .as_array()
                    .is_some_and(|ps| ps.iter().any(|p| p["port"] == LOCAL_DOCKER_PROXY_PORT))
            })
            .collect();
        assert_eq!(docker.len(), 1, "exactly one docker proxy rule");
        let to = &docker[0]["to"][0];
        assert_eq!(
            to["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "flux-system"
        );
        assert_eq!(
            to["podSelector"]["matchLabels"]["app"],
            LOCAL_DOCKER_PROXY_APP_LABEL
        );
        // The label must match the shipped proxy manifest.
        let manifest = include_str!("../../../deploy/bootstrap/manifests/local-docker-proxy.yaml");
        assert!(manifest.contains(&format!("app: {LOCAL_DOCKER_PROXY_APP_LABEL}")));
        assert!(manifest.contains(&format!("port: {LOCAL_DOCKER_PROXY_PORT}")));
    }

    #[test]
    fn pko_workspace_policy_never_allows_cloud_metadata() {
        let policy =
            serde_json::to_value(build_tenant_pko_workspace_network_policy("acme").unwrap())
                .unwrap();
        let egress = policy["spec"]["egress"].as_array().unwrap();
        let mut public_rules = 0;
        for rule in egress {
            for peer in rule["to"].as_array().unwrap() {
                let Some(block) = peer.get("ipBlock") else {
                    // Selector peers are pods (DNS, Envoy proxies, state backend), never
                    // link-local addresses, and always port-scoped.
                    assert!(rule["ports"].as_array().is_some_and(|p| !p.is_empty()));
                    continue;
                };
                let cidr = block["cidr"].as_str().unwrap();
                assert!(!cidr.starts_with("169.254."), "{cidr}");
                if cidr == "0.0.0.0/0" {
                    public_rules += 1;
                    let except: Vec<&str> = block["except"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    for private in [
                        "169.254.0.0/16",
                        "10.0.0.0/8",
                        "172.16.0.0/12",
                        "192.168.0.0/16",
                    ] {
                        assert!(except.contains(&private), "{private} must be excluded");
                    }
                }
            }
        }
        assert_eq!(public_rules, 1);
    }
}
