//! Kyverno CEL `ValidatingPolicy` (`policies.kyverno.io/v1`) & explicit local-dev `PolicyException` synthesis (`SDD.md` §6.2, `NFR-SEC-03`, `NFR-SEC-04`).
//!
//! Generates the three baseline admission policies (`disallow-privileged-containers`,
//! `protect-control-plane`, `restrict-pulumi-providers`) in `Enforce` mode while keeping
//! production defaults strictly `restricted`. When `local_dev_docker_socket_exception` is
//! explicitly enabled via CLI/init config, generates a scoped `policies.kyverno.io/v1` `PolicyException`
//! (`local-kind-docker-socket-exception`) allowing PKO workspace pods in `keel-previews` / `tenant-*`
//! to access `/var/run/docker.sock` or `DOCKER_HOST` for `kind`-in-`kind` provisioning.

use std::collections::BTreeMap;

use keel_core::{FLUX_SYSTEM_NAMESPACE, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KubeError;
use crate::tenant::{TENANT_RECONCILER_SERVICE_ACCOUNT, validate_resource_name};

pub const POLICY_DISALLOW_PRIVILEGED: &str = "disallow-privileged-containers";
pub const POLICY_PROTECT_CONTROL_PLANE: &str = "protect-control-plane";
pub const POLICY_RESTRICT_PULUMI_PROVIDERS: &str = "restrict-pulumi-providers";
/// Retired `GeneratingPolicy` that cloned control-plane admin credentials into every tenant.
/// Kept only so bootstrap can delete it (and its RBAC) from existing clusters.
pub const POLICY_SYNC_TENANT_SECRETS: &str = "sync-tenant-secrets";
/// Retired RBAC (`ClusterRole` + `ClusterRoleBinding`) used by [`POLICY_SYNC_TENANT_SECRETS`].
pub const RETIRED_SYNC_TENANT_SECRETS_RBAC: &str = "keel-kyverno-tenant-secret-sync";
pub const POLICY_RESTRICT_TENANT_RECONCILER: &str = "restrict-tenant-reconciler";
pub const EXCEPTION_LOCAL_KIND_DOCKER_SOCKET: &str = "local-kind-docker-socket-exception";

pub const RULE_DISALLOW_PRIVILEGED: &str = "disallow-privileged";
pub const RULE_DISALLOW_HOST_PATH_AND_DOCKER_SOCKET: &str = "disallow-host-path-and-docker-socket";

/// Default Day-1 Pulumi/OpenTofu providers permitted when no custom list is supplied (`SDD.md` §4.1).
pub fn default_allowed_pulumi_providers() -> Vec<String> {
    vec![
        "proxmox".to_string(),
        "aws".to_string(),
        "gcp".to_string(),
        "azure".to_string(),
        "hetzner".to_string(),
        "cloudflare".to_string(),
        "kubernetes".to_string(),
    ]
}

/// Customizable configuration for the opt-in `policies.kyverno.io/v1` `PolicyException`
/// (`local-kind-docker-socket-exception`) used in local `kind` development environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerSocketExceptionConfig {
    /// Name of the `PolicyException` resource (defaults to `local-kind-docker-socket-exception`).
    pub name: String,
    /// Namespace where Kyverno watches `PolicyException` CRs (`keel-system` so tenants cannot self-exempt).
    pub exception_namespace: String,
    /// Namespace patterns where PKO workspace pods may use the exception (`keel-previews`, `tenant-*`).
    pub allowed_namespaces: Vec<String>,
    /// Pod labels required on PKO workspace pods to qualify for the exception.
    pub pod_match_labels: BTreeMap<String, String>,
    /// Permitted host Docker socket path (defaults to `/var/run/docker.sock`).
    pub docker_socket_path: String,
    /// Whether `DOCKER_HOST` environment variable usage is also exempted for remote/socket daemon access.
    pub allow_docker_host_env: bool,
    /// Rule names in `disallow-privileged-containers` exempted for matching PKO workspace pods.
    pub exempted_rule_names: Vec<String>,
}

impl Default for DockerSocketExceptionConfig {
    fn default() -> Self {
        Self {
            name: EXCEPTION_LOCAL_KIND_DOCKER_SOCKET.to_string(),
            exception_namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            allowed_namespaces: vec![KEEL_PREVIEWS_NAMESPACE.to_string(), "tenant-*".to_string()],
            pod_match_labels: BTreeMap::from([(
                "auto.pulumi.com/component".to_string(),
                "workspace".to_string(),
            )]),
            docker_socket_path: "/var/run/docker.sock".to_string(),
            allow_docker_host_env: true,
            exempted_rule_names: vec![
                RULE_DISALLOW_PRIVILEGED.to_string(),
                RULE_DISALLOW_HOST_PATH_AND_DOCKER_SOCKET.to_string(),
            ],
        }
    }
}

/// Top-level Kyverno policy synthesis configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KyvernoPolicyConfig {
    /// Enabled Pulumi/OpenTofu providers allowed in `Stack` CRs.
    pub allowed_providers: Vec<String>,
    /// Explicit local-dev opt-in (`false` by default in production!).
    pub local_dev_docker_socket_exception: bool,
    /// Customization options when `local_dev_docker_socket_exception` is `true`.
    pub docker_socket_exception: DockerSocketExceptionConfig,
}

impl Default for KyvernoPolicyConfig {
    fn default() -> Self {
        Self {
            allowed_providers: default_allowed_pulumi_providers(),
            local_dev_docker_socket_exception: false,
            docker_socket_exception: DockerSocketExceptionConfig::default(),
        }
    }
}

/// Generates `ValidatingPolicy/disallow-privileged-containers` (`policies.kyverno.io/v1`).
///
/// Enforces non-privileged, non-root, `allowPrivilegeEscalation: false`, `drop: ["ALL"]` on every
/// container, init container, and ephemeral container; forbids `hostNetwork`/`hostPID`/`hostIPC`
/// and `hostPort`; and blocks `hostPath` volumes (including `/var/run/docker.sock`) and `DOCKER_HOST` injection
/// across `keel-previews` and `tenant-*` unless explicitly exempted via a `PolicyException`.
pub fn build_disallow_privileged_containers_policy() -> Value {
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_DISALLOW_PRIVILEGED,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/policy-tier": "baseline-security"
            }
        },
        "spec": {
            "validationActions": ["Deny"],
            "matchConstraints": {
                "resourceRules": [
                    {
                        "apiGroups": [""],
                        "apiVersions": ["v1"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["pods"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-or-preview-namespace",
                    "expression": format!(
                        "(has(request.namespace) ? request.namespace : '') == '{KEEL_PREVIEWS_NAMESPACE}' || (has(request.namespace) ? request.namespace : '').startsWith('tenant-')"
                    )
                }
            ],
            "variables": [
                {
                    "name": "allContainers",
                    "expression": "object.spec.containers + (has(object.spec.initContainers) ? object.spec.initContainers : []) + (has(object.spec.ephemeralContainers) ? object.spec.ephemeralContainers : [])"
                }
            ],
            "validations": [
                {
                    "expression": "variables.allContainers.all(c, has(c.securityContext) && (!has(c.securityContext.privileged) || !c.securityContext.privileged) && has(c.securityContext.allowPrivilegeEscalation) && !c.securityContext.allowPrivilegeEscalation && ((has(object.spec.securityContext) && has(object.spec.securityContext.runAsNonRoot) && object.spec.securityContext.runAsNonRoot) || (has(c.securityContext.runAsNonRoot) && c.securityContext.runAsNonRoot)) && has(c.securityContext.capabilities) && has(c.securityContext.capabilities.drop) && 'ALL' in c.securityContext.capabilities.drop)",
                    "message": "Privileged containers and privilege escalation are prohibited in containers, initContainers, and ephemeralContainers; pods must adhere to restricted PSS."
                },
                {
                    "expression": "(!has(object.spec.hostNetwork) || !object.spec.hostNetwork) && (!has(object.spec.hostPID) || !object.spec.hostPID) && (!has(object.spec.hostIPC) || !object.spec.hostIPC)",
                    "message": "Sharing host namespaces (hostNetwork, hostPID, hostIPC) is forbidden."
                },
                {
                    "expression": "variables.allContainers.all(c, !has(c.ports) || c.ports.all(p, !has(p.hostPort) || p.hostPort == 0))",
                    "message": "Binding container ports to host ports (hostPort) is forbidden."
                },
                {
                    "expression": "(!has(object.spec.volumes) || object.spec.volumes.all(v, !has(v.hostPath))) && variables.allContainers.all(c, !has(c.env) || c.env.all(e, e.name != 'DOCKER_HOST'))",
                    "message": "Mounting hostPath volumes (such as /var/run/docker.sock) or injecting DOCKER_HOST is forbidden in production."
                }
            ]
        }
    })
}

/// Generates `ValidatingPolicy/protect-control-plane` (`policies.kyverno.io/v1`).
///
/// Prevents deletion or unauthorized mutation of `management-cluster-core` PKO `Stack`s
/// (`NFR-REL-01`) and blocks tenant/preview `ServiceAccount`s from mutating `flux-system`
/// or `keel-system` (`NFR-SEC-04`).
pub fn build_protect_control_plane_policy() -> Value {
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_PROTECT_CONTROL_PLANE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/policy-tier": "anti-lockout"
            }
        },
        "spec": {
            "validationActions": ["Deny"],
            "evaluation": {
                "background": {
                    "enabled": false
                }
            },
            "matchConstraints": {
                "resourceRules": [
                    {
                        "apiGroups": ["pulumi.com"],
                        "apiVersions": ["v1"],
                        "operations": ["DELETE"],
                        "resources": ["stacks"]
                    },
                    {
                        "apiGroups": ["*"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE", "CONNECT"],
                        "resources": ["*"]
                    }
                ]
            },
            "validations": [
                {
                    "expression": "!(request.operation == 'DELETE' && request.kind.group == 'pulumi.com' && request.kind.kind == 'Stack' && (oldObject.metadata.name == 'management-cluster-core' || (has(oldObject.metadata.labels) && 'keel.io/protect' in oldObject.metadata.labels && oldObject.metadata.labels['keel.io/protect'] == 'true')))",
                    "message": "Protected control-plane Stack `management-cluster-core` cannot be deleted via standard admission; use `keel bootstrap recover` for break-glass maintenance."
                },
                {
                    "expression": format!(
                        "!(((has(request.namespace) ? request.namespace : '') == '{FLUX_SYSTEM_NAMESPACE}' || (has(request.namespace) ? request.namespace : '') == '{KEEL_SYSTEM_NAMESPACE}') && has(request.userInfo) && has(request.userInfo.username) && (request.userInfo.username.startsWith('system:serviceaccount:{KEEL_PREVIEWS_NAMESPACE}:') || request.userInfo.username.startsWith('system:serviceaccount:tenant-')))"
                    ),
                    "message": "Tenant and preview ServiceAccounts are strictly prohibited from mutating control-plane namespaces."
                },
                {
                    // Deleting the protected `flux2` HelmRelease would `helm uninstall` Flux itself;
                    // deleting a protected add-on HelmRelease/Kustomization uninstalls that add-on.
                    "expression": format!(
                        "!(request.operation == 'DELETE' && (has(request.namespace) ? request.namespace : '') == '{FLUX_SYSTEM_NAMESPACE}' && ((request.kind.group == 'helm.toolkit.fluxcd.io' && request.kind.kind == 'HelmRelease') || (request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization')) && has(oldObject.metadata.labels) && 'keel.io/protect' in oldObject.metadata.labels && oldObject.metadata.labels['keel.io/protect'] == 'true')"
                    ),
                    "message": "Protected control-plane Flux objects (`keel.io/protect: \"true\"`, e.g. the `flux2` HelmRelease whose deletion would uninstall Flux) cannot be deleted; break-glass: suspend the parent Kustomization (e.g. `core-addons`) or remove the label in Git, then remove the `keel.io/protect` label and delete."
                }
            ]
        }
    })
}

/// Generates `ValidatingPolicy/restrict-pulumi-providers` (`policies.kyverno.io/v1`).
///
/// Ensures PKO `Stack` resources in `tenant-*` and `keel-previews` only instantiate
/// providers enabled in `.keel/config.yaml` / `providers/registry.yaml`.
pub fn build_restrict_pulumi_providers_policy(
    allowed_providers: &[String],
) -> Result<Value, KubeError> {
    if allowed_providers.is_empty() || allowed_providers.iter().any(|p| p.trim().is_empty()) {
        return Err(KubeError::InvalidInput(
            "allowed_providers must contain at least one non-empty provider identifier".to_string(),
        ));
    }

    let pattern = allowed_providers.join(" | ");
    let providers_cel = serde_json::to_string(allowed_providers)?;

    Ok(json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_PULUMI_PROVIDERS,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/policy-tier": "provider-governance"
            }
        },
        "spec": {
            "validationActions": ["Deny"],
            "matchConstraints": {
                "resourceRules": [
                    {
                        "apiGroups": ["pulumi.com"],
                        "apiVersions": ["v1"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["stacks"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-or-preview-namespace",
                    "expression": format!(
                        "(has(request.namespace) ? request.namespace : '') == '{KEEL_PREVIEWS_NAMESPACE}' || (has(request.namespace) ? request.namespace : '').startsWith('tenant-')"
                    )
                }
            ],
            "validations": [
                {
                    "expression": format!(
                        "has(object.metadata.labels) && ('keel.io/provider' in object.metadata.labels) && object.metadata.labels['keel.io/provider'] in {providers_cel}"
                    ),
                    "message": format!("Stack `keel.io/provider` label must be one of the enabled providers: {pattern}")
                }
            ]
        }
    }))
}

/// Built-in Pulumi providers every Keel control plane relies on regardless of
/// `providers/registry.yaml` (manifest apps and blueprints synthesize `kubernetes` Stacks).
pub const BUILTIN_PULUMI_PROVIDERS: &[&str] = &["kubernetes"];

/// Returns the deduplicated `restrict-pulumi-providers` allowlist: registry-enabled providers
/// followed by [`BUILTIN_PULUMI_PROVIDERS`]. Blank ids are dropped.
pub fn pulumi_provider_allowlist<S: AsRef<str>>(enabled_provider_ids: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let ids = enabled_provider_ids
        .iter()
        .map(|p| p.as_ref().trim())
        .chain(BUILTIN_PULUMI_PROVIDERS.iter().copied());
    for id in ids {
        if !id.is_empty() && !out.iter().any(|p| p == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Single entry point for (re-)generating `restrict-pulumi-providers` from the provider registry
/// (Stage 0 bootstrap, `keel serve` startup, provider-registry PR merges). Always includes the
/// built-in providers so a re-apply can never deny Keel's own `kubernetes` Stacks (OPEN-3).
pub fn build_restrict_pulumi_providers_policy_for_registry<S: AsRef<str>>(
    enabled_provider_ids: &[S],
) -> Result<Value, KubeError> {
    build_restrict_pulumi_providers_policy(&pulumi_provider_allowlist(enabled_provider_ids))
}

fn pod_spec_docker_expr(pod_spec_path: &str, socket_path: &str, allow_docker_host: bool) -> String {
    if allow_docker_host {
        format!(
            "((has({pod_spec_path}.volumes) && {pod_spec_path}.volumes.exists(v, has(v.hostPath) && v.hostPath.path == '{socket_path}')) || (has({pod_spec_path}.containers) && {pod_spec_path}.containers.exists(c, has(c.env) && c.env.exists(e, e.name == 'DOCKER_HOST'))))"
        )
    } else {
        format!(
            "(has({pod_spec_path}.volumes) && {pod_spec_path}.volumes.exists(v, has(v.hostPath) && v.hostPath.path == '{socket_path}'))"
        )
    }
}

/// Generates an explicit `policies.kyverno.io/v1` `PolicyException` (`local-kind-docker-socket-exception`)
/// allowing PKO workspace pods in `keel-previews` / `tenant-*` to mount `/var/run/docker.sock`
/// (or use `DOCKER_HOST`) when `local_dev_docker_socket_exception` is enabled.
pub fn build_local_kind_docker_socket_exception(
    config: &DockerSocketExceptionConfig,
) -> Result<Value, KubeError> {
    validate_resource_name("docker_socket_exception.name", &config.name)?;
    validate_resource_name(
        "docker_socket_exception.exception_namespace",
        &config.exception_namespace,
    )?;

    if config.allowed_namespaces.is_empty() {
        return Err(KubeError::InvalidInput(
            "docker_socket_exception.allowed_namespaces must not be empty".to_string(),
        ));
    }
    if !config.docker_socket_path.starts_with('/') {
        return Err(KubeError::InvalidInput(
            "docker_socket_exception.docker_socket_path must be an absolute path".to_string(),
        ));
    }
    if config.exempted_rule_names.is_empty() {
        return Err(KubeError::InvalidInput(
            "docker_socket_exception.exempted_rule_names must not be empty".to_string(),
        ));
    }

    let ns_expr = config
        .allowed_namespaces
        .iter()
        .map(|ns| {
            if let Some(prefix) = ns.strip_suffix('*') {
                format!("(has(request.namespace) ? request.namespace : '').startsWith('{prefix}')")
            } else {
                format!("(has(request.namespace) ? request.namespace : '') == '{ns}'")
            }
        })
        .collect::<Vec<_>>()
        .join(" || ");

    let mut match_conditions = vec![json!({
        "name": "allowed-namespaces",
        "expression": ns_expr
    })];

    if !config.pod_match_labels.is_empty() {
        let label_expr = config
            .pod_match_labels
            .iter()
            .map(|(k, v)| {
                format!(
                    "((has(object.metadata.labels) && ('{k}' in object.metadata.labels) && object.metadata.labels['{k}'] == '{v}') || (has(object.spec.template) && has(object.spec.template.metadata) && has(object.spec.template.metadata.labels) && ('{k}' in object.spec.template.metadata.labels) && object.spec.template.metadata.labels['{k}'] == '{v}'))"
                )
            })
            .collect::<Vec<_>>()
            .join(" && ");
        match_conditions.push(json!({
            "name": "pko-workspace-pod-labels",
            "expression": label_expr
        }));
    }

    let pod_socket_expr = pod_spec_docker_expr(
        "object.spec",
        &config.docker_socket_path,
        config.allow_docker_host_env,
    );
    let template_socket_expr = pod_spec_docker_expr(
        "object.spec.template.spec",
        &config.docker_socket_path,
        config.allow_docker_host_env,
    );
    let socket_expr = format!(
        "({pod_socket_expr}) || (has(object.spec.template) && has(object.spec.template.spec) && {template_socket_expr})"
    );
    match_conditions.push(json!({
        "name": "docker-socket-or-host",
        "expression": socket_expr
    }));

    Ok(json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "PolicyException",
        "metadata": {
            "name": config.name,
            "namespace": config.exception_namespace,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/local-dev-only": "true"
            },
            "annotations": {
                "keel.io/description": "Explicit local-dev exception allowing PKO workspace pods to access Docker socket / DOCKER_HOST for nested kind provisioning."
            }
        },
        "spec": {
            "policyRefs": [
                {
                    "kind": "ValidatingPolicy",
                    "name": POLICY_DISALLOW_PRIVILEGED
                }
            ],
            "matchConditions": match_conditions
        }
    }))
}

/// Generates `ValidatingPolicy/restrict-tenant-reconciler` (`policies.kyverno.io/v1`).
///
/// Confines the `flux-system/keel-tenant-reconciler` `ServiceAccount` (impersonated by the root
/// tenants Flux `Kustomization` under multi-tenancy lockdown) to `tenant-*` namespaces: namespaced
/// writes must target `tenant-*`, `Namespace` objects must be named `tenant-*`, and the only
/// permitted cluster-scoped object is a `ClusterRoleBinding` to `system:auth-delegator` for
/// `tenant-*` `ServiceAccount`s. Fails closed (`failurePolicy: Fail`).
pub fn build_restrict_tenant_reconciler_policy() -> Value {
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_TENANT_RECONCILER,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/policy-tier": "tenant-isolation"
            }
        },
        "spec": {
            "validationActions": ["Deny"],
            "failurePolicy": "Fail",
            "evaluation": {
                "background": {
                    "enabled": false
                }
            },
            "matchConstraints": {
                "resourceRules": [
                    {
                        "apiGroups": ["*"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["*"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-reconciler-only",
                    "expression": format!(
                        "request.userInfo.username == 'system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:{TENANT_RECONCILER_SERVICE_ACCOUNT}'"
                    )
                }
            ],
            "validations": [
                {
                    "expression": "(has(request.namespace) ? request.namespace : '') != '' ? (has(request.namespace) ? request.namespace : '').startsWith('tenant-') : (request.kind.kind == 'Namespace' ? request.name.startsWith('tenant-') : request.kind.kind == 'ClusterRoleBinding')",
                    "message": "keel-tenant-reconciler may only manage objects in tenant-* namespaces."
                },
                {
                    "expression": "request.kind.kind != 'ClusterRoleBinding' || request.operation == 'DELETE' || (object.roleRef.name == 'system:auth-delegator' && has(object.subjects) && object.subjects.all(s, s.kind == 'ServiceAccount' && has(s.namespace) && s.namespace.startsWith('tenant-')))",
                    "message": "keel-tenant-reconciler may only bind ClusterRole/system:auth-delegator to tenant ServiceAccounts."
                },
                {
                    "expression": "request.kind.kind != 'RoleBinding' || request.operation == 'DELETE' || (object.subjects.all(s, s.kind != 'ServiceAccount' || (has(s.namespace) && s.namespace == (has(request.namespace) ? request.namespace : '')))) || (object.roleRef.kind == 'ClusterRole' && object.roleRef.name == 'keel-server-tenant' && has(object.subjects) && object.subjects.all(s, s.kind == 'ServiceAccount' && s.name == 'keel-server' && has(s.namespace) && s.namespace == 'keel-system'))",
                    "message": "Tenant RoleBindings may only reference ServiceAccounts from the same tenant namespace (or bind ClusterRole/keel-server-tenant to keel-system/keel-server)."
                }
            ]
        }
    })
}

/// Generated bundle of baseline Kyverno `ValidatingPolicy` manifests and optional local-dev `PolicyException`.
#[derive(Debug, Clone, PartialEq)]
pub struct KyvernoManifestBundle {
    pub cluster_policies: Vec<Value>,
    pub supporting_manifests: Vec<Value>,
    pub policy_exception: Option<Value>,
}

impl KyvernoManifestBundle {
    /// Generates baseline Kyverno `ValidatingPolicy` manifests and includes `PolicyException`
    /// only when `config.local_dev_docker_socket_exception` is explicitly `true`.
    pub fn generate(config: &KyvernoPolicyConfig) -> Result<Self, KubeError> {
        let cluster_policies = vec![
            build_disallow_privileged_containers_policy(),
            build_protect_control_plane_policy(),
            build_restrict_pulumi_providers_policy(&config.allowed_providers)?,
        ];

        let supporting_manifests = vec![build_restrict_tenant_reconciler_policy()];

        let policy_exception = if config.local_dev_docker_socket_exception {
            Some(build_local_kind_docker_socket_exception(
                &config.docker_socket_exception,
            )?)
        } else {
            None
        };

        Ok(Self {
            cluster_policies,
            supporting_manifests,
            policy_exception,
        })
    }

    /// Returns all generated JSON manifests in deterministic apply order.
    pub fn all_manifests(&self) -> Vec<Value> {
        let mut all = self.cluster_policies.clone();
        all.extend(self.supporting_manifests.iter().cloned());
        if let Some(exc) = &self.policy_exception {
            all.push(exc.clone());
        }
        all
    }

    /// Serializes all generated policies (and optional exception) to multi-document YAML (`---`).
    pub fn to_multi_doc_yaml(&self) -> Result<String, KubeError> {
        let manifests = self.all_manifests();
        let mut docs = Vec::with_capacity(manifests.len());
        for m in manifests {
            let mut s = serde_yml::to_string(&m)?;
            if !s.ends_with('\n') {
                s.push('\n');
            }
            docs.push(s);
        }
        Ok(docs.join("---\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_production_config_generates_three_enforced_policies_without_exception() {
        let cfg = KyvernoPolicyConfig::default();
        assert!(!cfg.local_dev_docker_socket_exception);

        let bundle = KyvernoManifestBundle::generate(&cfg).unwrap();
        assert_eq!(bundle.cluster_policies.len(), 3);
        assert_eq!(bundle.supporting_manifests.len(), 1);
        assert!(bundle.policy_exception.is_none());
        assert_eq!(bundle.all_manifests().len(), 4);
        assert!(
            !bundle
                .to_multi_doc_yaml()
                .unwrap()
                .contains("GeneratingPolicy")
        );

        let names: Vec<&str> = bundle
            .cluster_policies
            .iter()
            .map(|p| p["metadata"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "disallow-privileged-containers",
                "protect-control-plane",
                "restrict-pulumi-providers"
            ]
        );

        for policy in &bundle.cluster_policies {
            assert_eq!(policy["apiVersion"], "policies.kyverno.io/v1");
            assert_eq!(policy["kind"], "ValidatingPolicy");
            assert_eq!(policy["spec"]["validationActions"], json!(["Deny"]));
        }

        let yaml = bundle.to_multi_doc_yaml().unwrap();
        assert!(yaml.contains("disallow-privileged-containers"));
        assert!(yaml.contains("protect-control-plane"));
        assert!(yaml.contains("restrict-pulumi-providers"));
        assert!(!yaml.contains("PolicyException"));
    }

    #[test]
    fn local_dev_flag_emits_kyverno_v2_docker_socket_policy_exception() {
        let cfg = KyvernoPolicyConfig {
            local_dev_docker_socket_exception: true,
            ..KyvernoPolicyConfig::default()
        };

        let bundle = KyvernoManifestBundle::generate(&cfg).unwrap();
        assert_eq!(bundle.all_manifests().len(), 5);

        let exc = bundle
            .policy_exception
            .as_ref()
            .expect("PolicyException must be generated when enabled");
        assert_eq!(exc["apiVersion"], "policies.kyverno.io/v1");
        assert_eq!(exc["kind"], "PolicyException");
        assert_eq!(
            exc["metadata"]["name"],
            "local-kind-docker-socket-exception"
        );
        assert_eq!(exc["metadata"]["namespace"], "keel-system");
        assert_eq!(exc["spec"]["policyRefs"][0]["kind"], "ValidatingPolicy");
        assert_eq!(
            exc["spec"]["policyRefs"][0]["name"],
            "disallow-privileged-containers"
        );

        let yaml = bundle.to_multi_doc_yaml().unwrap();
        assert!(yaml.contains("apiVersion: policies.kyverno.io/v1"));
        assert!(yaml.contains("kind: PolicyException"));
        assert!(yaml.contains("/var/run/docker.sock"));
        assert!(yaml.contains("DOCKER_HOST"));
        assert!(yaml.contains("auto.pulumi.com/component"));
        assert!(yaml.contains("object.spec.template.spec"));
    }

    #[test]
    fn custom_docker_socket_exception_and_validation_errors() {
        let custom = DockerSocketExceptionConfig {
            name: "custom-kind-socket-exc".to_string(),
            docker_socket_path: "/run/user/1000/docker.sock".to_string(),
            allowed_namespaces: vec!["keel-previews".to_string(), "tenant-dev".to_string()],
            allow_docker_host_env: false,
            ..DockerSocketExceptionConfig::default()
        };

        let exc = build_local_kind_docker_socket_exception(&custom).unwrap();
        assert_eq!(exc["metadata"]["name"], "custom-kind-socket-exc");
        let conds = exc["spec"]["matchConditions"].as_array().unwrap();
        assert_eq!(conds.len(), 3);
        assert!(
            !conds[2]["expression"]
                .as_str()
                .unwrap()
                .contains("DOCKER_HOST")
        );

        // Relative socket path rejected
        let mut bad_socket = custom.clone();
        bad_socket.docker_socket_path = "var/run/docker.sock".to_string();
        assert!(build_local_kind_docker_socket_exception(&bad_socket).is_err());

        // Empty namespaces rejected
        let mut bad_ns = custom.clone();
        bad_ns.allowed_namespaces.clear();
        assert!(build_local_kind_docker_socket_exception(&bad_ns).is_err());

        // Empty rule names rejected
        let mut bad_rules = custom;
        bad_rules.exempted_rule_names.clear();
        assert!(build_local_kind_docker_socket_exception(&bad_rules).is_err());

        // Empty allowed providers rejected
        assert!(build_restrict_pulumi_providers_policy(&[]).is_err());
    }

    #[test]
    fn admin_credentials_are_never_cloned_into_tenants() {
        let bundle = KyvernoManifestBundle::generate(&KyvernoPolicyConfig::default()).unwrap();
        let yaml = bundle.to_multi_doc_yaml().unwrap();
        assert!(!yaml.contains("keel-s3-credentials"));
        assert!(!yaml.contains("git-credentials"));
        assert!(!yaml.contains(POLICY_SYNC_TENANT_SECRETS));
        assert!(!yaml.contains(RETIRED_SYNC_TENANT_SECRETS_RBAC));
    }

    #[test]
    fn restrict_tenant_reconciler_policy_confines_to_tenant_namespaces() {
        let policy = build_restrict_tenant_reconciler_policy();
        assert_eq!(policy["kind"], "ValidatingPolicy");
        assert_eq!(
            policy["metadata"]["name"],
            POLICY_RESTRICT_TENANT_RECONCILER
        );
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");
        assert_eq!(
            policy["spec"]["matchConditions"][0]["expression"],
            "request.userInfo.username == 'system:serviceaccount:flux-system:keel-tenant-reconciler'"
        );
        let exprs: Vec<&str> = policy["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["expression"].as_str().unwrap())
            .collect();
        assert!(
            exprs[0].contains(
                "(has(request.namespace) ? request.namespace : '').startsWith('tenant-')"
            )
        );
        assert!(exprs[0].contains("request.name.startsWith('tenant-')"));
        assert!(exprs[1].contains("system:auth-delegator"));
        assert!(exprs[2].contains("RoleBinding"));
        // New tenants carry the keel-server RoleBinding (cross-namespace SA subject), which the
        // policy must explicitly allow while still pinning the role and subject.
        assert!(exprs[2].contains("object.roleRef.name == 'keel-server-tenant'"));
        assert!(exprs[2].contains("s.name == 'keel-server'"));
        assert!(exprs[2].contains("s.namespace == 'keel-system'"));
    }

    /// Kyverno's CEL `request` omits `namespace` for cluster-scoped objects, so a bare
    /// `request.namespace` errors ("no such key") and the fail-closed policy denies the request
    /// (this blocked tenant ClusterRoleBindings on fresh clusters). Every reference must be
    /// guarded with `has(request.namespace)`.
    #[test]
    fn policies_never_dereference_request_namespace_unguarded() {
        let guarded = "(has(request.namespace) ? request.namespace : '')";
        let mut docs = vec![
            build_disallow_privileged_containers_policy(),
            build_protect_control_plane_policy(),
            build_restrict_pulumi_providers_policy(&["kubernetes".to_string()]).unwrap(),
            build_restrict_tenant_reconciler_policy(),
        ];
        for seed in [
            include_str!("../../../deploy/seed/kyverno-policies/restrict-pulumi-providers.yaml"),
            include_str!(
                "../../../deploy/seed/kyverno-policies/disallow-privileged-containers.yaml"
            ),
            include_str!(
                "../../../deploy/seed/kyverno-policies/local-kind-docker-socket-exception.yaml"
            ),
            include_str!("../../../deploy/seed/kyverno-policies/protect-control-plane.yaml"),
            include_str!("../../../deploy/seed/kyverno-policies/restrict-tenant-reconciler.yaml"),
        ] {
            docs.push(serde_yml::from_str(seed).unwrap());
        }
        for doc in docs {
            let text = doc.to_string();
            let stripped = text
                .replace(guarded, "")
                .replace("has(request.namespace)", "");
            assert!(
                !stripped.contains("request.namespace"),
                "unguarded request.namespace in policy {}",
                doc["metadata"]["name"]
            );
        }
    }

    #[test]
    fn protect_control_plane_policy_matches_seed_and_guards_protected_flux_objects() {
        let seed: Value = serde_yml::from_str(include_str!(
            "../../../deploy/seed/kyverno-policies/protect-control-plane.yaml"
        ))
        .unwrap();
        let generated = build_protect_control_plane_policy();
        assert_eq!(
            seed["spec"]["validations"],
            generated["spec"]["validations"]
        );
        let flux_guard = generated["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["expression"].as_str().unwrap())
            .find(|e| e.contains("helm.toolkit.fluxcd.io"))
            .expect("protected Flux object deletion guard");
        for needle in [
            "request.operation == 'DELETE'",
            "== 'flux-system'",
            "request.kind.kind == 'HelmRelease'",
            "request.kind.kind == 'Kustomization'",
            "oldObject.metadata.labels['keel.io/protect'] == 'true'",
        ] {
            assert!(flux_guard.contains(needle), "{needle}");
        }
    }

    #[test]
    fn restrict_tenant_reconciler_policy_matches_seed_manifest() {
        let seed: Value = serde_yml::from_str(include_str!(
            "../../../deploy/seed/kyverno-policies/restrict-tenant-reconciler.yaml"
        ))
        .unwrap();
        let generated = build_restrict_tenant_reconciler_policy();
        assert_eq!(
            seed["spec"]["validations"],
            generated["spec"]["validations"]
        );
        assert_eq!(
            seed["spec"]["matchConditions"],
            generated["spec"]["matchConditions"]
        );
    }

    #[test]
    fn disallow_privileged_covers_all_container_types_and_host_namespaces() {
        let policy = build_disallow_privileged_containers_policy();
        let all = policy["spec"]["variables"][0]["expression"]
            .as_str()
            .unwrap();
        assert!(all.contains("object.spec.initContainers"));
        assert!(all.contains("object.spec.ephemeralContainers"));
        let exprs: Vec<&str> = policy["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["expression"].as_str().unwrap())
            .collect();
        assert_eq!(exprs.len(), 4);
        assert!(exprs[0].starts_with("variables.allContainers.all("));
        assert!(exprs[1].contains("hostNetwork"));
        assert!(exprs[1].contains("hostPID"));
        assert!(exprs[1].contains("hostIPC"));
        assert!(exprs[2].contains("hostPort"));
        assert!(exprs[3].contains("hostPath"));
        assert!(exprs[3].contains("variables.allContainers"));
        assert!(
            !exprs
                .iter()
                .any(|e| e.contains("object.spec.containers.all"))
        );
    }

    #[test]
    fn to_multi_doc_yaml_separates_documents_cleanly() {
        let bundle = KyvernoManifestBundle::generate(&KyvernoPolicyConfig {
            local_dev_docker_socket_exception: true,
            ..KyvernoPolicyConfig::default()
        })
        .unwrap();
        let yaml = bundle.to_multi_doc_yaml().unwrap();
        assert!(yaml.ends_with('\n'));
        assert!(yaml.contains("\n---\n"));
        for line in yaml.lines() {
            if line.contains("---") {
                assert_eq!(line, "---", "document separator must be on its own line");
            }
        }
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 5);
        for doc in docs {
            let parsed: serde_json::Value =
                serde_yml::from_str(doc).expect("each Kyverno multi-doc YAML part must parse");
            assert!(parsed.get("kind").is_some());
        }
    }

    #[test]
    fn registry_policy_always_allows_builtin_kubernetes_provider() {
        let allow = pulumi_provider_allowlist(&["aws", " ", "kubernetes", "aws"]);
        assert_eq!(allow, vec!["aws".to_string(), "kubernetes".to_string()]);
        let empty: [&str; 0] = [];
        assert_eq!(
            pulumi_provider_allowlist(&empty),
            vec!["kubernetes".to_string()]
        );

        let policy = build_restrict_pulumi_providers_policy_for_registry(&["proxmox"]).unwrap();
        let expr = policy["spec"]["validations"][0]["expression"]
            .as_str()
            .unwrap();
        assert!(expr.contains("\"kubernetes\""));
        assert!(expr.contains("\"proxmox\""));
        // Even with nothing enabled, the policy is valid and keeps `kubernetes`.
        assert!(build_restrict_pulumi_providers_policy_for_registry(&empty).is_ok());
    }
}
