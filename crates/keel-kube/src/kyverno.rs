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
use crate::tenant::{
    KEEL_SERVER_NAMESPACE, KEEL_SERVER_SERVICE_ACCOUNT, PLATFORM_OWNED_LABEL,
    TENANT_RECONCILER_SERVICE_ACCOUNT, validate_resource_name,
};

pub const POLICY_DISALLOW_PRIVILEGED: &str = "disallow-privileged-containers";
pub const POLICY_PROTECT_CONTROL_PLANE: &str = "protect-control-plane";
pub const POLICY_RESTRICT_PULUMI_PROVIDERS: &str = "restrict-pulumi-providers";
/// Retired `GeneratingPolicy` that cloned control-plane admin credentials into every tenant.
/// Kept only so bootstrap can delete it (and its RBAC) from existing clusters.
pub const POLICY_SYNC_TENANT_SECRETS: &str = "sync-tenant-secrets";
/// Retired RBAC (`ClusterRole` + `ClusterRoleBinding`) used by [`POLICY_SYNC_TENANT_SECRETS`].
pub const RETIRED_SYNC_TENANT_SECRETS_RBAC: &str = "keel-kyverno-tenant-secret-sync";
pub const POLICY_RESTRICT_TENANT_RECONCILER: &str = "restrict-tenant-reconciler";
pub const POLICY_RESTRICT_TENANT_ROUTE_HOSTNAMES: &str = "restrict-tenant-route-hostnames";
pub const POLICY_RESTRICT_TENANT_FLUX_SOURCES: &str = "restrict-tenant-flux-sources";
pub const POLICY_RESTRICT_TENANT_FLUX_SOURCE_STATUS: &str = "restrict-tenant-flux-source-status";
pub const POLICY_PROTECT_PLATFORM_OWNED_TENANT_OBJECTS: &str =
    "protect-platform-owned-tenant-objects";
pub const EXCEPTION_LOCAL_KIND_DOCKER_SOCKET: &str = "local-kind-docker-socket-exception";

/// Requesting usernames treated as tenant subjects (prefix match): `ServiceAccount`s of `tenant-*`
/// namespaces (tenant Flux/PKO identities, workspace pods) and the `keel-previews` preview identity.
const TENANT_SUBJECT_USER_PREFIXES: [&str; 2] = [
    "system:serviceaccount:tenant-",
    "system:serviceaccount:keel-previews:",
];
/// Requesting groups treated as tenant subjects (prefix match): OIDC tenant groups and the
/// `ServiceAccount` groups of `tenant-*` namespaces.
const TENANT_SUBJECT_GROUP_PREFIXES: [&str; 2] = ["tenant-", "system:serviceaccounts:tenant-"];
/// Requesting groups treated as tenant subjects (exact match).
const TENANT_SUBJECT_GROUPS: [&str; 1] = ["system:serviceaccounts:keel-previews"];

/// CEL: `true` when the request comes from a tenant subject (see the `TENANT_SUBJECT_*` lists),
/// given CEL expressions for the requesting `username` (string) and `groups` (list). Matches the
/// requesting user, never the target namespace, so Flux controllers writing tenant objects are
/// not affected. Mirrored by [`is_tenant_subject`].
fn cel_tenant_subject(username: &str, groups: &str) -> String {
    let users = TENANT_SUBJECT_USER_PREFIXES
        .iter()
        .map(|p| format!("{username}.startsWith('{p}')"));
    let group_terms = TENANT_SUBJECT_GROUP_PREFIXES
        .iter()
        .map(|p| format!("g.startsWith('{p}')"))
        .chain(TENANT_SUBJECT_GROUPS.iter().map(|g| format!("g == '{g}'")))
        .collect::<Vec<_>>()
        .join(" || ");
    users
        .chain(std::iter::once(format!(
            "{groups}.exists(g, {group_terms})"
        )))
        .collect::<Vec<_>>()
        .join(" || ")
}

/// Rust mirror of [`cel_tenant_subject`] (same lists), for callers and tests that need to know
/// whether a requester is a tenant subject without a CEL evaluator.
pub fn is_tenant_subject(username: &str, groups: &[&str]) -> bool {
    TENANT_SUBJECT_USER_PREFIXES
        .iter()
        .any(|p| username.starts_with(p))
        || groups.iter().any(|g| {
            TENANT_SUBJECT_GROUP_PREFIXES
                .iter()
                .any(|p| g.starts_with(p))
                || TENANT_SUBJECT_GROUPS.contains(g)
        })
}

/// CEL for the requesting username / groups in `matchConditions` (which cannot use `variables`).
const CEL_REQUEST_USERNAME: &str =
    "(has(request.userInfo.username) ? request.userInfo.username : '')";
const CEL_REQUEST_GROUPS: &str = "(has(request.userInfo.groups) ? request.userInfo.groups : [])";
/// CEL for the request namespace (`''` for cluster-scoped objects).
const CEL_REQUEST_NAMESPACE: &str = "(has(request.namespace) ? request.namespace : '')";

/// CEL: an `UPDATE` that leaves `spec` unchanged (finalizer removal, labels, reconcile
/// annotations) or touches an object that is already being deleted. Such updates must always pass
/// so controllers can finish deleting objects (and tenant namespaces never hang in `Terminating`).
const CEL_SPEC_PRESERVING_UPDATE: &str = "(request.operation == 'UPDATE' && (has(object.metadata.deletionTimestamp) || (has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {})))";

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
    /// Platform hostnames (IdP issuer, keel-server, Forgejo) tenant Gateway API routes may never
    /// claim, exactly or through a wildcard (`restrict-tenant-route-hostnames`).
    pub reserved_route_hostnames: Vec<String>,
}

/// Default reserved platform hostnames (`idp|keel|forgejo.keel.local`).
pub fn default_reserved_route_hostnames() -> Vec<String> {
    keel_core::platform_reserved_hostnames(std::iter::empty())
        .into_iter()
        .collect()
}

impl Default for KyvernoPolicyConfig {
    fn default() -> Self {
        Self {
            allowed_providers: default_allowed_pulumi_providers(),
            local_dev_docker_socket_exception: false,
            docker_socket_exception: DockerSocketExceptionConfig::default(),
            reserved_route_hostnames: default_reserved_route_hostnames(),
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
                },
                {
                    // The keel-server SA may patch control-plane Flux objects / Stacks only to
                    // request reconciliation: a spec change (e.g. `spec.patches`, `spec.path` or a
                    // GitRepository URL) would be applied by the cluster-admin
                    // `kustomize-controller` (privilege escalation, NFR-SEC-04).
                    "expression": format!(
                        "!(request.operation == 'UPDATE' && ((has(request.namespace) ? request.namespace : '') == '{FLUX_SYSTEM_NAMESPACE}' || (has(request.namespace) ? request.namespace : '') == '{KEEL_SYSTEM_NAMESPACE}') && has(request.userInfo) && has(request.userInfo.username) && request.userInfo.username == 'system:serviceaccount:{KEEL_SYSTEM_NAMESPACE}:keel-server' && ((request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization') || (request.kind.group == 'helm.toolkit.fluxcd.io' && request.kind.kind == 'HelmRelease') || request.kind.group == 'source.toolkit.fluxcd.io' || (request.kind.group == 'pulumi.com' && request.kind.kind == 'Stack')) && !((has(object.spec) ? object.spec : {{}}) == (has(oldObject.spec) ? oldObject.spec : {{}}) && (has(object.metadata.labels) ? object.metadata.labels : {{}}) == (has(oldObject.metadata.labels) ? oldObject.metadata.labels : {{}}) && (has(object.metadata.finalizers) ? object.metadata.finalizers : []) == (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : []) && (has(object.metadata.annotations) ? object.metadata.annotations : {{}}).all(k, k == 'reconcile.fluxcd.io/requestedAt' || k == 'pulumi.com/reconcile-request' || (has(oldObject.metadata.annotations) && k in oldObject.metadata.annotations && oldObject.metadata.annotations[k] == object.metadata.annotations[k])) && (has(oldObject.metadata.annotations) ? oldObject.metadata.annotations : {{}}).all(k, k == 'reconcile.fluxcd.io/requestedAt' || k == 'pulumi.com/reconcile-request' || (has(object.metadata.annotations) && k in object.metadata.annotations))))"
                    ),
                    "message": "keel-server may only request reconciliation (`reconcile.fluxcd.io/requestedAt` / `pulumi.com/reconcile-request` annotations) on control-plane Flux objects and Stacks; spec, label, finalizer and other annotation changes must go through a GitOps PR."
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

/// Generates `ValidatingPolicy/restrict-tenant-flux-sources` (`policies.kyverno.io/v1`).
///
/// Closes the Flux source paths that would let a tenant republish or substitute repository
/// content it does not own (`NFR-SEC-02`). Fails closed (`failurePolicy: Fail`). The
/// `matchConditions` limit the webhook to writes in `tenant-*` namespaces, so every platform
/// namespace write (`flux-system` included) never reaches Kyverno (anti-lockout, `NFR-REL-01`).
/// Status writes are handled by [`build_restrict_tenant_flux_source_status_policy`].
///
/// * `ExternalArtifact` (`source.toolkit.fluxcd.io`) and every `source.extensions.fluxcd.io` kind
///   (`ArtifactGenerator`) are denied in `tenant-*` namespaces for every requester: they would
///   republish another source's artifact (the whole GitOps repository) into the tenant namespace.
///   Updates that leave `spec` unchanged and objects being deleted pass (finalizer removal), and
///   deletions are never matched.
/// * Flux `Kustomization`s in `tenant-<id>` must impersonate `tenant-<id>-sa`
///   (`spec.serviceAccountName`). Updates that leave `spec` unchanged (finalizer removal, reconcile
///   annotations) and objects being deleted are always allowed.
pub fn build_restrict_tenant_flux_sources_policy() -> Value {
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_TENANT_FLUX_SOURCES,
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
                        "apiGroups": ["source.toolkit.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["externalartifacts"]
                    },
                    {
                        "apiGroups": ["source.extensions.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["*"]
                    },
                    {
                        "apiGroups": ["kustomize.toolkit.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["kustomizations"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-namespace",
                    "expression": format!("{CEL_REQUEST_NAMESPACE}.startsWith('tenant-')")
                }
            ],
            "variables": [
                {
                    "name": "ns",
                    "expression": CEL_REQUEST_NAMESPACE
                }
            ],
            "validations": [
                {
                    "expression": format!("!((request.kind.group == 'source.toolkit.fluxcd.io' && request.kind.kind == 'ExternalArtifact') || request.kind.group == 'source.extensions.fluxcd.io') || {CEL_SPEC_PRESERVING_UPDATE}"),
                    "message": "ExternalArtifact and ArtifactGenerator objects are not allowed in tenant namespaces: they would republish another source's artifact (whole-repository read). Tenants only use their own GitRepository `keel-gitops`."
                },
                {
                    "expression": format!("!(request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization') || (has(object.spec) && has(object.spec.serviceAccountName) && object.spec.serviceAccountName == variables.ns + '-sa') || {CEL_SPEC_PRESERVING_UPDATE}"),
                    "message": "Flux Kustomizations in a tenant namespace `tenant-<id>` must set `spec.serviceAccountName: tenant-<id>-sa` (multi-tenancy lockdown, NFR-SEC-02)."
                }
            ]
        }
    })
}

/// Generates `ValidatingPolicy/restrict-tenant-flux-source-status` (`policies.kyverno.io/v1`).
///
/// Denies tenant subjects (`tenant-*` ServiceAccounts and groups, the `keel-previews` identity;
/// see [`is_tenant_subject`]) every write to `*/status` of `source.toolkit.fluxcd.io` /
/// `source.extensions.fluxcd.io` objects, in any namespace: a forged `status.artifact` would make
/// kustomize-controller apply another source's artifact (`NFR-SEC-02`). Fails closed.
///
/// The tenant-subject test sits inline in `matchConditions` (which cannot use `variables`), so
/// the API server only sends tenant requests to Kyverno: `source-controller`'s own status writes
/// never reach the webhook and an unavailable Kyverno cannot stall Flux artifact production
/// (anti-lockout, `NFR-REL-01`). Every request that reaches the policy is denied.
pub fn build_restrict_tenant_flux_source_status_policy() -> Value {
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_TENANT_FLUX_SOURCE_STATUS,
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
                        "apiGroups": ["source.toolkit.fluxcd.io", "source.extensions.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["UPDATE"],
                        "resources": ["*/status"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-subject",
                    "expression": cel_tenant_subject(CEL_REQUEST_USERNAME, CEL_REQUEST_GROUPS)
                }
            ],
            "validations": [
                {
                    "expression": "(has(request.subResource) ? request.subResource : '') != 'status'",
                    "message": "Tenant identities may not write the status of Flux source objects (a forged `status.artifact` would make Flux apply another source's content); only source-controller writes source status."
                }
            ]
        }
    })
}

/// An identity allowed to change `keel.io/platform-owned` objects
/// ([`build_protect_platform_owned_tenant_objects_policy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformOwnedWriter {
    /// Requesting username (`request.userInfo.username`).
    pub username: String,
    /// `Some(kind)`: allowed only for objects of this kind (the identity's own finalizer and
    /// metadata writes); `None`: allowed for every platform-owned kind.
    pub kind: Option<&'static str>,
    /// Why the identity needs write access (audit trail; asserted by tests).
    pub reason: &'static str,
}

/// Identities allowed to change `keel.io/platform-owned` objects, besides members of
/// `system:masters` / `kubeadm:cluster-admins` (break-glass cluster admins) and the built-in
/// `kube-system` controllers (garbage collection, namespace deletion).
///
/// `kustomize-controller` is deliberately absent: tenant baselines are applied by `keel-tenants`
/// impersonating `keel-tenant-reconciler`, no platform-owned `Kustomization` exists, and the
/// Flux-managed keel-server add-on does not ship the `tenant-default/keel-server` RoleBinding. As
/// a writer it would let every `flux-system` Kustomization (core add-ons, rauthy, keel-server, ...)
/// rewrite or delete any tenant's baseline.
pub fn platform_owned_writers() -> Vec<PlatformOwnedWriter> {
    vec![
        PlatformOwnedWriter {
            username: format!(
                "system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:{TENANT_RECONCILER_SERVICE_ACCOUNT}"
            ),
            kind: None,
            reason: "`keel-tenants` applies and prunes `tenants/<id>/` (tenant baseline, `keel-gitops`) as this identity",
        },
        PlatformOwnedWriter {
            username: format!(
                "system:serviceaccount:{KEEL_SERVER_NAMESPACE}:{KEEL_SERVER_SERVICE_ACCOUNT}"
            ),
            kind: None,
            reason: "keel-server provisions tenant baselines (tenant onboarding API)",
        },
        PlatformOwnedWriter {
            username: format!("system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:source-controller"),
            kind: Some("GitRepository"),
            reason: "source-controller adds and removes its finalizer on the platform-owned `keel-gitops` GitRepository",
        },
        PlatformOwnedWriter {
            username: "system:kube-controller-manager".to_string(),
            kind: None,
            reason: "garbage collection and namespace deletion",
        },
    ]
}

/// CEL admitting an UPDATE of a platform-owned `GitRepository` (the tenant `keel-gitops` source)
/// that only sets or changes the `reconcile.fluxcd.io/requestedAt` annotation, i.e. `flux
/// reconcile source git keel-gitops -n tenant-<id>`. Spec, labels, finalizers, owner references
/// and every other annotation must be unchanged, so the exemption cannot repoint, unlabel, orphan
/// or re-parent (garbage-collection) the source.
pub const CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY: &str = "request.operation == 'UPDATE' && request.kind.group == 'source.toolkit.fluxcd.io' && request.kind.kind == 'GitRepository' && object != null && oldObject != null && (has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {}) && (has(object.metadata.labels) ? object.metadata.labels : {}) == (has(oldObject.metadata.labels) ? oldObject.metadata.labels : {}) && (has(object.metadata.finalizers) ? object.metadata.finalizers : []) == (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : []) && (has(object.metadata.ownerReferences) ? object.metadata.ownerReferences : []) == (has(oldObject.metadata.ownerReferences) ? oldObject.metadata.ownerReferences : []) && (has(object.metadata.annotations) ? object.metadata.annotations : {}).all(k, k == 'reconcile.fluxcd.io/requestedAt' || (has(oldObject.metadata.annotations) && k in oldObject.metadata.annotations && oldObject.metadata.annotations[k] == object.metadata.annotations[k])) && (has(oldObject.metadata.annotations) ? oldObject.metadata.annotations : {}).all(k, k == 'reconcile.fluxcd.io/requestedAt' || (has(object.metadata.annotations) && k in object.metadata.annotations))";

/// Generates `ValidatingPolicy/protect-platform-owned-tenant-objects` (`policies.kyverno.io/v1`).
///
/// Objects the platform writes for a tenant (namespace, ServiceAccount, quota, LimitRange,
/// NetworkPolicies, RoleBindings, the `keel-gitops` GitRepository stub) carry
/// `keel.io/platform-owned: "true"`. Creating, changing (including removing the label) or deleting
/// such an object is reserved for `keel-tenant-reconciler`, `keel-server`, `source-controller` (its
/// own `GitRepository` finalizer only), the `kube-system` controllers and break-glass cluster
/// admins ([`platform_owned_writers`]); tenant identities (which hold `admin` in their namespace)
/// and every other `flux-system` identity are denied. The one exemption is a reconcile request on
/// the `keel-gitops` GitRepository ([`CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY`]), so `flux
/// reconcile source git` keeps working for anyone RBAC lets patch it. Fails closed.
pub fn build_protect_platform_owned_tenant_objects_policy() -> Value {
    let all_writers = platform_owned_writers();
    let writers = all_writers
        .iter()
        .filter(|w| w.kind.is_none())
        .map(|w| format!("'{}'", w.username))
        .collect::<Vec<_>>()
        .join(", ");
    let scoped_writers: String = all_writers
        .iter()
        .filter_map(|w| {
            w.kind.map(|kind| {
                format!(
                    " || (variables.username == '{}' && request.kind.kind == '{kind}')",
                    w.username
                )
            })
        })
        .collect();
    let labelled = |obj: &str| {
        format!(
            "({obj} != null && has({obj}.metadata) && has({obj}.metadata.labels) && '{PLATFORM_OWNED_LABEL}' in {obj}.metadata.labels)"
        )
    };
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_PROTECT_PLATFORM_OWNED_TENANT_OBJECTS,
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
                        "apiGroups": [""],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["namespaces", "serviceaccounts", "resourcequotas", "limitranges"]
                    },
                    {
                        "apiGroups": ["networking.k8s.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["networkpolicies"]
                    },
                    {
                        "apiGroups": ["rbac.authorization.k8s.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["rolebindings", "clusterrolebindings"]
                    },
                    {
                        "apiGroups": ["source.toolkit.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["gitrepositories"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "platform-owned",
                    "expression": format!("{} || {}", labelled("object"), labelled("oldObject"))
                }
            ],
            "variables": [
                {
                    "name": "username",
                    "expression": "has(request.userInfo.username) ? request.userInfo.username : ''"
                },
                {
                    "name": "groups",
                    "expression": "has(request.userInfo.groups) ? request.userInfo.groups : []"
                }
            ],
            "validations": [
                {
                    "expression": format!(
                        "variables.username in [{writers}]{scoped_writers} || variables.username.startsWith('system:serviceaccount:kube-system:') || variables.groups.exists(g, g == 'system:masters' || g == 'kubeadm:cluster-admins') || ({CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY})"
                    ),
                    "message": format!(
                        "Objects labelled `{PLATFORM_OWNED_LABEL}` (tenant namespace, RBAC, NetworkPolicies, the `keel-gitops` source) are managed by the platform (keel-tenant-reconciler / keel-server); change them through a GitOps PR to `tenants/<id>/` (only a `reconcile.fluxcd.io/requestedAt` reconcile request on `keel-gitops` is exempt)."
                    )
                }
            ]
        }
    })
}

/// Generates `ValidatingPolicy/restrict-tenant-route-hostnames` (`policies.kyverno.io/v1`).
///
/// Tenant `HTTPRoute`/`GRPCRoute`/`TLSRoute`s attach to the shared `keel-system/keel-ingress`
/// listeners (`allowedRoutes.namespaces.from: All`). Gateway API breaks exact-hostname ties by
/// creation time, so a tenant route on a platform host (e.g. the IdP issuer) could take over that
/// host when the platform route is re-created. This policy denies, in `tenant-*` namespaces, any
/// route hostname equal to a reserved platform host or a wildcard (`*.<suffix>`) covering one.
/// The deny list is precomputed (no CEL string extensions needed). Fails closed.
pub fn build_restrict_tenant_route_hostnames_policy<S: AsRef<str>>(
    reserved_hostnames: &[S],
) -> Result<Value, KubeError> {
    let mut denied = std::collections::BTreeSet::new();
    for raw in reserved_hostnames {
        let host = raw.as_ref().trim().to_ascii_lowercase();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            });
        if !valid {
            return Err(KubeError::InvalidInput(format!(
                "invalid reserved route hostname `{host}`"
            )));
        }
        let mut rest = host.as_str();
        while let Some((_, suffix)) = rest.split_once('.') {
            denied.insert(format!("*.{suffix}"));
            rest = suffix;
        }
        denied.insert(host);
    }
    let denied_list = denied
        .iter()
        .map(|h| format!("'{h}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_TENANT_ROUTE_HOSTNAMES,
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
                        "apiGroups": ["gateway.networking.k8s.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["httproutes", "grpcroutes", "tlsroutes"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-namespace",
                    "expression": "(has(request.namespace) ? request.namespace : '').startsWith('tenant-')"
                }
            ],
            "variables": [
                {
                    "name": "deniedHostnames",
                    "expression": format!("[{denied_list}]")
                },
                {
                    "name": "hostnames",
                    "expression": "has(object.spec.hostnames) ? object.spec.hostnames : []"
                }
            ],
            "validations": [
                {
                    "expression": "variables.hostnames.all(h, !(h in variables.deniedHostnames))",
                    "message": "Tenant routes may not claim reserved platform hostnames (IdP, keel-server, Forgejo) or wildcards covering them."
                }
            ]
        }
    }))
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

        let supporting_manifests = vec![
            build_restrict_tenant_reconciler_policy(),
            build_restrict_tenant_route_hostnames_policy(&config.reserved_route_hostnames)?,
            build_restrict_tenant_flux_sources_policy(),
            build_restrict_tenant_flux_source_status_policy(),
            build_protect_platform_owned_tenant_objects_policy(),
        ];

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
        assert_eq!(bundle.supporting_manifests.len(), 5);
        assert!(bundle.policy_exception.is_none());
        assert_eq!(bundle.all_manifests().len(), 8);
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
        assert_eq!(bundle.all_manifests().len(), 9);

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
            build_restrict_tenant_route_hostnames_policy(&default_reserved_route_hostnames())
                .unwrap(),
            build_restrict_tenant_flux_sources_policy(),
            build_restrict_tenant_flux_source_status_policy(),
            build_protect_platform_owned_tenant_objects_policy(),
        ];
        for seed in [
            include_str!("../../../deploy/seed/kyverno-policies/restrict-tenant-flux-sources.yaml"),
            include_str!(
                "../../../deploy/seed/kyverno-policies/restrict-tenant-flux-source-status.yaml"
            ),
            include_str!(
                "../../../deploy/seed/kyverno-policies/protect-platform-owned-tenant-objects.yaml"
            ),
            include_str!(
                "../../../deploy/seed/kyverno-policies/restrict-tenant-route-hostnames.yaml"
            ),
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
        // The keel-server SA may only request reconciliation on control-plane Flux objects.
        let server_guard = generated["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["expression"].as_str().unwrap())
            .find(|e| e.contains("system:serviceaccount:keel-system:keel-server"))
            .expect("keel-server Flux object patch guard");
        for needle in [
            "request.operation == 'UPDATE'",
            "== 'flux-system'",
            "== 'keel-system'",
            "request.kind.kind == 'Kustomization'",
            "request.kind.kind == 'HelmRelease'",
            "request.kind.group == 'source.toolkit.fluxcd.io'",
            "request.kind.kind == 'Stack'",
            "(has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {})",
            "object.metadata.labels",
            "object.metadata.finalizers",
            "k == 'reconcile.fluxcd.io/requestedAt'",
            "k == 'pulumi.com/reconcile-request'",
        ] {
            assert!(server_guard.contains(needle), "{needle}");
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

    fn expressions(policy: &Value) -> Vec<String> {
        policy["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["expression"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn restrict_tenant_flux_sources_policy_matches_seed_and_closes_artifact_paths() {
        let policy = build_restrict_tenant_flux_sources_policy();
        let seed: Value = serde_yml::from_str(include_str!(
            "../../../deploy/seed/kyverno-policies/restrict-tenant-flux-sources.yaml"
        ))
        .unwrap();
        assert_eq!(seed["spec"], policy["spec"]);
        assert_eq!(seed["metadata"], policy["metadata"]);
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");
        assert_eq!(policy["spec"]["validationActions"], json!(["Deny"]));

        let rules = policy["spec"]["matchConstraints"]["resourceRules"]
            .as_array()
            .unwrap();
        let covers = |group: &str, resource: &str, op: &str| {
            rules.iter().any(|r| {
                r["apiGroups"].as_array().unwrap().contains(&json!(group))
                    && r["resources"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(resource))
                    && r["operations"].as_array().unwrap().contains(&json!(op))
            })
        };
        assert!(covers(
            "source.toolkit.fluxcd.io",
            "externalartifacts",
            "CREATE"
        ));
        assert!(covers("source.extensions.fluxcd.io", "*", "CREATE"));
        assert!(covers(
            "kustomize.toolkit.fluxcd.io",
            "kustomizations",
            "CREATE"
        ));
        // Status writes live in their own policy (requester-filtered matchConditions).
        assert!(rules.iter().all(|r| {
            r["resources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|res| !res.as_str().unwrap().contains('/'))
        }));
        // Deletion of tenant Flux objects is never blocked (cleanup must always work).
        assert!(rules.iter().all(|r| {
            !r["operations"]
                .as_array()
                .unwrap()
                .contains(&json!("DELETE"))
        }));
        // Only writes in tenant namespaces reach Kyverno (flux-system never does).
        let conds = policy["spec"]["matchConditions"].as_array().unwrap();
        assert_eq!(conds.len(), 1);
        assert_eq!(
            conds[0]["expression"],
            "(has(request.namespace) ? request.namespace : '').startsWith('tenant-')"
        );

        let exprs = expressions(&policy);
        assert_eq!(exprs.len(), 2);
        // 1: artifact republishing kinds denied for every requester in tenant namespaces.
        assert!(exprs[0].contains("request.kind.kind == 'ExternalArtifact'"));
        assert!(exprs[0].contains("request.kind.group == 'source.extensions.fluxcd.io'"));
        assert!(!exprs[0].contains("userInfo"));
        // Finalizer removal / deletion of pre-existing objects is never blocked.
        for expr in &exprs {
            assert!(expr.ends_with(&format!(" || {CEL_SPEC_PRESERVING_UPDATE}")));
        }
        assert!(CEL_SPEC_PRESERVING_UPDATE.contains("has(object.metadata.deletionTimestamp)"));
        assert!(CEL_SPEC_PRESERVING_UPDATE.contains(
            "(has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {})"
        ));
        // 2: tenant Kustomizations impersonate `tenant-<id>-sa`.
        assert!(exprs[1].contains("object.spec.serviceAccountName == variables.ns + '-sa'"));
    }

    #[test]
    fn restrict_tenant_flux_source_status_policy_matches_seed_and_filters_on_tenant_subjects() {
        let policy = build_restrict_tenant_flux_source_status_policy();
        let seed: Value = serde_yml::from_str(include_str!(
            "../../../deploy/seed/kyverno-policies/restrict-tenant-flux-source-status.yaml"
        ))
        .unwrap();
        assert_eq!(seed["spec"], policy["spec"]);
        assert_eq!(seed["metadata"], policy["metadata"]);
        assert_eq!(
            policy["metadata"]["name"],
            POLICY_RESTRICT_TENANT_FLUX_SOURCE_STATUS
        );
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");
        assert_eq!(
            policy["spec"]["matchConstraints"]["resourceRules"],
            json!([{
                "apiGroups": ["source.toolkit.fluxcd.io", "source.extensions.fluxcd.io"],
                "apiVersions": ["*"],
                "operations": ["UPDATE"],
                "resources": ["*/status"]
            }])
        );
        // The tenant-subject test is inline (matchConditions cannot use variables), keyed on the
        // requesting user only: source-controller status writes never reach the webhook.
        let conds = policy["spec"]["matchConditions"].as_array().unwrap();
        assert_eq!(conds.len(), 1);
        let subject = conds[0]["expression"].as_str().unwrap();
        assert_eq!(
            subject,
            cel_tenant_subject(CEL_REQUEST_USERNAME, CEL_REQUEST_GROUPS)
        );
        assert!(!subject.contains("variables."));
        assert!(!subject.contains("request.namespace"));
        assert!(!subject.contains("flux-system"));
        for needle in [
            "'system:serviceaccount:tenant-'",
            "'system:serviceaccount:keel-previews:'",
            "g.startsWith('tenant-')",
        ] {
            assert!(subject.contains(needle), "{needle}");
        }
        assert!(policy["spec"].get("variables").is_none());
        let exprs = expressions(&policy);
        assert_eq!(
            exprs,
            vec!["(has(request.subResource) ? request.subResource : '') != 'status'".to_string()]
        );
        assert!(
            policy["spec"]["validations"][0]["message"]
                .as_str()
                .unwrap()
                .contains("may not write the status of Flux source objects")
        );
    }

    /// Every tenant Kustomization Keel synthesizes satisfies the `serviceAccountName` rule.
    #[test]
    fn synthesized_tenant_kustomizations_use_the_tenant_service_account() {
        let input = crate::FluxKustomizationSpecInput::new("acme", "app", "keel-gitops", "./x");
        let ks = crate::synthesize_flux_kustomization(&input).unwrap();
        let ns = ks["metadata"]["namespace"].as_str().unwrap();
        assert_eq!(ks["spec"]["serviceAccountName"], format!("{ns}-sa"));
    }

    #[test]
    fn protect_platform_owned_policy_matches_seed_and_allows_only_platform_writers() {
        let policy = build_protect_platform_owned_tenant_objects_policy();
        let seed: Value = serde_yml::from_str(include_str!(
            "../../../deploy/seed/kyverno-policies/protect-platform-owned-tenant-objects.yaml"
        ))
        .unwrap();
        assert_eq!(seed["spec"], policy["spec"]);
        assert_eq!(seed["metadata"], policy["metadata"]);
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");

        // Matches by label on the new or the old object, so dropping the label is also guarded,
        // and null-guards both (`object` is null on DELETE, `oldObject` on CREATE).
        let cond = policy["spec"]["matchConditions"][0]["expression"]
            .as_str()
            .unwrap();
        for needle in [
            "object != null && has(object.metadata) && has(object.metadata.labels) && 'keel.io/platform-owned' in object.metadata.labels",
            "oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.labels) && 'keel.io/platform-owned' in oldObject.metadata.labels",
        ] {
            assert!(cond.contains(needle), "{needle}");
        }

        // Every kind the platform labels is matched (and nothing cluster-wide like `*`).
        let bundle =
            crate::TenantManifestBundle::synthesize(&crate::TenantIsolationConfig::new("acme"))
                .unwrap();
        let rules = policy["spec"]["matchConstraints"]["resourceRules"]
            .as_array()
            .unwrap();
        let plural = |kind: &str| match kind {
            "Namespace" => "namespaces",
            "ServiceAccount" => "serviceaccounts",
            "ResourceQuota" => "resourcequotas",
            "LimitRange" => "limitranges",
            "NetworkPolicy" => "networkpolicies",
            "RoleBinding" => "rolebindings",
            "ClusterRoleBinding" => "clusterrolebindings",
            other => panic!("unexpected platform-owned kind {other}"),
        };
        for doc in bundle.to_json_values().unwrap() {
            assert_eq!(doc["metadata"]["labels"][PLATFORM_OWNED_LABEL], "true");
            let resource = plural(doc["kind"].as_str().unwrap());
            assert!(
                rules.iter().any(|r| r["resources"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(resource))),
                "{resource} not matched"
            );
        }
        assert!(
            rules
                .iter()
                .any(|r| r["resources"] == json!(["gitrepositories"]))
        );
        assert!(
            rules
                .iter()
                .all(|r| !r["resources"].as_array().unwrap().contains(&json!("*")))
        );

        // No platform-owned Kustomization exists: kustomize-controller finalizers on tenant
        // Kustomizations must never reach this policy.
        assert!(rules.iter().all(|r| {
            !r["resources"]
                .as_array()
                .unwrap()
                .contains(&json!("kustomizations"))
        }));

        let exprs = expressions(&policy);
        assert_eq!(exprs.len(), 1);
        assert!(exprs[0].contains(
            "variables.username in ['system:serviceaccount:flux-system:keel-tenant-reconciler', 'system:serviceaccount:keel-system:keel-server', 'system:kube-controller-manager']"
        ));
        // source-controller only for its own GitRepository finalizer.
        assert!(exprs[0].contains(
            "(variables.username == 'system:serviceaccount:flux-system:source-controller' && request.kind.kind == 'GitRepository')"
        ));
        assert!(exprs[0].contains("g == 'system:masters'"));
        // kustomize-controller would let every `flux-system` Kustomization rewrite tenant
        // baselines; it is never a writer.
        assert!(!exprs[0].contains("kustomize-controller"));
        // No tenant or preview identity is ever a platform writer.
        assert!(!exprs[0].contains("tenant-'"));
        assert!(!exprs[0].contains("keel-previews"));
        for writer in platform_owned_writers() {
            assert!(
                exprs[0].contains(&format!("'{}'", writer.username)),
                "{writer:?}"
            );
            assert!(!is_tenant_subject(&writer.username, &[]), "{writer:?}");
        }

        // `flux reconcile source git keel-gitops` stays possible for anyone RBAC allows: only an
        // UPDATE of a GitRepository whose spec, labels, finalizers, owner references and other
        // annotations are unchanged, differing at most in `reconcile.fluxcd.io/requestedAt`.
        assert!(exprs[0].ends_with(&format!(" || ({CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY})")));
        let cel = CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY;
        for needle in [
            "request.operation == 'UPDATE'",
            "request.kind.group == 'source.toolkit.fluxcd.io' && request.kind.kind == 'GitRepository'",
            "object != null && oldObject != null",
            "(has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {})",
            "(has(object.metadata.labels) ? object.metadata.labels : {}) == (has(oldObject.metadata.labels) ? oldObject.metadata.labels : {})",
            "(has(object.metadata.finalizers) ? object.metadata.finalizers : []) == (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : [])",
            "(has(object.metadata.ownerReferences) ? object.metadata.ownerReferences : []) == (has(oldObject.metadata.ownerReferences) ? oldObject.metadata.ownerReferences : [])",
            "k == 'reconcile.fluxcd.io/requestedAt' || (has(oldObject.metadata.annotations) && k in oldObject.metadata.annotations && oldObject.metadata.annotations[k] == object.metadata.annotations[k])",
            "k == 'reconcile.fluxcd.io/requestedAt' || (has(object.metadata.annotations) && k in object.metadata.annotations)",
        ] {
            assert!(cel.contains(needle), "{needle}");
        }
        // Only the Flux reconcile annotation, never PKO's or a wildcard.
        assert_eq!(cel.matches("k == '").count(), 2);
        assert!(!cel.contains("pulumi.com/reconcile-request"));
        assert!(!cel.contains("CREATE") && !cel.contains("DELETE"));
    }

    /// The exact writer allowlist, each with the reason it needs write access.
    #[test]
    fn platform_owned_writers_are_exactly_the_platform_identities() {
        let writers: Vec<(String, Option<&str>)> = platform_owned_writers()
            .into_iter()
            .map(|w| {
                assert!(!w.reason.trim().is_empty(), "{} has no reason", w.username);
                (w.username, w.kind)
            })
            .collect();
        assert_eq!(
            writers,
            vec![
                (
                    "system:serviceaccount:flux-system:keel-tenant-reconciler".to_string(),
                    None
                ),
                (
                    "system:serviceaccount:keel-system:keel-server".to_string(),
                    None
                ),
                (
                    "system:serviceaccount:flux-system:source-controller".to_string(),
                    Some("GitRepository")
                ),
                ("system:kube-controller-manager".to_string(), None),
            ]
        );
    }

    /// Requests that reach `restrict-tenant-flux-source-status` (its `matchConditions` are exactly
    /// `cel_tenant_subject`, mirrored by [`is_tenant_subject`]; pinned by
    /// [`restrict_tenant_flux_source_status_policy_matches_seed_and_filters_on_tenant_subjects`]).
    #[test]
    fn flux_source_status_policy_only_sees_tenant_requesters() {
        let sc = "system:serviceaccount:flux-system:source-controller";
        let sc_groups = [
            "system:serviceaccounts",
            "system:serviceaccounts:flux-system",
            "system:authenticated",
        ];
        // Flux controllers (and admins) never reach Kyverno for status writes.
        assert!(!is_tenant_subject(sc, &sc_groups));
        assert!(!is_tenant_subject(
            "system:serviceaccount:flux-system:kustomize-controller",
            &sc_groups
        ));
        assert!(!is_tenant_subject(
            "kubernetes-admin",
            &["kubeadm:cluster-admins"]
        ));
        // Tenant service accounts, tenant OIDC groups and the preview identity do (any namespace).
        assert!(is_tenant_subject(
            "system:serviceaccount:tenant-default:tenant-default-sa",
            &[]
        ));
        assert!(is_tenant_subject(
            "system:serviceaccount:keel-previews:keel-preview-readonly",
            &[]
        ));
        assert!(is_tenant_subject("alice", &["tenant-acme"]));
    }

    #[test]
    fn tenant_subject_cel_and_rust_mirror_agree_on_the_lists() {
        assert_eq!(
            cel_tenant_subject("variables.username", "variables.groups"),
            "variables.username.startsWith('system:serviceaccount:tenant-') || variables.username.startsWith('system:serviceaccount:keel-previews:') || variables.groups.exists(g, g.startsWith('tenant-') || g.startsWith('system:serviceaccounts:tenant-') || g == 'system:serviceaccounts:keel-previews')"
        );
        assert!(is_tenant_subject(
            "system:serviceaccount:tenant-acme:tenant-acme-sa",
            &[]
        ));
        assert!(is_tenant_subject(
            "system:serviceaccount:keel-previews:keel-preview-readonly",
            &[]
        ));
        assert!(is_tenant_subject("bob", &["tenant-acme"]));
        assert!(is_tenant_subject(
            "x",
            &["system:serviceaccounts:tenant-acme"]
        ));
        assert!(is_tenant_subject(
            "x",
            &["system:serviceaccounts:keel-previews"]
        ));
        assert!(!is_tenant_subject(
            "system:serviceaccount:flux-system:source-controller",
            &["system:serviceaccounts:flux-system"]
        ));
        assert!(!is_tenant_subject(
            "system:serviceaccount:keel-system:keel-server",
            &[]
        ));
        assert!(!is_tenant_subject("admin", &["system:masters"]));
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
        assert_eq!(docs.len(), 9);
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

    #[test]
    fn restrict_tenant_route_hostnames_policy_denies_platform_hosts_and_matches_seed() {
        let policy = build_restrict_tenant_route_hostnames_policy(&[
            "idp.corp.example",
            "keel.corp.example",
        ])
        .unwrap();
        let denied = policy["spec"]["variables"][0]["expression"]
            .as_str()
            .unwrap();
        for host in [
            "'idp.corp.example'",
            "'keel.corp.example'",
            "'*.corp.example'",
            "'*.example'",
        ] {
            assert!(denied.contains(host), "{host} in {denied}");
        }
        assert!(!denied.contains("'corp.example'"));
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");
        assert_eq!(
            policy["spec"]["matchConstraints"]["resourceRules"][0]["resources"],
            json!(["httproutes", "grpcroutes", "tlsroutes"])
        );
        // CEL-injection guard: only DNS hostnames are accepted.
        assert!(build_restrict_tenant_route_hostnames_policy(&["x' || true || '"]).is_err());
        assert!(build_restrict_tenant_route_hostnames_policy(&["Bad_Host"]).is_err());

        let seed: Value = serde_yml::from_str(include_str!(
            "../../../deploy/seed/kyverno-policies/restrict-tenant-route-hostnames.yaml"
        ))
        .unwrap();
        let generated =
            build_restrict_tenant_route_hostnames_policy(&default_reserved_route_hostnames())
                .unwrap();
        assert_eq!(seed["spec"], generated["spec"]);
        assert_eq!(seed["metadata"], generated["metadata"]);
    }
}
