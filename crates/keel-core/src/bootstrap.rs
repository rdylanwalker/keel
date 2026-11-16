//! Provider-agnostic Stage 0 cluster bootstrap trait and drivers (`SDD.md` §2.1).
//!
//! External CLI invocations (`pulumi`) strictly use `tokio::process::Command` with explicit
//! argument vectors and never invoke a shell (`NFR-SEC-06`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::CoreError;
use crate::models::{BootstrapConfig, BootstrapTargetKind};

/// Connection details returned after a Stage 0 `BootstrapTarget` provisions or attaches to a seed cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterConnection {
    pub target: BootstrapTargetKind,
    pub cluster_name: String,
    pub kubeconfig_yaml: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_endpoint: Option<String>,
}

/// Provider-agnostic Stage 0 cluster bootstrap interface (`SDD.md` §1.2, §2.1).
#[async_trait]
pub trait BootstrapTarget: Send + Sync {
    /// Returns the target provider kind handled by this driver.
    fn kind(&self) -> BootstrapTargetKind;

    /// Provisions or validates connectivity to the Stage 0 seed Kubernetes cluster.
    async fn provision_cluster(
        &self,
        cfg: &BootstrapConfig,
    ) -> Result<ClusterConnection, CoreError>;

    /// Tears down or detaches from the Stage 0 seed cluster.
    async fn teardown(&self, cfg: &BootstrapConfig) -> Result<(), CoreError>;
}

/// Instantiates a Stage 0 `BootstrapTarget` driver for the requested `BootstrapTargetKind`.
pub fn factory(kind: BootstrapTargetKind) -> Result<Box<dyn BootstrapTarget>, CoreError> {
    match kind {
        BootstrapTargetKind::Kubeconfig => Ok(Box::new(KubeconfigBootstrapTarget::new())),
        BootstrapTargetKind::Kind
        | BootstrapTargetKind::Proxmox
        | BootstrapTargetKind::Aws
        | BootstrapTargetKind::Gcp
        | BootstrapTargetKind::Azure
        | BootstrapTargetKind::Hetzner => Ok(Box::new(PulumiBootstrapTarget::new(kind))),
    }
}

/// Unified Stage 0 driver backed by the `pulumi` CLI for `kind`, `proxmox`, `aws`, `gcp`,
/// `azure`, and `hetzner` bootstrap targets (`SDD.md` §2.1, `NFR-SEC-06`).
#[derive(Debug, Clone)]
pub struct PulumiBootstrapTarget {
    target: BootstrapTargetKind,
    pulumi_bin: PathBuf,
}

impl PulumiBootstrapTarget {
    /// Creates a `PulumiBootstrapTarget` resolving `pulumi` from `PATH`.
    pub fn new(target: BootstrapTargetKind) -> Self {
        Self {
            target,
            pulumi_bin: PathBuf::from("pulumi"),
        }
    }

    /// Creates a `PulumiBootstrapTarget` using an explicit path to the `pulumi` binary.
    pub fn with_binary(target: BootstrapTargetKind, pulumi_bin: impl Into<PathBuf>) -> Self {
        Self {
            target,
            pulumi_bin: pulumi_bin.into(),
        }
    }

    /// Resolves the Stage 0 Pulumi program directory from `cfg.pulumi_program_dir` or the
    /// default `deploy/bootstrap/<target>` path.
    pub fn resolve_program_dir(&self, cfg: &BootstrapConfig) -> PathBuf {
        cfg.pulumi_program_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("deploy/bootstrap").join(self.target.to_string()))
    }

    /// Returns the embedded Stage 0 `Pulumi.yaml` template for this target, if available.
    pub fn embedded_pulumi_yaml(&self) -> Option<&'static str> {
        match self.target {
            BootstrapTargetKind::Kind => {
                Some(include_str!("../../../deploy/bootstrap/kind/Pulumi.yaml"))
            }
            BootstrapTargetKind::Proxmox => Some(include_str!(
                "../../../deploy/bootstrap/proxmox/Pulumi.yaml"
            )),
            BootstrapTargetKind::Aws => {
                Some(include_str!("../../../deploy/bootstrap/aws/Pulumi.yaml"))
            }
            BootstrapTargetKind::Gcp => {
                Some(include_str!("../../../deploy/bootstrap/gcp/Pulumi.yaml"))
            }
            BootstrapTargetKind::Azure => {
                Some(include_str!("../../../deploy/bootstrap/azure/Pulumi.yaml"))
            }
            BootstrapTargetKind::Hetzner => Some(include_str!(
                "../../../deploy/bootstrap/hetzner/Pulumi.yaml"
            )),
            BootstrapTargetKind::Kubeconfig => None,
        }
    }

    async fn materialize_program_dir(
        &self,
        cfg: &BootstrapConfig,
        state_dir: &Path,
    ) -> Result<PathBuf, CoreError> {
        if let Some(explicit) = &cfg.pulumi_program_dir {
            if !explicit.is_dir() {
                return Err(CoreError::Validation(format!(
                    "Pulumi program directory does not exist: {}",
                    explicit.display()
                )));
            }
            return Ok(explicit.clone());
        }

        let rel = PathBuf::from("deploy/bootstrap").join(self.target.to_string());
        if rel.join("Pulumi.yaml").is_file() {
            return Ok(rel);
        }
        let parent_rel = PathBuf::from("..").join(&rel);
        if parent_rel.join("Pulumi.yaml").is_file() {
            return Ok(parent_rel);
        }

        if let Some(yaml) = self.embedded_pulumi_yaml() {
            let embedded_dir = state_dir.join("program");
            tokio::fs::create_dir_all(&embedded_dir).await?;
            tokio::fs::write(embedded_dir.join("Pulumi.yaml"), yaml).await?;
            return Ok(embedded_dir);
        }

        Err(CoreError::Validation(format!(
            "No Stage 0 Pulumi program found for target '{}'",
            self.target
        )))
    }

    /// Returns the isolated local Pulumi state directory for this Stage 0 target and cluster.
    pub fn state_dir(&self, cfg: &BootstrapConfig) -> PathBuf {
        std::env::temp_dir()
            .join("keel-stage0-pulumi")
            .join(format!("{}-{}", self.target, cfg.cluster_name))
    }

    /// Returns the Pulumi stack name for this Stage 0 target and cluster.
    pub fn stack_name(&self, cfg: &BootstrapConfig) -> String {
        format!("stage0-{}-{}", self.target, cfg.cluster_name)
    }

    /// Builds explicit CLI arguments for `pulumi login file://<state_dir>`.
    pub fn build_login_args(state_dir: &Path) -> Vec<String> {
        vec![
            "login".to_string(),
            format!("file://{}", state_dir.display()),
        ]
    }

    /// Builds explicit CLI arguments for `pulumi stack select <stack> [--create]`.
    pub fn build_stack_select_args(
        &self,
        cfg: &BootstrapConfig,
        create: bool,
    ) -> Result<Vec<String>, CoreError> {
        self.ensure_target(cfg)?;
        let mut args = vec![
            "stack".to_string(),
            "select".to_string(),
            self.stack_name(cfg),
        ];
        if create {
            args.push("--create".to_string());
        }
        Ok(args)
    }

    /// Builds explicit CLI arguments for `pulumi install` (materializing parameterized providers).
    pub fn build_install_args() -> Vec<String> {
        vec!["install".to_string()]
    }

    /// Builds `(key, value)` Pulumi configuration pairs for the Stage 0 stack.
    pub fn build_config_set_pairs(
        &self,
        cfg: &BootstrapConfig,
    ) -> Result<Vec<(String, String)>, CoreError> {
        self.ensure_target(cfg)?;
        let mut pairs = vec![("clusterName".to_string(), cfg.cluster_name.clone())];

        if self.target == BootstrapTargetKind::Kind {
            let mount_idx = if cfg.local_dev_docker_socket_exception {
                "1"
            } else {
                "0"
            };
            pairs.push(("dockerSocketMountIndex".to_string(), mount_idx.to_string()));
        }

        if let Some(region) = &cfg.region {
            pairs.push(("region".to_string(), region.clone()));
        }
        if let Some(proxmox_url) = &cfg.proxmox_url {
            pairs.push(("proxmoxUrl".to_string(), proxmox_url.clone()));
        }
        if let Some(distro) = &cfg.distro {
            pairs.push(("distro".to_string(), distro.to_string()));
        }

        Ok(pairs)
    }

    /// Builds explicit CLI arguments for `pulumi config set <key> <value>`.
    pub fn build_config_set_args(key: &str, value: &str) -> Vec<String> {
        vec![
            "config".to_string(),
            "set".to_string(),
            key.to_string(),
            value.to_string(),
        ]
    }

    /// Builds explicit CLI arguments for `pulumi up --yes --non-interactive`.
    pub fn build_up_args() -> Vec<String> {
        vec![
            "up".to_string(),
            "--yes".to_string(),
            "--non-interactive".to_string(),
        ]
    }

    /// Builds explicit CLI arguments for `pulumi stack output kubeconfig --show-secrets`.
    pub fn build_stack_output_kubeconfig_args() -> Vec<String> {
        vec![
            "stack".to_string(),
            "output".to_string(),
            "kubeconfig".to_string(),
            "--show-secrets".to_string(),
        ]
    }

    /// Builds explicit CLI arguments for `pulumi destroy --yes --non-interactive`.
    pub fn build_destroy_args() -> Vec<String> {
        vec![
            "destroy".to_string(),
            "--yes".to_string(),
            "--non-interactive".to_string(),
        ]
    }

    fn ensure_target(&self, cfg: &BootstrapConfig) -> Result<(), CoreError> {
        if self.target == BootstrapTargetKind::Kubeconfig {
            return Err(CoreError::Validation(
                "PulumiBootstrapTarget does not support the 'kubeconfig' target; use KubeconfigBootstrapTarget".to_string(),
            ));
        }
        if cfg.target != self.target {
            return Err(CoreError::Validation(format!(
                "PulumiBootstrapTarget expected target '{}', got '{}'",
                self.target, cfg.target
            )));
        }
        cfg.validate()
    }

    async fn run_pulumi(&self, program_dir: &Path, args: &[String]) -> Result<String, CoreError> {
        let program_label = self.pulumi_bin.display().to_string();
        let mut cmd = Command::new(&self.pulumi_bin);
        cmd.args(args)
            .current_dir(program_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PULUMI_SKIP_UPDATE_CHECK", "true");

        if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_none() {
            cmd.env("PULUMI_CONFIG_PASSPHRASE", "");
        }

        let output = cmd.output().await.map_err(|e| CoreError::CommandFailed {
            program: program_label.clone(),
            message: e.to_string(),
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            return Err(CoreError::CommandFailed {
                program: program_label,
                message: format!("exit status {}: {}", output.status, detail),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[async_trait]
impl BootstrapTarget for PulumiBootstrapTarget {
    fn kind(&self) -> BootstrapTargetKind {
        self.target
    }

    async fn provision_cluster(
        &self,
        cfg: &BootstrapConfig,
    ) -> Result<ClusterConnection, CoreError> {
        self.ensure_target(cfg)?;
        let state_dir = self.state_dir(cfg);
        tokio::fs::create_dir_all(&state_dir).await?;
        let program_dir = self.materialize_program_dir(cfg, &state_dir).await?;

        tracing::info!(
            target = %self.target,
            cluster = %cfg.cluster_name,
            program_dir = %program_dir.display(),
            "Provisioning Stage 0 cluster via Pulumi"
        );

        self.run_pulumi(&program_dir, &Self::build_login_args(&state_dir))
            .await?;
        let select_args = self.build_stack_select_args(cfg, true)?;
        self.run_pulumi(&program_dir, &select_args).await?;
        self.run_pulumi(&program_dir, &Self::build_install_args())
            .await?;

        for (key, value) in self.build_config_set_pairs(cfg)? {
            let set_args = Self::build_config_set_args(&key, &value);
            self.run_pulumi(&program_dir, &set_args).await?;
        }

        self.run_pulumi(&program_dir, &Self::build_up_args())
            .await?;

        let kubeconfig_yaml = self
            .run_pulumi(&program_dir, &Self::build_stack_output_kubeconfig_args())
            .await?;
        let validated = validate_kubeconfig_yaml(&kubeconfig_yaml, cfg.context.as_deref())?;

        Ok(ClusterConnection {
            target: self.target,
            cluster_name: cfg.cluster_name.clone(),
            kubeconfig_yaml,
            kubeconfig_path: cfg.kubeconfig.clone(),
            context: Some(validated.resolved_context),
            server_endpoint: Some(validated.server_endpoint),
        })
    }

    async fn teardown(&self, cfg: &BootstrapConfig) -> Result<(), CoreError> {
        self.ensure_target(cfg)?;
        let state_dir = self.state_dir(cfg);
        tokio::fs::create_dir_all(&state_dir).await?;
        let program_dir = self.materialize_program_dir(cfg, &state_dir).await?;

        tracing::info!(
            target = %self.target,
            cluster = %cfg.cluster_name,
            program_dir = %program_dir.display(),
            "Tearing down Stage 0 cluster via Pulumi"
        );

        self.run_pulumi(&program_dir, &Self::build_login_args(&state_dir))
            .await?;
        let select_args = self.build_stack_select_args(cfg, false)?;
        self.run_pulumi(&program_dir, &select_args).await?;
        self.run_pulumi(&program_dir, &Self::build_destroy_args())
            .await?;

        Ok(())
    }
}

/// Pre-existing Kubernetes cluster Stage 0 driver (`k3s`, `Talos`, managed K8s) via `kubeconfig`.
#[derive(Debug, Clone, Default)]
pub struct KubeconfigBootstrapTarget;

impl KubeconfigBootstrapTarget {
    pub fn new() -> Self {
        Self
    }

    /// Resolves the target kubeconfig file path from `BootstrapConfig` or standard environment locations.
    pub fn resolve_kubeconfig_path(cfg: &BootstrapConfig) -> Result<PathBuf, CoreError> {
        if let Some(path) = &cfg.kubeconfig {
            return Ok(path.clone());
        }
        if let Ok(env_kc) = std::env::var("KUBECONFIG")
            && let Some(first) = std::env::split_paths(&env_kc).find(|p| !p.as_os_str().is_empty())
        {
            return Ok(first);
        }
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
            && !home.trim().is_empty()
        {
            return Ok(Path::new(&home).join(".kube").join("config"));
        }
        Err(CoreError::Validation(
            "Unable to resolve kubeconfig path; pass --kubeconfig explicitly".to_string(),
        ))
    }
}

#[async_trait]
impl BootstrapTarget for KubeconfigBootstrapTarget {
    fn kind(&self) -> BootstrapTargetKind {
        BootstrapTargetKind::Kubeconfig
    }

    async fn provision_cluster(
        &self,
        cfg: &BootstrapConfig,
    ) -> Result<ClusterConnection, CoreError> {
        if cfg.target != BootstrapTargetKind::Kubeconfig {
            return Err(CoreError::Validation(format!(
                "KubeconfigBootstrapTarget expected target 'kubeconfig', got '{}'",
                cfg.target
            )));
        }
        cfg.validate()?;

        let kc_path = Self::resolve_kubeconfig_path(cfg)?;
        let kubeconfig_yaml = tokio::fs::read_to_string(&kc_path).await.map_err(|e| {
            CoreError::Validation(format!(
                "Failed to read kubeconfig at '{}': {}",
                kc_path.display(),
                e
            ))
        })?;

        let validated = validate_kubeconfig_yaml(&kubeconfig_yaml, cfg.context.as_deref())?;
        Ok(ClusterConnection {
            target: BootstrapTargetKind::Kubeconfig,
            cluster_name: cfg.cluster_name.clone(),
            kubeconfig_yaml,
            kubeconfig_path: Some(kc_path),
            context: Some(validated.resolved_context),
            server_endpoint: Some(validated.server_endpoint),
        })
    }

    async fn teardown(&self, cfg: &BootstrapConfig) -> Result<(), CoreError> {
        if cfg.target != BootstrapTargetKind::Kubeconfig {
            return Err(CoreError::Validation(format!(
                "KubeconfigBootstrapTarget expected target 'kubeconfig', got '{}'",
                cfg.target
            )));
        }
        cfg.validate()?;
        tracing::info!(
            cluster = %cfg.cluster_name,
            "Detaching from external kubeconfig cluster without destructive teardown"
        );
        Ok(())
    }
}

/// Summary extracted after validating a Kubernetes `kubeconfig` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedKubeconfig {
    pub resolved_context: String,
    pub cluster_name: String,
    pub server_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct RawKubeconfig {
    #[serde(default)]
    clusters: Vec<NamedCluster>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
    #[serde(default)]
    users: Vec<NamedUser>,
    #[serde(rename = "current-context", default)]
    current_context: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedCluster {
    name: String,
    cluster: ClusterEndpoint,
}

#[derive(Debug, Deserialize)]
struct ClusterEndpoint {
    #[serde(default)]
    server: String,
}

#[derive(Debug, Deserialize)]
struct NamedContext {
    name: String,
    context: ContextSpec,
}

#[derive(Debug, Deserialize)]
struct ContextSpec {
    #[serde(default)]
    cluster: String,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedUser {
    name: String,
}

/// Parses and validates a Kubernetes `kubeconfig` YAML payload and optional context override.
pub fn validate_kubeconfig_yaml(
    yaml: &str,
    requested_context: Option<&str>,
) -> Result<ValidatedKubeconfig, CoreError> {
    if yaml.trim().is_empty() {
        return Err(CoreError::Validation(
            "Kubeconfig content is empty".to_string(),
        ));
    }

    let raw: RawKubeconfig = serde_yml::from_str(yaml)?;
    if raw.clusters.is_empty() {
        return Err(CoreError::Validation(
            "Kubeconfig contains no 'clusters' entries".to_string(),
        ));
    }
    if raw.contexts.is_empty() {
        return Err(CoreError::Validation(
            "Kubeconfig contains no 'contexts' entries".to_string(),
        ));
    }

    let context_name = match requested_context {
        Some(ctx) if !ctx.trim().is_empty() => ctx.trim(),
        _ => raw
            .current_context
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| {
                CoreError::Validation(
                    "No context specified and kubeconfig has no 'current-context'".to_string(),
                )
            })?,
    };

    let ctx_entry = raw
        .contexts
        .iter()
        .find(|c| c.name == context_name)
        .ok_or_else(|| {
            CoreError::Validation(format!(
                "Context '{}' not found in kubeconfig",
                context_name
            ))
        })?;

    if ctx_entry.context.cluster.trim().is_empty() {
        return Err(CoreError::Validation(format!(
            "Context '{}' does not reference a cluster",
            context_name
        )));
    }

    if let Some(user) = ctx_entry.context.user.as_deref()
        && !user.trim().is_empty()
        && !raw.users.iter().any(|u| u.name == user)
    {
        return Err(CoreError::Validation(format!(
            "Context '{}' references user '{}' which does not exist in kubeconfig",
            context_name, user
        )));
    }

    let cluster_entry = raw
        .clusters
        .iter()
        .find(|cl| cl.name == ctx_entry.context.cluster)
        .ok_or_else(|| {
            CoreError::Validation(format!(
                "Cluster '{}' referenced by context '{}' not found in kubeconfig",
                ctx_entry.context.cluster, context_name
            ))
        })?;

    let server = cluster_entry.cluster.server.trim();
    if !(server.starts_with("https://") || server.starts_with("http://")) {
        return Err(CoreError::Validation(format!(
            "Cluster '{}' has invalid server endpoint '{}'; expected http(s):// URL",
            cluster_entry.name, server
        )));
    }

    Ok(ValidatedKubeconfig {
        resolved_context: context_name.to_string(),
        cluster_name: cluster_entry.name.clone(),
        server_endpoint: server.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
current-context: kind-keel-mgmt
clusters:
  - name: kind-keel-mgmt
    cluster:
      server: https://127.0.0.1:6443
  - name: staging-k3s
    cluster:
      server: https://10.0.0.10:6443
contexts:
  - name: kind-keel-mgmt
    context:
      cluster: kind-keel-mgmt
      user: kind-keel-mgmt
  - name: staging
    context:
      cluster: staging-k3s
      user: staging-admin
users:
  - name: kind-keel-mgmt
  - name: staging-admin
"#;

    #[test]
    fn test_factory_provider_agnostic_targets() {
        for kind in BootstrapTargetKind::all() {
            let driver = factory(*kind).unwrap();
            assert_eq!(driver.kind(), *kind);
        }
    }

    #[test]
    fn test_pulumi_command_arg_vectors_and_validation() {
        let driver = PulumiBootstrapTarget::new(BootstrapTargetKind::Kind);
        let mut cfg = BootstrapConfig::new(BootstrapTargetKind::Kind);
        cfg.cluster_name = "keel-dev".to_string();

        assert_eq!(
            driver.resolve_program_dir(&cfg),
            PathBuf::from("deploy/bootstrap/kind")
        );
        cfg.pulumi_program_dir = Some(PathBuf::from("/custom/pulumi/kind"));
        assert_eq!(
            driver.resolve_program_dir(&cfg),
            PathBuf::from("/custom/pulumi/kind")
        );

        let state_dir = driver.state_dir(&cfg);
        assert!(state_dir.ends_with("keel-stage0-pulumi/kind-keel-dev"));
        assert_eq!(
            PulumiBootstrapTarget::build_login_args(&state_dir),
            vec!["login", &format!("file://{}", state_dir.display())]
        );

        assert_eq!(
            driver.build_stack_select_args(&cfg, true).unwrap(),
            vec!["stack", "select", "stage0-kind-keel-dev", "--create"]
        );
        assert_eq!(
            driver.build_stack_select_args(&cfg, false).unwrap(),
            vec!["stack", "select", "stage0-kind-keel-dev"]
        );

        assert_eq!(
            driver.build_config_set_pairs(&cfg).unwrap(),
            vec![
                ("clusterName".to_string(), "keel-dev".to_string()),
                ("dockerSocketMountIndex".to_string(), "0".to_string()),
            ]
        );

        cfg.local_dev_docker_socket_exception = true;
        assert_eq!(
            driver.build_config_set_pairs(&cfg).unwrap(),
            vec![
                ("clusterName".to_string(), "keel-dev".to_string()),
                ("dockerSocketMountIndex".to_string(), "1".to_string()),
            ]
        );

        assert_eq!(
            PulumiBootstrapTarget::build_config_set_args("clusterName", "keel-dev"),
            vec!["config", "set", "clusterName", "keel-dev"]
        );
        assert_eq!(
            PulumiBootstrapTarget::build_up_args(),
            vec!["up", "--yes", "--non-interactive"]
        );
        assert_eq!(
            PulumiBootstrapTarget::build_stack_output_kubeconfig_args(),
            vec!["stack", "output", "kubeconfig", "--show-secrets"]
        );
        assert_eq!(
            PulumiBootstrapTarget::build_destroy_args(),
            vec!["destroy", "--yes", "--non-interactive"]
        );

        // Optional region / proxmoxUrl / distro config pairs
        let pve_driver = PulumiBootstrapTarget::new(BootstrapTargetKind::Proxmox);
        let mut pve_cfg = BootstrapConfig::new(BootstrapTargetKind::Proxmox);
        pve_cfg.cluster_name = "pve-mgmt".to_string();
        pve_cfg.region = Some("eu-central".to_string());
        pve_cfg.proxmox_url = Some("https://pve.example.com:8006".to_string());
        pve_cfg.distro = Some(crate::models::BootstrapDistro::Talos);
        assert_eq!(
            pve_driver.build_config_set_pairs(&pve_cfg).unwrap(),
            vec![
                ("clusterName".to_string(), "pve-mgmt".to_string()),
                ("region".to_string(), "eu-central".to_string()),
                (
                    "proxmoxUrl".to_string(),
                    "https://pve.example.com:8006".to_string()
                ),
                ("distro".to_string(), "talos".to_string()),
            ]
        );

        // Wrong target rejected
        let wrong = BootstrapConfig::new(BootstrapTargetKind::Aws);
        assert!(driver.build_stack_select_args(&wrong, true).is_err());
        assert!(driver.build_config_set_pairs(&wrong).is_err());

        // Kubeconfig target rejected by PulumiBootstrapTarget
        let kc_driver = PulumiBootstrapTarget::new(BootstrapTargetKind::Kubeconfig);
        let kc_cfg = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        assert!(kc_driver.build_stack_select_args(&kc_cfg, true).is_err());
        assert!(kc_driver.build_config_set_pairs(&kc_cfg).is_err());

        // Flag injection rejected via BootstrapConfig::validate
        let mut bad_cfg = BootstrapConfig::new(BootstrapTargetKind::Kind);
        bad_cfg.pulumi_program_dir = Some(PathBuf::from("-evil"));
        assert!(driver.build_stack_select_args(&bad_cfg, true).is_err());
    }

    #[test]
    fn test_validate_kubeconfig_yaml_cases() {
        // Default current-context
        let v1 = validate_kubeconfig_yaml(SAMPLE_KUBECONFIG, None).unwrap();
        assert_eq!(v1.resolved_context, "kind-keel-mgmt");
        assert_eq!(v1.cluster_name, "kind-keel-mgmt");
        assert_eq!(v1.server_endpoint, "https://127.0.0.1:6443");

        // Explicit context override
        let v2 = validate_kubeconfig_yaml(SAMPLE_KUBECONFIG, Some("staging")).unwrap();
        assert_eq!(v2.resolved_context, "staging");
        assert_eq!(v2.cluster_name, "staging-k3s");
        assert_eq!(v2.server_endpoint, "https://10.0.0.10:6443");

        // Missing context
        assert!(validate_kubeconfig_yaml(SAMPLE_KUBECONFIG, Some("missing")).is_err());

        // Empty YAML
        assert!(validate_kubeconfig_yaml("   ", None).is_err());

        // No clusters
        assert!(validate_kubeconfig_yaml("contexts: []\nclusters: []\n", None).is_err());

        // No contexts
        let no_ctx = r#"
clusters:
  - name: c1
    cluster:
      server: https://127.0.0.1:6443
contexts: []
"#;
        assert!(validate_kubeconfig_yaml(no_ctx, None).is_err());

        // Missing current-context when none requested
        let no_current = r#"
clusters:
  - name: c1
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: ctx1
    context:
      cluster: c1
"#;
        assert!(validate_kubeconfig_yaml(no_current, None).is_err());

        // Missing referenced user
        let missing_user = r#"
current-context: ctx1
clusters:
  - name: c1
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: ctx1
    context:
      cluster: c1
      user: ghost
users: []
"#;
        assert!(validate_kubeconfig_yaml(missing_user, None).is_err());

        // Missing referenced cluster
        let missing_cluster = r#"
current-context: ctx1
clusters:
  - name: other
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: ctx1
    context:
      cluster: c1
"#;
        assert!(validate_kubeconfig_yaml(missing_cluster, None).is_err());

        // Invalid server URL
        let bad_server = r#"
current-context: ctx1
clusters:
  - name: c1
    cluster:
      server: unix:///var/run/k8s.sock
contexts:
  - name: ctx1
    context:
      cluster: c1
"#;
        assert!(validate_kubeconfig_yaml(bad_server, None).is_err());
    }

    #[tokio::test]
    async fn test_kubeconfig_bootstrap_driver_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("keel-kc-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let kc_file = temp_dir.join("config");
        tokio::fs::write(&kc_file, SAMPLE_KUBECONFIG).await.unwrap();

        let driver = KubeconfigBootstrapTarget::new();
        assert_eq!(driver.kind(), BootstrapTargetKind::Kubeconfig);

        let mut cfg = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        cfg.kubeconfig = Some(kc_file.clone());
        cfg.context = Some("staging".to_string());

        let conn = driver.provision_cluster(&cfg).await.unwrap();
        assert_eq!(conn.target, BootstrapTargetKind::Kubeconfig);
        assert_eq!(conn.context.as_deref(), Some("staging"));
        assert_eq!(
            conn.server_endpoint.as_deref(),
            Some("https://10.0.0.10:6443")
        );
        assert_eq!(conn.kubeconfig_path.as_deref(), Some(kc_file.as_path()));

        driver.teardown(&cfg).await.unwrap();

        // Missing file error
        let mut missing_cfg = cfg.clone();
        missing_cfg.kubeconfig = Some(temp_dir.join("does-not-exist"));
        assert!(driver.provision_cluster(&missing_cfg).await.is_err());

        // Wrong target error
        let wrong_target = BootstrapConfig::new(BootstrapTargetKind::Kind);
        assert!(driver.provision_cluster(&wrong_target).await.is_err());
        assert!(driver.teardown(&wrong_target).await.is_err());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_pulumi_bootstrap_driver_with_mock_binary() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir =
            std::env::temp_dir().join(format!("keel-pulumi-test-{}", uuid::Uuid::new_v4()));
        let program_dir = temp_dir.join("program");
        tokio::fs::create_dir_all(&program_dir).await.unwrap();
        let mock_pulumi = temp_dir.join("mock-pulumi");
        let log_file = temp_dir.join("invocations.log");

        let script = format!(
            "#!/bin/sh\necho \"$@\" >> \"{}\"\nif [ \"$1\" = \"login\" ] || [ \"$1\" = \"install\" ] || [ \"$1\" = \"up\" ] || [ \"$1\" = \"destroy\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"stack\" ] && [ \"$2\" = \"select\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"config\" ] && [ \"$2\" = \"set\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"stack\" ] && [ \"$2\" = \"output\" ] && [ \"$3\" = \"kubeconfig\" ]; then\n  cat << 'EOF'\n{}\nEOF\n  exit 0\nfi\nexit 1\n",
            log_file.display(),
            SAMPLE_KUBECONFIG
        );
        // Write atomically via temp file + rename to avoid ETXTBSY with parallel test forks
        let mock_pulumi_tmp = temp_dir.join("mock-pulumi.tmp");
        std::fs::write(&mock_pulumi_tmp, script.as_bytes()).unwrap();
        let mut perms = std::fs::metadata(&mock_pulumi_tmp).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&mock_pulumi_tmp, perms).unwrap();
        std::fs::rename(&mock_pulumi_tmp, &mock_pulumi).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let driver = PulumiBootstrapTarget::with_binary(BootstrapTargetKind::Kind, &mock_pulumi);
        assert_eq!(driver.kind(), BootstrapTargetKind::Kind);

        let mut cfg = BootstrapConfig::new(BootstrapTargetKind::Kind);
        cfg.cluster_name = format!("test-{}", uuid::Uuid::new_v4().simple());
        cfg.pulumi_program_dir = Some(program_dir.clone());
        cfg.local_dev_docker_socket_exception = true;

        let conn = driver.provision_cluster(&cfg).await.unwrap();
        assert_eq!(conn.target, BootstrapTargetKind::Kind);
        assert_eq!(conn.cluster_name, cfg.cluster_name);
        assert_eq!(conn.context.as_deref(), Some("kind-keel-mgmt"));
        assert_eq!(
            conn.server_endpoint.as_deref(),
            Some("https://127.0.0.1:6443")
        );

        driver.teardown(&cfg).await.unwrap();

        let logged = tokio::fs::read_to_string(&log_file).await.unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert!(lines.iter().any(|l| l.starts_with("login file://")));
        assert!(lines.contains(&"install"));
        assert!(
            lines
                .iter()
                .any(|l| *l == format!("stack select stage0-kind-{} --create", cfg.cluster_name))
        );
        assert!(
            lines
                .iter()
                .any(|l| *l == format!("config set clusterName {}", cfg.cluster_name))
        );
        assert!(lines.contains(&"config set dockerSocketMountIndex 1"));
        assert!(lines.contains(&"up --yes --non-interactive"));
        assert!(lines.contains(&"stack output kubeconfig --show-secrets"));
        assert!(
            lines
                .iter()
                .any(|l| *l == format!("stack select stage0-kind-{}", cfg.cluster_name))
        );
        assert!(lines.contains(&"destroy --yes --non-interactive"));

        // Clean up isolated state dir created by test
        let _ = tokio::fs::remove_dir_all(driver.state_dir(&cfg)).await;

        // Non-existent binary error handling
        let bad_driver = PulumiBootstrapTarget::with_binary(
            BootstrapTargetKind::Kind,
            temp_dir.join("missing-pulumi-bin"),
        );
        assert!(bad_driver.provision_cluster(&cfg).await.is_err());

        // Non-zero exit code error handling
        let failing_pulumi = temp_dir.join("failing-pulumi");
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&failing_pulumi).await.unwrap();
            file.write_all(b"#!/bin/sh\necho 'pulumi login failed' >&2\nexit 1\n")
                .await
                .unwrap();
            file.sync_all().await.unwrap();
        }
        let mut fail_perms = tokio::fs::metadata(&failing_pulumi)
            .await
            .unwrap()
            .permissions();
        fail_perms.set_mode(0o755);
        tokio::fs::set_permissions(&failing_pulumi, fail_perms)
            .await
            .unwrap();

        let failing_driver =
            PulumiBootstrapTarget::with_binary(BootstrapTargetKind::Kind, &failing_pulumi);
        let err = failing_driver.provision_cluster(&cfg).await.unwrap_err();
        assert!(err.to_string().contains("pulumi login failed"));

        // Default constructor & resolve_kubeconfig_path fallback
        let _default_kc = KubeconfigBootstrapTarget;
        let fallback_cfg = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        let resolved = KubeconfigBootstrapTarget::resolve_kubeconfig_path(&fallback_cfg);
        assert!(resolved.is_ok());

        let _ = tokio::fs::remove_dir_all(driver.state_dir(&cfg)).await;
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
