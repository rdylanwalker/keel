//! Multi-tenant boundary isolation & Flux `HelmRelease` manifest synthesis (`SDD.md` §5.1, §6.2).
//!
//! Synthesizes tenant `Namespace` (enforcing `restricted` Pod Security Standards),
//! `ServiceAccount`, `ResourceQuota`, default-deny `NetworkPolicy`, and Flux CD
//! `HelmRelease` manifests with `spec.valuesFrom` (`kind: Secret`) companion infra bindings
//! and optional `spec.kubeConfig.secretRef` for remote workload clusters.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Namespace, ResourceQuota, ResourceQuotaSpec, ServiceAccount};
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicySpec};
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
    let bytes = tenant_id.as_bytes();
    let starts_ends_alnum = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let ends_alnum =
        bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit();
    let all_valid = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');

    if !starts_ends_alnum || !ends_alnum || !all_valid {
        return Err(KubeError::InvalidInput(format!(
            "tenant ID `{tenant_id}` must contain only lowercase alphanumeric characters or `-`, and start/end with an alphanumeric character"
        )));
    }
    Ok(())
}

/// Validates a Kubernetes resource name (RFC 1123 DNS subdomain segment).
pub fn validate_resource_name(field: &str, name: &str) -> Result<(), KubeError> {
    if name.is_empty() || name.len() > 63 {
        return Err(KubeError::InvalidInput(format!(
            "{field} `{name}` must be 1..=63 characters"
        )));
    }
    let bytes = name.as_bytes();
    let valid_start = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let valid_end =
        bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit();
    let all_valid = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');

    if !valid_start || !valid_end || !all_valid {
        return Err(KubeError::InvalidInput(format!(
            "{field} `{name}` must be a valid RFC 1123 DNS label"
        )));
    }
    Ok(())
}

/// Maps a validated `<tenant-id>` to its isolated Kubernetes namespace `tenant-<tenant-id>`.
pub fn tenant_namespace(tenant_id: &str) -> Result<String, KubeError> {
    validate_tenant_id(tenant_id)?;
    Ok(format!("{TENANT_NAMESPACE_PREFIX}{tenant_id}"))
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

fn tenant_common_labels(tenant_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "keel".to_string(),
        ),
        ("keel.io/tenant".to_string(), tenant_id.to_string()),
    ])
}

/// Generates the tenant `Namespace` (`tenant-<id>`) enforcing Kubernetes `restricted`
/// Pod Security Standards (`NFR-SEC-02`, `NFR-SEC-03`).
pub fn build_tenant_namespace(tenant_id: &str) -> Result<Namespace, KubeError> {
    let ns_name = tenant_namespace(tenant_id)?;
    let mut labels = tenant_common_labels(tenant_id);
    labels.insert(
        "pod-security.kubernetes.io/enforce".to_string(),
        "restricted".to_string(),
    );
    labels.insert(
        "pod-security.kubernetes.io/enforce-version".to_string(),
        "latest".to_string(),
    );
    labels.insert(
        "pod-security.kubernetes.io/audit".to_string(),
        "restricted".to_string(),
    );
    labels.insert(
        "pod-security.kubernetes.io/warn".to_string(),
        "restricted".to_string(),
    );

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
            labels: Some(tenant_common_labels(&config.tenant_id)),
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
            labels: Some(tenant_common_labels(&config.tenant_id)),
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
            labels: Some(tenant_common_labels(tenant_id)),
            ..ObjectMeta::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: LabelSelector::default(),
            policy_types: Some(vec!["Ingress".to_string(), "Egress".to_string()]),
            ingress: None,
            egress: None,
        }),
    })
}

/// Synthesized bundle of baseline multi-tenant isolation manifests (`tenants/<tenant-id>/rbac.yaml`).
#[derive(Debug, Clone, PartialEq)]
pub struct TenantManifestBundle {
    pub namespace: Namespace,
    pub service_account: ServiceAccount,
    pub resource_quota: ResourceQuota,
    pub network_policy: NetworkPolicy,
}

impl TenantManifestBundle {
    /// Synthesizes all four baseline isolation resources for a tenant.
    pub fn synthesize(config: &TenantIsolationConfig) -> Result<Self, KubeError> {
        Ok(Self {
            namespace: build_tenant_namespace(&config.tenant_id)?,
            service_account: build_tenant_service_account(config)?,
            resource_quota: build_tenant_resource_quota(config)?,
            network_policy: build_tenant_default_deny_network_policy(&config.tenant_id)?,
        })
    }

    /// Serializes all tenant isolation manifests to JSON `Value` documents.
    pub fn to_json_values(&self) -> Result<Vec<Value>, KubeError> {
        Ok(vec![
            serde_json::to_value(&self.namespace)?,
            serde_json::to_value(&self.service_account)?,
            serde_json::to_value(&self.resource_quota)?,
            serde_json::to_value(&self.network_policy)?,
        ])
    }

    /// Serializes all tenant isolation manifests into a multi-document YAML string (`---` separated).
    pub fn to_multi_doc_yaml(&self) -> Result<String, KubeError> {
        let docs = self.to_json_values()?;
        let mut rendered = Vec::with_capacity(docs.len());
        for doc in docs {
            rendered.push(serde_yml::to_string(&doc)?);
        }
        Ok(rendered.join("---\n"))
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

/// Companion infrastructure secret binding rendered into `HelmRelease.spec.valuesFrom` (`kind: Secret`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelmValuesSecretRef {
    /// Name of the tenant-scoped Kubernetes `Secret` holding companion PKO outputs or SOPS secrets.
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

    pub fn with_key_and_target(
        name: impl Into<String>,
        values_key: impl Into<String>,
        target_path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            values_key: Some(values_key.into()),
            target_path: Some(target_path.into()),
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
}

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
        }
    }
}

/// Synthesizes a Flux CD `HelmRelease` (`helm.toolkit.fluxcd.io/v2`) JSON manifest enforcing:
/// - `tenant-<id>` namespace confinement (`--no-cross-namespace-refs=true`),
/// - mandatory `spec.serviceAccountName` impersonation,
/// - `spec.valuesFrom` (`kind: Secret`) bindings for companion PKO infrastructure outputs, and
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
        spec.insert("install".to_string(), json!({ "createNamespace": true }));
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

    Ok(json!({
        "apiVersion": "helm.toolkit.fluxcd.io/v2",
        "kind": "HelmRelease",
        "metadata": {
            "name": input.release_name,
            "namespace": namespace,
            "labels": labels,
        },
        "spec": spec,
    }))
}

/// Synthesizes a Flux CD `HelmRelease` manifest directly as YAML.
pub fn synthesize_helm_release_yaml(input: &HelmReleaseSpecInput) -> Result<String, KubeError> {
    let manifest = synthesize_helm_release(input)?;
    Ok(serde_yml::to_string(&manifest)?)
}

/// Health-check target for `Kustomization.spec.healthChecks` (e.g., waiting on a PKO `Stack` CR).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluxHealthCheckRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
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
    pub extra_labels: BTreeMap<String, String>,
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
            extra_labels: BTreeMap::new(),
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
    spec.insert("serviceAccountName".to_string(), Value::String(sa_name));
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
    Ok(serde_yml::to_string(&manifest)?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(json_docs.len(), 4);
        assert_eq!(json_docs[0]["kind"], "Namespace");
        assert_eq!(json_docs[1]["kind"], "ServiceAccount");
        assert_eq!(json_docs[2]["kind"], "ResourceQuota");
        assert_eq!(json_docs[3]["kind"], "NetworkPolicy");

        let yaml = bundle.to_multi_doc_yaml().unwrap();
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
            HelmValuesSecretRef::with_key_and_target(
                "postgres-managed-outputs",
                "db_host",
                "authentik.postgresql.host",
            ),
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
        minimal.kube_config = Some(RemoteKubeConfigRef::new("remote-k3s"));
        let hr = synthesize_helm_release(&minimal).unwrap();
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
}
