//! Kyverno `ClusterPolicy` & explicit local-dev `PolicyException` synthesis (`SDD.md` §6.2, `NFR-SEC-03`, `NFR-SEC-04`).
//!
//! Generates the three baseline admission policies (`disallow-privileged-containers`,
//! `protect-control-plane`, `restrict-pulumi-providers`) in `Enforce` mode while keeping
//! production defaults strictly `restricted`. When `local_dev_docker_socket_exception` is
//! explicitly enabled via CLI/init config, generates a scoped `kyverno.io/v2` `PolicyException`
//! (`local-kind-docker-socket-exception`) allowing PKO workspace pods in `keel-previews` / `tenant-*`
//! to access `/var/run/docker.sock` or `DOCKER_HOST` for `kind`-in-`kind` provisioning.

use std::collections::BTreeMap;

use keel_core::{FLUX_SYSTEM_NAMESPACE, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KubeError;
use crate::tenant::validate_resource_name;

pub const POLICY_DISALLOW_PRIVILEGED: &str = "disallow-privileged-containers";
pub const POLICY_PROTECT_CONTROL_PLANE: &str = "protect-control-plane";
pub const POLICY_RESTRICT_PULUMI_PROVIDERS: &str = "restrict-pulumi-providers";
pub const POLICY_SYNC_TENANT_SECRETS: &str = "sync-tenant-secrets";
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

/// Customizable configuration for the opt-in `kyverno.io/v2` `PolicyException`
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
/// Enforces non-privileged, non-root, `allowPrivilegeEscalation: false`, `drop: ["ALL"]`,
/// and blocks `hostPath` volumes (including `/var/run/docker.sock`) and `DOCKER_HOST` injection
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
                        "request.namespace == '{KEEL_PREVIEWS_NAMESPACE}' || request.namespace.startsWith('tenant-')"
                    )
                }
            ],
            "validations": [
                {
                    "expression": "object.spec.containers.all(c, has(c.securityContext) && (!has(c.securityContext.privileged) || !c.securityContext.privileged) && has(c.securityContext.allowPrivilegeEscalation) && !c.securityContext.allowPrivilegeEscalation && ((has(object.spec.securityContext) && has(object.spec.securityContext.runAsNonRoot) && object.spec.securityContext.runAsNonRoot) || (has(c.securityContext.runAsNonRoot) && c.securityContext.runAsNonRoot)) && has(c.securityContext.capabilities) && has(c.securityContext.capabilities.drop) && 'ALL' in c.securityContext.capabilities.drop)",
                    "message": "Privileged containers and privilege escalation are prohibited; pods must adhere to restricted PSS."
                },
                {
                    "expression": "(!has(object.spec.volumes) || object.spec.volumes.all(v, !has(v.hostPath))) && object.spec.containers.all(c, !has(c.env) || c.env.all(e, e.name != 'DOCKER_HOST'))",
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
                        "!((request.namespace == '{FLUX_SYSTEM_NAMESPACE}' || request.namespace == '{KEEL_SYSTEM_NAMESPACE}') && has(request.userInfo) && has(request.userInfo.username) && (request.userInfo.username.startsWith('system:serviceaccount:{KEEL_PREVIEWS_NAMESPACE}:') || request.userInfo.username.startsWith('system:serviceaccount:tenant-')))"
                    ),
                    "message": "Tenant and preview ServiceAccounts are strictly prohibited from mutating control-plane namespaces."
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
                        "request.namespace == '{KEEL_PREVIEWS_NAMESPACE}' || request.namespace.startsWith('tenant-')"
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
                format!("request.namespace.startsWith('{prefix}')")
            } else {
                format!("request.namespace == '{ns}'")
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

/// Generates `GeneratingPolicy/sync-tenant-secrets` (`policies.kyverno.io/v1`) to clone and
/// synchronize `keel-s3-credentials` and `git-credentials` from `flux-system` into every
/// `tenant-*` namespace.
pub fn build_sync_tenant_secrets_policy() -> Value {
    let generate_expr = "generator.Apply(object.metadata.name, [\n\
         \x20 {\n\
         \x20   \"apiVersion\": dyn(\"v1\"),\n\
         \x20   \"kind\": dyn(\"Secret\"),\n\
         \x20   \"metadata\": dyn({\n\
         \x20     \"name\": \"keel-s3-credentials\",\n\
         \x20     \"labels\": {\n\
         \x20       \"app.kubernetes.io/managed-by\": \"keel\",\n\
         \x20       \"keel.io/secret-kind\": \"s3-credentials\"\n\
         \x20     }\n\
         \x20   }),\n\
         \x20   \"type\": dyn(variables.s3Secret.type),\n\
         \x20   \"data\": dyn(variables.s3Secret.data)\n\
         \x20 },\n\
         \x20 {\n\
         \x20   \"apiVersion\": dyn(\"v1\"),\n\
         \x20   \"kind\": dyn(\"Secret\"),\n\
         \x20   \"metadata\": dyn({\n\
         \x20     \"name\": \"git-credentials\",\n\
         \x20     \"labels\": {\n\
         \x20       \"app.kubernetes.io/managed-by\": \"keel\",\n\
         \x20       \"keel.io/secret-kind\": \"git-credentials\"\n\
         \x20     }\n\
         \x20   }),\n\
         \x20   \"type\": dyn(variables.gitSecret.type),\n\
         \x20   \"data\": dyn(variables.gitSecret.data)\n\
         \x20 }\n\
         ])";

    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "GeneratingPolicy",
        "metadata": {
            "name": POLICY_SYNC_TENANT_SECRETS,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/policy-tier": "tenant-secret-sync"
            }
        },
        "spec": {
            "evaluation": {
                "generateExisting": {
                    "enabled": true
                },
                "synchronize": {
                    "enabled": true
                }
            },
            "matchConstraints": {
                "resourceRules": [
                    {
                        "apiGroups": [""],
                        "apiVersions": ["v1"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["namespaces"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-namespace",
                    "expression": "object.metadata.name.startsWith('tenant-')"
                }
            ],
            "variables": [
                {
                    "name": "s3Secret",
                    "expression": format!("resource.Get('v1', 'secrets', '{FLUX_SYSTEM_NAMESPACE}', 'keel-s3-credentials')")
                },
                {
                    "name": "gitSecret",
                    "expression": format!("resource.Get('v1', 'secrets', '{FLUX_SYSTEM_NAMESPACE}', 'git-credentials')")
                }
            ],
            "generate": [
                {
                    "expression": generate_expr
                }
            ]
        }
    })
}

/// Generates scoped RBAC (`ClusterRole` + `ClusterRoleBinding`) granting Kyverno's
/// `background-controller` permission to clone and synchronize `keel-s3-credentials` and
/// `git-credentials` into `tenant-*` namespaces.
pub fn build_sync_tenant_secrets_rbac() -> [Value; 2] {
    let cluster_role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": "keel-kyverno-tenant-secret-sync",
            "labels": {
                "app.kubernetes.io/managed-by": "keel"
            }
        },
        "rules": [
            {
                "apiGroups": [""],
                "resources": ["secrets"],
                "verbs": ["list", "watch", "create"]
            },
            {
                "apiGroups": [""],
                "resources": ["secrets"],
                "resourceNames": ["keel-s3-credentials", "git-credentials"],
                "verbs": ["get", "update", "patch", "delete"]
            }
        ]
    });

    let cluster_role_binding = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": "keel-kyverno-tenant-secret-sync",
            "labels": {
                "app.kubernetes.io/managed-by": "keel"
            }
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "keel-kyverno-tenant-secret-sync"
        },
        "subjects": [
            {
                "kind": "ServiceAccount",
                "name": "kyverno-background-controller",
                "namespace": "kyverno"
            },
            {
                "kind": "ServiceAccount",
                "name": "kyverno-admission-controller",
                "namespace": "kyverno"
            }
        ]
    });

    [cluster_role, cluster_role_binding]
}

/// Generated bundle of baseline Kyverno `ClusterPolicy` manifests and optional local-dev `PolicyException`.
#[derive(Debug, Clone, PartialEq)]
pub struct KyvernoManifestBundle {
    pub cluster_policies: Vec<Value>,
    pub supporting_manifests: Vec<Value>,
    pub policy_exception: Option<Value>,
}

impl KyvernoManifestBundle {
    /// Generates baseline Kyverno `ClusterPolicy` manifests and includes `PolicyException`
    /// only when `config.local_dev_docker_socket_exception` is explicitly `true`.
    pub fn generate(config: &KyvernoPolicyConfig) -> Result<Self, KubeError> {
        let cluster_policies = vec![
            build_disallow_privileged_containers_policy(),
            build_protect_control_plane_policy(),
            build_restrict_pulumi_providers_policy(&config.allowed_providers)?,
        ];

        let [sync_role, sync_binding] = build_sync_tenant_secrets_rbac();
        let supporting_manifests =
            vec![sync_role, sync_binding, build_sync_tenant_secrets_policy()];

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
        assert_eq!(bundle.supporting_manifests.len(), 3);
        assert!(bundle.policy_exception.is_none());
        assert_eq!(bundle.all_manifests().len(), 6);

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
        assert_eq!(bundle.all_manifests().len(), 7);

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
    fn sync_tenant_secrets_policy_and_rbac_are_generated() {
        let policy = build_sync_tenant_secrets_policy();
        assert_eq!(policy["apiVersion"], "policies.kyverno.io/v1");
        assert_eq!(policy["kind"], "GeneratingPolicy");
        assert_eq!(policy["metadata"]["name"], POLICY_SYNC_TENANT_SECRETS);
        assert_eq!(
            policy["spec"]["evaluation"]["generateExisting"]["enabled"],
            true
        );
        assert_eq!(policy["spec"]["evaluation"]["synchronize"]["enabled"], true);

        let vars = policy["spec"]["variables"].as_array().unwrap();
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0]["name"], "s3Secret");
        assert!(
            vars[0]["expression"]
                .as_str()
                .unwrap()
                .contains("keel-s3-credentials")
        );
        assert_eq!(vars[1]["name"], "gitSecret");
        assert!(
            vars[1]["expression"]
                .as_str()
                .unwrap()
                .contains("git-credentials")
        );

        let generate = policy["spec"]["generate"].as_array().unwrap();
        assert_eq!(generate.len(), 1);
        let expr = generate[0]["expression"].as_str().unwrap();
        assert!(expr.contains("generator.Apply"));
        assert!(expr.contains("keel-s3-credentials"));
        assert!(expr.contains("git-credentials"));

        let [role, binding] = build_sync_tenant_secrets_rbac();
        assert_eq!(role["kind"], "ClusterRole");
        assert_eq!(role["metadata"]["name"], "keel-kyverno-tenant-secret-sync");
        assert_eq!(
            role["rules"][1]["resourceNames"],
            json!(["keel-s3-credentials", "git-credentials"])
        );
        assert_eq!(binding["kind"], "ClusterRoleBinding");
        assert_eq!(
            binding["subjects"][0]["name"],
            "kyverno-background-controller"
        );
        assert_eq!(
            binding["subjects"][1]["name"],
            "kyverno-admission-controller"
        );
        assert_eq!(binding["subjects"][0]["namespace"], "kyverno");
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
        assert_eq!(docs.len(), 7);
        for doc in docs {
            let parsed: serde_json::Value =
                serde_yml::from_str(doc).expect("each Kyverno multi-doc YAML part must parse");
            assert!(parsed.get("kind").is_some());
        }
    }
}
