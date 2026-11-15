//! Provider-agnostic Stage 0 cluster bootstrap trait and drivers (`SDD.md` §2.1).
//!
//! External CLI invocations (`kind`) strictly use `tokio::process::Command` with explicit
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
///
/// Cloud and hypervisor targets (`proxmox`, `aws`, `gcp`, `azure`, `hetzner`) return
/// `CoreError::NotImplemented` until the Stage 0 Pulumi runner is wired, preserving a
/// unified provider-agnostic interface.
pub fn factory(kind: BootstrapTargetKind) -> Result<Box<dyn BootstrapTarget>, CoreError> {
    match kind {
        BootstrapTargetKind::Kind => Ok(Box::new(KindBootstrapTarget::new())),
        BootstrapTargetKind::Kubeconfig => Ok(Box::new(KubeconfigBootstrapTarget::new())),
        BootstrapTargetKind::Proxmox
        | BootstrapTargetKind::Aws
        | BootstrapTargetKind::Gcp
        | BootstrapTargetKind::Azure
        | BootstrapTargetKind::Hetzner => Err(CoreError::NotImplemented(format!(
            "Stage 0 bootstrap driver for '{}' requires the Stage 0 Pulumi runner",
            kind
        ))),
    }
}

/// Local workstation Stage 0 driver backed by `kind` (`Kubernetes IN Docker`).
#[derive(Debug, Clone)]
pub struct KindBootstrapTarget {
    kind_bin: PathBuf,
}

impl Default for KindBootstrapTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl KindBootstrapTarget {
    /// Creates a `KindBootstrapTarget` resolving `kind` from `PATH`.
    pub fn new() -> Self {
        Self {
            kind_bin: PathBuf::from("kind"),
        }
    }

    /// Creates a `KindBootstrapTarget` using an explicit path to the `kind` binary.
    pub fn with_binary(kind_bin: impl Into<PathBuf>) -> Self {
        Self {
            kind_bin: kind_bin.into(),
        }
    }

    /// Builds the `kind` cluster config YAML mounting `/var/run/docker.sock` for local-dev nested `kind`.
    pub fn build_local_dev_kind_config_yaml() -> &'static str {
        "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes:\n  - role: control-plane\n    extraMounts:\n      - hostPath: /var/run/docker.sock\n        containerPath: /var/run/docker.sock\n"
    }

    /// Builds explicit CLI arguments for `kind create cluster`.
    pub fn build_create_args(cfg: &BootstrapConfig) -> Result<Vec<String>, CoreError> {
        Self::ensure_kind_target(cfg)?;
        let mut args = vec![
            "create".to_string(),
            "cluster".to_string(),
            "--name".to_string(),
            cfg.cluster_name.clone(),
        ];
        if let Some(config_path) = &cfg.kind_config_path {
            args.push("--config".to_string());
            args.push(config_path.to_string_lossy().into_owned());
        }
        if let Some(kubeconfig) = &cfg.kubeconfig {
            args.push("--kubeconfig".to_string());
            args.push(kubeconfig.to_string_lossy().into_owned());
        }
        Ok(args)
    }

    /// Builds explicit CLI arguments for `kind get kubeconfig`.
    pub fn build_get_kubeconfig_args(cfg: &BootstrapConfig) -> Result<Vec<String>, CoreError> {
        Self::ensure_kind_target(cfg)?;
        Ok(vec![
            "get".to_string(),
            "kubeconfig".to_string(),
            "--name".to_string(),
            cfg.cluster_name.clone(),
        ])
    }

    /// Builds explicit CLI arguments for `kind delete cluster`.
    pub fn build_delete_args(cfg: &BootstrapConfig) -> Result<Vec<String>, CoreError> {
        Self::ensure_kind_target(cfg)?;
        let mut args = vec![
            "delete".to_string(),
            "cluster".to_string(),
            "--name".to_string(),
            cfg.cluster_name.clone(),
        ];
        if let Some(kubeconfig) = &cfg.kubeconfig {
            args.push("--kubeconfig".to_string());
            args.push(kubeconfig.to_string_lossy().into_owned());
        }
        Ok(args)
    }

    fn ensure_kind_target(cfg: &BootstrapConfig) -> Result<(), CoreError> {
        if cfg.target != BootstrapTargetKind::Kind {
            return Err(CoreError::Validation(format!(
                "KindBootstrapTarget expected target 'kind', got '{}'",
                cfg.target
            )));
        }
        cfg.validate()
    }

    async fn run_kind(&self, args: &[String]) -> Result<String, CoreError> {
        let program_label = self.kind_bin.display().to_string();
        let output = Command::new(&self.kind_bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| CoreError::CommandFailed {
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
impl BootstrapTarget for KindBootstrapTarget {
    fn kind(&self) -> BootstrapTargetKind {
        BootstrapTargetKind::Kind
    }

    async fn provision_cluster(
        &self,
        cfg: &BootstrapConfig,
    ) -> Result<ClusterConnection, CoreError> {
        let mut temp_config_path: Option<PathBuf> = None;
        let effective_cfg =
            if cfg.kind_config_path.is_none() && cfg.local_dev_docker_socket_exception {
                let path = std::env::temp_dir()
                    .join(format!("keel-kind-config-{}.yaml", cfg.cluster_name));
                tokio::fs::write(&path, Self::build_local_dev_kind_config_yaml()).await?;
                temp_config_path = Some(path.clone());
                let mut cloned = cfg.clone();
                cloned.kind_config_path = Some(path);
                Some(cloned)
            } else {
                None
            };
        let create_args = Self::build_create_args(effective_cfg.as_ref().unwrap_or(cfg))?;
        tracing::info!(
            cluster = %cfg.cluster_name,
            local_dev_docker_socket_exception = cfg.local_dev_docker_socket_exception,
            "Provisioning Stage 0 kind cluster"
        );
        let run_result = self.run_kind(&create_args).await;
        if let Some(path) = temp_config_path {
            let _ = tokio::fs::remove_file(&path).await;
        }
        run_result?;

        let get_kc_args = Self::build_get_kubeconfig_args(cfg)?;
        let kubeconfig_yaml = self.run_kind(&get_kc_args).await?;
        let validated = validate_kubeconfig_yaml(&kubeconfig_yaml, cfg.context.as_deref())?;

        Ok(ClusterConnection {
            target: BootstrapTargetKind::Kind,
            cluster_name: cfg.cluster_name.clone(),
            kubeconfig_yaml,
            kubeconfig_path: cfg.kubeconfig.clone(),
            context: Some(validated.resolved_context),
            server_endpoint: Some(validated.server_endpoint),
        })
    }

    async fn teardown(&self, cfg: &BootstrapConfig) -> Result<(), CoreError> {
        let delete_args = Self::build_delete_args(cfg)?;
        tracing::info!(
            cluster = %cfg.cluster_name,
            "Tearing down Stage 0 kind cluster"
        );
        self.run_kind(&delete_args).await?;
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
        let kind_driver = factory(BootstrapTargetKind::Kind).unwrap();
        assert_eq!(kind_driver.kind(), BootstrapTargetKind::Kind);

        let kc_driver = factory(BootstrapTargetKind::Kubeconfig).unwrap();
        assert_eq!(kc_driver.kind(), BootstrapTargetKind::Kubeconfig);

        for unimplemented in [
            BootstrapTargetKind::Proxmox,
            BootstrapTargetKind::Aws,
            BootstrapTargetKind::Gcp,
            BootstrapTargetKind::Azure,
            BootstrapTargetKind::Hetzner,
        ] {
            let err = match factory(unimplemented) {
                Ok(_) => panic!("expected NotImplemented for {unimplemented}"),
                Err(err) => err,
            };
            assert!(matches!(err, CoreError::NotImplemented(_)));
            assert!(err.to_string().contains(unimplemented.as_str()));
        }
    }

    #[test]
    fn test_kind_command_arg_vectors() {
        let mut cfg = BootstrapConfig::new(BootstrapTargetKind::Kind);
        cfg.cluster_name = "keel-dev".to_string();
        cfg.kind_config_path = Some(PathBuf::from("/tmp/kind-config.yaml"));
        cfg.kubeconfig = Some(PathBuf::from("/tmp/kubeconfig"));

        let create_args = KindBootstrapTarget::build_create_args(&cfg).unwrap();
        assert_eq!(
            create_args,
            vec![
                "create",
                "cluster",
                "--name",
                "keel-dev",
                "--config",
                "/tmp/kind-config.yaml",
                "--kubeconfig",
                "/tmp/kubeconfig",
            ]
        );

        let get_kc_args = KindBootstrapTarget::build_get_kubeconfig_args(&cfg).unwrap();
        assert_eq!(get_kc_args, vec!["get", "kubeconfig", "--name", "keel-dev"]);

        let delete_args = KindBootstrapTarget::build_delete_args(&cfg).unwrap();
        assert_eq!(
            delete_args,
            vec![
                "delete",
                "cluster",
                "--name",
                "keel-dev",
                "--kubeconfig",
                "/tmp/kubeconfig",
            ]
        );

        // Wrong target rejected
        let wrong = BootstrapConfig::new(BootstrapTargetKind::Aws);
        assert!(KindBootstrapTarget::build_create_args(&wrong).is_err());
        assert!(KindBootstrapTarget::build_get_kubeconfig_args(&wrong).is_err());
        assert!(KindBootstrapTarget::build_delete_args(&wrong).is_err());

        let kind_cfg = KindBootstrapTarget::build_local_dev_kind_config_yaml();
        assert!(kind_cfg.contains("kind.x-k8s.io/v1alpha4"));
        assert!(kind_cfg.contains("/var/run/docker.sock"));
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
    async fn test_kind_bootstrap_driver_with_mock_binary() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir =
            std::env::temp_dir().join(format!("keel-kind-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let mock_kind = temp_dir.join("mock-kind");

        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"get\" ] && [ \"$2\" = \"kubeconfig\" ]; then\ncat << 'EOF'\n{}\nEOF\nexit 0\nfi\nif [ \"$1\" = \"create\" ] || [ \"$1\" = \"delete\" ]; then\nexit 0\nfi\nexit 1\n",
            SAMPLE_KUBECONFIG
        );
        // Write atomically via temp file + rename to avoid ETXTBSY with parallel test forks
        let mock_kind_tmp = temp_dir.join("mock-kind.tmp");
        std::fs::write(&mock_kind_tmp, script.as_bytes()).unwrap();
        let mut perms = std::fs::metadata(&mock_kind_tmp).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&mock_kind_tmp, perms).unwrap();
        std::fs::rename(&mock_kind_tmp, &mock_kind).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let driver = KindBootstrapTarget::with_binary(&mock_kind);
        assert_eq!(driver.kind(), BootstrapTargetKind::Kind);

        let cfg = BootstrapConfig::new(BootstrapTargetKind::Kind);
        let conn = driver.provision_cluster(&cfg).await.unwrap();
        assert_eq!(conn.target, BootstrapTargetKind::Kind);
        assert_eq!(conn.cluster_name, "keel-mgmt");
        assert_eq!(conn.context.as_deref(), Some("kind-keel-mgmt"));
        assert_eq!(
            conn.server_endpoint.as_deref(),
            Some("https://127.0.0.1:6443")
        );

        driver.teardown(&cfg).await.unwrap();

        // Non-existent binary error handling
        let bad_driver = KindBootstrapTarget::with_binary(temp_dir.join("missing-kind-bin"));
        assert!(bad_driver.provision_cluster(&cfg).await.is_err());

        // Non-zero exit code error handling
        let failing_kind = temp_dir.join("failing-kind");
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&failing_kind).await.unwrap();
            file.write_all(b"#!/bin/sh\necho 'docker daemon unavailable' >&2\nexit 1\n")
                .await
                .unwrap();
            file.sync_all().await.unwrap();
        }
        let mut fail_perms = tokio::fs::metadata(&failing_kind)
            .await
            .unwrap()
            .permissions();
        fail_perms.set_mode(0o755);
        tokio::fs::set_permissions(&failing_kind, fail_perms)
            .await
            .unwrap();

        let failing_driver = KindBootstrapTarget::with_binary(&failing_kind);
        let err = failing_driver.provision_cluster(&cfg).await.unwrap_err();
        assert!(err.to_string().contains("docker daemon unavailable"));

        // Default constructors & resolve_kubeconfig_path fallback
        let _default_kind = KindBootstrapTarget::default();
        let _default_kc = KubeconfigBootstrapTarget;
        let fallback_cfg = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        let resolved = KubeconfigBootstrapTarget::resolve_kubeconfig_path(&fallback_cfg);
        assert!(resolved.is_ok());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
