//! PKO v2 `Stack` (`pulumi.com/v1`) and Flux CD `GitRepository` (`source.toolkit.fluxcd.io/v1`)
//! Custom Resource synthesis for management anti-lockout stacks, ephemeral PR previews,
//! and multi-tenant infrastructure stacks with companion output `Secret` bindings.

use std::collections::{BTreeMap, BTreeSet};

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
/// Provider label enforced by Kyverno `restrict-pulumi-providers` on `tenant-*`/`keel-previews` Stacks.
pub const LABEL_PROVIDER: &str = "keel.io/provider";

/// Namespace-local `Secret` holding the Pulumi state encryption passphrase for every PKO `Stack`
/// in that namespace (`tenant-<id>`, `flux-system`, `keel-system`; per-preview copies in
/// `keel-previews`). Generated once (≥256-bit, URL-safe) and never rotated automatically.
pub const PULUMI_PASSPHRASE_SECRET_NAME: &str = "keel-pulumi-passphrase";
/// Key inside [`PULUMI_PASSPHRASE_SECRET_NAME`] holding the passphrase.
pub const PULUMI_PASSPHRASE_SECRET_KEY: &str = "passphrase";
/// Pulumi passphrase environment variable; only ever injected via `valueFrom.secretKeyRef`.
pub const PULUMI_CONFIG_PASSPHRASE_ENV: &str = "PULUMI_CONFIG_PASSPHRASE";
const PULUMI_CONFIG_PASSPHRASE_FILE_ENV: &str = "PULUMI_CONFIG_PASSPHRASE_FILE";

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

/// Environment variable entry on a PKO workspace container (`corev1.EnvVar` subset): either a
/// literal `value` or a `valueFrom.secretKeyRef` to a namespace-local `Secret`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<WorkspaceEnvVarSource>,
}

impl WorkspaceEnvVar {
    /// Literal-valued environment variable.
    pub fn literal(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: Some(value.into()),
            value_from: None,
        }
    }

    /// Environment variable sourced from `secret_name`/`key` in the workspace pod's namespace.
    pub fn from_secret(
        name: impl Into<String>,
        secret_name: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: None,
            value_from: Some(WorkspaceEnvVarSource {
                secret_key_ref: SecretKeySelector {
                    name: secret_name.into(),
                    key: key.into(),
                },
            }),
        }
    }
}

/// `valueFrom` source for a workspace environment variable (`corev1.EnvVarSource` subset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEnvVarSource {
    pub secret_key_ref: SecretKeySelector,
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

/// Resource requests and limits for a PKO workspace container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceResourceRequirements {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requests: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<WorkspaceResourceRequirements>,
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
    /// Returns the hardened workspace template compliant with `NFR-SEC-03` and `NFR-REL-04`.
    ///
    /// `PULUMI_CONFIG_PASSPHRASE` is always sourced from `passphrase_secret_name`
    /// (key [`PULUMI_PASSPHRASE_SECRET_KEY`]) in the Stack's namespace via `secretKeyRef`, so
    /// Pulumi `secret: true` config and outputs are encrypted with a per-namespace key and the
    /// workspace pod fails to start (rather than falling back to an empty passphrase) when the
    /// Secret is missing.
    pub fn restricted(passphrase_secret_name: &str) -> Self {
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
                            env: vec![WorkspaceEnvVar::from_secret(
                                PULUMI_CONFIG_PASSPHRASE_ENV,
                                passphrase_secret_name,
                                PULUMI_PASSPHRASE_SECRET_KEY,
                            )],
                            volume_mounts: Vec::new(),
                            // Explicit limits override the tenant LimitRange 1Gi default so heavy
                            // providers like azure-native do not OOMKill mid-update and leave locks.
                            resources: Some(WorkspaceResourceRequirements {
                                requests: BTreeMap::from([
                                    ("cpu".to_string(), "100m".to_string()),
                                    ("memory".to_string(), "256Mi".to_string()),
                                ]),
                                limits: BTreeMap::from([
                                    ("cpu".to_string(), "2".to_string()),
                                    ("memory".to_string(), "4Gi".to_string()),
                                ]),
                            }),
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

    /// Appends a literal environment variable to the `pulumi` workspace container.
    ///
    /// The Pulumi passphrase variables are reserved (Secret-sourced only) and are never
    /// overridden with a literal value.
    pub fn with_env_var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        if name == PULUMI_CONFIG_PASSPHRASE_ENV || name == PULUMI_CONFIG_PASSPHRASE_FILE_ENV {
            return self;
        }
        if let Some(container) = self
            .spec
            .pod_template
            .spec
            .containers
            .iter_mut()
            .find(|c| c.name == "pulumi")
        {
            container.env.push(WorkspaceEnvVar::literal(name, value));
        }
        self
    }

    /// Returns the `Secret` reference `PULUMI_CONFIG_PASSPHRASE` is sourced from, if any.
    pub fn passphrase_secret_ref(&self) -> Option<&SecretKeySelector> {
        self.spec
            .pod_template
            .spec
            .containers
            .iter()
            .find(|c| c.name == "pulumi")?
            .env
            .iter()
            .find(|e| e.name == PULUMI_CONFIG_PASSPHRASE_ENV)?
            .value_from
            .as_ref()
            .map(|v| &v.secret_key_ref)
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets_ref: BTreeMap<String, PkoEnvRef>,
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
    /// Namespace-local Secret holding the state passphrase (default [`PULUMI_PASSPHRASE_SECRET_NAME`]).
    pub passphrase_secret_name: String,
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
            passphrase_secret_name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
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
    /// Pulumi config secrets (`spec.secretsRef`), only ever pointing at per-preview Secret copies
    /// in `keel-previews` (see [`EphemeralPreviewParams::inherit_tenant_credentials`]).
    pub secrets_ref: BTreeMap<String, PkoEnvRef>,
    /// Optional `DOCKER_HOST` endpoint injected into the PKO workspace pod for local `kind`-in-`kind` dev.
    pub docker_host: Option<String>,
    /// Pulumi provider id stamped as `keel.io/provider` (required by Kyverno `restrict-pulumi-providers`).
    pub provider: Option<String>,
    /// PR head commit SHA recorded as `keel.io/head-sha` so restarts/replicas can detect new pushes.
    pub head_sha: Option<String>,
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
            secrets_ref: BTreeMap::new(),
            docker_host: None,
            provider: None,
            head_sha: None,
        })
    }
}

impl EphemeralPreviewParams {
    /// Aligns the preview with the tenant `Stack` it previews: same Pulumi stack name and
    /// tenant-scoped state backend (so `pulumi preview` diffs against the real checkpoint),
    /// same stack config, and the same `keel.io/provider` label.
    ///
    /// Tenant `envRefs`/`secretsRef` are **not** copied verbatim: they reference Secrets in
    /// `tenant-<id>` that do not exist in `keel-previews`, and control-plane admin credentials
    /// must never be substituted for them (OPEN-9). Use
    /// [`EphemeralPreviewParams::inherit_tenant_credentials`] to plan per-preview copies.
    ///
    /// A backend outside the tenant prefix is never adopted (OPEN-9b): the preview holds only the
    /// tenant-scoped state identity, so [`synthesize_ephemeral_preview`] always pins the preview to
    /// `s3://<bucket>/tenants/<tenant>` regardless of the inherited value.
    pub fn inherit_tenant_stack(&mut self, tenant_stack: &PkoStack) {
        self.pulumi_stack_name = tenant_stack.spec.stack.clone();
        if let Some(backend) = &tenant_stack.spec.backend
            && is_tenant_scoped_backend(backend, &self.tenant)
        {
            self.backend_url = backend.clone();
        }
        for (k, v) in &tenant_stack.spec.config {
            self.config.insert(k.clone(), v.clone());
        }
        if let Some(provider) = tenant_stack.metadata.labels.get(LABEL_PROVIDER) {
            self.provider = Some(provider.clone());
        }
    }

    /// Per-preview name of the tenant state passphrase copy in `keel-previews`
    /// (`keel-preview-<pr>-keel-pulumi-passphrase`), referenced by the preview workspace.
    pub fn passphrase_secret_copy_name(&self) -> Result<String, PkoError> {
        preview_secret_copy_name(&self.pr_id, PULUMI_PASSPHRASE_SECRET_NAME)
    }

    /// Plans the per-preview copy of the tenant's `keel-pulumi-passphrase` Secret so the
    /// preview can decrypt (and encrypt) the tenant's Pulumi state.
    pub fn passphrase_secret_copy(&self) -> Result<PreviewSecretCopy, PkoError> {
        Ok(PreviewSecretCopy {
            source_name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
            target_name: self.passphrase_secret_copy_name()?,
            keys: BTreeSet::from([PULUMI_PASSPHRASE_SECRET_KEY.to_string()]),
            proposed_data: None,
        })
    }

    /// Plans per-preview copies of exactly the Secrets the tenant `Stack` references through
    /// `envRefs` / `secretsRef` (cloud provider credentials) and rewrites this preview's refs to
    /// the copies (`keel-preview-<pr>-<name>` in `keel-previews`). The tenant state passphrase
    /// copy ([`EphemeralPreviewParams::passphrase_secret_copy`]) is always included.
    ///
    /// Secrets named in `excluded` (state-backend / forge credentials the preview runner already
    /// mirrors with tenant-scoped values) are neither copied nor referenced. Only the referenced
    /// keys are planned for each Secret. Non-`Secret` ref types are ignored.
    pub fn inherit_tenant_credentials(
        &mut self,
        tenant_stack: &PkoStack,
        excluded: &BTreeSet<String>,
    ) -> Result<Vec<PreviewSecretCopy>, PkoError> {
        let mut copies: BTreeMap<String, PreviewSecretCopy> = BTreeMap::new();
        let mut plan = |r: &PkoEnvRef| -> Result<Option<PkoEnvRef>, PkoError> {
            if r.selector_type != "Secret" || excluded.contains(&r.secret.name) {
                return Ok(None);
            }
            validate_k8s_name("secret_ref", &r.secret.name)?;
            let entry = match copies.entry(r.secret.name.clone()) {
                std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::btree_map::Entry::Vacant(v) => v.insert(PreviewSecretCopy {
                    source_name: r.secret.name.clone(),
                    target_name: preview_secret_copy_name(&self.pr_id, &r.secret.name)?,
                    keys: BTreeSet::new(),
                    proposed_data: None,
                }),
            };
            entry.keys.insert(r.secret.key.clone());
            Ok(Some(PkoEnvRef::from_secret(
                entry.target_name.clone(),
                r.secret.key.clone(),
            )))
        };
        let mut env_refs = BTreeMap::new();
        for (k, r) in &tenant_stack.spec.env_refs {
            if let Some(rewritten) = plan(r)? {
                env_refs.insert(k.clone(), rewritten);
            }
        }
        let mut secrets_ref = BTreeMap::new();
        for (k, r) in &tenant_stack.spec.secrets_ref {
            if let Some(rewritten) = plan(r)? {
                secrets_ref.insert(k.clone(), rewritten);
            }
        }
        self.env_refs.extend(env_refs);
        self.secrets_ref.extend(secrets_ref);
        let passphrase = self.passphrase_secret_copy()?;
        copies.insert(passphrase.source_name.clone(), passphrase);
        Ok(copies.into_values().collect())
    }
}

/// A tenant Secret copied into `keel-previews` for one preview `Stack` (cloud provider
/// credentials). The copy is labelled `keel.io/pr-id` / `keel.io/tenant` and owned by the
/// preview `Stack`, so Kubernetes garbage-collects it with the preview.
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewSecretCopy {
    /// Secret name in `tenant-<id>` referenced by the tenant `Stack`.
    pub source_name: String,
    /// Per-preview Secret name in `keel-previews` (`keel-preview-<pr>-<source>`).
    pub target_name: String,
    /// Only these keys are copied (exactly those referenced by the Stack).
    pub keys: BTreeSet<String>,
    /// Values proposed by the PR itself (e.g. SOPS-encrypted `<instance>-secrets` not yet
    /// decrypted into the tenant namespace by Flux). When set, they are used instead of the live
    /// tenant Secret so the preview evaluates the PR's credentials.
    pub proposed_data: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for PreviewSecretCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewSecretCopy")
            .field("source_name", &self.source_name)
            .field("target_name", &self.target_name)
            .field("keys", &self.keys)
            .field(
                "proposed_data",
                &self.proposed_data.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Deterministic per-preview Secret copy name: `keel-preview-<pr>-<source>`, shortened with a
/// stable hash suffix when it would exceed a DNS label (63 chars).
pub fn preview_secret_copy_name(pr_id: &str, source_name: &str) -> Result<String, PkoError> {
    let pr = sanitize_pr_id(pr_id)?;
    validate_k8s_name("source_secret", source_name)?;
    let flat = source_name.replace('.', "-");
    let full = format!("keel-preview-{pr}-{flat}");
    if full.len() <= 63 {
        return Ok(full);
    }
    // FNV-1a keeps the shortened name stable across replicas/restarts.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in full.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let suffix = format!("{:010x}", hash & 0xff_ffff_ffff);
    let head = full[..63 - suffix.len() - 1].trim_end_matches('-');
    let name = format!("{head}-{suffix}");
    validate_k8s_dns_label("preview_secret", &name)?;
    Ok(name)
}

/// `true` when `backend_url` is not an S3 DIY backend (nothing to scope) or already lives under
/// the tenant prefix `s3://<bucket>/tenants/<tenant>`.
pub fn is_tenant_scoped_backend(backend_url: &str, tenant: &str) -> bool {
    let Some(rest) = backend_url.strip_prefix("s3://") else {
        return true;
    };
    let location = rest.split('?').next().unwrap_or_default();
    let mut parts = location.splitn(2, '/');
    let _bucket = parts.next();
    let path = parts.next().unwrap_or_default().trim_end_matches('/');
    let prefix = format!("tenants/{tenant}");
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Pins an S3 DIY backend to the tenant prefix (`s3://<bucket>/tenants/<tenant>[?query]`),
/// matching `keel_kube::tenant::tenant_s3_backend_url`. Already-scoped and non-S3 backends are
/// returned unchanged.
pub fn tenant_scoped_preview_backend(backend_url: &str, tenant: &str) -> String {
    if is_tenant_scoped_backend(backend_url, tenant) {
        return backend_url.to_string();
    }
    let rest = backend_url.strip_prefix("s3://").unwrap_or(backend_url);
    let (location, query) = match rest.split_once('?') {
        Some((l, q)) => (l, Some(q)),
        None => (rest, None),
    };
    let bucket = location.split('/').next().unwrap_or_default();
    let mut url = format!("s3://{bucket}/tenants/{tenant}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    url
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
    pub secrets_ref: BTreeMap<String, PkoEnvRef>,
    /// Optional `DOCKER_HOST` endpoint injected into the PKO workspace pod for local `kind`-in-`kind` dev.
    pub docker_host: Option<String>,
    /// `tenant-<id>` Secret holding the tenant state passphrase (default
    /// [`PULUMI_PASSPHRASE_SECRET_NAME`]).
    pub passphrase_secret_name: String,
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
            secrets_ref: BTreeMap::new(),
            docker_host: None,
            passphrase_secret_name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
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
    validate_k8s_name("passphrase_secret_name", &params.passphrase_secret_name)?;

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
            secrets_ref: BTreeMap::new(),
            workspace_template: Some(PkoWorkspaceTemplate::restricted(
                &params.passphrase_secret_name,
            )),
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

    let passphrase_copy = params.passphrase_secret_copy_name()?;
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
    if let Some(provider) = params
        .provider
        .as_deref()
        .or_else(|| params.config.get("keel:provider").map(String::as_str))
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        validate_k8s_dns_label("provider", provider)?;
        labels.insert(LABEL_PROVIDER.to_string(), provider.to_string());
    }

    let mut annotations = BTreeMap::new();
    annotations.insert(ANNOTATION_BRANCH.to_string(), params.branch.clone());
    if let Some(sha) = params
        .head_sha
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        annotations.insert(crate::run::ANNOTATION_HEAD_SHA.to_string(), sha.to_string());
    }

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
            // OPEN-9b: every tenant preview runs against the tenant state prefix (the only one its
            // scoped identity can reach), for existing and brand-new stacks alike.
            backend: Some(tenant_scoped_preview_backend(
                &params.backend_url,
                &params.tenant,
            )),
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
            secrets_ref: params.secrets_ref.clone(),
            workspace_template: Some({
                let tpl = PkoWorkspaceTemplate::restricted(&passphrase_copy);
                match &params.docker_host {
                    Some(host) => tpl.with_env_var("DOCKER_HOST", host),
                    None => tpl,
                }
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

    validate_k8s_name("passphrase_secret_name", &params.passphrase_secret_name)?;

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
        labels.insert(LABEL_PROVIDER.to_string(), provider.clone());
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
            secrets_ref: params.secrets_ref.clone(),
            workspace_template: Some({
                let base = PkoWorkspaceTemplate::restricted(&params.passphrase_secret_name);
                let mut tpl = match &params.docker_host {
                    Some(host) => base.with_env_var("DOCKER_HOST", host),
                    None => base,
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

/// Synthesizes a Pulumi YAML program that deploys raw Kubernetes manifests via `kubernetes:yaml/v2:ConfigGroup`,
/// declaring `schema_properties` in `configuration` so `Stack.spec.config` values pass Pulumi validation.
pub fn synthesize_k8s_manifest_pulumi_yaml(
    stack_name: &str,
    rendered_manifests_yaml: &str,
    schema_properties: &[String],
    target_kubeconfig_defined: bool,
) -> Result<String, PkoError> {
    validate_k8s_name("stack_name", stack_name)?;
    if rendered_manifests_yaml.trim().is_empty() {
        return Err(PkoError::Validation(
            "rendered_manifests_yaml must not be empty".to_string(),
        ));
    }

    let mut configuration = serde_json::Map::new();
    for prop in schema_properties {
        let trimmed = prop.trim();
        if !trimmed.is_empty() {
            configuration.insert(
                trimmed.to_string(),
                serde_json::json!({
                    "type": "string",
                    "default": ""
                }),
            );
        }
    }
    if target_kubeconfig_defined {
        configuration
            .entry("kubeconfigSecretName".to_string())
            .or_insert_with(|| {
                serde_json::json!({
                    "type": "string",
                    "default": ""
                })
            });
        configuration
            .entry("kubeconfigSecretNamespace".to_string())
            .or_insert_with(|| {
                serde_json::json!({
                    "type": "string",
                    "default": ""
                })
            });
        configuration
            .entry("kubeconfigPath".to_string())
            .or_insert_with(|| {
                serde_json::json!({
                    "type": "string",
                    "default": "/etc/keel/kubeconfig/kubeconfig"
                })
            });
    }

    let mut resources = serde_json::Map::new();
    let mut manifest_resource = serde_json::Map::new();
    manifest_resource.insert(
        "type".to_string(),
        serde_json::Value::String("kubernetes:yaml/v2:ConfigGroup".to_string()),
    );
    if target_kubeconfig_defined {
        resources.insert(
            "targetCluster".to_string(),
            serde_json::json!({
                "type": "pulumi:providers:kubernetes",
                "properties": {
                    "kubeconfig": {
                        "fn::readFile": "/etc/keel/kubeconfig/kubeconfig"
                    },
                    "enableServerSideApply": true,
                    "upsertExistingObjects": true
                }
            }),
        );
        manifest_resource.insert(
            "options".to_string(),
            serde_json::json!({
                "provider": "${targetCluster}"
            }),
        );
    }
    manifest_resource.insert(
        "properties".to_string(),
        serde_json::json!({
            "yaml": rendered_manifests_yaml
        }),
    );
    resources.insert(
        "manifests".to_string(),
        serde_json::Value::Object(manifest_resource),
    );

    let mut doc = serde_json::Map::new();
    doc.insert(
        "name".to_string(),
        serde_json::Value::String(stack_name.to_string()),
    );
    doc.insert(
        "runtime".to_string(),
        serde_json::Value::String("yaml".to_string()),
    );
    doc.insert(
        "description".to_string(),
        serde_json::Value::String(format!(
            "Keel PKO Kubernetes manifest stack for {stack_name}"
        )),
    );
    if !configuration.is_empty() {
        doc.insert(
            "configuration".to_string(),
            serde_json::Value::Object(configuration),
        );
    }
    doc.insert(
        "resources".to_string(),
        serde_json::Value::Object(resources),
    );
    doc.insert(
        "outputs".to_string(),
        serde_json::json!({
            "stackName": stack_name
        }),
    );

    let yaml = serde_yml::to_string(&serde_json::Value::Object(doc))?;
    Ok(ensure_trailing_newline(&yaml))
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
    keel_core::validate_dns_label(value, field).map_err(|e| PkoError::Validation(e.to_string()))
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
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"][0]["resources"]
                ["requests"]["memory"],
            "256Mi"
        );
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"][0]["resources"]
                ["limits"]["memory"],
            "4Gi"
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

        let mut labelled = params.clone();
        labelled.provider = Some("kubernetes".to_string());
        labelled.head_sha = Some("abc123".to_string());
        let labelled = synthesize_ephemeral_preview(&labelled).expect("labelled preview");
        assert_eq!(
            labelled
                .stack
                .metadata
                .labels
                .get(LABEL_PROVIDER)
                .map(String::as_str),
            Some("kubernetes")
        );
        assert_eq!(
            labelled
                .stack
                .metadata
                .annotations
                .get(crate::run::ANNOTATION_HEAD_SHA)
                .map(String::as_str),
            Some("abc123")
        );
        let mut bad_provider = params.clone();
        bad_provider.provider = Some("Bad Provider".to_string());
        assert!(synthesize_ephemeral_preview(&bad_provider).is_err());

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

    #[test]
    fn synthesize_k8s_manifest_pulumi_yaml_creates_config_group_and_configuration() {
        let raw_manifests = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: demo-cm\n";
        let props = vec!["replicas".to_string(), "imageTag".to_string()];

        let yaml =
            synthesize_k8s_manifest_pulumi_yaml("demo-manifest", raw_manifests, &props, false)
                .expect("should synthesize Pulumi.yaml");
        let parsed: serde_json::Value = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(parsed["name"], "demo-manifest");
        assert_eq!(parsed["runtime"], "yaml");
        assert_eq!(parsed["configuration"]["replicas"]["type"], "string");
        assert_eq!(parsed["configuration"]["imageTag"]["type"], "string");
        assert_eq!(
            parsed["resources"]["manifests"]["type"],
            "kubernetes:yaml/v2:ConfigGroup"
        );
        assert_eq!(
            parsed["resources"]["manifests"]["properties"]["yaml"],
            raw_manifests
        );
        assert!(parsed["resources"].get("targetCluster").is_none());

        let yaml_remote =
            synthesize_k8s_manifest_pulumi_yaml("demo-manifest", raw_manifests, &props, true)
                .expect("should synthesize remote Pulumi.yaml");
        let parsed_remote: serde_json::Value = serde_yml::from_str(&yaml_remote).unwrap();
        assert_eq!(
            parsed_remote["resources"]["targetCluster"]["type"],
            "pulumi:providers:kubernetes"
        );
        assert_eq!(
            parsed_remote["resources"]["manifests"]["options"]["provider"],
            "${targetCluster}"
        );
        assert!(
            parsed_remote["configuration"]
                .get("kubeconfigSecretName")
                .is_some()
        );
    }

    #[test]
    fn preview_inherits_tenant_stack_identity_without_tenant_secrets() {
        let mut tparams = TenantStackParams::new(
            "acme",
            "web",
            "organization/web/acme-web",
            "s3://pulumi-state/tenants/acme?region=us-east-1",
            "keel-gitops",
            "tenants/acme/apps/web",
        );
        tparams
            .config
            .insert("keel:provider".to_string(), "kubernetes".to_string());
        tparams.env_refs.insert(
            "API_TOKEN".to_string(),
            PkoEnvRef::from_secret("web-secrets", "token"),
        );
        let tenant_stack = synthesize_tenant_stack(&tparams).unwrap();

        let mut params = EphemeralPreviewParams::for_action(
            "9",
            "acme",
            "deploy",
            1_700_000_000,
            "https://forge/x.git",
            "tenants/acme/apps/web",
            "organization/web-pr-9/acme",
            "s3://pulumi-state",
        )
        .unwrap();
        params.inherit_tenant_stack(&tenant_stack);
        let bundle = synthesize_ephemeral_preview(&params).unwrap();
        assert_eq!(bundle.stack.spec.stack, "organization/web/acme-web");
        assert_eq!(
            bundle.stack.spec.backend.as_deref(),
            Some("s3://pulumi-state/tenants/acme?region=us-east-1")
        );
        assert_eq!(
            bundle
                .stack
                .metadata
                .labels
                .get(LABEL_PROVIDER)
                .map(String::as_str),
            Some("kubernetes")
        );
        assert!(bundle.stack.spec.env_refs.is_empty());
        assert!(bundle.stack.is_preview());
        assert_eq!(bundle.stack.spec.destroy_on_finalize, Some(false));
    }

    fn tenant_stack_with_creds(backend: &str) -> PkoStack {
        let mut tparams = TenantStackParams::new(
            "acme",
            "vm1",
            "organization/aws-vm/acme-vm1",
            backend,
            "keel-gitops",
            "tenants/acme/infra/vm1",
        );
        tparams
            .config
            .insert("keel:provider".to_string(), "aws".to_string());
        attach_s3_credentials_env_refs(&mut tparams.env_refs, "keel-s3-credentials");
        tparams.env_refs.insert(
            "GITHUB_TOKEN".to_string(),
            PkoEnvRef::from_secret("keel-github-token", "token"),
        );
        tparams.secrets_ref.insert(
            "aws:accessKey".to_string(),
            PkoEnvRef::from_secret("vm1-secrets", "awsAccessKeyId"),
        );
        tparams.secrets_ref.insert(
            "aws:secretKey".to_string(),
            PkoEnvRef::from_secret("vm1-secrets", "awsSecretAccessKey"),
        );
        tparams.env_refs.insert(
            "HCLOUD_TOKEN".to_string(),
            PkoEnvRef::from_secret("hetzner-creds", "token"),
        );
        synthesize_tenant_stack(&tparams).unwrap()
    }

    fn base_preview_params(pr: &str) -> EphemeralPreviewParams {
        EphemeralPreviewParams::for_action(
            pr,
            "acme",
            "provision",
            1_700_000_000,
            "https://forge/x.git",
            "tenants/acme/infra/vm1",
            format!("organization/vm1-pr-{pr}/acme"),
            "s3://pulumi-state?region=us-east-1&endpoint=seaweedfs",
        )
        .unwrap()
    }

    /// OPEN-9b: brand-new stacks (no tenant Stack to inherit) and existing stacks both preview
    /// against the tenant prefix; a foreign/base backend on the tenant Stack is never adopted.
    #[test]
    fn preview_backend_is_always_tenant_scoped() {
        let tenant_backend = "s3://pulumi-state/tenants/acme?region=us-east-1&endpoint=seaweedfs";

        let new_stack = synthesize_ephemeral_preview(&base_preview_params("11")).unwrap();
        assert_eq!(
            new_stack.stack.spec.backend.as_deref(),
            Some(tenant_backend)
        );

        let mut existing = base_preview_params("12");
        existing.inherit_tenant_stack(&tenant_stack_with_creds(tenant_backend));
        let existing = synthesize_ephemeral_preview(&existing).unwrap();
        assert_eq!(existing.stack.spec.backend.as_deref(), Some(tenant_backend));

        let mut legacy = base_preview_params("13");
        legacy.inherit_tenant_stack(&tenant_stack_with_creds("s3://pulumi-state"));
        let legacy = synthesize_ephemeral_preview(&legacy).unwrap();
        assert_eq!(legacy.stack.spec.backend.as_deref(), Some(tenant_backend));

        let mut other_tenant = base_preview_params("14");
        other_tenant.backend_url = "s3://pulumi-state/tenants/evil".to_string();
        let other_tenant = synthesize_ephemeral_preview(&other_tenant).unwrap();
        assert_eq!(
            other_tenant.stack.spec.backend.as_deref(),
            Some("s3://pulumi-state/tenants/acme")
        );

        assert!(is_tenant_scoped_backend("file:///tmp/state", "acme"));
        assert!(is_tenant_scoped_backend(
            "s3://b/tenants/acme/sub?x=1",
            "acme"
        ));
        assert!(!is_tenant_scoped_backend("s3://b/tenants/acme2", "acme"));
        assert_eq!(
            tenant_scoped_preview_backend("file:///tmp/state", "acme"),
            "file:///tmp/state"
        );
    }

    #[test]
    fn preview_credentials_copy_only_referenced_secrets_and_rewrite_refs() {
        let tenant_stack =
            tenant_stack_with_creds("s3://pulumi-state/tenants/acme?region=us-east-1");
        let mut params = base_preview_params("#42");
        params.inherit_tenant_stack(&tenant_stack);
        let excluded = BTreeSet::from([
            "keel-s3-credentials".to_string(),
            "keel-github-token".to_string(),
        ]);
        let copies = params
            .inherit_tenant_credentials(&tenant_stack, &excluded)
            .unwrap();

        let names: Vec<_> = copies.iter().map(|c| c.source_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["hetzner-creds", "keel-pulumi-passphrase", "vm1-secrets"]
        );
        let pass = copies
            .iter()
            .find(|c| c.source_name == PULUMI_PASSPHRASE_SECRET_NAME)
            .unwrap();
        assert_eq!(pass.target_name, "keel-preview-42-keel-pulumi-passphrase");
        assert_eq!(
            pass.keys,
            BTreeSet::from([PULUMI_PASSPHRASE_SECRET_KEY.to_string()])
        );
        let vm = copies
            .iter()
            .find(|c| c.source_name == "vm1-secrets")
            .unwrap();
        assert_eq!(vm.target_name, "keel-preview-42-vm1-secrets");
        assert_eq!(
            vm.keys,
            BTreeSet::from([
                "awsAccessKeyId".to_string(),
                "awsSecretAccessKey".to_string()
            ])
        );
        let hz = copies
            .iter()
            .find(|c| c.source_name == "hetzner-creds")
            .unwrap();
        assert_eq!(hz.keys, BTreeSet::from(["token".to_string()]));

        let bundle = synthesize_ephemeral_preview(&params).unwrap();
        let spec = &bundle.stack.spec;
        assert_eq!(
            spec.secrets_ref["aws:accessKey"],
            PkoEnvRef::from_secret("keel-preview-42-vm1-secrets", "awsAccessKeyId")
        );
        assert_eq!(
            spec.env_refs["HCLOUD_TOKEN"],
            PkoEnvRef::from_secret("keel-preview-42-hetzner-creds", "token")
        );
        // Excluded (runner-mirrored) secrets are neither copied nor referenced.
        assert!(!spec.env_refs.contains_key("GITHUB_TOKEN"));
        assert!(!spec.env_refs.contains_key("AWS_ACCESS_KEY_ID"));
        // No ref in the preview points at a tenant-namespace Secret name.
        for r in spec.env_refs.values().chain(spec.secrets_ref.values()) {
            assert!(r.secret.name.starts_with("keel-preview-42-"), "{r:?}");
        }
        // Proposed values never leak through Debug.
        let mut with_data = vm.clone();
        with_data.proposed_data = Some(BTreeMap::from([(
            "awsSecretAccessKey".to_string(),
            "SUPERSECRET".to_string(),
        )]));
        assert!(!format!("{with_data:?}").contains("SUPERSECRET"));
    }

    #[test]
    fn preview_secret_copy_names_are_bounded_and_stable() {
        let long = "a".repeat(60);
        let n1 = preview_secret_copy_name("7", &long).unwrap();
        let n2 = preview_secret_copy_name("7", &long).unwrap();
        assert_eq!(n1, n2);
        assert!(n1.len() <= 63 && n1.starts_with("keel-preview-7-"));
        assert_ne!(n1, preview_secret_copy_name("8", &long).unwrap());
        assert_eq!(
            preview_secret_copy_name("7", "my.creds").unwrap(),
            "keel-preview-7-my-creds"
        );
        assert!(preview_secret_copy_name("7", "Bad/Name").is_err());
    }

    /// Recursively collects every `PULUMI_CONFIG_PASSPHRASE*` env entry in a serialized document.
    fn passphrase_env_entries(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if map
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|n| n.starts_with(PULUMI_CONFIG_PASSPHRASE_ENV))
                {
                    out.push(v.clone());
                }
                map.values().for_each(|c| passphrase_env_entries(c, out));
            }
            serde_json::Value::Array(items) => {
                items.iter().for_each(|c| passphrase_env_entries(c, out));
            }
            _ => {}
        }
    }

    /// Guard: no synthesized Stack (management, tenant, preview; JSON or YAML) ever carries a
    /// literal (let alone empty) `PULUMI_CONFIG_PASSPHRASE`; it is always a `secretKeyRef` to
    /// the namespace-local passphrase Secret.
    #[test]
    fn synthesized_stacks_never_carry_a_literal_or_empty_passphrase() {
        let mgmt = synthesize_management_core_stack(&ManagementCoreStackParams::new(
            IN_CLUSTER_S3_BACKEND_URL,
            "flux-system",
            "clusters/management",
        ))
        .unwrap();
        let mut tparams = TenantStackParams::new(
            "acme",
            "vm1",
            "organization/vm1/acme-vm1",
            "s3://pulumi-state/tenants/acme",
            "keel-gitops",
            "tenants/acme/infra/vm1",
        );
        tparams.docker_host = Some("tcp://proxy:2375".to_string());
        let tenant = synthesize_tenant_stack(&tparams).unwrap();
        let mut preview_params = base_preview_params("42");
        preview_params.docker_host = Some("tcp://proxy:2375".to_string());
        let preview = synthesize_ephemeral_preview(&preview_params).unwrap();

        for (stack, expected_secret) in [
            (&mgmt, PULUMI_PASSPHRASE_SECRET_NAME),
            (&tenant, PULUMI_PASSPHRASE_SECRET_NAME),
            (&preview.stack, "keel-preview-42-keel-pulumi-passphrase"),
        ] {
            let json = stack.to_json_value().unwrap();
            let mut entries = Vec::new();
            passphrase_env_entries(&json, &mut entries);
            assert_eq!(entries.len(), 1, "{}: {entries:?}", stack.metadata.name);
            let entry = &entries[0];
            assert_eq!(entry["name"], PULUMI_CONFIG_PASSPHRASE_ENV);
            assert!(entry.get("value").is_none(), "literal passphrase: {entry}");
            assert_eq!(
                entry["valueFrom"]["secretKeyRef"],
                serde_json::json!({
                    "name": expected_secret,
                    "key": PULUMI_PASSPHRASE_SECRET_KEY,
                })
            );
            let tpl = stack.spec.workspace_template.as_ref().unwrap();
            assert_eq!(
                tpl.passphrase_secret_ref().map(|r| r.name.as_str()),
                Some(expected_secret)
            );

            // Text-level guard over the serialized YAML/JSON forms.
            let yaml = stack.to_yaml().unwrap();
            let compact = serde_json::to_string(&json).unwrap();
            for needle in [
                r#""name":"PULUMI_CONFIG_PASSPHRASE","value""#,
                r#""value":"""#,
            ] {
                assert!(!compact.contains(needle), "{needle} in {compact}");
            }
            assert!(!yaml.contains("value: ''"), "{yaml}");
            assert!(!yaml.contains("value: \"\""), "{yaml}");
            // Round-trip keeps the secretKeyRef form.
            assert_eq!(&PkoStack::from_yaml(&yaml).unwrap(), stack);
        }
    }

    #[test]
    fn passphrase_env_cannot_be_overridden_with_a_literal() {
        let tpl = PkoWorkspaceTemplate::restricted("tenant-pass")
            .with_env_var(PULUMI_CONFIG_PASSPHRASE_ENV, "")
            .with_env_var("PULUMI_CONFIG_PASSPHRASE_FILE", "/tmp/x")
            .with_env_var("DOCKER_HOST", "tcp://proxy:2375");
        let env = &tpl.spec.pod_template.spec.containers[0].env;
        assert_eq!(env.len(), 2);
        assert_eq!(
            env[0],
            WorkspaceEnvVar::from_secret(
                PULUMI_CONFIG_PASSPHRASE_ENV,
                "tenant-pass",
                PULUMI_PASSPHRASE_SECRET_KEY
            )
        );
        assert_eq!(
            env[1],
            WorkspaceEnvVar::literal("DOCKER_HOST", "tcp://proxy:2375")
        );
        assert_eq!(tpl.passphrase_secret_ref().unwrap().name, "tenant-pass");

        // An invalid passphrase Secret name is rejected at synthesis time.
        let mut tparams = TenantStackParams::new(
            "acme",
            "vm1",
            "organization/vm1/acme-vm1",
            "s3://pulumi-state/tenants/acme",
            "keel-gitops",
            "tenants/acme/infra/vm1",
        );
        tparams.passphrase_secret_name = String::new();
        assert!(synthesize_tenant_stack(&tparams).is_err());
        let mut mparams = ManagementCoreStackParams::new(
            IN_CLUSTER_S3_BACKEND_URL,
            "flux-system",
            "clusters/management",
        );
        mparams.passphrase_secret_name = "Bad Name".to_string();
        assert!(synthesize_management_core_stack(&mparams).is_err());
    }
}
