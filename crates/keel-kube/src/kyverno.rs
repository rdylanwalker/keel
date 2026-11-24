//! Kyverno CEL `ValidatingPolicy` (`policies.kyverno.io/v1`) & explicit local-dev `PolicyException` synthesis (`SDD.md` §6.2, `NFR-SEC-03`, `NFR-SEC-04`).
//!
//! Generates the three baseline admission policies (`disallow-privileged-containers`,
//! `protect-control-plane`, `restrict-pulumi-providers`) in `Enforce` mode while keeping
//! production defaults strictly `restricted`. When `local_dev_docker_socket_exception` is
//! explicitly enabled via CLI/init config, generates a scoped `policies.kyverno.io/v1` `PolicyException`
//! (`local-kind-docker-socket-exception`) allowing PKO workspace pods in `keel-previews` / `tenant-*`
//! to access `/var/run/docker.sock` or `DOCKER_HOST` for `kind`-in-`kind` provisioning.

use std::collections::BTreeMap;

use keel_core::{
    FLUX_SYSTEM_NAMESPACE, KEEL_CA_BUNDLE_CONFIGMAP, KEEL_CA_BUNDLE_KEY, KEEL_PREVIEWS_NAMESPACE,
    KEEL_SYSTEM_NAMESPACE,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::KubeError;
use crate::tenant::{
    FLAGGER_CANARY_PROVIDER, KEEL_INGRESS_GATEWAY_NAME, KEEL_SERVER_NAMESPACE,
    KEEL_SERVER_SERVICE_ACCOUNT, PLATFORM_OWNED_LABEL, TENANT_RECONCILER_SERVICE_ACCOUNT,
    validate_resource_name,
};

pub const POLICY_DISALLOW_PRIVILEGED: &str = "disallow-privileged-containers";
pub const POLICY_PROTECT_CONTROL_PLANE: &str = "protect-control-plane";
pub const POLICY_RESTRICT_PULUMI_PROVIDERS: &str = "restrict-pulumi-providers";
pub const POLICY_RESTRICT_TENANT_RECONCILER: &str = "restrict-tenant-reconciler";
pub const POLICY_RESTRICT_TENANT_ROUTE_HOSTNAMES: &str = "restrict-tenant-route-hostnames";
/// `ValidatingPolicy` limiting tenant Flagger `Canary` specs to what Keel synthesizes
/// ([`build_restrict_tenant_flagger_canaries_policy`]).
pub const POLICY_RESTRICT_TENANT_FLAGGER_CANARIES: &str = "restrict-tenant-flagger-canaries";
pub const POLICY_RESTRICT_TENANT_FLUX_SOURCES: &str = "restrict-tenant-flux-sources";
pub const POLICY_RESTRICT_TENANT_FLUX_SOURCE_STATUS: &str = "restrict-tenant-flux-source-status";
/// `ValidatingPolicy` denying tenant subjects writes to PKO-internal objects (`auto.pulumi.com`
/// Workspaces / Updates, `stacks/status`) and the anti-lockout markers on Stacks
/// ([`build_restrict_tenant_pulumi_objects_policy`]).
pub const POLICY_RESTRICT_TENANT_PULUMI_OBJECTS: &str = "restrict-tenant-pulumi-objects";
/// `GeneratingPolicy` cloning the public `keel-system/keel-ca-bundle` into `keel-previews` and
/// every `tenant-*` namespace ([`build_sync_keel_ca_bundle_manifests`]).
pub const POLICY_SYNC_KEEL_CA_BUNDLE: &str = "sync-keel-ca-bundle";
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
/// not affected. Mirrored by the test-only `is_tenant_subject`.
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

/// Rust mirror of [`cel_tenant_subject`] (same lists), so tests can check whether a requester is
/// a tenant subject without a CEL evaluator.
#[cfg(test)]
pub(crate) fn is_tenant_subject(username: &str, groups: &[&str]) -> bool {
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

/// CEL: an `UPDATE` that leaves `spec` unchanged, **without** the deletion exemption of
/// [`CEL_SPEC_PRESERVING_UPDATE`]. Used by rules that must also hold while an object is being
/// deleted: a tenant can keep an object in `Terminating` with its own finalizer, so exempting
/// every update during deletion would let it rewrite `spec` freely. Finalizer removal only
/// changes metadata, so cleanup still passes.
const CEL_SPEC_UNCHANGED_UPDATE: &str = "(request.operation == 'UPDATE' && (has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {}))";

/// CEL regex (standard `matches`, no string extensions) of a route hostname wildcard that covers
/// a whole top-level domain (`*.com`) or everything (`*`): practically a catch-all on a shared
/// Gateway listener. Must stay in sync with `keel_core::is_tld_wide_wildcard`.
const CEL_TLD_WIDE_WILDCARD_REGEX: &str = "^[*]([.][^.]*)?$";

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
/// followed by `BUILTIN_PULUMI_PROVIDERS`. Blank ids are dropped.
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
/// * Flux `Kustomization`s and `HelmRelease`s in `tenant-<id>` must impersonate `tenant-<id>-sa`
///   (`spec.serviceAccountName`; without it the controllers impersonate the namespace's `default`
///   ServiceAccount under the `--default-service-account=default` lockdown). Keel pins the single
///   tenant identity so RBAC and quota do not depend on that controller flag. Updates that leave
///   `spec` unchanged (finalizer removal, reconcile annotations) and objects being deleted are
///   always allowed, so releases written before the rule can still be finalized and deleted.
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
                    },
                    {
                        "apiGroups": ["helm.toolkit.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["helmreleases"]
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
                },
                {
                    "expression": format!("!(request.kind.group == 'helm.toolkit.fluxcd.io' && request.kind.kind == 'HelmRelease') || (has(object.spec) && has(object.spec.serviceAccountName) && object.spec.serviceAccountName == variables.ns + '-sa') || {CEL_SPEC_PRESERVING_UPDATE}"),
                    "message": "Flux HelmReleases in a tenant namespace `tenant-<id>` must set `spec.serviceAccountName: tenant-<id>-sa`; without it helm-controller impersonates the namespace `default` ServiceAccount (`--default-service-account=default` lockdown), and Keel pins the single tenant identity (NFR-SEC-02)."
                }
            ]
        }
    })
}

/// Generates `ValidatingPolicy/restrict-tenant-flux-source-status` (`policies.kyverno.io/v1`).
///
/// Denies tenant subjects (`tenant-*` ServiceAccounts and groups, the `keel-previews` identity;
/// see `cel_tenant_subject`) every write to `*/status` of `source.toolkit.fluxcd.io` /
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

/// CEL: the request targets the main resource (no subresource such as `status`).
const CEL_NO_SUBRESOURCE: &str = "(has(request.subResource) ? request.subResource : '') == ''";

/// CEL list of the `Stack` `spec.config` keys Keel treats as protection
/// ([`crate::live::is_stack_object_protected`], [`keel_pko::PkoStack::is_protected`]).
const CEL_STACK_PROTECT_CONFIG_KEYS: &str = "['protect', 'keel:protect']";

/// Generates `ValidatingPolicy/restrict-tenant-pulumi-objects` (`policies.kyverno.io/v1`).
///
/// The PKO chart's aggregated `edit`/`admin` roles (`rbac.createClusterAggregationRoles`, pinned
/// in the seed and Stage 0 PKO releases) grant every `pulumi.com` / `auto.pulumi.com` resource to
/// tenant `admin`s, which tenant Stacks and Programs need. They also cover the operator's own
/// objects, so this policy denies **tenant subjects** (`cel_tenant_subject`), in any namespace:
///
/// * every `CREATE`/`UPDATE`/`DELETE` of `auto.pulumi.com` objects (`Workspace`, `Update` and
///   their `status`): they are operator-owned and tenants go through Stacks. This removes a
///   duplicate path only; it does not by itself enforce provider governance, because
///   `restrict-pulumi-providers` checks only the Stack's `keel.io/provider` label value and not
///   `gitSource`/`programRef`/`workspaceTemplate`;
/// * writes to `stacks/status` (a forged status misreports previews and outputs);
/// * adding or changing the `keel.io/protect` label on a `Stack` (`protect-control-plane` denies
///   deleting labelled Stacks, so a tenant could block pruning and preview GC); an `UPDATE` that
///   keeps the existing value (Stacks labelled before this policy) or drops the label passes;
/// * adding or changing `spec.config.protect` / `spec.config["keel:protect"]` on a `Stack` (Keel's
///   force-delete and preview GC treat them as protection, and previews inherit tenant config);
///   same keep-or-drop semantics as the label;
/// * creating a `Stack` named `management-cluster-core` (also undeletable by name).
///
/// Like [`build_restrict_tenant_flux_source_status_policy`], the tenant-subject test sits inline
/// in `matchConditions`: the PKO operator, keel-server, keel-tenant-reconciler and cluster admins
/// never reach the webhook, so an unavailable Kyverno cannot stall PKO reconciliation
/// (anti-lockout, `NFR-REL-01`). Fails closed for tenant requests.
pub fn build_restrict_tenant_pulumi_objects_policy() -> Value {
    let protect = keel_pko::LABEL_PROTECT;
    let core = keel_pko::MANAGEMENT_CORE_STACK_NAME;
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_TENANT_PULUMI_OBJECTS,
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
                        "apiGroups": ["auto.pulumi.com"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["*", "*/status"]
                    },
                    {
                        "apiGroups": ["pulumi.com"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["stacks", "stacks/status"]
                    }
                ]
            },
            "matchConditions": [
                {
                    "name": "tenant-subject",
                    "expression": cel_tenant_subject(CEL_REQUEST_USERNAME, CEL_REQUEST_GROUPS)
                }
            ],
            "variables": [
                {
                    "name": "stack",
                    "expression": format!("request.kind.group == 'pulumi.com' && request.kind.kind == 'Stack' && {CEL_NO_SUBRESOURCE}")
                }
            ],
            "validations": [
                {
                    "expression": "request.kind.group != 'auto.pulumi.com'",
                    "message": "Tenant identities may not create, change or delete PKO `auto.pulumi.com` objects (Workspace, Update): they are owned by the Pulumi Kubernetes Operator, which creates them for Stacks. Commit a `Stack` instead."
                },
                {
                    "expression": format!("!(request.kind.group == 'pulumi.com' && request.kind.kind == 'Stack') || {CEL_NO_SUBRESOURCE}"),
                    "message": "Tenant identities may not write the status of PKO Stacks; only the Pulumi Kubernetes Operator writes Stack status."
                },
                {
                    "expression": format!("!variables.stack || !(object != null && has(object.metadata.labels) && '{protect}' in object.metadata.labels) || (request.operation == 'UPDATE' && oldObject != null && has(oldObject.metadata.labels) && '{protect}' in oldObject.metadata.labels && oldObject.metadata.labels['{protect}'] == object.metadata.labels['{protect}'])"),
                    "message": format!("Tenant Stacks may not set the `{protect}` label: it makes a Stack undeletable (`protect-control-plane`) and would block pruning and preview garbage collection. Only platform identities may set it.")
                },
                {
                    "expression": format!("!variables.stack || !(object != null && has(object.spec) && has(object.spec.config)) || {keys}.all(k, !(k in object.spec.config) || (request.operation == 'UPDATE' && oldObject != null && has(oldObject.spec) && has(oldObject.spec.config) && (k in oldObject.spec.config) && oldObject.spec.config[k] == object.spec.config[k]))", keys = CEL_STACK_PROTECT_CONFIG_KEYS),
                    "message": "Tenant Stacks may not set `spec.config.protect` or `spec.config[\"keel:protect\"]`: Keel treats them as protection, so force-delete and preview garbage collection would skip the Stack and its previews. Only platform identities may set them."
                },
                {
                    "expression": format!("!variables.stack || request.operation != 'CREATE' || request.name != '{core}'"),
                    "message": format!("The Stack name `{core}` is reserved for the protected management control-plane Stack.")
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
    /// Extra CEL the request must also satisfy (only with `kind`), e.g.
    /// [`CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE`]; `None`: any write to that kind.
    pub only_if: Option<&'static str>,
    /// Why the identity needs write access (audit trail; asserted by tests).
    pub reason: &'static str,
}

/// Identities allowed to change `keel.io/platform-owned` objects, besides members of
/// `system:masters` / `kubeadm:cluster-admins` (break-glass cluster admins) and the built-in
/// `kube-system` controllers (garbage collection, namespace deletion).
///
/// `kustomize-controller` is a writer only for the per-tenant repository root `Kustomization`
/// (`keel-tenant-root`) and only for adding or removing its own `finalizers.fluxcd.io`
/// ([`CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE`]): tenant baselines are applied by `keel-tenants` impersonating `keel-tenant-reconciler`, and the
/// Flux-managed keel-server add-on does not ship the `tenant-default/keel-server` RoleBinding. As
/// an unrestricted writer it would let every `flux-system` Kustomization (core add-ons, rauthy,
/// keel-server, ...) rewrite or delete any tenant's baseline.
pub fn platform_owned_writers() -> Vec<PlatformOwnedWriter> {
    vec![
        PlatformOwnedWriter {
            username: format!(
                "system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:{TENANT_RECONCILER_SERVICE_ACCOUNT}"
            ),
            kind: None,
            only_if: None,
            reason: "`keel-tenants` applies and prunes `tenants/<id>/` (tenant baseline, `keel-gitops`, `keel-tenant-root`) as this identity",
        },
        PlatformOwnedWriter {
            username: format!(
                "system:serviceaccount:{KEEL_SERVER_NAMESPACE}:{KEEL_SERVER_SERVICE_ACCOUNT}"
            ),
            kind: None,
            only_if: None,
            reason: "keel-server provisions tenant baselines (tenant onboarding API)",
        },
        PlatformOwnedWriter {
            username: format!("system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:source-controller"),
            kind: Some("GitRepository"),
            only_if: None,
            reason: "source-controller adds and removes its finalizer on the platform-owned `keel-gitops` GitRepository",
        },
        PlatformOwnedWriter {
            username: format!("system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:kustomize-controller"),
            kind: Some("Kustomization"),
            only_if: Some(CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE),
            reason: "kustomize-controller adds and removes its finalizer on the platform-owned `keel-tenant-root` Kustomization (nothing else)",
        },
        PlatformOwnedWriter {
            username: "system:kube-controller-manager".to_string(),
            kind: None,
            only_if: None,
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

/// CEL admitting an UPDATE of a platform-owned `Kustomization` (the per-tenant repository root
/// `keel-tenant-root`) that only sets or changes `reconcile.fluxcd.io/requestedAt` (`flux
/// reconcile kustomization keel-tenant-root -n tenant-<id>`); same rules as
/// [`CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY`].
pub const CEL_KUSTOMIZATION_RECONCILE_REQUEST_ONLY: &str = "request.operation == 'UPDATE' && request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization' && object != null && oldObject != null && (has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {}) && (has(object.metadata.labels) ? object.metadata.labels : {}) == (has(oldObject.metadata.labels) ? oldObject.metadata.labels : {}) && (has(object.metadata.finalizers) ? object.metadata.finalizers : []) == (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : []) && (has(object.metadata.ownerReferences) ? object.metadata.ownerReferences : []) == (has(oldObject.metadata.ownerReferences) ? oldObject.metadata.ownerReferences : []) && (has(object.metadata.annotations) ? object.metadata.annotations : {}).all(k, k == 'reconcile.fluxcd.io/requestedAt' || (has(oldObject.metadata.annotations) && k in oldObject.metadata.annotations && oldObject.metadata.annotations[k] == object.metadata.annotations[k])) && (has(oldObject.metadata.annotations) ? oldObject.metadata.annotations : {}).all(k, k == 'reconcile.fluxcd.io/requestedAt' || (has(object.metadata.annotations) && k in object.metadata.annotations))";

/// CEL admitting an UPDATE of a Flux `kustomize.toolkit.fluxcd.io` `Kustomization` that only
/// adds or removes kustomize-controller's own `finalizers.fluxcd.io` finalizer: spec, labels,
/// annotations and owner references unchanged, and every other finalizer kept as it is (the
/// symmetric difference of the old and new finalizer lists is at most `finalizers.fluxcd.io`).
/// A foreign finalizer could block deletion of `keel-tenant-root`; stripping someone else's (or,
/// by anyone but kustomize-controller, Flux's) would skip its cleanup.
pub const CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE: &str = "request.operation == 'UPDATE' && request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization' && object != null && oldObject != null && (has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {}) && (has(object.metadata.labels) ? object.metadata.labels : {}) == (has(oldObject.metadata.labels) ? oldObject.metadata.labels : {}) && (has(object.metadata.annotations) ? object.metadata.annotations : {}) == (has(oldObject.metadata.annotations) ? oldObject.metadata.annotations : {}) && (has(object.metadata.ownerReferences) ? object.metadata.ownerReferences : []) == (has(oldObject.metadata.ownerReferences) ? oldObject.metadata.ownerReferences : []) && (has(object.metadata.finalizers) ? object.metadata.finalizers : []).all(f, f == 'finalizers.fluxcd.io' || f in (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : [])) && (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : []).all(f, f == 'finalizers.fluxcd.io' || f in (has(object.metadata.finalizers) ? object.metadata.finalizers : []))";

/// Generates `ValidatingPolicy/protect-platform-owned-tenant-objects` (`policies.kyverno.io/v1`).
///
/// Objects the platform writes for a tenant (namespace, ServiceAccount, quota, LimitRange,
/// NetworkPolicies, RoleBindings, the `keel-gitops` GitRepository stub and, for tenants with their
/// own repository, the `keel-tenant-root` Kustomization and the cluster-only
/// `keel-gitops-credentials` Secret) carry `keel.io/platform-owned: "true"`. Creating, changing
/// (including removing the label) or deleting such an object is reserved for
/// `keel-tenant-reconciler`, `keel-server`, `source-controller` (its own `GitRepository`
/// finalizer only), `kustomize-controller` (only adding/removing `finalizers.fluxcd.io` on
/// Flux `Kustomization`s), the
/// `kube-system` controllers and break-glass cluster admins ([`platform_owned_writers`]); tenant
/// identities (which hold `admin` in their namespace) and every other `flux-system` identity are
/// denied. The exemptions are reconcile requests on the `keel-gitops` GitRepository
/// ([`CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY`]) and the `keel-tenant-root` Kustomization
/// ([`CEL_KUSTOMIZATION_RECONCILE_REQUEST_ONLY`]), so `flux reconcile` keeps working for anyone
/// RBAC lets patch them. Fails closed.
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
                let only_if = w.only_if.map(|c| format!(" && ({c})")).unwrap_or_default();
                format!(
                    " || (variables.username == '{}' && request.kind.kind == '{kind}'{only_if})",
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
                    },
                    {
                        "apiGroups": ["kustomize.toolkit.fluxcd.io"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["kustomizations"]
                    },
                    {
                        "apiGroups": [""],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "resources": ["secrets"]
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
                        "variables.username in [{writers}]{scoped_writers} || variables.username.startsWith('system:serviceaccount:kube-system:') || variables.groups.exists(g, g == 'system:masters' || g == 'kubeadm:cluster-admins') || ({CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY}) || ({CEL_KUSTOMIZATION_RECONCILE_REQUEST_ONLY})"
                    ),
                    "message": format!(
                        "Objects labelled `{PLATFORM_OWNED_LABEL}` (tenant namespace, RBAC, NetworkPolicies, the `keel-gitops` source, the `keel-tenant-root` Kustomization, the `keel-gitops-credentials` Secret) are managed by the platform (keel-tenant-reconciler / keel-server); change them through a GitOps PR to `tenants/<id>/` of the platform repository (only a `reconcile.fluxcd.io/requestedAt` reconcile request on `keel-gitops` / `keel-tenant-root` is exempt)."
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
///
/// It also denies tenant routes that attach to a Gateway in another namespace (the shared
/// `keel-system/keel-ingress`) without any `spec.hostnames`: such a route matches every host on
/// the shared listeners and would capture other tenants' traffic. This covers every writer,
/// including Flagger, which copies a `Canary`'s `spec.service.hosts` into the routes it generates
/// (`restrict-tenant-flagger-canaries` requires those hosts). Tenant route hostnames may not be a
/// wildcard covering a whole top-level domain (`*.com`), which is practically a catch-all too.
/// These two rules skip only updates that leave `spec` unchanged (finalizer removal included, so
/// cleanup never hangs); unlike most tenant policies they also hold on objects being deleted, so a
/// tenant cannot hold a route in `Terminating` with its own finalizer and then drop its hostnames.
/// The deny list is precomputed (no CEL string extensions needed; the wildcard check uses the
/// standard `matches`). Fails closed.
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
                },
                {
                    // Not CEL_SPEC_PRESERVING_UPDATE: a tenant could otherwise hold a route in
                    // `Terminating` with its own finalizer and then drop its hostnames.
                    "name": "specUnchanged",
                    "expression": CEL_SPEC_UNCHANGED_UPDATE
                },
                {
                    // A parentRef to a Gateway outside the route's namespace (a missing
                    // `namespace` means the route's own; missing `kind`/`group` mean Gateway).
                    "name": "attachesToSharedGateway",
                    "expression": format!("has(object.spec.parentRefs) && object.spec.parentRefs.exists(p, has(p.namespace) && p.namespace != {CEL_REQUEST_NAMESPACE} && (!has(p.kind) || p.kind == 'Gateway') && (!has(p.group) || p.group == 'gateway.networking.k8s.io'))")
                }
            ],
            "validations": [
                {
                    "expression": "variables.hostnames.all(h, !(h in variables.deniedHostnames))",
                    "message": "Tenant routes may not claim reserved platform hostnames (IdP, keel-server, Forgejo) or wildcards covering them."
                },
                {
                    "expression": format!("variables.specUnchanged || variables.hostnames.all(h, !h.matches('{CEL_TLD_WIDE_WILDCARD_REGEX}'))"),
                    "message": "Tenant routes may not use a wildcard hostname covering a whole top-level domain (`*.com`): it would capture every unclaimed host under it on the shared Gateway."
                },
                {
                    "expression": "variables.specUnchanged || !variables.attachesToSharedGateway || size(variables.hostnames) > 0",
                    "message": "Tenant routes attached to a shared Gateway (keel-system/keel-ingress) must set `spec.hostnames`: a route without hostnames matches every host and would capture other tenants' traffic (Flagger Canaries need `spec.service.hosts`)."
                }
            ]
        }
    }))
}

/// Generates `ValidatingPolicy/restrict-tenant-flagger-canaries` (`policies.kyverno.io/v1`).
///
/// Tenant identities may manage Flagger `Canary` objects in their namespace
/// (`ClusterRole/keel-tenant-flagger-canaries`, aggregated into `admin`). Flagger runs
/// cluster-wide in `keel-system` and queries the shared platform Prometheus, so a free-form
/// `Canary` would make it act for the tenant outside the tenant's own network and data
/// boundaries. In `tenant-*` namespaces this policy admits only the shape Keel synthesizes
/// (`synthesize_flagger_canary`), for every requester (Flux, `kubectl`, platform admins):
///
/// * `spec.provider` is `gatewayapi:v1`;
/// * every `spec.service.gatewayRefs` entry is the shared `keel-system/keel-ingress` Gateway;
/// * no `spec.analysis.webhooks` (Flagger would call any URL from `keel-system`: SSRF around the
///   tenant's egress NetworkPolicies);
/// * every `spec.analysis.metrics` entry references a platform `MetricTemplate` in `keel-system`
///   and carries neither an inline `query` nor `templateVariables` (otherwise the tenant runs
///   arbitrary PromQL against the shared Prometheus and reads other tenants' metrics back through
///   Canary events and status);
/// * `spec.analysis.alerts` only reference `AlertProvider`s in the Canary's own namespace;
/// * `spec.service.hosts` holds at least one non-empty hostname without a wildcard: Flagger copies
///   them into the routes it generates, and a route without hostnames is a catch-all on the
///   shared Gateway. This rule also holds on Canaries being deleted (only spec-preserving updates
///   skip it).
///
/// Updates that leave `spec` unchanged (finalizers, annotations, Flagger's own metadata writes)
/// and (except for the hosts rule) objects being deleted always pass. The routes Flagger generates are checked again by
/// `restrict-tenant-route-hostnames` (reserved platform hosts, no host-less route on the shared
/// Gateway, no TLD-wide wildcard), which matches route writes in `tenant-*` namespaces whoever the
/// writer is. Fails closed; the `matchConditions` keep platform namespaces out of the webhook
/// (anti-lockout, `NFR-REL-01`).
pub fn build_restrict_tenant_flagger_canaries_policy() -> Value {
    let ns = KEEL_SYSTEM_NAMESPACE;
    let gateway = KEEL_INGRESS_GATEWAY_NAME;
    let provider = FLAGGER_CANARY_PROVIDER;
    json!({
        "apiVersion": "policies.kyverno.io/v1",
        "kind": "ValidatingPolicy",
        "metadata": {
            "name": POLICY_RESTRICT_TENANT_FLAGGER_CANARIES,
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
                        "apiGroups": ["flagger.app"],
                        "apiVersions": ["*"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["canaries"]
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
                },
                {
                    "name": "exempt",
                    "expression": CEL_SPEC_PRESERVING_UPDATE
                },
                {
                    // The hosts rule must also hold on objects being deleted (see
                    // CEL_SPEC_UNCHANGED_UPDATE).
                    "name": "specUnchanged",
                    "expression": CEL_SPEC_UNCHANGED_UPDATE
                },
                {
                    "name": "spec",
                    "expression": "has(object.spec) ? object.spec : {}"
                },
                {
                    "name": "service",
                    "expression": "has(object.spec) && has(object.spec.service) ? object.spec.service : {}"
                },
                {
                    "name": "analysis",
                    "expression": "has(object.spec) && has(object.spec.analysis) ? object.spec.analysis : {}"
                }
            ],
            "validations": [
                {
                    "expression": format!("variables.exempt || (has(variables.spec.provider) && variables.spec.provider == '{provider}')"),
                    "message": format!("Tenant Flagger Canaries must set `spec.provider: {provider}` (Envoy Gateway on the shared `{ns}/{gateway}` Gateway).")
                },
                {
                    "expression": format!("variables.exempt || (has(variables.service.gatewayRefs) && size(variables.service.gatewayRefs) > 0 && variables.service.gatewayRefs.all(g, has(g.name) && g.name == '{gateway}' && has(g.namespace) && g.namespace == '{ns}' && (!has(g.kind) || g.kind == 'Gateway') && (!has(g.group) || g.group == 'gateway.networking.k8s.io')))"),
                    "message": format!("Tenant Flagger Canaries may only route through the shared `{ns}/{gateway}` Gateway (`spec.service.gatewayRefs`).")
                },
                {
                    "expression": "variables.specUnchanged || (has(variables.service.hosts) && size(variables.service.hosts) > 0 && variables.service.hosts.all(h, h != '' && !h.contains('*')))",
                    "message": format!("Tenant Flagger Canaries must set `spec.service.hosts` to concrete hostnames (no wildcard): the routes Flagger generates would otherwise match every host (or a whole domain) on the shared `{ns}/{gateway}` Gateway.")
                },
                {
                    "expression": "variables.exempt || !has(variables.analysis.webhooks) || size(variables.analysis.webhooks) == 0",
                    "message": "Tenant Flagger Canaries may not set `spec.analysis.webhooks`: Flagger would call the URL from the platform namespace, bypassing the tenant's network policies."
                },
                {
                    "expression": format!("variables.exempt || !has(variables.analysis.metrics) || variables.analysis.metrics.all(m, has(m.templateRef) && has(m.templateRef.namespace) && m.templateRef.namespace == '{ns}' && !has(m.query) && !has(m.templateVariables))"),
                    "message": format!("Tenant Flagger Canary metrics must reference a platform MetricTemplate in `{ns}` (`templateRef` with `namespace: {ns}`) and may not set an inline `query` or `templateVariables`: they would run arbitrary queries against the shared platform Prometheus.")
                },
                {
                    "expression": "variables.exempt || !has(variables.analysis.alerts) || variables.analysis.alerts.all(a, !has(a.providerRef) || !has(a.providerRef.namespace) || a.providerRef.namespace == variables.ns)",
                    "message": "Tenant Flagger Canary alerts may only reference an AlertProvider in the Canary's own namespace."
                }
            ]
        }
    })
}

/// Generates the `sync-keel-ca-bundle` Kyverno `GeneratingPolicy` (`policies.kyverno.io/v1`) and
/// the least-privilege RBAC its controllers need (`ClusterRole`/`ClusterRoleBinding`
/// `keel-kyverno-ca-bundle-sync`).
///
/// On every `keel-previews` / `tenant-*` Namespace (new ones on creation, existing ones through
/// `generateExisting`) Kyverno writes ConfigMap `keel-ca-bundle` with exactly the `ca.crt` of
/// `keel-system/keel-ca-bundle` (the **public** Keel CA certificate that `keel bootstrap init`
/// copies out of `keel-ca-secret`), and keeps it synchronized (a deleted or edited copy is
/// restored). PKO workspace pods mount it (`optional: true`) to trust the in-cluster Forgejo HTTPS
/// ingress. No Secret is read: when no local CA exists the source is absent, nothing is generated
/// and workspaces just use the system roots (external forge with a public CA).
pub fn build_sync_keel_ca_bundle_manifests() -> Vec<Value> {
    let labels = json!({ "app.kubernetes.io/managed-by": "keel" });
    vec![
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "ClusterRole",
            "metadata": { "name": "keel-kyverno-ca-bundle-sync", "labels": labels },
            "rules": [
                {
                    "apiGroups": [""],
                    "resources": ["configmaps"],
                    "verbs": ["list", "watch", "create"]
                },
                {
                    "apiGroups": [""],
                    "resources": ["configmaps"],
                    "resourceNames": [KEEL_CA_BUNDLE_CONFIGMAP],
                    "verbs": ["get", "update", "patch", "delete"]
                }
            ]
        }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "ClusterRoleBinding",
            "metadata": { "name": "keel-kyverno-ca-bundle-sync", "labels": labels },
            "roleRef": {
                "apiGroup": "rbac.authorization.k8s.io",
                "kind": "ClusterRole",
                "name": "keel-kyverno-ca-bundle-sync"
            },
            "subjects": [
                { "kind": "ServiceAccount", "name": "kyverno-background-controller", "namespace": "kyverno" },
                { "kind": "ServiceAccount", "name": "kyverno-admission-controller", "namespace": "kyverno" }
            ]
        }),
        json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "GeneratingPolicy",
            "metadata": {
                "name": POLICY_SYNC_KEEL_CA_BUNDLE,
                "labels": {
                    "app.kubernetes.io/managed-by": "keel",
                    "keel.io/policy-tier": "tenant-ca-trust"
                }
            },
            "spec": {
                "evaluation": {
                    "generateExisting": { "enabled": true },
                    "synchronize": { "enabled": true }
                },
                "matchConstraints": {
                    "resourceRules": [{
                        "apiGroups": [""],
                        "apiVersions": ["v1"],
                        "operations": ["CREATE", "UPDATE"],
                        "resources": ["namespaces"]
                    }]
                },
                "matchConditions": [{
                    "name": "previews-or-tenant-namespace",
                    "expression": format!(
                        "object.metadata.name == '{KEEL_PREVIEWS_NAMESPACE}' || (object.metadata.name.startsWith('tenant-') && object.metadata.name.size() > 7)"
                    )
                }],
                "variables": [{
                    "name": "caBundle",
                    "expression": format!(
                        "resource.Get('v1', 'configmaps', '{KEEL_SYSTEM_NAMESPACE}', '{KEEL_CA_BUNDLE_CONFIGMAP}')"
                    )
                }],
                "generate": [{
                    "expression": format!(
                        "generator.Apply(object.metadata.name, [{{'apiVersion': dyn('v1'), 'kind': dyn('ConfigMap'), 'metadata': dyn({{'name': '{KEEL_CA_BUNDLE_CONFIGMAP}', 'labels': {{'app.kubernetes.io/managed-by': 'keel', 'keel.io/ca-bundle': 'true'}}}}), 'data': dyn({{'{KEEL_CA_BUNDLE_KEY}': variables.caBundle.data['{KEEL_CA_BUNDLE_KEY}']}})}}])"
                    )
                }]
            }
        }),
    ]
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

        let mut supporting_manifests = vec![
            build_restrict_tenant_reconciler_policy(),
            build_restrict_tenant_route_hostnames_policy(&config.reserved_route_hostnames)?,
            build_restrict_tenant_flagger_canaries_policy(),
            build_restrict_tenant_flux_sources_policy(),
            build_restrict_tenant_flux_source_status_policy(),
            build_restrict_tenant_pulumi_objects_policy(),
            build_protect_platform_owned_tenant_objects_policy(),
        ];
        supporting_manifests.extend(build_sync_keel_ca_bundle_manifests());

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
        Ok(keel_core::to_yaml_documents(self.all_manifests())?)
    }
}

/// Kinds the platform writes in a tenant's baseline files (`rbac.yaml`, `git-repository.yaml`,
/// `sync.yaml`) and labels [`PLATFORM_OWNED_LABEL`]; every one must be matched by
/// `protect-platform-owned-tenant-objects`.
#[cfg(test)]
pub(crate) const PLATFORM_OWNED_KINDS: [&str; 9] = [
    "Namespace",
    "ServiceAccount",
    "ResourceQuota",
    "LimitRange",
    "NetworkPolicy",
    "RoleBinding",
    "ClusterRoleBinding",
    "GitRepository",
    "Kustomization",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_production_config_generates_three_enforced_policies_without_exception() {
        let cfg = KyvernoPolicyConfig::default();
        assert!(!cfg.local_dev_docker_socket_exception);

        let bundle = KyvernoManifestBundle::generate(&cfg).unwrap();
        assert_eq!(bundle.cluster_policies.len(), 3);
        // 7 ValidatingPolicies + sync-keel-ca-bundle (ClusterRole, binding, GeneratingPolicy).
        assert_eq!(bundle.supporting_manifests.len(), 10);
        assert!(bundle.policy_exception.is_none());
        assert_eq!(bundle.all_manifests().len(), 13);
        // The only GeneratingPolicy copies the public CA bundle; the retired
        // `sync-tenant-secrets` (control-plane credentials into tenants) never comes back.
        let generating: Vec<Value> = bundle
            .all_manifests()
            .into_iter()
            .filter(|m| m["kind"] == "GeneratingPolicy")
            .map(|m| m["metadata"]["name"].clone())
            .collect();
        assert_eq!(generating, vec![json!(POLICY_SYNC_KEEL_CA_BUNDLE)]);
        assert!(
            !bundle
                .to_multi_doc_yaml()
                .unwrap()
                .contains("sync-tenant-secrets")
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
        assert_eq!(bundle.all_manifests().len(), 14);

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
        // Control-plane credentials are never cloned into tenant namespaces.
        assert!(!yaml.contains("sync-tenant-secrets"));
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
            build_restrict_tenant_pulumi_objects_policy(),
            build_protect_platform_owned_tenant_objects_policy(),
        ];
        docs.push(
            build_local_kind_docker_socket_exception(&DockerSocketExceptionConfig::default())
                .unwrap(),
        );
        docs.extend(build_sync_keel_ca_bundle_manifests());
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
    fn sync_keel_ca_bundle_copies_only_the_public_ca() {
        let generated = build_sync_keel_ca_bundle_manifests();
        let policy = &generated[2];
        assert_eq!(policy["kind"], "GeneratingPolicy");
        assert_eq!(policy["metadata"]["name"], POLICY_SYNC_KEEL_CA_BUNDLE);
        let text = policy.to_string();
        // Never reads or copies a Secret (the CA Secret holds the private key).
        assert!(!text.contains("secrets"), "{text}");
        assert!(!text.contains("keel-ca-secret"), "{text}");
        assert!(text.contains("'configmaps', 'keel-system', 'keel-ca-bundle'"));
        let cond = policy["spec"]["matchConditions"][0]["expression"]
            .as_str()
            .unwrap();
        assert!(cond.contains("keel-previews") && cond.contains("startsWith('tenant-')"));
        // RBAC is limited to ConfigMaps, and to `keel-ca-bundle` for reads/updates.
        for rule in generated[0]["rules"].as_array().unwrap() {
            assert_eq!(rule["resources"], json!(["configmaps"]));
            if rule["verbs"].as_array().unwrap().contains(&json!("get")) {
                assert_eq!(rule["resourceNames"], json!(["keel-ca-bundle"]));
            }
        }
        let bundle = KyvernoManifestBundle::generate(&KyvernoPolicyConfig::default()).unwrap();
        assert!(
            bundle
                .all_manifests()
                .iter()
                .any(|m| m["metadata"]["name"] == POLICY_SYNC_KEEL_CA_BUNDLE)
        );
    }

    #[test]
    fn protect_control_plane_policy_matches_seed_and_guards_protected_flux_objects() {
        let seed: Value = noyalib::from_str(include_str!(
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
        let seed: Value = noyalib::from_str(include_str!(
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
    fn restrict_tenant_flux_sources_policy_closes_artifact_paths() {
        let policy = build_restrict_tenant_flux_sources_policy();
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
        assert!(covers("helm.toolkit.fluxcd.io", "helmreleases", "CREATE"));
        assert!(covers("helm.toolkit.fluxcd.io", "helmreleases", "UPDATE"));
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
        assert_eq!(exprs.len(), 3);
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
        // 3: tenant HelmReleases too (without it helm-controller would impersonate the namespace
        // `default` ServiceAccount under the `--default-service-account=default` lockdown).
        assert!(exprs[2].starts_with(
            "!(request.kind.group == 'helm.toolkit.fluxcd.io' && request.kind.kind == 'HelmRelease')"
        ));
        assert!(exprs[2].contains("object.spec.serviceAccountName == variables.ns + '-sa'"));
    }

    #[test]
    fn restrict_tenant_flux_source_status_policy_filters_on_tenant_subjects() {
        let policy = build_restrict_tenant_flux_source_status_policy();
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
        let ks = crate::tenant::synthesize_flux_kustomization(&input).unwrap();
        let ns = ks["metadata"]["namespace"].as_str().unwrap();
        assert_eq!(ks["spec"]["serviceAccountName"], format!("{ns}-sa"));
        // The per-tenant repository root (`tenants/<id>/sync.yaml`, applied by
        // keel-tenant-reconciler) satisfies the same rule and only references its own namespace.
        let root = crate::tenant_repo::build_tenant_root_kustomization("acme").unwrap();
        let ns = root["metadata"]["namespace"].as_str().unwrap();
        assert_eq!(root["spec"]["serviceAccountName"], format!("{ns}-sa"));
        assert_eq!(root["spec"]["sourceRef"]["namespace"], ns);
    }

    /// Every tenant HelmRelease Keel synthesizes (local and remote workload clusters) satisfies the
    /// HelmRelease `serviceAccountName` rule of `restrict-tenant-flux-sources`.
    #[test]
    fn synthesized_tenant_helm_releases_use_the_tenant_service_account() {
        let source = crate::HelmChartSourceRef {
            kind: crate::HelmChartSourceKind::HelmRepository,
            name: "podinfo".to_string(),
            namespace: None,
        };
        let local = crate::HelmReleaseSpecInput::new("acme", "web", "podinfo", source.clone());
        let mut remote = local.clone();
        remote.kube_config = Some(crate::tenant::RemoteKubeConfigRef::new(
            "workload-kubeconfig",
        ));
        for input in [local, remote] {
            let hr = crate::tenant::synthesize_helm_release(&input).unwrap();
            let ns = hr["metadata"]["namespace"].as_str().unwrap();
            assert_eq!(ns, "tenant-acme");
            assert_eq!(hr["spec"]["serviceAccountName"], format!("{ns}-sa"));
        }
    }

    #[test]
    fn restrict_tenant_pulumi_objects_policy_filters_on_tenant_subjects() {
        let policy = build_restrict_tenant_pulumi_objects_policy();
        assert_eq!(
            policy["metadata"]["name"],
            POLICY_RESTRICT_TENANT_PULUMI_OBJECTS
        );
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");
        assert_eq!(policy["spec"]["validationActions"], json!(["Deny"]));
        assert_eq!(policy["spec"]["evaluation"]["background"]["enabled"], false);

        // Every auto.pulumi.com write (incl. status and deletion); Stack create/update + status.
        assert_eq!(
            policy["spec"]["matchConstraints"]["resourceRules"],
            json!([
                {
                    "apiGroups": ["auto.pulumi.com"],
                    "apiVersions": ["*"],
                    "operations": ["CREATE", "UPDATE", "DELETE"],
                    "resources": ["*", "*/status"]
                },
                {
                    "apiGroups": ["pulumi.com"],
                    "apiVersions": ["*"],
                    "operations": ["CREATE", "UPDATE"],
                    "resources": ["stacks", "stacks/status"]
                }
            ])
        );
        // Stack deletion (pruning, preview GC) is never matched; Programs stay tenant-writable.
        let rules = policy["spec"]["matchConstraints"]["resourceRules"]
            .as_array()
            .unwrap();
        assert!(
            !rules[1]["operations"]
                .as_array()
                .unwrap()
                .contains(&json!("DELETE"))
        );
        assert!(
            !rules[1]["resources"]
                .as_array()
                .unwrap()
                .contains(&json!("programs"))
        );

        // Only tenant requesters reach Kyverno: the PKO operator, keel-server, the tenant
        // reconciler and admins never do (anti-lockout).
        let conds = policy["spec"]["matchConditions"].as_array().unwrap();
        assert_eq!(conds.len(), 1);
        assert_eq!(
            conds[0]["expression"].as_str().unwrap(),
            cel_tenant_subject(CEL_REQUEST_USERNAME, CEL_REQUEST_GROUPS)
        );
        for platform in [
            "system:serviceaccount:keel-system:pulumi-kubernetes-operator-controller-manager",
            "system:serviceaccount:keel-system:keel-server",
            "system:serviceaccount:flux-system:keel-tenant-reconciler",
            "system:serviceaccount:flux-system:kustomize-controller",
        ] {
            assert!(
                !is_tenant_subject(platform, &["system:serviceaccounts"]),
                "{platform}"
            );
        }
        assert!(is_tenant_subject(
            "system:serviceaccount:tenant-acme:tenant-acme-sa",
            &[]
        ));

        assert_eq!(
            policy["spec"]["variables"],
            json!([{
                "name": "stack",
                "expression": format!("request.kind.group == 'pulumi.com' && request.kind.kind == 'Stack' && {CEL_NO_SUBRESOURCE}")
            }])
        );
        let exprs = expressions(&policy);
        assert_eq!(exprs.len(), 5);
        // 1: no auto.pulumi.com write at all.
        assert_eq!(exprs[0], "request.kind.group != 'auto.pulumi.com'");
        // 2: no Stack status write.
        assert_eq!(
            exprs[1],
            format!(
                "!(request.kind.group == 'pulumi.com' && request.kind.kind == 'Stack') || {CEL_NO_SUBRESOURCE}"
            )
        );
        // 3: `keel.io/protect` may not be added or changed; kept or dropped on UPDATE passes.
        for needle in [
            "!variables.stack",
            "'keel.io/protect' in object.metadata.labels",
            "request.operation == 'UPDATE' && oldObject != null",
            "oldObject.metadata.labels['keel.io/protect'] == object.metadata.labels['keel.io/protect']",
        ] {
            assert!(exprs[2].contains(needle), "{needle}");
        }
        // 4: `spec.config.protect` / `spec.config["keel:protect"]` may not be added or changed;
        // kept or dropped on UPDATE passes. Every key Keel treats as protection is covered.
        for needle in [
            "!variables.stack",
            "has(object.spec.config)",
            "['protect', 'keel:protect'].all(k, !(k in object.spec.config)",
            "request.operation == 'UPDATE' && oldObject != null",
            "(k in oldObject.spec.config) && oldObject.spec.config[k] == object.spec.config[k]",
        ] {
            assert!(exprs[3].contains(needle), "{needle}");
        }
        for key in keel_pko::CONFIG_PROTECT_KEYS {
            let protected = json!({ "spec": { "config": { key: "true" } } });
            assert!(
                crate::live::is_stack_object_protected(None, &protected),
                "protection key missing from is_stack_object_protected"
            );
            assert!(
                CEL_STACK_PROTECT_CONFIG_KEYS.contains(&format!("'{key}'")),
                "protection key missing from the policy"
            );
        }
        let message = policy["spec"]["validations"][3]["message"]
            .as_str()
            .unwrap();
        assert!(message.starts_with("Tenant Stacks may not set `spec.config.protect`"));
        // 5: the anti-lockout Stack name is reserved.
        assert!(exprs[4].contains("request.operation != 'CREATE'"));
        assert!(exprs[4].contains("request.name != 'management-cluster-core'"));
        // The name and label match what `protect-control-plane` treats as undeletable.
        let guard = expressions(&build_protect_control_plane_policy())[0].clone();
        assert!(guard.contains(keel_pko::MANAGEMENT_CORE_STACK_NAME));
        assert!(guard.contains(&format!("'{}'", keel_pko::LABEL_PROTECT)));
    }

    #[test]
    fn protect_platform_owned_policy_allows_only_platform_writers() {
        let policy = build_protect_platform_owned_tenant_objects_policy();
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

        // The per-tenant repository stub (`git-repository.yaml`, `sync.yaml`) is matched too.
        let source =
            crate::TenantRepoSource::new("acme", "https://forgejo.example/keel-tenants/r.git")
                .unwrap();
        let stub_docs = [
            crate::tenant_repo::build_tenant_repo_git_repository(&source).unwrap(),
            crate::tenant_repo::build_tenant_root_kustomization("acme").unwrap(),
        ];
        for doc in &stub_docs {
            assert_eq!(doc["metadata"]["labels"][PLATFORM_OWNED_LABEL], "true");
        }
        assert!(rules.iter().any(|r| {
            r["apiGroups"] == json!(["kustomize.toolkit.fluxcd.io"])
                && r["resources"] == json!(["kustomizations"])
        }));
        // The cluster-only tenant repository credential is platform-owned and matched, so a
        // tenant admin cannot replace its `ca.crt`/`known_hosts`/token (read access stays, D1).
        let credential = crate::TenantRepoCredential::HttpsToken {
            username: "keel-tenant-reader".to_string(),
            token: "t".to_string(),
            ca_crt: None,
        };
        let secret =
            crate::tenant_repo::build_tenant_gitops_credentials_secret(&source, &credential)
                .unwrap();
        assert_eq!(secret["metadata"]["labels"][PLATFORM_OWNED_LABEL], "true");
        assert!(
            rules
                .iter()
                .any(|r| { r["apiGroups"] == json!([""]) && r["resources"] == json!(["secrets"]) })
        );
        // Every platform-owned kind is matched by some rule.
        for kind in PLATFORM_OWNED_KINDS {
            let resource = match kind {
                "GitRepository" => "gitrepositories",
                "Kustomization" => "kustomizations",
                other => plural(other),
            };
            assert!(
                rules.iter().any(|r| r["resources"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(resource))),
                "{kind} not matched"
            );
        }

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
        // baselines: it is a writer only for finalizer-only updates of Kustomizations
        // (`keel-tenant-root`), never in the unrestricted writer list.
        assert!(exprs[0].contains(&format!(
            "(variables.username == 'system:serviceaccount:flux-system:kustomize-controller' && request.kind.kind == 'Kustomization' && ({CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE}))"
        )));
        assert_eq!(exprs[0].matches("kustomize-controller").count(), 1);
        let unrestricted = exprs[0].split(']').next().unwrap();
        assert!(!unrestricted.contains("kustomize-controller"));
        for needle in [
            "request.operation == 'UPDATE'",
            "(has(object.spec) ? object.spec : {}) == (has(oldObject.spec) ? oldObject.spec : {})",
            "(has(object.metadata.labels) ? object.metadata.labels : {}) == (has(oldObject.metadata.labels) ? oldObject.metadata.labels : {})",
            "(has(object.metadata.annotations) ? object.metadata.annotations : {}) == (has(oldObject.metadata.annotations) ? oldObject.metadata.annotations : {})",
            "(has(object.metadata.ownerReferences) ? object.metadata.ownerReferences : []) == (has(oldObject.metadata.ownerReferences) ? oldObject.metadata.ownerReferences : [])",
        ] {
            assert!(
                CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE.contains(needle),
                "{needle}"
            );
        }
        // Finalizers: only `finalizers.fluxcd.io` may be added or removed; every other finalizer
        // must be kept in both directions (no foreign finalizer added, none stripped).
        for needle in [
            "request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization'",
            "(has(object.metadata.finalizers) ? object.metadata.finalizers : []).all(f, f == 'finalizers.fluxcd.io' || f in (has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : []))",
            "(has(oldObject.metadata.finalizers) ? oldObject.metadata.finalizers : []).all(f, f == 'finalizers.fluxcd.io' || f in (has(object.metadata.finalizers) ? object.metadata.finalizers : []))",
        ] {
            assert!(
                CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE.contains(needle),
                "{needle}"
            );
        }
        assert!(!CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE.contains("CREATE"));
        assert!(!CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE.contains("DELETE"));
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
        assert!(exprs[0].ends_with(&format!(
            " || ({CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY}) || ({CEL_KUSTOMIZATION_RECONCILE_REQUEST_ONLY})"
        )));
        // `flux reconcile kustomization keel-tenant-root`: the same rule for the root
        // Kustomization of a per-tenant repository.
        assert_eq!(
            CEL_KUSTOMIZATION_RECONCILE_REQUEST_ONLY,
            CEL_GITREPOSITORY_RECONCILE_REQUEST_ONLY.replace(
                "request.kind.group == 'source.toolkit.fluxcd.io' && request.kind.kind == 'GitRepository'",
                "request.kind.group == 'kustomize.toolkit.fluxcd.io' && request.kind.kind == 'Kustomization'"
            )
        );
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
                assert!(
                    !w.reason.trim().is_empty(),
                    "every platform-owned writer documents a reason"
                );
                // A CEL restriction only narrows a kind-scoped writer.
                assert!(w.only_if.is_none() || w.kind.is_some(), "{w:?}");
                (w.username, w.kind)
            })
            .collect();
        let kustomize = platform_owned_writers()
            .into_iter()
            .find(|w| w.username.ends_with(":kustomize-controller"))
            .unwrap();
        assert_eq!(
            kustomize.only_if,
            Some(CEL_KUSTOMIZATION_FLUX_FINALIZER_ONLY_UPDATE)
        );
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
                (
                    "system:serviceaccount:flux-system:kustomize-controller".to_string(),
                    Some("Kustomization")
                ),
                ("system:kube-controller-manager".to_string(), None),
            ]
        );
    }

    /// Requests that reach `restrict-tenant-flux-source-status` (its `matchConditions` are exactly
    /// `cel_tenant_subject`, mirrored by [`is_tenant_subject`]; pinned by
    /// [`restrict_tenant_flux_source_status_policy_filters_on_tenant_subjects`]).
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
        assert_eq!(docs.len(), 14);
        for doc in docs {
            let parsed: serde_json::Value =
                noyalib::from_str(doc).expect("each Kyverno multi-doc YAML part must parse");
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
    fn restrict_tenant_route_hostnames_policy_denies_platform_hosts() {
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
    }

    /// Admission outcome of a `ValidatingPolicy` for one request, evaluated with the `cel`
    /// interpreter: `Ok(())` when the request is admitted (or not matched), else the messages of
    /// the failing validations. Mirrors Kyverno's order: `matchConditions`, `variables` (in
    /// order, each seeing the earlier ones), `validations`.
    fn evaluate_validating_policy(
        policy: &Value,
        operation: &str,
        namespace: &str,
        object: &Value,
        old_object: Option<&Value>,
    ) -> Result<(), Vec<String>> {
        use std::collections::HashMap;

        let request = json!({
            "operation": operation,
            "namespace": namespace,
            "name": object["metadata"]["name"],
            "kind": {
                "group": object["apiVersion"].as_str().unwrap().split('/').next().unwrap(),
                "kind": object["kind"]
            },
            "userInfo": {
                "username": format!("system:serviceaccount:{namespace}:{namespace}-sa"),
                "groups": ["system:serviceaccounts", format!("system:serviceaccounts:{namespace}")]
            }
        });
        let base = |variables: &HashMap<String, cel::Value>| {
            let mut ctx = cel::Context::default();
            ctx.add_variable("request", cel::to_value(&request).unwrap())
                .unwrap();
            ctx.add_variable("object", cel::to_value(object).unwrap())
                .unwrap();
            match old_object {
                Some(old) => ctx
                    .add_variable("oldObject", cel::to_value(old).unwrap())
                    .unwrap(),
                None => ctx.add_variable_from_value("oldObject", cel::Value::Null),
            }
            ctx.add_variable_from_value("variables", cel::Value::from(variables.clone()));
            ctx
        };
        let eval = |expr: &str, variables: &HashMap<String, cel::Value>| {
            cel::Program::compile(expr)
                .unwrap_or_else(|e| panic!("CEL `{expr}` does not compile: {e}"))
                .execute(&base(variables))
                .unwrap_or_else(|e| panic!("CEL `{expr}` failed: {e}"))
        };

        let mut variables = HashMap::new();
        for condition in policy["spec"]["matchConditions"].as_array().unwrap() {
            if eval(condition["expression"].as_str().unwrap(), &variables) != cel::Value::Bool(true)
            {
                return Ok(());
            }
        }
        for variable in policy["spec"]["variables"].as_array().into_iter().flatten() {
            let value = eval(variable["expression"].as_str().unwrap(), &variables);
            variables.insert(variable["name"].as_str().unwrap().to_string(), value);
        }
        let denied: Vec<String> = policy["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(
                |v| match eval(v["expression"].as_str().unwrap(), &variables) {
                    cel::Value::Bool(ok) => !ok,
                    other => panic!("validation returned a non-bool: {other:?}"),
                },
            )
            .map(|v| v["message"].as_str().unwrap().to_string())
            .collect();
        if denied.is_empty() {
            Ok(())
        } else {
            Err(denied)
        }
    }

    fn synthesized_canary() -> Value {
        let mut input = crate::tenant::FlaggerCanarySpecInput::new("acme", "web");
        input.hosts.push("web.acme.example".to_string());
        crate::tenant::synthesize_flagger_canary(&input).unwrap()
    }

    #[test]
    fn restrict_tenant_flagger_canaries_policy_is_enforced_at_stage0() {
        let policy = build_restrict_tenant_flagger_canaries_policy();
        assert_eq!(
            policy["metadata"]["name"],
            POLICY_RESTRICT_TENANT_FLAGGER_CANARIES
        );
        assert_eq!(policy["spec"]["validationActions"], json!(["Deny"]));
        assert_eq!(policy["spec"]["failurePolicy"], "Fail");
        assert_eq!(
            policy["spec"]["matchConstraints"]["resourceRules"],
            json!([{
                "apiGroups": ["flagger.app"],
                "apiVersions": ["*"],
                "operations": ["CREATE", "UPDATE"],
                "resources": ["canaries"]
            }])
        );
        let bundle = KyvernoManifestBundle::generate(&KyvernoPolicyConfig::default()).unwrap();
        assert!(bundle.supporting_manifests.contains(&policy));
    }

    #[test]
    fn restrict_tenant_flagger_canaries_policy_admits_synthesized_canaries() {
        let policy = build_restrict_tenant_flagger_canaries_policy();
        let canary = synthesized_canary();
        assert_eq!(
            evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &canary, None),
            Ok(())
        );
        let mut updated = canary.clone();
        updated["spec"]["analysis"]["maxWeight"] = json!(30);
        assert_eq!(
            evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &updated, Some(&canary)),
            Ok(())
        );
        // An AlertProvider in the Canary's own namespace is fine.
        let mut alerting = canary.clone();
        alerting["spec"]["analysis"]["alerts"] = json!([{ "name": "on-call", "providerRef": { "name": "slack", "namespace": "tenant-acme" } }]);
        assert_eq!(
            evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &alerting, None),
            Ok(())
        );
    }

    #[test]
    fn restrict_tenant_flagger_canaries_policy_denies_flagger_side_channels() {
        let policy = build_restrict_tenant_flagger_canaries_policy();
        let canary = synthesized_canary();
        type Mutation = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Mutation, &str)> = vec![
            (
                "webhook",
                Box::new(|c| {
                    c["spec"]["analysis"]["webhooks"] = json!([{
                        "name": "probe",
                        "url": "http://keel-server.keel-system:8080/api/v1/tenants",
                        "type": "pre-rollout"
                    }])
                }),
                "webhooks",
            ),
            (
                "inline query",
                Box::new(|c| {
                    c["spec"]["analysis"]["metrics"][0] = json!({
                        "name": "steal",
                        "query": "sum(rate(http_requests_total{namespace=\"tenant-other\"}[1m]))",
                        "thresholdRange": { "min": 0 }
                    })
                }),
                "MetricTemplate",
            ),
            (
                "inline query next to a platform templateRef",
                Box::new(|c| {
                    c["spec"]["analysis"]["metrics"][0]["query"] = json!("up");
                }),
                "MetricTemplate",
            ),
            (
                "foreign templateRef",
                Box::new(|c| {
                    c["spec"]["analysis"]["metrics"][1]["templateRef"]["namespace"] =
                        json!("tenant-other");
                }),
                "MetricTemplate",
            ),
            (
                "templateRef without namespace",
                Box::new(|c| {
                    c["spec"]["analysis"]["metrics"][1]["templateRef"]
                        .as_object_mut()
                        .unwrap()
                        .remove("namespace");
                }),
                "MetricTemplate",
            ),
            (
                "templateVariables",
                Box::new(|c| {
                    c["spec"]["analysis"]["metrics"][0]["templateVariables"] =
                        json!({ "namespace": "tenant-other" });
                }),
                "MetricTemplate",
            ),
            (
                "foreign gateway namespace",
                Box::new(|c| {
                    c["spec"]["service"]["gatewayRefs"][0]["namespace"] = json!("tenant-other");
                }),
                "gatewayRefs",
            ),
            (
                "foreign gateway name",
                Box::new(|c| {
                    c["spec"]["service"]["gatewayRefs"][0]["name"] = json!("internal");
                }),
                "gatewayRefs",
            ),
            (
                "second gateway",
                Box::new(|c| {
                    c["spec"]["service"]["gatewayRefs"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({ "name": "internal", "namespace": "keel-system" }));
                }),
                "gatewayRefs",
            ),
            (
                "no gateway",
                Box::new(|c| {
                    c["spec"]["service"]
                        .as_object_mut()
                        .unwrap()
                        .remove("gatewayRefs");
                }),
                "gatewayRefs",
            ),
            (
                "other provider",
                Box::new(|c| c["spec"]["provider"] = json!("nginx")),
                "spec.provider",
            ),
            (
                "no hosts",
                Box::new(|c| {
                    c["spec"]["service"]
                        .as_object_mut()
                        .unwrap()
                        .remove("hosts");
                }),
                "spec.service.hosts",
            ),
            (
                "empty hosts",
                Box::new(|c| c["spec"]["service"]["hosts"] = json!([])),
                "spec.service.hosts",
            ),
            (
                "blank host",
                Box::new(|c| c["spec"]["service"]["hosts"] = json!(["web.acme.example", ""])),
                "spec.service.hosts",
            ),
            (
                "wildcard host",
                Box::new(|c| c["spec"]["service"]["hosts"] = json!(["*.com"])),
                "spec.service.hosts",
            ),
            (
                "subdomain wildcard host",
                Box::new(|c| c["spec"]["service"]["hosts"] = json!(["*.acme.example"])),
                "spec.service.hosts",
            ),
            (
                "foreign alert provider",
                Box::new(|c| {
                    c["spec"]["analysis"]["alerts"] = json!([{
                        "name": "spam",
                        "providerRef": { "name": "platform-pager", "namespace": "keel-system" }
                    }])
                }),
                "AlertProvider",
            ),
        ];
        for (case, mutate, expected) in cases {
            let mut bad = canary.clone();
            mutate(&mut bad);
            let denied = evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &bad, None)
                .expect_err(case);
            assert!(
                denied.iter().any(|m| m.contains(expected)),
                "{case}: {denied:?}"
            );
            // Changing an admitted Canary into the bad shape is denied too.
            assert!(
                evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &bad, Some(&canary))
                    .is_err(),
                "{case} (update)"
            );
        }
    }

    #[test]
    fn restrict_tenant_flagger_canaries_policy_never_blocks_cleanup_or_platform_namespaces() {
        let policy = build_restrict_tenant_flagger_canaries_policy();
        let mut bad = synthesized_canary();
        bad["spec"]["analysis"]["webhooks"] = json!([{ "name": "x", "url": "http://x" }]);
        // Metadata-only updates (finalizers, annotations) and deletion of an existing Canary.
        let mut annotated = bad.clone();
        annotated["metadata"]["annotations"] = json!({ "reconcile.fluxcd.io/requestedAt": "now" });
        assert_eq!(
            evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &annotated, Some(&bad)),
            Ok(())
        );
        let mut deleting = bad.clone();
        deleting["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        deleting["spec"]["analysis"]["maxWeight"] = json!(10);
        assert_eq!(
            evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &deleting, Some(&bad)),
            Ok(())
        );
        // Platform namespaces never reach the policy.
        assert_eq!(
            evaluate_validating_policy(&policy, "CREATE", "keel-system", &bad, None),
            Ok(())
        );
    }

    fn tenant_route(kind: &str, hostnames: Option<Value>, parent_refs: Value) -> Value {
        let mut route = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": kind,
            "metadata": { "name": "web", "namespace": "tenant-acme" },
            "spec": {
                "parentRefs": parent_refs,
                "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
            }
        });
        if let Some(hostnames) = hostnames {
            route["spec"]["hostnames"] = hostnames;
        }
        route
    }

    #[test]
    fn restrict_tenant_route_hostnames_policy_denies_catch_all_routes_on_shared_gateway() {
        let policy =
            build_restrict_tenant_route_hostnames_policy(&default_reserved_route_hostnames())
                .unwrap();
        let shared =
            json!([{ "name": "keel-ingress", "namespace": "keel-system", "sectionName": "https" }]);
        let catch_all = "must set `spec.hostnames`";
        for kind in ["HTTPRoute", "GRPCRoute", "TLSRoute"] {
            // Missing or empty hostnames on keel-ingress: denied (any writer, incl. Flagger).
            for hostnames in [None, Some(json!([]))] {
                let route = tenant_route(kind, hostnames, shared.clone());
                let denied =
                    evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &route, None)
                        .expect_err(kind);
                assert!(denied.iter().any(|m| m.contains(catch_all)), "{denied:?}");
            }
            // With a hostname: admitted.
            let route = tenant_route(kind, Some(json!(["web.acme.example"])), shared.clone());
            assert_eq!(
                evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &route, None),
                Ok(())
            );
        }
        // Explicit default kind/group still count as the shared Gateway.
        let explicit = json!([{ "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": "keel-ingress", "namespace": "keel-system" }]);
        assert!(
            evaluate_validating_policy(
                &policy,
                "CREATE",
                "tenant-acme",
                &tenant_route("HTTPRoute", None, explicit),
                None
            )
            .is_err()
        );
        // A route that only attaches inside its own namespace (no cross-namespace Gateway) or to
        // a Service (GAMMA) is not a shared-listener catch-all.
        for local in [
            json!([{ "name": "own-gateway" }]),
            json!([{ "name": "own-gateway", "namespace": "tenant-acme" }]),
            json!([{ "group": "", "kind": "Service", "name": "web", "namespace": "keel-system" }]),
        ] {
            assert_eq!(
                evaluate_validating_policy(
                    &policy,
                    "CREATE",
                    "tenant-acme",
                    &tenant_route("HTTPRoute", None, local.clone()),
                    None
                ),
                Ok(()),
                "{local}"
            );
        }
        // Spec-preserving updates and deletion of a pre-existing host-less route always pass.
        let old = tenant_route("HTTPRoute", None, shared.clone());
        let mut annotated = old.clone();
        annotated["metadata"]["annotations"] = json!({ "flagger.app/x": "y" });
        assert_eq!(
            evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &annotated, Some(&old)),
            Ok(())
        );
        let mut deleting = old.clone();
        deleting["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert_eq!(
            evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &deleting, Some(&old)),
            Ok(())
        );
        // A route held in `Terminating` (tenant finalizer) cannot drop its hostnames: only
        // spec-preserving updates are exempt, finalizer removal still passes.
        let hosted_terminating = {
            let mut r = tenant_route(
                "HTTPRoute",
                Some(json!(["web.acme.example"])),
                shared.clone(),
            );
            r["metadata"]["finalizers"] = json!(["acme.example/hold"]);
            r["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
            r
        };
        for hostnames in [None, Some(json!([]))] {
            let mut dropped = tenant_route("HTTPRoute", hostnames, shared.clone());
            dropped["metadata"] = hosted_terminating["metadata"].clone();
            let denied = evaluate_validating_policy(
                &policy,
                "UPDATE",
                "tenant-acme",
                &dropped,
                Some(&hosted_terminating),
            )
            .expect_err("terminating route dropping hostnames");
            assert!(denied.iter().any(|m| m.contains(catch_all)), "{denied:?}");
        }
        let mut released = hosted_terminating.clone();
        released["metadata"]["finalizers"] = json!([]);
        assert_eq!(
            evaluate_validating_policy(
                &policy,
                "UPDATE",
                "tenant-acme",
                &released,
                Some(&hosted_terminating)
            ),
            Ok(())
        );
        // Dropping the hostnames of an admitted route is denied.
        let hosted = tenant_route(
            "HTTPRoute",
            Some(json!(["web.acme.example"])),
            shared.clone(),
        );
        assert!(
            evaluate_validating_policy(&policy, "UPDATE", "tenant-acme", &old, Some(&hosted))
                .is_err()
        );
        // Reserved platform hosts stay denied; platform namespaces never reach the policy.
        let idp = tenant_route("HTTPRoute", Some(json!(["idp.keel.local"])), shared.clone());
        assert!(evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &idp, None).is_err());
        assert_eq!(
            evaluate_validating_policy(
                &policy,
                "CREATE",
                "keel-system",
                &tenant_route("HTTPRoute", None, shared),
                None
            ),
            Ok(())
        );
    }

    #[test]
    fn restrict_tenant_route_hostnames_policy_denies_tld_wide_wildcards() {
        let policy =
            build_restrict_tenant_route_hostnames_policy(&default_reserved_route_hostnames())
                .unwrap();
        let shared = json!([{ "name": "keel-ingress", "namespace": "keel-system" }]);
        for hostnames in [
            json!(["*.com"]),
            json!(["web.acme.example", "*.io"]),
            json!(["*.dev", "*.net"]),
        ] {
            for parents in [shared.clone(), json!([{ "name": "own-gateway" }])] {
                let route = tenant_route("HTTPRoute", Some(hostnames.clone()), parents);
                let denied =
                    evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &route, None)
                        .expect_err("TLD-wide wildcard");
                assert!(
                    denied.iter().any(|m| m.contains("whole top-level domain")),
                    "{hostnames}: {denied:?}"
                );
            }
        }
        // A wildcard under a registrable domain (two labels after `*.`) is admitted.
        let route = tenant_route("HTTPRoute", Some(json!(["*.acme.example"])), shared.clone());
        assert_eq!(
            evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &route, None),
            Ok(())
        );
        // Turning an admitted route into a TLD-wide wildcard is denied, even while terminating.
        let mut terminating = route.clone();
        terminating["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        let mut widened = terminating.clone();
        widened["spec"]["hostnames"] = json!(["*.com"]);
        assert!(
            evaluate_validating_policy(
                &policy,
                "UPDATE",
                "tenant-acme",
                &widened,
                Some(&terminating)
            )
            .is_err()
        );
    }

    /// A Canary held in `Terminating` cannot drop or widen its hosts.
    #[test]
    fn restrict_tenant_flagger_canaries_policy_keeps_hosts_while_terminating() {
        let policy = build_restrict_tenant_flagger_canaries_policy();
        let mut terminating = synthesized_canary();
        terminating["metadata"]["finalizers"] = json!(["acme.example/hold"]);
        terminating["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        for hosts in [json!([]), json!(["*.com"])] {
            let mut bad = terminating.clone();
            bad["spec"]["service"]["hosts"] = hosts.clone();
            let denied = evaluate_validating_policy(
                &policy,
                "UPDATE",
                "tenant-acme",
                &bad,
                Some(&terminating),
            )
            .expect_err("terminating canary changing hosts");
            assert!(
                denied.iter().any(|m| m.contains("spec.service.hosts")),
                "{hosts}: {denied:?}"
            );
        }
        let mut released = terminating.clone();
        released["metadata"]["finalizers"] = json!([]);
        assert_eq!(
            evaluate_validating_policy(
                &policy,
                "UPDATE",
                "tenant-acme",
                &released,
                Some(&terminating)
            ),
            Ok(())
        );
    }

    /// The route Flagger generates from a Keel-synthesized Canary (hostnames = `spec.service.hosts`)
    /// is admitted; the one it would generate without hosts is not.
    #[test]
    fn flagger_generated_routes_from_synthesized_canaries_are_admitted() {
        let policy =
            build_restrict_tenant_route_hostnames_policy(&default_reserved_route_hostnames())
                .unwrap();
        let canary = synthesized_canary();
        let parent_refs = canary["spec"]["service"]["gatewayRefs"].clone();
        let route = tenant_route(
            "HTTPRoute",
            Some(canary["spec"]["service"]["hosts"].clone()),
            parent_refs.clone(),
        );
        assert_eq!(
            evaluate_validating_policy(&policy, "CREATE", "tenant-acme", &route, None),
            Ok(())
        );
        assert!(
            evaluate_validating_policy(
                &policy,
                "CREATE",
                "tenant-acme",
                &tenant_route("HTTPRoute", None, parent_refs),
                None
            )
            .is_err()
        );
    }
}
