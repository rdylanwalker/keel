//! PKO v2 `Stack` (`pulumi.com/v1`) and Flux CD `GitRepository` (`source.toolkit.fluxcd.io/v1`)
//! Custom Resource synthesis for management anti-lockout stacks, ephemeral PR previews,
//! and multi-tenant infrastructure stacks with companion output `Secret` bindings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::PkoError;

/// Kubernetes API version for PKO v2 `Stack` resources.
pub const PKO_STACK_API_VERSION: &str = "pulumi.com/v1";
/// Kubernetes Kind for PKO v2 `Stack` resources.
pub const PKO_STACK_KIND: &str = "Stack";

/// Kubernetes API version for Flux CD `GitRepository` resources.
pub const FLUX_GIT_REPO_API_VERSION: &str = "source.toolkit.fluxcd.io/v1";
/// Kubernetes Kind for Flux CD `GitRepository` resources.
pub const FLUX_GIT_REPO_KIND: &str = "GitRepository";

/// Canonical resource name for the protected Stage 0/1 seed management cluster stack.
pub const MANAGEMENT_CORE_STACK_NAME: &str = "management-cluster-core";

/// Default in-cluster S3-compatible Pulumi state backend URL (`keel-system/s3-state-backend-seaweedfs-s3`).
pub const IN_CLUSTER_S3_BACKEND_URL: &str = "s3://pulumi-state?endpoint=s3-state-backend-seaweedfs-s3.keel-system.svc.cluster.local:8333&region=us-east-1&s3ForcePathStyle=true&disableSSL=true";

pub use keel_core::{FLUX_SYSTEM_NAMESPACE, KEEL_PREVIEWS_NAMESPACE};

/// Default outbound pull-polling interval for NAT/firewall resilience (`AGENTS.md` §3.1).
pub const DEFAULT_FLUX_POLL_INTERVAL: &str = "1m";
/// Default read-only ServiceAccount used by ephemeral preview workspace pods (`SDD.md` §4.2).
pub const DEFAULT_PREVIEW_SERVICE_ACCOUNT: &str = "keel-preview-readonly";
/// Default ServiceAccount used by the seed management cluster core stack.
pub const DEFAULT_MANAGEMENT_SERVICE_ACCOUNT: &str = "keel-management-pko";
/// 10-minute termination grace period for clean Pulumi state unlocks (`NFR-REL-04`).
pub const DEFAULT_TERMINATION_GRACE_PERIOD_SECONDS: u64 = 600;

/// Standard Kubernetes and Keel metadata label/annotation keys.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const LABEL_STACK_ROLE: &str = "keel.io/stack-role";
pub const LABEL_PROTECT: &str = "keel.io/protect";
pub const LABEL_EPHEMERAL_PREVIEW: &str = "keel.io/ephemeral-preview";
pub const LABEL_PR_ID: &str = "keel.io/pr-id";
pub const LABEL_TENANT: &str = "keel.io/tenant";
pub const ANNOTATION_BRANCH: &str = "keel.io/branch";

/// Standard Kubernetes `metadata` block for synthesized CRs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrObjectMeta {
    pub name: String,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// Local Kubernetes `Secret` or object reference (`{ name: ... }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalObjectReference {
    pub name: String,
}

/// Git reference selector for a Flux `GitRepository` CR.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FluxGitRepositoryRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Spec for a Flux CD `source.toolkit.fluxcd.io/v1` `GitRepository` CR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FluxGitRepositorySpec {
    pub interval: String,
    pub url: String,
    #[serde(rename = "ref")]
    pub git_ref: FluxGitRepositoryRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<LocalObjectReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// Typed Flux CD `source.toolkit.fluxcd.io/v1` `GitRepository` Custom Resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FluxGitRepository {
    pub api_version: String,
    pub kind: String,
    pub metadata: CrObjectMeta,
    pub spec: FluxGitRepositorySpec,
}

impl FluxGitRepository {
    /// Serializes this `GitRepository` CR to a `serde_json::Value`.
    pub fn to_json_value(&self) -> Result<serde_json::Value, PkoError> {
        Ok(serde_json::to_value(self)?)
    }

    /// Deserializes a `GitRepository` CR from a `serde_json::Value`.
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, PkoError> {
        Ok(serde_json::from_value(value)?)
    }

    /// Serializes this `GitRepository` CR to a YAML manifest string.
    pub fn to_yaml(&self) -> Result<String, PkoError> {
        Ok(serde_yml::to_string(self)?)
    }

    /// Deserializes a `GitRepository` CR from a YAML manifest string.
    pub fn from_yaml(yaml: &str) -> Result<Self, PkoError> {
        Ok(serde_yml::from_str(yaml)?)
    }
}

/// Flux source reference inside a PKO v2 `Stack` (`spec.fluxSource.sourceRef`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoFluxSourceRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
}

/// Flux source binding inside a PKO v2 `Stack` (`spec.fluxSource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoFluxSource {
    pub source_ref: PkoFluxSourceRef,
    pub dir: String,
}

impl PkoFluxSource {
    /// Creates a `PkoFluxSource` referencing a same-namespace Flux `GitRepository`.
    pub fn git_repository(name: impl Into<String>, dir: impl Into<String>) -> Self {
        Self {
            source_ref: PkoFluxSourceRef {
                api_version: FLUX_GIT_REPO_API_VERSION.to_string(),
                kind: FLUX_GIT_REPO_KIND.to_string(),
                name: name.into(),
            },
            dir: dir.into(),
        }
    }
}

/// Secret key selector for environment variable injection in PKO workspace pods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeySelector {
    pub name: String,
    pub key: String,
}

/// Environment variable reference (`spec.envRefs`) for PKO v2 `Stack` CRs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoEnvRef {
    #[serde(rename = "type")]
    pub selector_type: String,
    pub secret: SecretKeySelector,
}

impl PkoEnvRef {
    /// References a key inside a namespace-local Kubernetes `Secret`.
    pub fn from_secret(secret_name: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            selector_type: "Secret".to_string(),
            secret: SecretKeySelector {
                name: secret_name.into(),
                key: key.into(),
            },
        }
    }
}

/// Seccomp profile configuration (`RuntimeDefault`) for `restricted` Pod Security Standards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeccompProfile {
    #[serde(rename = "type")]
    pub profile_type: String,
}

/// Linux capabilities configuration (`drop: ["ALL"]`) for `restricted` Pod Security Standards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub drop: Vec<String>,
}

/// Pod-level security context enforcing non-root execution (`NFR-SEC-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodSecurityContext {
    pub run_as_non_root: bool,
    pub run_as_user: u64,
    pub seccomp_profile: SeccompProfile,
}

/// Container-level security context enforcing `restricted` PSS (`NFR-SEC-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSecurityContext {
    pub allow_privilege_escalation: bool,
    pub read_only_root_filesystem: bool,
    pub capabilities: Capabilities,
}

/// Environment variable entry on a PKO workspace container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEnvVar {
    pub name: String,
    pub value: String,
}

/// Volume mount entry on a PKO workspace container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceVolumeMount {
    pub name: String,
    pub mount_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// Secret volume source for mounting namespace-local Secrets into a PKO workspace pod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSecretVolumeSource {
    pub secret_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

/// Volume entry on a PKO workspace pod spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceVolume {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<WorkspaceSecretVolumeSource>,
}

/// Container entry inside `WorkspacePodSpec` enforcing `restricted` PSS on the `pulumi` container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceContainer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<WorkspaceEnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<WorkspaceVolumeMount>,
    pub security_context: ContainerSecurityContext,
}

/// Workspace pod spec configuration for PKO v2 (`NFR-SEC-03`, `NFR-REL-04`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePodSpec {
    pub termination_grace_period_seconds: u64,
    pub security_context: PodSecurityContext,
    pub containers: Vec<WorkspaceContainer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<WorkspaceVolume>,
}

/// Pod template wrapper inside `PkoWorkspaceTemplate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePodTemplate {
    pub spec: WorkspacePodSpec,
}

/// Workspace spec inside `PkoWorkspaceTemplate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoWorkspaceTemplateSpec {
    pub pod_template: WorkspacePodTemplate,
}

/// PKO v2 `workspaceTemplate` enforcing `restricted` Pod Security Standards and graceful state unlocking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoWorkspaceTemplate {
    pub spec: PkoWorkspaceTemplateSpec,
}

impl PkoWorkspaceTemplate {
    /// Returns the hardened default workspace template compliant with `NFR-SEC-03` and `NFR-REL-04`.
    pub fn restricted_default() -> Self {
        Self {
            spec: PkoWorkspaceTemplateSpec {
                pod_template: WorkspacePodTemplate {
                    spec: WorkspacePodSpec {
                        termination_grace_period_seconds: DEFAULT_TERMINATION_GRACE_PERIOD_SECONDS,
                        security_context: PodSecurityContext {
                            run_as_non_root: true,
                            run_as_user: 1000,
                            seccomp_profile: SeccompProfile {
                                profile_type: "RuntimeDefault".to_string(),
                            },
                        },
                        containers: vec![WorkspaceContainer {
                            name: "pulumi".to_string(),
                            env: vec![WorkspaceEnvVar {
                                name: "PULUMI_CONFIG_PASSPHRASE".to_string(),
                                value: String::new(),
                            }],
                            volume_mounts: Vec::new(),
                            security_context: ContainerSecurityContext {
                                allow_privilege_escalation: false,
                                read_only_root_filesystem: false,
                                capabilities: Capabilities {
                                    drop: vec!["ALL".to_string()],
                                },
                            },
                        }],
                        volumes: Vec::new(),
                    },
                },
            },
        }
    }

    /// Appends an environment variable to the `pulumi` workspace container.
    pub fn with_env_var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let Some(container) = self
            .spec
            .pod_template
            .spec
            .containers
            .iter_mut()
            .find(|c| c.name == "pulumi")
        {
            container.env.push(WorkspaceEnvVar {
                name: name.into(),
                value: value.into(),
            });
        }
        self
    }

    /// Mounts a namespace-local Kubernetes `Secret` read-only into the `pulumi` workspace container.
    pub fn with_secret_volume(
        mut self,
        volume_name: impl Into<String>,
        secret_name: impl Into<String>,
        mount_path: impl Into<String>,
    ) -> Self {
        let volume_name = volume_name.into();
        self.spec.pod_template.spec.volumes.push(WorkspaceVolume {
            name: volume_name.clone(),
            secret: Some(WorkspaceSecretVolumeSource {
                secret_name: secret_name.into(),
                optional: Some(true),
            }),
        });
        if let Some(container) = self
            .spec
            .pod_template
            .spec
            .containers
            .iter_mut()
            .find(|c| c.name == "pulumi")
        {
            container.volume_mounts.push(WorkspaceVolumeMount {
                name: volume_name,
                mount_path: mount_path.into(),
                read_only: Some(true),
            });
        }
        self
    }
}

/// Spec for a PKO v2 `pulumi.com/v1` `Stack` CR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoStackSpec {
    pub stack: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_account_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flux_source: Option<PkoFluxSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destroy_on_finalize: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_resync_on_commit_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resync_frequency_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_secret_name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_template: Option<PkoWorkspaceTemplate>,
}

/// Flux `HelmRelease` `spec.valuesFrom` Secret reference for consuming companion `PkoStack` outputs (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelmValuesFromRef {
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

/// Typed PKO v2 `pulumi.com/v1` `Stack` Custom Resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoStack {
    pub api_version: String,
    pub kind: String,
    pub metadata: CrObjectMeta,
    pub spec: PkoStackSpec,
}

impl PkoStack {
    /// Serializes this `Stack` CR to a `serde_json::Value`.
    pub fn to_json_value(&self) -> Result<serde_json::Value, PkoError> {
        Ok(serde_json::to_value(self)?)
    }

    /// Deserializes a `Stack` CR from a `serde_json::Value`.
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, PkoError> {
        Ok(serde_json::from_value(value)?)
    }

    /// Serializes this `Stack` CR to a YAML manifest string.
    pub fn to_yaml(&self) -> Result<String, PkoError> {
        Ok(serde_yml::to_string(self)?)
    }

    /// Deserializes a `Stack` CR from a YAML manifest string.
    pub fn from_yaml(yaml: &str) -> Result<Self, PkoError> {
        Ok(serde_yml::from_str(yaml)?)
    }

    /// Returns `true` if anti-lockout protection is active on this stack.
    pub fn is_protected(&self) -> bool {
        self.metadata.labels.get(LABEL_PROTECT).map(String::as_str) == Some("true")
            || self.spec.config.get("protect").map(String::as_str) == Some("true")
    }

    /// Returns `true` if this stack runs strictly in ephemeral preview mode (`spec.preview: true`).
    pub fn is_preview(&self) -> bool {
        self.spec.preview == Some(true)
    }

    /// Produces a Flux `HelmRelease` `spec.valuesFrom` Secret reference if `output_secret_name` is bound.
    pub fn helm_values_from_ref(&self) -> Option<HelmValuesFromRef> {
        self.spec
            .output_secret_name
            .as_ref()
            .map(|secret_name| HelmValuesFromRef {
                kind: "Secret".to_string(),
                name: secret_name.clone(),
                values_key: None,
                target_path: None,
                optional: Some(false),
            })
    }
}

/// Synthesized bundle of ephemeral PR preview CRs in `keel-previews` (`SDD.md` §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewStack {
    pub pr_id: String,
    pub tenant: String,
    pub branch: String,
    pub git_repository: FluxGitRepository,
    pub stack: PkoStack,
}

impl PreviewStack {
    /// Renders both the Flux `GitRepository` and PKO `Stack` as a multi-document YAML manifest.
    pub fn to_multi_doc_yaml(&self) -> Result<String, PkoError> {
        let repo_yaml = self.git_repository.to_yaml()?;
        let stack_yaml = self.stack.to_yaml()?;
        Ok(format!(
            "---\n{}---\n{}",
            ensure_trailing_newline(&repo_yaml),
            stack_yaml
        ))
    }

    /// Converts both the Flux `GitRepository` and PKO `Stack` into `(GitRepository, Stack)` JSON values.
    pub fn to_json_values(&self) -> Result<(serde_json::Value, serde_json::Value), PkoError> {
        Ok((
            self.git_repository.to_json_value()?,
            self.stack.to_json_value()?,
        ))
    }
}

/// Parameters for synthesizing the anti-lockout `management-cluster-core` `Stack` CR.
#[derive(Debug, Clone)]
pub struct ManagementCoreStackParams {
    pub namespace: String,
    pub pulumi_stack_name: String,
    pub backend_url: String,
    pub git_source_name: String,
    pub repo_dir: String,
    pub service_account_name: Option<String>,
    pub config: BTreeMap<String, String>,
    pub env_refs: BTreeMap<String, PkoEnvRef>,
}

impl ManagementCoreStackParams {
    /// Creates standard parameters for `management-cluster-core` in `flux-system`.
    pub fn new(
        backend_url: impl Into<String>,
        git_source_name: impl Into<String>,
        repo_dir: impl Into<String>,
    ) -> Self {
        Self {
            namespace: FLUX_SYSTEM_NAMESPACE.to_string(),
            pulumi_stack_name: format!("organization/{MANAGEMENT_CORE_STACK_NAME}/prod"),
            backend_url: backend_url.into(),
            git_source_name: git_source_name.into(),
            repo_dir: repo_dir.into(),
            service_account_name: None,
            config: BTreeMap::new(),
            env_refs: BTreeMap::new(),
        }
    }
}

/// Parameters for synthesizing an ephemeral PR preview (`GitRepository` + PKO `Stack` in `keel-previews`).
#[derive(Debug, Clone)]
pub struct EphemeralPreviewParams {
    pub pr_id: String,
    pub tenant: String,
    pub branch: String,
    pub repo_url: String,
    pub repo_dir: String,
    pub pulumi_stack_name: String,
    pub backend_url: String,
    pub preview_service_account: Option<String>,
    pub git_secret_name: Option<String>,
    pub config: BTreeMap<String, String>,
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    /// Optional `DOCKER_HOST` endpoint injected into the PKO workspace pod for local `kind`-in-`kind` dev.
    pub docker_host: Option<String>,
}

impl EphemeralPreviewParams {
    /// Constructs ephemeral preview parameters and builds a canonical `keel/<tenant>/<action>-<timestamp>` branch.
    #[allow(clippy::too_many_arguments)]
    pub fn for_action(
        pr_id: impl Into<String>,
        tenant: impl Into<String>,
        action: impl Into<String>,
        timestamp: u64,
        repo_url: impl Into<String>,
        repo_dir: impl Into<String>,
        pulumi_stack_name: impl Into<String>,
        backend_url: impl Into<String>,
    ) -> Result<Self, PkoError> {
        let tenant_str = tenant.into();
        let action_str = action.into();
        let branch = build_preview_branch(&tenant_str, &action_str, timestamp)?;
        Ok(Self {
            pr_id: pr_id.into(),
            tenant: tenant_str,
            branch,
            repo_url: repo_url.into(),
            repo_dir: repo_dir.into(),
            pulumi_stack_name: pulumi_stack_name.into(),
            backend_url: backend_url.into(),
            preview_service_account: None,
            git_secret_name: None,
            config: BTreeMap::new(),
            env_refs: BTreeMap::new(),
            docker_host: None,
        })
    }
}

/// Parameters for synthesizing a multi-tenant infrastructure `Stack` CR in `tenant-<tenant>`.
#[derive(Debug, Clone)]
pub struct TenantStackParams {
    pub tenant: String,
    pub name: String,
    pub pulumi_stack_name: String,
    pub backend_url: String,
    pub git_source_name: String,
    pub repo_dir: String,
    pub service_account_name: Option<String>,
    pub output_secret_name: Option<String>,
    pub config: BTreeMap<String, String>,
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    /// Optional `DOCKER_HOST` endpoint injected into the PKO workspace pod for local `kind`-in-`kind` dev.
    pub docker_host: Option<String>,
}

impl TenantStackParams {
    /// Creates standard parameters for a tenant infrastructure stack.
    pub fn new(
        tenant: impl Into<String>,
        name: impl Into<String>,
        pulumi_stack_name: impl Into<String>,
        backend_url: impl Into<String>,
        git_source_name: impl Into<String>,
        repo_dir: impl Into<String>,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            name: name.into(),
            pulumi_stack_name: pulumi_stack_name.into(),
            backend_url: backend_url.into(),
            git_source_name: git_source_name.into(),
            repo_dir: repo_dir.into(),
            service_account_name: None,
            output_secret_name: None,
            config: BTreeMap::new(),
            env_refs: BTreeMap::new(),
            docker_host: None,
        }
    }
}

/// Builds a canonical proposal branch `keel/<tenant>/<action>-<timestamp>`.
pub fn build_preview_branch(
    tenant: &str,
    action: &str,
    timestamp: u64,
) -> Result<String, PkoError> {
    validate_k8s_dns_label("tenant", tenant)?;
    validate_k8s_dns_label("action", action)?;
    if timestamp == 0 {
        return Err(PkoError::Validation(
            "timestamp must be greater than zero".to_string(),
        ));
    }
    Ok(format!("keel/{tenant}/{action}-{timestamp}"))
}

/// Validates that a proposal branch conforms to `keel/<tenant>/<action>-<timestamp>`.
pub fn validate_preview_branch(tenant: &str, branch: &str) -> Result<(), PkoError> {
    let expected_prefix = format!("keel/{tenant}/");
    let Some(rest) = branch.strip_prefix(&expected_prefix) else {
        return Err(PkoError::Validation(format!(
            "preview branch '{branch}' must start with '{expected_prefix}' (<action>-<timestamp>)"
        )));
    };

    let Some((action, ts_part)) = rest.rsplit_once('-') else {
        return Err(PkoError::Validation(format!(
            "preview branch '{branch}' must end with '-<timestamp>'"
        )));
    };

    validate_k8s_dns_label("action", action)?;
    if ts_part.is_empty() || !ts_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(PkoError::Validation(format!(
            "preview branch '{branch}' has invalid numeric timestamp '{ts_part}'"
        )));
    }

    Ok(())
}

/// Synthesizes the Stage 0/1 `management-cluster-core` PKO v2 `Stack` CR with mandatory
/// `protect: true` anti-lockout guardrails (`SDD.md` §2.1, §2.2, `NFR-REL-01`).
pub fn synthesize_management_core_stack(
    params: &ManagementCoreStackParams,
) -> Result<PkoStack, PkoError> {
    validate_k8s_dns_label("namespace", &params.namespace)?;
    validate_non_empty("pulumi_stack_name", &params.pulumi_stack_name)?;
    validate_non_empty("backend_url", &params.backend_url)?;
    validate_k8s_name("git_source_name", &params.git_source_name)?;
    validate_repo_dir(&params.repo_dir)?;

    let sa = match &params.service_account_name {
        Some(custom) => {
            validate_k8s_name("service_account_name", custom)?;
            custom.clone()
        }
        None => DEFAULT_MANAGEMENT_SERVICE_ACCOUNT.to_string(),
    };

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), "keel".to_string());
    labels.insert(LABEL_STACK_ROLE.to_string(), "management-core".to_string());
    labels.insert(LABEL_PROTECT.to_string(), "true".to_string());

    let mut config = params.config.clone();
    // Enforce anti-lockout protection unconditionally regardless of caller-supplied config.
    config.insert("protect".to_string(), "true".to_string());
    config.insert("keel:protect".to_string(), "true".to_string());

    Ok(PkoStack {
        api_version: PKO_STACK_API_VERSION.to_string(),
        kind: PKO_STACK_KIND.to_string(),
        metadata: CrObjectMeta {
            name: MANAGEMENT_CORE_STACK_NAME.to_string(),
            namespace: params.namespace.clone(),
            labels,
            annotations: BTreeMap::new(),
        },
        spec: PkoStackSpec {
            stack: params.pulumi_stack_name.clone(),
            service_account_name: Some(sa),
            backend: Some(params.backend_url.clone()),
            flux_source: Some(PkoFluxSource::git_repository(
                &params.git_source_name,
                &params.repo_dir,
            )),
            preview: None,
            destroy_on_finalize: Some(false),
            continue_resync_on_commit_match: Some(true),
            resync_frequency_seconds: Some(300),
            output_secret_name: None,
            config,
            env_refs: params.env_refs.clone(),
            workspace_template: Some(PkoWorkspaceTemplate::restricted_default()),
        },
    })
}

/// Synthesizes an ephemeral Flux `GitRepository` pinned to `keel/<tenant>/<action>-<timestamp>`
/// and a PKO v2 `Stack` CR with `preview: true` in `keel-previews` using a read-only preview
/// ServiceAccount (`SDD.md` §4.2, `NFR-REL-02`).
pub fn synthesize_ephemeral_preview(
    params: &EphemeralPreviewParams,
) -> Result<PreviewStack, PkoError> {
    let normalized_pr_id = sanitize_pr_id(&params.pr_id)?;
    validate_k8s_dns_label("tenant", &params.tenant)?;
    validate_preview_branch(&params.tenant, &params.branch)?;
    validate_non_empty("repo_url", &params.repo_url)?;
    validate_repo_dir(&params.repo_dir)?;
    validate_non_empty("pulumi_stack_name", &params.pulumi_stack_name)?;
    validate_non_empty("backend_url", &params.backend_url)?;

    let preview_sa = match &params.preview_service_account {
        Some(sa) => {
            validate_k8s_name("preview_service_account", sa)?;
            sa.clone()
        }
        None => DEFAULT_PREVIEW_SERVICE_ACCOUNT.to_string(),
    };

    let secret_ref = match &params.git_secret_name {
        Some(secret) => {
            validate_k8s_name("git_secret_name", secret)?;
            Some(LocalObjectReference {
                name: secret.clone(),
            })
        }
        None => None,
    };

    let git_repo_name = format!("preview-source-pr-{normalized_pr_id}");
    let stack_name = format!("preview-stack-pr-{normalized_pr_id}");

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), "keel".to_string());
    labels.insert(LABEL_EPHEMERAL_PREVIEW.to_string(), "true".to_string());
    labels.insert(
        LABEL_STACK_ROLE.to_string(),
        "ephemeral-preview".to_string(),
    );
    labels.insert(LABEL_PR_ID.to_string(), normalized_pr_id.clone());
    labels.insert(LABEL_TENANT.to_string(), params.tenant.clone());

    let mut annotations = BTreeMap::new();
    annotations.insert(ANNOTATION_BRANCH.to_string(), params.branch.clone());

    let git_repository = FluxGitRepository {
        api_version: FLUX_GIT_REPO_API_VERSION.to_string(),
        kind: FLUX_GIT_REPO_KIND.to_string(),
        metadata: CrObjectMeta {
            name: git_repo_name.clone(),
            namespace: KEEL_PREVIEWS_NAMESPACE.to_string(),
            labels: labels.clone(),
            annotations: annotations.clone(),
        },
        spec: FluxGitRepositorySpec {
            interval: DEFAULT_FLUX_POLL_INTERVAL.to_string(),
            url: params.repo_url.clone(),
            git_ref: FluxGitRepositoryRef {
                branch: Some(params.branch.clone()),
                tag: None,
                commit: None,
            },
            secret_ref,
            timeout: Some("60s".to_string()),
        },
    };

    let stack = PkoStack {
        api_version: PKO_STACK_API_VERSION.to_string(),
        kind: PKO_STACK_KIND.to_string(),
        metadata: CrObjectMeta {
            name: stack_name,
            namespace: KEEL_PREVIEWS_NAMESPACE.to_string(),
            labels,
            annotations,
        },
        spec: PkoStackSpec {
            stack: params.pulumi_stack_name.clone(),
            service_account_name: Some(preview_sa),
            backend: Some(params.backend_url.clone()),
            flux_source: Some(PkoFluxSource::git_repository(
                &git_repo_name,
                &params.repo_dir,
            )),
            preview: Some(true),
            destroy_on_finalize: Some(false),
            continue_resync_on_commit_match: Some(false),
            resync_frequency_seconds: None,
            output_secret_name: None,
            config: params.config.clone(),
            env_refs: params.env_refs.clone(),
            workspace_template: Some(match &params.docker_host {
                Some(host) => {
                    PkoWorkspaceTemplate::restricted_default().with_env_var("DOCKER_HOST", host)
                }
                None => PkoWorkspaceTemplate::restricted_default(),
            }),
        },
    };

    Ok(PreviewStack {
        pr_id: normalized_pr_id,
        tenant: params.tenant.clone(),
        branch: params.branch.clone(),
        git_repository,
        stack,
    })
}

/// Synthesizes a tenant PKO v2 `Stack` CR strictly isolated to `tenant-<tenant>` with optional
/// `output_secret_name` binding for Flux `HelmRelease` `spec.valuesFrom` consumption (`SDD.md` §5.1, §6.2).
pub fn synthesize_tenant_stack(params: &TenantStackParams) -> Result<PkoStack, PkoError> {
    validate_k8s_dns_label("tenant", &params.tenant)?;
    validate_k8s_name("name", &params.name)?;
    validate_non_empty("pulumi_stack_name", &params.pulumi_stack_name)?;
    validate_non_empty("backend_url", &params.backend_url)?;
    validate_k8s_name("git_source_name", &params.git_source_name)?;
    validate_repo_dir(&params.repo_dir)?;

    if params.name == MANAGEMENT_CORE_STACK_NAME {
        return Err(PkoError::Validation(format!(
            "tenant stack cannot use reserved name '{MANAGEMENT_CORE_STACK_NAME}'"
        )));
    }

    let tenant_namespace = format!("tenant-{}", params.tenant);

    let service_account = match &params.service_account_name {
        Some(sa) => {
            validate_k8s_name("service_account_name", sa)?;
            sa.clone()
        }
        None => format!("tenant-{}-sa", params.tenant),
    };

    let output_secret_name = match &params.output_secret_name {
        Some(secret_name) => {
            validate_k8s_name("output_secret_name", secret_name)?;
            Some(secret_name.clone())
        }
        None => None,
    };

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), "keel".to_string());
    labels.insert(LABEL_STACK_ROLE.to_string(), "tenant".to_string());
    labels.insert(LABEL_TENANT.to_string(), params.tenant.clone());
    if let Some(provider) = params.config.get("keel:provider") {
        labels.insert("keel.io/provider".to_string(), provider.clone());
    }

    Ok(PkoStack {
        api_version: PKO_STACK_API_VERSION.to_string(),
        kind: PKO_STACK_KIND.to_string(),
        metadata: CrObjectMeta {
            name: params.name.clone(),
            namespace: tenant_namespace.clone(),
            labels,
            annotations: BTreeMap::new(),
        },
        spec: PkoStackSpec {
            stack: params.pulumi_stack_name.clone(),
            service_account_name: Some(service_account),
            backend: Some(params.backend_url.clone()),
            flux_source: Some(PkoFluxSource::git_repository(
                &params.git_source_name,
                &params.repo_dir,
            )),
            preview: None,
            destroy_on_finalize: Some(true),
            continue_resync_on_commit_match: Some(true),
            resync_frequency_seconds: Some(300),
            output_secret_name,
            config: params.config.clone(),
            env_refs: params.env_refs.clone(),
            workspace_template: Some({
                let mut tpl = match &params.docker_host {
                    Some(host) => {
                        PkoWorkspaceTemplate::restricted_default().with_env_var("DOCKER_HOST", host)
                    }
                    None => PkoWorkspaceTemplate::restricted_default(),
                };
                if let Some(secret_name) = params.config.get("kubeconfigSecretName") {
                    let ns_matches = params
                        .config
                        .get("kubeconfigSecretNamespace")
                        .is_none_or(|ns| ns == &tenant_namespace);
                    if !secret_name.trim().is_empty() && ns_matches {
                        tpl = tpl.with_secret_volume(
                            "target-kubeconfig",
                            secret_name,
                            "/etc/keel/kubeconfig",
                        );
                    }
                }
                tpl
            }),
        },
    })
}

/// Attaches `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` `envRefs` pointing to `secret_name`.
pub fn attach_s3_credentials_env_refs(
    env_refs: &mut BTreeMap<String, PkoEnvRef>,
    secret_name: &str,
) {
    env_refs.insert(
        "AWS_ACCESS_KEY_ID".to_string(),
        PkoEnvRef::from_secret(secret_name, "accessKey"),
    );
    env_refs.insert(
        "AWS_SECRET_ACCESS_KEY".to_string(),
        PkoEnvRef::from_secret(secret_name, "secretKey"),
    );
}

/// Attaches `GITHUB_TOKEN` from a namespace-local Kubernetes `Secret` (`token` key) to `env_refs`.
pub fn attach_github_token_env_ref(env_refs: &mut BTreeMap<String, PkoEnvRef>, secret_name: &str) {
    env_refs.insert(
        "GITHUB_TOKEN".to_string(),
        PkoEnvRef::from_secret(secret_name, "token"),
    );
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), PkoError> {
    if value.trim().is_empty() {
        return Err(PkoError::Validation(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_repo_dir(dir: &str) -> Result<(), PkoError> {
    validate_non_empty("repo_dir", dir)?;
    if dir.split('/').any(|seg| seg == "..") {
        return Err(PkoError::Validation(format!(
            "repo_dir '{dir}' must not contain parent path traversal ('..')"
        )));
    }
    Ok(())
}

fn validate_k8s_dns_label(field: &str, value: &str) -> Result<(), PkoError> {
    if value.is_empty() || value.len() > 63 {
        return Err(PkoError::Validation(format!(
            "{field} '{value}' must be 1..=63 characters"
        )));
    }
    let bytes = value.as_bytes();
    let starts_alnum = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let ends_alnum =
        bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit();
    let valid_chars = value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');

    if !starts_alnum || !ends_alnum || !valid_chars {
        return Err(PkoError::Validation(format!(
            "{field} '{value}' must be a valid RFC 1123 DNS label (lowercase alphanumeric and '-')"
        )));
    }
    Ok(())
}

fn validate_k8s_name(field: &str, value: &str) -> Result<(), PkoError> {
    if value.is_empty() || value.len() > 253 || value.contains('/') {
        return Err(PkoError::Validation(format!(
            "{field} '{value}' must be a valid Kubernetes resource name without namespace slashes"
        )));
    }
    for segment in value.split('.') {
        validate_k8s_dns_label(field, segment)?;
    }
    Ok(())
}

pub(crate) fn sanitize_pr_id(pr_id: &str) -> Result<String, PkoError> {
    let trimmed = pr_id.trim().trim_start_matches('#');
    if trimmed.is_empty() {
        return Err(PkoError::Validation("pr_id must not be empty".to_string()));
    }
    let lowered = trimmed.to_ascii_lowercase();
    validate_k8s_dns_label("pr_id", &lowered)?;
    Ok(lowered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_core_stack_enforces_protect_true_and_anti_lockout() {
        let mut params = ManagementCoreStackParams::new(
            "s3://pulumi-state",
            "flux-system",
            "clusters/management",
        );
        // Attempt to pass protect=false in user config; synthesis must override to true.
        params
            .config
            .insert("protect".to_string(), "false".to_string());
        params
            .config
            .insert("keel:protect".to_string(), "false".to_string());
        params.env_refs.insert(
            "AWS_SECRET_ACCESS_KEY".to_string(),
            PkoEnvRef::from_secret("s3-creds", "secretKey"),
        );

        let stack = synthesize_management_core_stack(&params).expect("valid management stack");

        let mut custom_sa_params = params.clone();
        custom_sa_params.service_account_name = Some("custom.mgmt-sa".to_string());
        let custom_sa_stack =
            synthesize_management_core_stack(&custom_sa_params).expect("valid custom sa stack");
        assert_eq!(
            custom_sa_stack.spec.service_account_name.as_deref(),
            Some("custom.mgmt-sa")
        );
        assert_eq!(stack.api_version, "pulumi.com/v1");
        assert_eq!(stack.kind, "Stack");
        assert_eq!(stack.metadata.name, "management-cluster-core");
        assert_eq!(stack.metadata.namespace, "flux-system");
        assert!(stack.is_protected());
        assert!(!stack.is_preview());
        assert_eq!(stack.spec.destroy_on_finalize, Some(false));
        assert_eq!(
            stack.spec.config.get("protect").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            stack.spec.config.get("keel:protect").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            stack.spec.service_account_name.as_deref(),
            Some(DEFAULT_MANAGEMENT_SERVICE_ACCOUNT)
        );

        let json = stack.to_json_value().expect("json serialization");
        assert_eq!(json["apiVersion"], "pulumi.com/v1");
        assert_eq!(json["kind"], "Stack");
        assert_eq!(json["metadata"]["labels"]["keel.io/protect"], "true");
        assert_eq!(json["spec"]["destroyOnFinalize"], false);
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["terminationGracePeriodSeconds"],
            600
        );
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["securityContext"]["runAsNonRoot"],
            true
        );

        let yaml = stack.to_yaml().expect("yaml serialization");
        let roundtrip = PkoStack::from_yaml(&yaml).expect("yaml roundtrip");
        assert_eq!(roundtrip, stack);
        let json_roundtrip = PkoStack::from_json_value(json).expect("json roundtrip");
        assert_eq!(json_roundtrip, stack);
    }

    #[test]
    fn ephemeral_preview_synthesizes_pinned_git_repo_and_preview_stack() {
        let params = EphemeralPreviewParams::for_action(
            "#42",
            "payments",
            "provision-postgres",
            1_763_100_000,
            "https://forge.internal/keel/infra-gitops.git",
            "tenants/payments/infra/postgres",
            "keel/payments-postgres/prod",
            "s3://pulumi-state",
        )
        .expect("valid preview params");

        let mut params = params;
        params.git_secret_name = Some("forge-git-auth".to_string());

        let mut custom_preview_params = params.clone();
        custom_preview_params.preview_service_account = Some("preview-ro.sa".to_string());
        let custom_preview =
            synthesize_ephemeral_preview(&custom_preview_params).expect("valid custom preview sa");
        assert_eq!(
            custom_preview.stack.spec.service_account_name.as_deref(),
            Some("preview-ro.sa")
        );

        let bundle = synthesize_ephemeral_preview(&params).expect("valid ephemeral preview");
        assert_eq!(bundle.pr_id, "42");
        assert_eq!(bundle.branch, "keel/payments/provision-postgres-1763100000");

        // Verify Flux GitRepository CR
        let repo = &bundle.git_repository;
        assert_eq!(repo.api_version, "source.toolkit.fluxcd.io/v1");
        assert_eq!(repo.kind, "GitRepository");
        assert_eq!(repo.metadata.name, "preview-source-pr-42");
        assert_eq!(repo.metadata.namespace, "keel-previews");
        assert_eq!(repo.spec.interval, "1m");
        assert_eq!(
            repo.spec.git_ref.branch.as_deref(),
            Some("keel/payments/provision-postgres-1763100000")
        );
        assert_eq!(
            repo.spec.secret_ref.as_ref().map(|r| r.name.as_str()),
            Some("forge-git-auth")
        );

        // Verify PKO v2 Stack CR
        let stack = &bundle.stack;
        assert_eq!(stack.api_version, "pulumi.com/v1");
        assert_eq!(stack.kind, "Stack");
        assert_eq!(stack.metadata.name, "preview-stack-pr-42");
        assert_eq!(stack.metadata.namespace, "keel-previews");
        assert!(stack.is_preview());
        assert_eq!(stack.spec.preview, Some(true));
        assert_eq!(stack.spec.destroy_on_finalize, Some(false));
        assert_eq!(
            stack.spec.service_account_name.as_deref(),
            Some(DEFAULT_PREVIEW_SERVICE_ACCOUNT)
        );
        let flux_src = stack.spec.flux_source.as_ref().expect("flux source");
        assert_eq!(flux_src.source_ref.name, "preview-source-pr-42");
        assert_eq!(flux_src.dir, "tenants/payments/infra/postgres");

        // Verify JSON & YAML roundtrips
        let (repo_json, stack_json) = bundle.to_json_values().expect("json values");
        assert_eq!(
            FluxGitRepository::from_json_value(repo_json).unwrap(),
            *repo
        );
        assert_eq!(stack_json["spec"]["preview"], true);
        assert_eq!(
            stack_json["spec"]["serviceAccountName"],
            "keel-preview-readonly"
        );

        let repo_yaml = repo.to_yaml().expect("repo yaml");
        assert_eq!(FluxGitRepository::from_yaml(&repo_yaml).unwrap(), *repo);

        let multi_doc = bundle.to_multi_doc_yaml().expect("multi-doc yaml");
        assert!(multi_doc.contains("kind: GitRepository"));
        assert!(multi_doc.contains("kind: Stack"));
        assert!(multi_doc.contains("preview: true"));
    }

    #[test]
    fn tenant_stack_synthesizes_namespace_isolation_and_output_secret_binding() {
        let mut params = TenantStackParams::new(
            "analytics",
            "postgres-managed",
            "keel/analytics-postgres/prod",
            "s3://pulumi-state",
            "tenant-git-source",
            "tenants/analytics/infra/postgres-managed",
        );
        params.output_secret_name = Some("postgres-companion-outputs".to_string());

        let stack = synthesize_tenant_stack(&params).expect("valid tenant stack");
        assert_eq!(stack.metadata.name, "postgres-managed");
        assert_eq!(stack.metadata.namespace, "tenant-analytics");
        assert_eq!(
            stack.spec.service_account_name.as_deref(),
            Some("tenant-analytics-sa")
        );
        assert_eq!(
            stack.spec.output_secret_name.as_deref(),
            Some("postgres-companion-outputs")
        );
        assert!(!stack.is_protected());
        assert!(!stack.is_preview());

        let values_from = stack
            .helm_values_from_ref()
            .expect("helm valuesFrom secret binding");
        assert_eq!(values_from.kind, "Secret");
        assert_eq!(values_from.name, "postgres-companion-outputs");
        assert_eq!(values_from.optional, Some(false));

        let json = stack.to_json_value().expect("json value");
        assert_eq!(
            json["spec"]["outputSecretName"],
            "postgres-companion-outputs"
        );

        // Also test tenant stack without companion output_secret_name
        params.output_secret_name = None;
        params.service_account_name = Some("custom-analytics-sa".to_string());
        let stack_no_secret = synthesize_tenant_stack(&params).expect("valid tenant stack");
        assert!(stack_no_secret.helm_values_from_ref().is_none());
        assert_eq!(
            stack_no_secret.spec.service_account_name.as_deref(),
            Some("custom-analytics-sa")
        );

        params.docker_host =
            Some("tcp://keel-docker-proxy.flux-system.svc.cluster.local:2375".to_string());
        let stack_docker =
            synthesize_tenant_stack(&params).expect("valid tenant stack with docker host");
        let docker_json = stack_docker.to_json_value().expect("json value");
        let env =
            docker_json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"]
                [0]["env"]
                .as_array()
                .unwrap();
        assert!(env.iter().any(|e| {
            e["name"] == "DOCKER_HOST"
                && e["value"] == "tcp://keel-docker-proxy.flux-system.svc.cluster.local:2375"
        }));

        // Verify kubeconfigSecretName in same tenant namespace mounts secret volume
        params.config.insert(
            "kubeconfigSecretName".to_string(),
            "workload-dev-kubeconfig".to_string(),
        );
        params.config.insert(
            "kubeconfigSecretNamespace".to_string(),
            "tenant-analytics".to_string(),
        );
        let stack_kc =
            synthesize_tenant_stack(&params).expect("valid tenant stack with kubeconfig");
        let kc_json = stack_kc.to_json_value().expect("json value");
        let volumes =
            kc_json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["volumes"]
                .as_array()
                .unwrap();
        assert_eq!(volumes[0]["name"], "target-kubeconfig");
        assert_eq!(
            volumes[0]["secret"]["secretName"],
            "workload-dev-kubeconfig"
        );
        let mounts = kc_json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]
            ["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap();
        assert_eq!(mounts[0]["name"], "target-kubeconfig");
        assert_eq!(mounts[0]["mountPath"], "/etc/keel/kubeconfig");
        assert_eq!(mounts[0]["readOnly"], true);
    }

    #[test]
    fn validation_rejects_invalid_branches_traversals_and_reserved_names() {
        // Invalid branch prefix
        let mut bad_preview = EphemeralPreviewParams::for_action(
            "10",
            "alpha",
            "deploy",
            1_700_000_000,
            "https://git.example/repo.git",
            "tenants/alpha/infra/vm",
            "keel/vm/prod",
            "s3://pulumi-state",
        )
        .unwrap();
        bad_preview.branch = "main".to_string();
        assert!(synthesize_ephemeral_preview(&bad_preview).is_err());

        // Branch missing numeric timestamp
        assert!(validate_preview_branch("alpha", "keel/alpha/deploy-notanumber").is_err());
        assert!(validate_preview_branch("alpha", "keel/alpha/notimestamp").is_err());
        assert!(build_preview_branch("alpha", "deploy", 0).is_err());

        // Path traversal in repo_dir
        bad_preview.branch = "keel/alpha/deploy-1700000000".to_string();
        bad_preview.repo_dir = "../secrets".to_string();
        assert!(synthesize_ephemeral_preview(&bad_preview).is_err());

        // Cross-namespace slash in output_secret_name
        let mut bad_tenant = TenantStackParams::new(
            "alpha",
            "db",
            "keel/db/prod",
            "s3://pulumi-state",
            "source",
            "tenants/alpha/infra/db",
        );
        bad_tenant.output_secret_name = Some("other-ns/secret".to_string());
        assert!(synthesize_tenant_stack(&bad_tenant).is_err());

        // Reserved management stack name in tenant namespace
        bad_tenant.output_secret_name = None;
        bad_tenant.name = MANAGEMENT_CORE_STACK_NAME.to_string();
        assert!(synthesize_tenant_stack(&bad_tenant).is_err());

        // Invalid uppercase DNS label
        bad_tenant.name = "InvalidUpper".to_string();
        assert!(synthesize_tenant_stack(&bad_tenant).is_err());

        // Empty PR ID
        assert!(sanitize_pr_id("   ").is_err());
    }
}
