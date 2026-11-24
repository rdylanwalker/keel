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
///
/// `kubeconfig_yaml` carries cluster-admin credentials: it is redacted from `Debug` output and
/// never serialized (`NFR-SEC-05`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterConnection {
    pub target: BootstrapTargetKind,
    pub cluster_name: String,
    #[serde(default, skip_serializing)]
    pub kubeconfig_yaml: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_endpoint: Option<String>,
}

impl std::fmt::Debug for ClusterConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterConnection")
            .field("target", &self.target)
            .field("cluster_name", &self.cluster_name)
            .field("kubeconfig_yaml", &"[REDACTED]")
            .field("kubeconfig_path", &self.kubeconfig_path)
            .field("context", &self.context)
            .field("server_endpoint", &self.server_endpoint)
            .finish()
    }
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
    /// Override for the Stage 0 state root (defaults to [`stage0_state_root`]).
    state_root: Option<PathBuf>,
}

impl PulumiBootstrapTarget {
    /// Creates a `PulumiBootstrapTarget` resolving `pulumi` from `PATH`.
    pub fn new(target: BootstrapTargetKind) -> Self {
        Self {
            target,
            pulumi_bin: PathBuf::from("pulumi"),
            state_root: None,
        }
    }

    /// Creates a `PulumiBootstrapTarget` using an explicit path to the `pulumi` binary.
    #[cfg(test)]
    pub fn with_binary(target: BootstrapTargetKind, pulumi_bin: impl Into<PathBuf>) -> Self {
        Self {
            target,
            pulumi_bin: pulumi_bin.into(),
            state_root: None,
        }
    }

    /// Overrides the Stage 0 state root directory.
    #[cfg(test)]
    pub fn with_state_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.state_root = Some(root.into());
        self
    }

    fn root(&self) -> PathBuf {
        self.state_root.clone().unwrap_or_else(stage0_state_root)
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
        cluster_dir: &Path,
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

        // Without an explicit `--pulumi-program-dir`, only the program embedded in the binary is
        // used (never a cwd-relative `deploy/bootstrap/<target>` copy). It is materialized into
        // the private per-cluster program dir so the stack config file (`Pulumi.<stack>.yaml`,
        // holding the passphrase salt and encrypted config) lives next to the durable state
        // rather than in the repository checkout or `$TMP`.
        if let Some(yaml) = self.embedded_pulumi_yaml() {
            let program_dir = stage0_program_dir_in(cluster_dir, self.target.as_str());
            ensure_private_dir(&program_dir)?;
            tokio::fs::write(program_dir.join("Pulumi.yaml"), yaml).await?;
            return Ok(program_dir);
        }

        Err(CoreError::Validation(format!(
            "No Stage 0 Pulumi program found for target '{}'",
            self.target
        )))
    }

    /// Returns the private per-cluster Stage 0 directory (`<config>/keel/stage0/<cluster>`).
    pub fn cluster_dir(&self, cfg: &BootstrapConfig) -> PathBuf {
        self.root().join(&cfg.cluster_name)
    }

    /// Returns the local DIY Pulumi backend directory shared by all Stage 0 stacks of the cluster.
    pub fn state_dir(&self, cfg: &BootstrapConfig) -> PathBuf {
        self.cluster_dir(cfg).join(STAGE0_BACKEND_DIR)
    }

    /// Returns the Pulumi stack name for this Stage 0 target and cluster.
    pub fn stack_name(&self, cfg: &BootstrapConfig) -> String {
        format!("stage0-{}-{}", self.target, cfg.cluster_name)
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
        // `--proxmox-url` configures the default bpg/proxmox provider; credentials stay in the
        // environment (`PROXMOX_VE_API_TOKEN`, ...) and never reach Pulumi config.
        if self.target == BootstrapTargetKind::Proxmox
            && let Some(proxmox_url) = &cfg.proxmox_url
        {
            pairs.push(("proxmox:endpoint".to_string(), proxmox_url.clone()));
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

    async fn run_pulumi(
        &self,
        program_dir: &Path,
        state_dir: &Path,
        passphrase: &Stage0Passphrase,
        args: &[String],
    ) -> Result<String, CoreError> {
        let program_label = self.pulumi_bin.display().to_string();
        let mut cmd = Command::new(&self.pulumi_bin);
        cmd.args(args)
            .current_dir(program_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PULUMI_SKIP_UPDATE_CHECK", "true")
            .env(
                "PULUMI_BACKEND_URL",
                format!("file://{}", state_dir.display()),
            )
            // Passed via environment only, never argv (`NFR-SEC-06`).
            .env("PULUMI_CONFIG_PASSPHRASE", passphrase.expose())
            .env_remove("PULUMI_CONFIG_PASSPHRASE_FILE");

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
        let root = self.root();
        if self.target == BootstrapTargetKind::Kind {
            clear_stale_kind_stage0_state_in(&root, &cfg.cluster_name).await;
        }
        let cluster_dir = self.cluster_dir(cfg);
        let state_dir = self.state_dir(cfg);
        ensure_private_dir(&state_dir)?;
        let passphrase = resolve_stage0_passphrase_in(&root, &cfg.cluster_name)?;
        let program_dir = self.materialize_program_dir(cfg, &cluster_dir).await?;

        tracing::info!(
            target = %self.target,
            cluster = %cfg.cluster_name,
            program_dir = %program_dir.display(),
            state_dir = %state_dir.display(),
            "Provisioning Stage 0 cluster via Pulumi"
        );

        let run = |args: Vec<String>| {
            let program_dir = program_dir.clone();
            let state_dir = state_dir.clone();
            let passphrase = &passphrase;
            async move {
                self.run_pulumi(&program_dir, &state_dir, passphrase, &args)
                    .await
            }
        };
        run(self.build_stack_select_args(cfg, true)?).await?;
        run(Self::build_install_args()).await?;
        for (key, value) in self.build_config_set_pairs(cfg)? {
            run(Self::build_config_set_args(&key, &value)).await?;
        }
        run(Self::build_up_args()).await?;
        let kubeconfig_yaml = run(Self::build_stack_output_kubeconfig_args()).await?;
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
        let root = self.root();
        let cluster_dir = self.cluster_dir(cfg);
        let state_dir = self.state_dir(cfg);
        ensure_private_dir(&state_dir)?;
        let passphrase = resolve_stage0_passphrase_in(&root, &cfg.cluster_name)?;
        let program_dir = self.materialize_program_dir(cfg, &cluster_dir).await?;

        tracing::info!(
            target = %self.target,
            cluster = %cfg.cluster_name,
            program_dir = %program_dir.display(),
            "Tearing down Stage 0 cluster via Pulumi"
        );

        let select_args = self.build_stack_select_args(cfg, false)?;
        self.run_pulumi(&program_dir, &state_dir, &passphrase, &select_args)
            .await?;
        self.run_pulumi(
            &program_dir,
            &state_dir,
            &passphrase,
            &Self::build_destroy_args(),
        )
        .await?;

        Ok(())
    }
}

// ============================================================================
// Durable Stage 0 state layout & Pulumi passphrase (`NFR-REL-01`, `NFR-SEC-05`)
// ============================================================================

/// Sub-directory of a cluster's Stage 0 dir holding the local DIY Pulumi backend.
pub const STAGE0_BACKEND_DIR: &str = "state";
/// File (0600) inside a cluster's Stage 0 dir holding the generated Pulumi passphrase.
pub const STAGE0_PASSPHRASE_FILE: &str = "pulumi-passphrase";
/// Marker (`0600`, next to [`STAGE0_PASSPHRASE_FILE`]) recording that the operator has not yet
/// been shown the escrow notice for a generated passphrase. [`resolve_stage0_passphrase_in`]
/// writes it before it writes a newly generated passphrase, whichever caller (CLI bootstrap or a
/// target driver) triggered the generation; the CLI removes it only once a bootstrap summary
/// carrying the notice was produced (`NFR-REL-01`).
pub const STAGE0_ESCROW_NOTICE_PENDING_FILE: &str = "pulumi-passphrase.escrow-pending";

/// Returns the Keel CLI config directory: `$KEEL_CONFIG_DIR`, `$XDG_CONFIG_HOME/keel`,
/// `$HOME/.config/keel` (or `%APPDATA%\keel`), falling back to `<tmp>/keel` only if no home exists.
pub fn keel_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("KEEL_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("keel");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join(".config").join("keel");
    }
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(appdata).join("keel");
    }
    std::env::temp_dir().join("keel")
}

/// Root of all durable Stage 0 state (`<config>/stage0`).
pub fn stage0_state_root() -> PathBuf {
    keel_config_dir().join("stage0")
}

/// Per-cluster Stage 0 directory under `root`.
pub fn stage0_cluster_dir_in(root: &Path, cluster_name: &str) -> PathBuf {
    root.join(cluster_name)
}

/// Per-program materialized Pulumi project directory for a cluster.
pub fn stage0_program_dir_in(cluster_dir: &Path, program: &str) -> PathBuf {
    cluster_dir.join("programs").join(program)
}

/// Creates `path` (and parents) and restricts it to the current user (`0700` on Unix).
pub fn ensure_private_dir(path: &Path) -> Result<(), CoreError> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes `contents` to `path` with `0600` permissions (created or truncated).
pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), CoreError> {
    use std::io::Write;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_private_dir(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

/// Where the Stage 0 Pulumi passphrase came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage0PassphraseSource {
    /// `PULUMI_CONFIG_PASSPHRASE` was set by the operator.
    Environment,
    /// Read from an existing local `0600` passphrase file.
    LocalFile(PathBuf),
    /// Freshly generated and written to this `0600` file (must be escrowed by the operator).
    Generated(PathBuf),
}

/// Stage 0 Pulumi passphrase (zeroized on drop, never printed by `Debug`).
#[derive(Clone)]
pub struct Stage0Passphrase {
    value: zeroize::Zeroizing<String>,
    pub source: Stage0PassphraseSource,
}

impl std::fmt::Debug for Stage0Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage0Passphrase")
            .field("value", &"[REDACTED]")
            .field("source", &self.source)
            .finish()
    }
}

impl Stage0Passphrase {
    /// Wraps an explicit passphrase value.
    pub fn new(value: impl Into<String>, source: Stage0PassphraseSource) -> Self {
        Self {
            value: zeroize::Zeroizing::new(value.into()),
            source,
        }
    }

    /// Returns the passphrase for `PULUMI_CONFIG_PASSPHRASE` (environment only, never argv).
    pub fn expose(&self) -> &str {
        self.value.as_str()
    }

    /// `true` when the passphrase was generated during this invocation.
    pub fn was_generated(&self) -> bool {
        matches!(self.source, Stage0PassphraseSource::Generated(_))
    }
}

/// Generates a strong random Stage 0 passphrase (288 bits, URL-safe base64).
pub fn generate_stage0_passphrase() -> String {
    use base64::Engine;
    let bytes: [u8; 36] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn dir_has_pulumi_stacks(backend_dir: &Path) -> bool {
    std::fs::read_dir(backend_dir.join(".pulumi").join("stacks"))
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Resolves the Stage 0 Pulumi passphrase for `cluster_name` under `root`:
/// 1. `PULUMI_CONFIG_PASSPHRASE` (operator-provided),
/// 2. the existing local `0600` passphrase file,
/// 3. otherwise a freshly generated strong passphrase persisted to a `0600` file.
///
/// Fails when Stage 0 state already exists without a passphrase file: a new passphrase could not
/// decrypt it. Set `PULUMI_CONFIG_PASSPHRASE` or remove the cluster's Stage 0 directory to start
/// over.
pub fn resolve_stage0_passphrase_in(
    root: &Path,
    cluster_name: &str,
) -> Result<Stage0Passphrase, CoreError> {
    crate::validate_dns_label(cluster_name, "cluster_name")?;
    if let Ok(env) = std::env::var("PULUMI_CONFIG_PASSPHRASE") {
        return Ok(Stage0Passphrase::new(
            env,
            Stage0PassphraseSource::Environment,
        ));
    }
    let cluster_dir = stage0_cluster_dir_in(root, cluster_name);
    let file = cluster_dir.join(STAGE0_PASSPHRASE_FILE);
    if let Ok(existing) = std::fs::read_to_string(&file) {
        let trimmed = existing.trim_end_matches(['\r', '\n']).to_string();
        if !trimmed.is_empty() {
            return Ok(Stage0Passphrase::new(
                trimmed,
                Stage0PassphraseSource::LocalFile(file),
            ));
        }
    }
    if dir_has_pulumi_stacks(&cluster_dir.join(STAGE0_BACKEND_DIR)) {
        return Err(CoreError::Validation(format!(
            "Stage 0 state exists in {} without a passphrase file; set PULUMI_CONFIG_PASSPHRASE \
             or remove {} to start over",
            cluster_dir.join(STAGE0_BACKEND_DIR).display(),
            cluster_dir.display()
        )));
    }
    let generated = generate_stage0_passphrase();
    // Marker first: a crash between the two writes can never leave an unannounced passphrase.
    write_private_file(&cluster_dir.join(STAGE0_ESCROW_NOTICE_PENDING_FILE), b"")?;
    write_private_file(&file, format!("{generated}\n").as_bytes())?;
    Ok(Stage0Passphrase::new(
        generated,
        Stage0PassphraseSource::Generated(file),
    ))
}

/// Clears stale Stage 0 state when the `kind` cluster it describes no longer exists.
/// The passphrase file is preserved.
pub async fn clear_stale_kind_stage0_state_in(root: &Path, cluster_name: &str) {
    let cluster_dir = stage0_cluster_dir_in(root, cluster_name);
    let backend = cluster_dir.join(STAGE0_BACKEND_DIR);
    if !backend.exists() {
        return;
    }
    let Ok(output) = Command::new("kind")
        .arg("get")
        .arg("clusters")
        .output()
        .await
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if parse_kind_clusters_output(&stdout, cluster_name) {
        return;
    }
    tracing::info!(
        cluster = %cluster_name,
        state_dir = %backend.display(),
        "Clearing stale Stage 0 Pulumi state because kind cluster no longer exists"
    );
    let _ = tokio::fs::remove_dir_all(&backend).await;
    let _ = tokio::fs::remove_dir_all(cluster_dir.join("programs")).await;
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

/// Returns `true` if `cluster_name` appears as an exact line in `kind get clusters` output.
pub(crate) fn parse_kind_clusters_output(stdout: &str, cluster_name: &str) -> bool {
    stdout.lines().any(|line| line.trim() == cluster_name)
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

    let raw: RawKubeconfig = noyalib::from_str(yaml)?;
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

    /// Extracts every `INSTALL_K3S_EXEC="..."` value from a Stage 0 program.
    fn k3s_install_exec_values(program: &str) -> Vec<&str> {
        program
            .match_indices("INSTALL_K3S_EXEC=\"")
            .map(|(idx, marker)| {
                let rest = &program[idx + marker.len()..];
                &rest[..rest.find('"').expect("unterminated INSTALL_K3S_EXEC")]
            })
            .collect()
    }

    #[test]
    fn k3s_stage0_targets_disable_bundled_traefik_only() {
        // (target, resource that runs the k3s install script, its ignored boot-script input)
        let expected = [
            // Proxmox installs over SSH (no snippets datastore needed) from a one-shot command.
            (BootstrapTargetKind::Proxmox, "installK3s", "create"),
            (BootstrapTargetKind::Aws, "seedControlPlaneNode", "userData"),
            (
                BootstrapTargetKind::Gcp,
                "seedControlPlaneVm",
                "metadata[\"startup-script\"]",
            ),
            (
                BootstrapTargetKind::Azure,
                "seedControlPlaneVm",
                "osProfile.customData",
            ),
            (
                BootstrapTargetKind::Hetzner,
                "seedControlPlaneServer",
                "userData",
            ),
        ];
        let mut installing = Vec::new();
        for kind in BootstrapTargetKind::all() {
            let Some(program) = PulumiBootstrapTarget::new(*kind).embedded_pulumi_yaml() else {
                continue;
            };
            let execs = k3s_install_exec_values(program);
            if program.contains("get.k3s.io") {
                installing.push(*kind);
                assert!(
                    !execs.is_empty(),
                    "{kind}: k3s install without INSTALL_K3S_EXEC"
                );
            }
            for exec in execs {
                let flags: Vec<&str> = exec.split_whitespace().collect();
                assert!(
                    flags.contains(&"--disable=traefik"),
                    "{kind}: k3s must not install Traefik alongside Envoy Gateway: {exec}"
                );
                // servicelb, local-storage and metrics-server stay k3s-owned.
                for flag in &flags {
                    if let Some(component) = flag.strip_prefix("--disable=") {
                        assert_eq!(component, "traefik", "{kind}: unexpected {flag}");
                    }
                    assert_ne!(*flag, "--disable", "{kind}: use --disable=<component>");
                }
            }
        }
        assert_eq!(
            installing,
            expected.iter().map(|(k, _, _)| *k).collect::<Vec<_>>(),
            "k3s-installing Stage 0 targets changed; extend this test and the docs"
        );

        for (kind, resource, ignored) in expected {
            let program = PulumiBootstrapTarget::new(kind)
                .embedded_pulumi_yaml()
                .unwrap();
            let doc: noyalib::Value = noyalib::from_str(program).unwrap();
            let options = &doc["resources"][resource]["options"];
            assert_eq!(options["protect"], noyalib::Value::Bool(true), "{kind}");
            let ignore: Vec<&str> = options["ignoreChanges"]
                .as_sequence()
                .unwrap_or_else(|| panic!("{kind}: {resource} lacks ignoreChanges"))
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                ignore.contains(&ignored),
                "{kind}: editing the k3s boot script must not churn the protected node"
            );
        }
    }

    /// Pulumi YAML interpolates every `${...}` in any string, so a shell
    /// variable in an embedded boot script must be escaped as `$${VAR}`.
    /// Otherwise `pulumi preview`/`up` fails with an unknown-reference error.
    #[test]
    fn stage0_programs_escape_shell_variables() {
        for kind in BootstrapTargetKind::all() {
            let Some(program) = PulumiBootstrapTarget::new(*kind).embedded_pulumi_yaml() else {
                continue;
            };
            for (idx, _) in program.match_indices("${") {
                if program[..idx].ends_with('$') {
                    continue; // `$${VAR}` renders a literal `${VAR}` for the shell.
                }
                let rest = &program[idx + 2..];
                let name = &rest[..rest.find('}').unwrap_or(rest.len())];
                let shell_like = name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
                    && name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                assert!(
                    !shell_like,
                    "{kind}: unescaped shell variable ${{{name}}} is a Pulumi YAML \
                     interpolation; write $${{{name}}}"
                );
            }
        }
        for kind in [
            BootstrapTargetKind::Aws,
            BootstrapTargetKind::Gcp,
            BootstrapTargetKind::Azure,
            BootstrapTargetKind::Hetzner,
        ] {
            let program = PulumiBootstrapTarget::new(kind)
                .embedded_pulumi_yaml()
                .unwrap();
            assert!(
                program.contains("--tls-san=$${PUBLIC_IP}"),
                "{kind}: k3s --tls-san must reference the escaped shell PUBLIC_IP"
            );
        }
        // Proxmox installs over SSH and knows the guest-agent address up front.
        let pve = PulumiBootstrapTarget::new(BootstrapTargetKind::Proxmox)
            .embedded_pulumi_yaml()
            .unwrap();
        assert!(pve.contains("--tls-san=${controlPlaneIp}"));
    }

    /// Every VM-backed Stage 0 program must hand `provision_cluster` a reachable kubeconfig
    /// (`pulumi stack output kubeconfig --show-secrets`) and must run unattended: `keel bootstrap
    /// init` sets only `clusterName` (plus a few flag-driven keys), so no config may be required.
    #[test]
    fn vm_stage0_programs_export_a_secret_kubeconfig_and_need_no_extra_config() {
        use noyalib::Value;
        let vm_targets = [
            BootstrapTargetKind::Proxmox,
            BootstrapTargetKind::Aws,
            BootstrapTargetKind::Gcp,
            BootstrapTargetKind::Azure,
            BootstrapTargetKind::Hetzner,
        ];
        for kind in vm_targets {
            let program = PulumiBootstrapTarget::new(kind)
                .embedded_pulumi_yaml()
                .unwrap();
            let doc: Value = noyalib::from_str(program)
                .unwrap_or_else(|e| panic!("{kind}: Pulumi.yaml does not parse: {e}"));
            assert_eq!(doc["runtime"].as_str(), Some("yaml"), "{kind}");

            let config = doc["config"].as_mapping().expect("config block");
            for (key, spec) in config {
                assert!(
                    !spec["default"].is_null(),
                    "{kind}: config '{}' has no default, so bootstrap init cannot run it",
                    key
                );
            }
            assert!(config.contains_key("clusterName"), "{kind}");

            // Generated SSH key: the private half only ever lives in (encrypted) Pulumi state.
            let resources = &doc["resources"];
            assert_eq!(
                resources["controlPlaneSshKey"]["type"].as_str(),
                Some("tls:index:PrivateKey"),
                "{kind}"
            );
            let fetch = &resources["fetchKubeconfig"];
            assert_eq!(
                fetch["type"].as_str(),
                Some("command:remote:Command"),
                "{kind}"
            );
            assert_eq!(
                fetch["properties"]["connection"]["privateKey"].as_str(),
                Some("${controlPlaneSshKey.privateKeyOpenssh}"),
                "{kind}"
            );
            // stdout is the admin kubeconfig: secret in state, never logged as a diagnostic
            // (`run_pulumi` surfaces Pulumi's stdout on failure).
            assert_eq!(
                fetch["properties"]["logging"].as_str(),
                Some("none"),
                "{kind}: fetchKubeconfig must not log stdout"
            );
            let secret_outputs: Vec<&str> = fetch["options"]["additionalSecretOutputs"]
                .as_sequence()
                .unwrap_or_else(|| panic!("{kind}: fetchKubeconfig needs additionalSecretOutputs"))
                .iter()
                .filter_map(Value::as_str)
                .collect();
            assert!(secret_outputs.contains(&"stdout"), "{kind}");
            let create = fetch["properties"]["create"].as_str().unwrap();
            assert!(create.contains("/etc/rancher/k3s/k3s.yaml"), "{kind}");
            assert!(
                create.contains("s/127.0.0.1/${"),
                "{kind}: rewrite to the node IP"
            );

            let outputs = &doc["outputs"];
            assert_eq!(
                outputs["kubeconfig"]["fn::secret"].as_str(),
                Some("${fetchKubeconfig.stdout}"),
                "{kind}: kubeconfig must be a secret output"
            );
            for key in ["clusterName", "sshHost", "sshUser", "sshPort"] {
                assert!(!outputs[key].is_null(), "{kind}: missing output {key}");
            }
            for (key, value) in outputs.as_mapping().unwrap() {
                assert!(
                    !value.as_str().unwrap_or_default().contains("privateKey"),
                    "{kind}: output '{}' must not export the SSH private key",
                    key
                );
            }
        }

        // Provider-specific shape the drivers rely on.
        let pve: Value = noyalib::from_str(
            PulumiBootstrapTarget::new(BootstrapTargetKind::Proxmox)
                .embedded_pulumi_yaml()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            pve["packages"]["proxmox"]["parameters"][0].as_str(),
            Some("bpg/proxmox")
        );
        let vm = &pve["resources"]["seedControlPlaneVm"];
        assert_eq!(
            vm["type"].as_str(),
            Some("proxmox:index/virtualEnvironmentVm:VirtualEnvironmentVm")
        );
        assert_eq!(vm["options"]["protect"], Value::Bool(true));
        assert_eq!(vm["properties"]["agent"]["enabled"], Value::Bool(true));
        assert_eq!(
            vm["properties"]["initialization"]["userAccount"]["keys"][0].as_str(),
            Some("${controlPlaneSshKey.publicKeyOpenssh}")
        );
        assert_eq!(
            pve["variables"]["controlPlaneIp"].as_str(),
            Some("${seedControlPlaneVm.ipv4Addresses[1][0]}")
        );
        assert_eq!(
            pve["resources"]["fetchKubeconfig"]["options"]["dependsOn"][0].as_str(),
            Some("${installK3s}")
        );
        // bpg/proxmox matches clone disks by interface: the resized disk must be the
        // template's boot disk (configurable, default = the usual `--scsi0` template recipe).
        assert_eq!(
            vm["properties"]["disks"][0]["interface"].as_str(),
            Some("${diskInterface}")
        );
        assert_eq!(
            pve["config"]["diskInterface"]["default"].as_str(),
            Some("scsi0")
        );
        assert_eq!(
            vm["properties"]["scsiHardware"].as_str(),
            Some("virtio-scsi-single")
        );
        // 9000 is the template ID of the common PVE cloud-init guides; the new VM avoids it.
        assert_eq!(
            pve["config"]["templateVmId"]["default"].as_i64(),
            Some(9000)
        );
        assert_ne!(
            pve["config"]["vmId"]["default"].as_i64(),
            pve["config"]["templateVmId"]["default"].as_i64()
        );

        let hz: Value = noyalib::from_str(
            PulumiBootstrapTarget::new(BootstrapTargetKind::Hetzner)
                .embedded_pulumi_yaml()
                .unwrap(),
        )
        .unwrap();
        let server = &hz["resources"]["seedControlPlaneServer"];
        assert_eq!(server["options"]["protect"], Value::Bool(true));
        assert_eq!(
            server["properties"]["sshKeys"][0].as_str(),
            Some("${controlPlaneHcloudSshKey.id}")
        );
        assert_eq!(
            hz["resources"]["fetchKubeconfig"]["properties"]["connection"]["host"].as_str(),
            Some("${seedControlPlaneServer.ipv4Address}")
        );
        // The firewall (22 and 6443 for the workstation) is attached at creation, not by an
        // asynchronous label selector, so the node is never unfiltered.
        let fw = &hz["resources"]["controlPlaneFirewall"];
        assert_eq!(fw["options"]["protect"], Value::Bool(true));
        assert!(
            !fw["properties"]
                .as_mapping()
                .unwrap()
                .contains_key("applyTos")
        );
        let attach = &server["properties"]["firewallIds"][0]["fn::invoke"];
        assert_eq!(attach["function"].as_str(), Some("std:parseint"));
        assert_eq!(
            attach["arguments"]["input"].as_str(),
            Some("${controlPlaneFirewall.id}")
        );
        assert_eq!(attach["return"].as_str(), Some("result"));
        let ports: Vec<&str> = fw["properties"]["rules"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|r| r["port"].as_str())
            .collect();
        assert!(ports.contains(&"22") && ports.contains(&"6443"));
    }

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

        let state_dir = driver.state_dir(&cfg);
        assert!(state_dir.ends_with("stage0/keel-dev/state"));
        assert!(!state_dir.starts_with(std::env::temp_dir()) || std::env::var_os("HOME").is_none());

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
        pve_cfg.distro = Some(crate::models::BootstrapDistro::K3s);
        assert_eq!(
            pve_driver.build_config_set_pairs(&pve_cfg).unwrap(),
            vec![
                ("clusterName".to_string(), "pve-mgmt".to_string()),
                ("region".to_string(), "eu-central".to_string()),
                (
                    "proxmox:endpoint".to_string(),
                    "https://pve.example.com:8006".to_string()
                ),
                ("distro".to_string(), "k3s".to_string()),
            ]
        );
        // The built-in Proxmox program installs k3s, so Talos is refused up front.
        pve_cfg.distro = Some(crate::models::BootstrapDistro::Talos);
        assert!(pve_driver.build_config_set_pairs(&pve_cfg).is_err());

        // `--proxmox-url` only configures the Proxmox provider.
        let hz_driver = PulumiBootstrapTarget::new(BootstrapTargetKind::Hetzner);
        let mut hz_cfg = BootstrapConfig::new(BootstrapTargetKind::Hetzner);
        hz_cfg.proxmox_url = Some("https://pve.example.com:8006".to_string());
        assert_eq!(
            hz_driver.build_config_set_pairs(&hz_cfg).unwrap(),
            vec![("clusterName".to_string(), "keel-mgmt".to_string())]
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
            "#!/bin/sh\necho \"$@\" >> \"{}\"\necho \"backend=$PULUMI_BACKEND_URL pass=${{#PULUMI_CONFIG_PASSPHRASE}}\" >> \"{}.env\"\nif [ \"$1\" = \"login\" ] || [ \"$1\" = \"install\" ] || [ \"$1\" = \"up\" ] || [ \"$1\" = \"destroy\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"stack\" ] && [ \"$2\" = \"select\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"config\" ] && [ \"$2\" = \"set\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"stack\" ] && [ \"$2\" = \"output\" ] && [ \"$3\" = \"kubeconfig\" ]; then\n  cat << 'EOF'\n{}\nEOF\n  exit 0\nfi\nexit 1\n",
            log_file.display(),
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

        let state_root = temp_dir.join("stage0-root");
        let driver = PulumiBootstrapTarget::with_binary(BootstrapTargetKind::Kind, &mock_pulumi)
            .with_state_root(&state_root);
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
        // Backend chosen via PULUMI_BACKEND_URL (no global `pulumi login`), non-empty passphrase.
        assert!(!lines.iter().any(|l| l.starts_with("login")));
        assert!(lines.contains(&"install"));
        let env_log = tokio::fs::read_to_string(format!("{}.env", log_file.display()))
            .await
            .unwrap();
        if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_none() {
            assert!(env_log.lines().all(|l| l.contains(&format!(
                "backend=file://{}",
                driver.state_dir(&cfg).display()
            )) && !l.ends_with("pass=0")));
            let pass_file = driver.cluster_dir(&cfg).join(STAGE0_PASSPHRASE_FILE);
            #[cfg(unix)]
            assert_eq!(
                std::fs::metadata(&pass_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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
        )
        .with_state_root(&state_root);
        assert!(bad_driver.provision_cluster(&cfg).await.is_err());

        // Non-zero exit code error handling
        let failing_pulumi = temp_dir.join("failing-pulumi");
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&failing_pulumi).await.unwrap();
            file.write_all(b"#!/bin/sh\necho 'pulumi stack select failed' >&2\nexit 1\n")
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
            PulumiBootstrapTarget::with_binary(BootstrapTargetKind::Kind, &failing_pulumi)
                .with_state_root(&state_root);
        let err = failing_driver.provision_cluster(&cfg).await.unwrap_err();
        assert!(err.to_string().contains("pulumi stack select failed"));

        // Default constructor & resolve_kubeconfig_path fallback
        let _default_kc = KubeconfigBootstrapTarget;
        let fallback_cfg = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        let resolved = KubeconfigBootstrapTarget::resolve_kubeconfig_path(&fallback_cfg);
        assert!(resolved.is_ok());

        let _ = tokio::fs::remove_dir_all(driver.state_dir(&cfg)).await;
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    /// Proxmox and Hetzner run their embedded k3s programs and hand the exported kubeconfig to
    /// the seed install exactly like the cloud targets (no "VM only" stop).
    #[cfg(unix)]
    #[tokio::test]
    async fn vm_targets_provision_from_embedded_program_and_return_kubeconfig() {
        use std::os::unix::fs::PermissionsExt;

        const NODE_KUBECONFIG: &str = "apiVersion: v1\nkind: Config\nclusters:\n  - name: default\n    cluster:\n      server: https://203.0.113.10:6443\ncontexts:\n  - name: default\n    context:\n      cluster: default\n      user: default\ncurrent-context: default\nusers:\n  - name: default\n";

        for (kind, extra) in [
            (
                BootstrapTargetKind::Proxmox,
                "config set proxmox:endpoint https://pve.example.com:8006",
            ),
            (BootstrapTargetKind::Hetzner, "config set clusterName"),
        ] {
            let temp_dir =
                std::env::temp_dir().join(format!("keel-vm-target-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&temp_dir).await.unwrap();
            let log_file = temp_dir.join("invocations.log");
            let script = format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\nif [ \"$1\" = \"stack\" ] && [ \"$2\" = \"output\" ]; then\n  [ \"$3\" = \"kubeconfig\" ] && [ \"$4\" = \"--show-secrets\" ] || exit 1\n  cat << 'EOF'\n{}EOF\nfi\nexit 0\n",
                log_file.display(),
                NODE_KUBECONFIG
            );
            let mock = temp_dir.join("mock-pulumi");
            let mock_tmp = temp_dir.join("mock-pulumi.tmp");
            std::fs::write(&mock_tmp, script.as_bytes()).unwrap();
            std::fs::set_permissions(&mock_tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&mock_tmp, &mock).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;

            let driver = PulumiBootstrapTarget::with_binary(kind, &mock)
                .with_state_root(temp_dir.join("stage0-root"));
            let mut cfg = BootstrapConfig::new(kind);
            cfg.cluster_name = format!("vm-{}", uuid::Uuid::new_v4().simple());
            cfg.proxmox_url = Some("https://pve.example.com:8006".to_string());

            let conn = driver.provision_cluster(&cfg).await.unwrap();
            assert_eq!(conn.target, kind);
            assert_eq!(conn.kubeconfig_yaml, NODE_KUBECONFIG);
            assert_eq!(conn.context.as_deref(), Some("default"));
            assert_eq!(
                conn.server_endpoint.as_deref(),
                Some("https://203.0.113.10:6443")
            );

            // The embedded program (with the k3s install) is what Pulumi ran.
            let materialized =
                stage0_program_dir_in(&driver.cluster_dir(&cfg), kind.as_str()).join("Pulumi.yaml");
            assert_eq!(
                tokio::fs::read_to_string(&materialized).await.unwrap(),
                driver.embedded_pulumi_yaml().unwrap()
            );
            let logged = tokio::fs::read_to_string(&log_file).await.unwrap();
            assert!(logged.lines().any(|l| l.starts_with(extra)), "{kind}");
            assert!(logged.contains("up --yes --non-interactive"));
            assert!(logged.contains("stack output kubeconfig --show-secrets"));
            assert_eq!(
                logged.contains("proxmox:endpoint"),
                kind == BootstrapTargetKind::Proxmox
            );

            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        }
    }

    #[test]
    fn test_parse_kind_clusters_output() {
        let out = "keel-mgmt\nworkload-dev\n";
        assert!(parse_kind_clusters_output(out, "keel-mgmt"));
        assert!(parse_kind_clusters_output(out, "workload-dev"));
        assert!(!parse_kind_clusters_output(out, "keel-mgmt-2"));
        assert!(!parse_kind_clusters_output(
            "No kind clusters found.\n",
            "keel-mgmt"
        ));
        assert!(!parse_kind_clusters_output("", "keel-mgmt"));
    }

    #[test]
    fn test_stage0_passphrase_resolution() {
        if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_some() {
            eprintln!("SKIPPED: PULUMI_CONFIG_PASSPHRASE set in test environment");
            return;
        }
        let root = std::env::temp_dir().join(format!("keel-stage0-pp-{}", uuid::Uuid::new_v4()));
        let first = resolve_stage0_passphrase_in(&root, "pp-test").unwrap();
        assert!(first.was_generated());
        assert!(first.expose().len() >= 40);
        // Generation (whichever caller triggers it) leaves the escrow-notice marker behind.
        let marker =
            stage0_cluster_dir_in(&root, "pp-test").join(STAGE0_ESCROW_NOTICE_PENDING_FILE);
        assert!(marker.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&marker).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&marker).unwrap();
        assert!(!format!("{first:?}").contains(first.expose()));
        let second = resolve_stage0_passphrase_in(&root, "pp-test").unwrap();
        assert_eq!(second.expose(), first.expose());
        assert!(matches!(
            second.source,
            Stage0PassphraseSource::LocalFile(_)
        ));
        // Reusing the local file never (re)creates the marker.
        assert!(!marker.exists());
        // An empty file (e.g. a crash mid-write) is regenerated and announced again.
        let pp_file = stage0_cluster_dir_in(&root, "pp-test").join(STAGE0_PASSPHRASE_FILE);
        std::fs::write(&pp_file, "\n").unwrap();
        let regenerated = resolve_stage0_passphrase_in(&root, "pp-test").unwrap();
        assert!(regenerated.was_generated());
        assert!(marker.is_file());
        assert!(resolve_stage0_passphrase_in(&root, "../evil").is_err());

        // State without a passphrase file cannot be decrypted by a new passphrase: refuse.
        let orphan_stacks = stage0_cluster_dir_in(&root, "old")
            .join(STAGE0_BACKEND_DIR)
            .join(".pulumi/stacks/keel-stage0-control-plane");
        std::fs::create_dir_all(&orphan_stacks).unwrap();
        std::fs::write(orphan_stacks.join("stage0-control-plane-old.json"), "{}").unwrap();
        let err = resolve_stage0_passphrase_in(&root, "old")
            .unwrap_err()
            .to_string();
        assert!(err.contains("start over"), "{err}");
        assert!(
            !stage0_cluster_dir_in(&root, "old")
                .join(STAGE0_PASSPHRASE_FILE)
                .exists()
        );
        assert!(
            !stage0_cluster_dir_in(&root, "old")
                .join(STAGE0_ESCROW_NOTICE_PENDING_FILE)
                .exists()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_cluster_connection_debug_and_serialize_redact_kubeconfig() {
        let conn = ClusterConnection {
            target: BootstrapTargetKind::Kind,
            cluster_name: "c".to_string(),
            kubeconfig_yaml: "token: super-secret-kubeconfig".to_string(),
            kubeconfig_path: None,
            context: None,
            server_endpoint: None,
        };
        assert!(!format!("{conn:?}").contains("super-secret"));
        let json = serde_json::to_string(&conn).unwrap();
        assert!(!json.contains("super-secret"));
        assert!(!json.contains("kubeconfigYaml"));
    }
}
