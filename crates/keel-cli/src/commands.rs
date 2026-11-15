//! Command execution handlers for `keel` (`SDD.md` §2.1, §2.2, §4.2, §6.1, §7.2).

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use keel_core::{
    AuthPhase, BootstrapConfig, BootstrapTarget, CatalogApplication, CatalogInfraBlueprint,
    ClusterConnection, CoreError, DEFAULT_POLL_INTERVAL, EntityMeta, EnvelopeCrypto,
    FLUX_SYSTEM_NAMESPACE, ForgeConfig, ForgeKind, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE,
    KeelConfig, OidcConfig, PreparedHelmValues, ProviderRegistry, SopsConfig, SopsManager,
    bootstrap_target_factory, prepare_helm_values, validate_dns_label, validate_schema_values,
};
use keel_git::{
    CommitAuthor, ForgeError, ForgeProvider, ForgejoGiteaProvider, GitCredentials, GitError,
    GitHubProvider, GitLabProvider, GitRunner, LocalGitProvider, PullRequestInfo, PullRequestState,
    parse_remote_repo_url,
};
use keel_kube::{
    BOOTSTRAP_TOKEN_SECRET_NAME, CrdReconcileStatus, CrdWatchEvent, DockerSocketExceptionConfig,
    InMemorySecretStore, KubeClusterStatusReader, KubeError, KyvernoManifestBundle,
    KyvernoPolicyConfig, SecretStateStore, TelemetryMux, TenantIsolationConfig,
    TenantManifestBundle, WatchedCrdKind, create_secret_manifest, hash_bootstrap_token,
};
use keel_pko::{
    CrObjectMeta, DEFAULT_FLUX_POLL_INTERVAL, FLUX_GIT_REPO_API_VERSION, FLUX_GIT_REPO_KIND,
    FluxGitRepository, FluxGitRepositoryRef, FluxGitRepositorySpec, LocalObjectReference,
    ManagementCoreStackParams, PkoError, PkoStack, PreviewReport, synthesize_management_core_stack,
};
use keel_server::{
    AppState, KubeClusterStateProvider, RepoClusterStateProvider, SESSION_COOKIE_NAME,
    StackSummary, build_router,
};
use reqwest::header::{AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cli::{
    AppCommands, AppInstallArgs, AuthCommands, AuthLoginArgs, BootstrapCommands, BootstrapInitArgs,
    BootstrapRecoverArgs, Cli, Commands, InfraCommands, InfraListArgs, InfraProvisionArgs,
    OutputFormat, PreviewCommands, PreviewStatusArgs, ServeArgs,
};

/// Unified error type returned by `keel` CLI command handlers.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Core(#[from] CoreError),

    #[error(transparent)]
    Git(#[from] GitError),

    #[error(transparent)]
    Forge(#[from] ForgeError),

    #[error(transparent)]
    Pko(#[from] PkoError),

    #[error(transparent)]
    Kube(#[from] KubeError),

    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yml::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Server API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },

    #[error("CLI execution error: {0}")]
    Execution(String),
}

/// Kyverno policy bundle generator with explicit control over the local-dev Docker socket
/// `PolicyException` (`SDD.md` §6.2, `NFR-SEC-03`, `NFR-SEC-04`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KyvernoPolicyGenerator {
    pub allowed_providers: Vec<String>,
    pub local_dev_docker_socket_exception: bool,
}

impl KyvernoPolicyGenerator {
    /// Creates a generator for the specified enabled Pulumi/OpenTofu providers and explicit
    /// `local_dev_docker_socket_exception` flag (`false` in production; opt-in on local `kind`).
    pub fn new(allowed_providers: Vec<String>, local_dev_docker_socket_exception: bool) -> Self {
        Self {
            allowed_providers,
            local_dev_docker_socket_exception,
        }
    }

    /// Creates a generator seeded with the Day-1 default allowed Pulumi providers (`keel_kube::kyverno`).
    pub fn with_default_providers(local_dev_docker_socket_exception: bool) -> Self {
        Self::new(
            keel_kube::kyverno::default_allowed_pulumi_providers(),
            local_dev_docker_socket_exception,
        )
    }

    /// Synthesizes the baseline `ClusterPolicy` manifests and optional local-dev `PolicyException`.
    pub fn generate(&self) -> Result<KyvernoManifestBundle, CliError> {
        let cfg = KyvernoPolicyConfig {
            allowed_providers: self.allowed_providers.clone(),
            local_dev_docker_socket_exception: self.local_dev_docker_socket_exception,
            docker_socket_exception: DockerSocketExceptionConfig::default(),
        };
        Ok(KyvernoManifestBundle::generate(&cfg)?)
    }
}

/// Stored CLI session map keyed by normalized server URL (never logged).
#[derive(Default, Serialize, Deserialize)]
struct StoredCredentialsFile {
    #[serde(default)]
    sessions: BTreeMap<String, String>,
}

/// Runtime context for CLI command execution (HTTP client + session credential store).
#[derive(Clone)]
pub struct CliContext {
    http: Client,
    no_redirect_http: Client,
    credentials_path: PathBuf,
    session_override: Option<String>,
}

impl std::fmt::Debug for CliContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliContext")
            .field("credentials_path", &self.credentials_path)
            .field(
                "session_override",
                &self.session_override.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl Default for CliContext {
    fn default() -> Self {
        Self::new()
    }
}

impl CliContext {
    /// Creates a default [`CliContext`] storing session tokens under the user's config directory.
    pub fn new() -> Self {
        Self::with_credentials_path(default_credentials_path())
    }

    /// Creates a [`CliContext`] using a custom session credentials file path.
    pub fn with_credentials_path(credentials_path: impl Into<PathBuf>) -> Self {
        let http = Client::builder()
            .user_agent("keel-cli/0.1.0")
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        let no_redirect_http = Client::builder()
            .user_agent("keel-cli/0.1.0")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http,
            no_redirect_http,
            credentials_path: credentials_path.into(),
            session_override: None,
        }
    }

    /// Returns a copy of this context configured with an explicit in-memory session token.
    pub fn with_session_token(mut self, session_token: impl Into<String>) -> Self {
        self.session_override = Some(session_token.into());
        self
    }

    /// Persists an opaque session token for `server_url` with `0600` file permissions (`NFR-SEC-05`).
    pub fn save_session_token(&self, server_url: &str, token: &str) -> Result<(), CliError> {
        let normalized = normalize_server_url(server_url)?;
        let mut store = self.read_store();
        store.sessions.insert(normalized, token.trim().to_string());

        if let Some(parent) = self.credentials_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let payload = serde_json::to_vec_pretty(&store)?;
        write_restricted_file(&self.credentials_path, &payload)?;
        Ok(())
    }

    /// Loads the active session token for `server_url` if available.
    pub fn load_session_token(&self, server_url: &str) -> Option<String> {
        if let Some(tok) = &self.session_override {
            return Some(tok.clone());
        }
        let normalized = normalize_server_url(server_url).ok()?;
        let store = self.read_store();
        store.sessions.get(&normalized).cloned()
    }

    fn read_store(&self) -> StoredCredentialsFile {
        std::fs::read_to_string(&self.credentials_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<StoredCredentialsFile>(&raw).ok())
            .unwrap_or_default()
    }
}

fn default_credentials_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".config/keel/credentials.json")
    } else {
        std::env::temp_dir().join("keel-cli-credentials.json")
    }
}

fn write_restricted_file(path: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
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
    use std::io::Write;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

/// Validates and normalizes an HTTP(S) `keel-server` base URL.
pub fn normalize_server_url(raw: &str) -> Result<String, CliError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty()
        || !(trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        || trimmed.chars().any(char::is_control)
    {
        return Err(CliError::Validation(format!(
            "server URL must start with 'http://' or 'https://', got '{raw}'"
        )));
    }
    Ok(trimmed.to_string())
}

// ============================================================================
// 1. `keel bootstrap init`
// ============================================================================

/// Seed control-plane artifacts synthesized identically across all Stage 0 `BootstrapTarget`s.
#[derive(Clone)]
pub struct SeedControlPlaneArtifacts {
    pub keel_config: KeelConfig,
    pub provider_registry: ProviderRegistry,
    pub sops_config: SopsConfig,
    pub flux_git_repository: FluxGitRepository,
    pub kyverno_bundle: KyvernoManifestBundle,
    pub management_stack: PkoStack,
    /// Raw single-use 256-bit Stage 0 bootstrap token (returned to operator stdout, never logged).
    pub bootstrap_token: String,
    /// SHA-256 hex digest persisted inside `keel-system/bootstrap-token`.
    pub bootstrap_token_sha256: String,
    /// Kubernetes `Secret` metadata for `keel-system/bootstrap-token`.
    pub bootstrap_secret_namespace: String,
    pub bootstrap_secret_name: String,
}

impl std::fmt::Debug for SeedControlPlaneArtifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeedControlPlaneArtifacts")
            .field("keel_config", &self.keel_config)
            .field("bootstrap_token", &"[REDACTED]")
            .field("bootstrap_token_sha256", &self.bootstrap_token_sha256)
            .field(
                "bootstrap_secret_namespace",
                &self.bootstrap_secret_namespace,
            )
            .field("bootstrap_secret_name", &self.bootstrap_secret_name)
            .finish()
    }
}

/// Result of initializing the Stage 0 local Git repository (`--repo-dir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRepoInitResult {
    pub repo_dir: PathBuf,
    pub bare_repo_dir: PathBuf,
    pub initial_commit_sha: String,
    pub written_files: Vec<String>,
}

/// Outcome of `keel bootstrap init`.
#[derive(Debug, Clone)]
pub struct BootstrapInitOutcome {
    pub connection: ClusterConnection,
    pub artifacts: SeedControlPlaneArtifacts,
    pub seed_repo: Option<SeedRepoInitResult>,
    pub output: String,
}

/// Synthesizes all target-agnostic Stage 0 seed control-plane manifests (`SDD.md` §2.1, `AGENTS.md` §3.1).
pub async fn synthesize_seed_control_plane(
    cfg: &BootstrapConfig,
    repo_url_override: Option<&str>,
) -> Result<SeedControlPlaneArtifacts, CliError> {
    cfg.validate()?;

    let mut provider_registry = ProviderRegistry::default_day1();
    if cfg.target.requires_stage0_infra() {
        let _ = provider_registry.set_enabled(cfg.target.as_str(), true);
    }
    provider_registry.validate()?;
    let mut enabled_provider_ids: Vec<String> = provider_registry
        .enabled_providers()
        .into_iter()
        .map(|p| p.id.clone())
        .collect();
    if !enabled_provider_ids.iter().any(|p| p == "kubernetes") {
        enabled_provider_ids.push("kubernetes".to_string());
    }

    let repo_url = repo_url_override.map(str::to_owned).unwrap_or_else(|| {
        if cfg.with_forge {
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git"
                .to_string()
        } else {
            "https://forge.internal/keel/infra-gitops.git".to_string()
        }
    });

    let mut keel_config = KeelConfig {
        cluster_name: cfg.cluster_name.clone(),
        bootstrap_target: cfg.target,
        auth_phase: AuthPhase::Bootstrap,
        enabled_providers: enabled_provider_ids.clone(),
        forge: ForgeConfig {
            provider: if cfg.with_forge {
                ForgeKind::Forgejo
            } else {
                ForgeKind::Local
            },
            repo_url: repo_url.clone(),
            api_url: if cfg.with_forge {
                Some("http://forgejo-http.keel-system.svc.cluster.local:3000".to_string())
            } else {
                None
            },
            default_branch: "main".to_string(),
            // Mandatory outbound pull-polling interval (`1m`) for NAT/firewall resilience (`AGENTS.md` §3.1)
            poll_interval: DEFAULT_POLL_INTERVAL.to_string(),
        },
        oidc: if cfg.with_oidc {
            Some(OidcConfig {
                enabled: true,
                issuer_url: "https://oidc.keel.internal".to_string(),
                client_id: "keel-portal".to_string(),
                redirect_url: "http://127.0.0.1:8080/api/v1/auth/callback".to_string(),
                scopes: vec![
                    "openid".to_string(),
                    "profile".to_string(),
                    "email".to_string(),
                    "groups".to_string(),
                ],
            })
        } else {
            None
        },
        ..KeelConfig::default()
    };
    if keel_config.oidc.as_ref().is_some_and(|o| o.enabled) {
        keel_config.auth_phase = AuthPhase::OidcActive;
    }
    keel_config.validate()?;

    // Generate root SOPS config (populate root age rule if `age-keygen` is installed on PATH)
    let mut sops_config = SopsConfig::default();
    if let Ok(keypair) = SopsManager::new().generate_age_keypair().await {
        sops_config
            .creation_rules
            .push(keel_core::SopsCreationRule {
                path_regex: r"^clusters/management/.*\.enc\.yaml$".to_string(),
                age: keypair.public_key,
                encrypted_regex: Some("^(data|stringData)$".to_string()),
            });
    }

    // Flux CD GitRepository with outbound pull-polling (`interval: 1m`)
    let flux_git_repository = FluxGitRepository {
        api_version: FLUX_GIT_REPO_API_VERSION.to_string(),
        kind: FLUX_GIT_REPO_KIND.to_string(),
        metadata: CrObjectMeta {
            name: FLUX_SYSTEM_NAMESPACE.to_string(),
            namespace: FLUX_SYSTEM_NAMESPACE.to_string(),
            labels: BTreeMap::from([(
                "app.kubernetes.io/managed-by".to_string(),
                "keel".to_string(),
            )]),
            annotations: BTreeMap::new(),
        },
        spec: FluxGitRepositorySpec {
            interval: DEFAULT_FLUX_POLL_INTERVAL.to_string(),
            url: repo_url,
            git_ref: FluxGitRepositoryRef {
                branch: Some("main".to_string()),
                tag: None,
                commit: None,
            },
            secret_ref: None,
            timeout: Some("60s".to_string()),
        },
    };

    // Kyverno admission policies + explicit local-dev Docker socket PolicyException guard
    let kyverno_gen =
        KyvernoPolicyGenerator::new(enabled_provider_ids, cfg.local_dev_docker_socket_exception);
    let kyverno_bundle = kyverno_gen.generate()?;

    // Protected anti-lockout `management-cluster-core` PKO v2 Stack (`protect: true`)
    let mgmt_backend = if cfg.target == keel_core::BootstrapTargetKind::Kind {
        "file:///tmp".to_string()
    } else {
        let s3_bucket = keel_config
            .s3
            .as_ref()
            .map(|s| s.bucket.as_str())
            .unwrap_or("pulumi-state");
        format!("s3://{s3_bucket}")
    };
    let mgmt_params =
        ManagementCoreStackParams::new(mgmt_backend, FLUX_SYSTEM_NAMESPACE, "clusters/management");
    let management_stack = synthesize_management_core_stack(&mgmt_params)?;

    // Single-use 256-bit Stage 0 bootstrap token (`NFR-SEC-01`, `NFR-SEC-05`)
    let bootstrap_token = EnvelopeCrypto::generate_bootstrap_token();
    let bootstrap_token_sha256 = EnvelopeCrypto::sha256_hex(bootstrap_token.as_bytes());
    debug_assert_eq!(
        bootstrap_token_sha256,
        hash_bootstrap_token(&bootstrap_token)
    );

    let secret_meta = EntityMeta::new(BOOTSTRAP_TOKEN_SECRET_NAME, KEEL_SYSTEM_NAMESPACE);
    let secret_manifest = create_secret_manifest(&secret_meta);
    let bootstrap_secret_namespace = secret_manifest
        .metadata
        .namespace
        .unwrap_or_else(|| KEEL_SYSTEM_NAMESPACE.to_string());
    let bootstrap_secret_name = secret_manifest
        .metadata
        .name
        .unwrap_or_else(|| BOOTSTRAP_TOKEN_SECRET_NAME.to_string());

    Ok(SeedControlPlaneArtifacts {
        keel_config,
        provider_registry,
        sops_config,
        flux_git_repository,
        kyverno_bundle,
        management_stack,
        bootstrap_token,
        bootstrap_token_sha256,
        bootstrap_secret_namespace,
        bootstrap_secret_name,
    })
}

/// Resolves a path to an absolute, lexically normalized [`PathBuf`] (without requiring it to exist yet).
fn absolutize_path(path: &Path) -> Result<PathBuf, CliError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn is_relative_local_remote(url: &str) -> bool {
    !url.is_empty() && !url.starts_with('/') && !url.contains("://") && !url.contains(':')
}

/// Computes the companion bare repository path (`<name>.git`) for a working tree directory.
pub fn default_bare_repo_for_workdir(repo_dir: &Path) -> PathBuf {
    let dir_name = repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("keel-gitops");
    repo_dir.with_file_name(format!("{dir_name}.git"))
}

const STARTER_KIND_CLUSTER_CATALOG_YAML: &str =
    include_str!("../../../deploy/catalog/infra/kind-cluster/catalog.yaml");
const STARTER_KIND_CLUSTER_PULUMI_YAML: &str =
    include_str!("../../../deploy/catalog/infra/kind-cluster/Pulumi.yaml");
const STARTER_PODINFO_CATALOG_YAML: &str =
    include_str!("../../../deploy/catalog/apps/podinfo/catalog.yaml");

async fn is_dir_empty_or_missing(dir: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return true;
    };
    !matches!(entries.next_entry().await, Ok(Some(_)))
}

const FORGE_TOKEN_SECRET_NAME: &str = "forge-token";

/// RAII guard for a background `kubectl port-forward` child process.
struct TempPortForward {
    local_port: u16,
    child: tokio::process::Child,
}

impl Drop for TempPortForward {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn start_forgejo_port_forward(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<TempPortForward, CliError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let local_port = listener.local_addr()?.port();
    drop(listener);

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(kc) = kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.args([
        "-n",
        KEEL_SYSTEM_NAMESPACE,
        "port-forward",
        "svc/forgejo-http",
        &format!("{local_port}:3000"),
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| CliError::Validation(format!("failed to spawn kubectl port-forward: {e}")))?;
    let pf = TempPortForward { local_port, child };

    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| CliError::Validation(e.to_string()))?;
    let health_url = format!("http://127.0.0.1:{local_port}/api/v1/version");

    for _ in 0..40 {
        if let Ok(resp) = client.get(&health_url).send().await
            && resp.status().is_success()
        {
            return Ok(pf);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(CliError::Validation(format!(
        "timed out waiting for Forgejo port-forward on 127.0.0.1:{local_port}"
    )))
}

async fn provision_in_cluster_forgejo(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
    local_port: u16,
) -> Result<String, CliError> {
    let base = format!("http://127.0.0.1:{local_port}");
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| CliError::Validation(e.to_string()))?;

    // 1. Ensure `keel/infra-gitops` repository exists (201 Created or 409 Already Exists)
    let repo_resp = client
        .post(format!("{base}/api/v1/user/repos"))
        .basic_auth("keel", Some("keel-admin-dev"))
        .json(&serde_json::json!({
            "name": "infra-gitops",
            "private": false,
            "auto_init": false
        }))
        .send()
        .await
        .map_err(|e| CliError::Validation(format!("Forgejo repo create failed: {e}")))?;
    if !repo_resp.status().is_success() && repo_resp.status() != StatusCode::CONFLICT {
        let status = repo_resp.status();
        let body = repo_resp.text().await.unwrap_or_default();
        return Err(CliError::Validation(format!(
            "Forgejo repo create returned HTTP {status}: {body}"
        )));
    }

    // 2. Mint a fresh API token for `keel serve`
    let token_name = format!("keel-stage0-{}", chrono::Utc::now().timestamp());
    let tok_resp = client
        .post(format!("{base}/api/v1/users/keel/tokens"))
        .basic_auth("keel", Some("keel-admin-dev"))
        .json(&serde_json::json!({
            "name": token_name,
            "scopes": ["write:repository", "write:issue", "read:user"]
        }))
        .send()
        .await
        .map_err(|e| CliError::Validation(format!("Forgejo token create failed: {e}")))?;
    if !tok_resp.status().is_success() {
        let status = tok_resp.status();
        let body = tok_resp.text().await.unwrap_or_default();
        return Err(CliError::Validation(format!(
            "Forgejo token create returned HTTP {status}: {body}"
        )));
    }
    let tok_json: Value = tok_resp
        .json()
        .await
        .map_err(|e| CliError::Validation(format!("Forgejo token parse failed: {e}")))?;
    let token = tok_json
        .get("sha1")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CliError::Validation("Forgejo token response missing `sha1` field".to_string())
        })?
        .to_string();

    // 3. Persist token in `keel-system/forge-token` Secret for zero-config `keel serve`
    let secret_doc = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": FORGE_TOKEN_SECRET_NAME,
            "namespace": KEEL_SYSTEM_NAMESPACE,
        },
        "type": "Opaque",
        "stringData": {
            "token": token,
        },
    });
    let secret_yaml = serde_yml::to_string(&secret_doc)?;
    run_kubectl_apply(context, kubeconfig, &secret_yaml).await?;

    Ok(token)
}

async fn ensure_external_repo_exists(
    forge_kind: ForgeKind,
    repo_url: &str,
    api_url: Option<&str>,
    token: &str,
) {
    let Ok(slug) = parse_remote_repo_url(repo_url) else {
        return;
    };
    let Ok(client) = Client::builder().timeout(Duration::from_secs(15)).build() else {
        return;
    };

    match forge_kind {
        ForgeKind::GitHub => {
            let base = api_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.github.com".to_string());
            let check = client
                .get(format!("{base}/repos/{}/{}", slug.owner, slug.repo))
                .bearer_auth(token)
                .header("User-Agent", "keel-cli")
                .send()
                .await;
            if let Ok(resp) = check
                && resp.status() == StatusCode::NOT_FOUND
            {
                // Try org repo first, then personal user repo
                let org_resp = client
                    .post(format!("{base}/orgs/{}/repos", slug.owner))
                    .bearer_auth(token)
                    .header("User-Agent", "keel-cli")
                    .json(&serde_json::json!({ "name": slug.repo, "private": true }))
                    .send()
                    .await;
                if org_resp.as_ref().is_ok_and(|r| r.status().is_success()) {
                    tracing::info!(repo = %slug.project_path, "Created GitHub org repository");
                } else {
                    let user_resp = client
                        .post(format!("{base}/user/repos"))
                        .bearer_auth(token)
                        .header("User-Agent", "keel-cli")
                        .json(&serde_json::json!({ "name": slug.repo, "private": true }))
                        .send()
                        .await;
                    if user_resp.as_ref().is_ok_and(|r| r.status().is_success()) {
                        tracing::info!(repo = %slug.project_path, "Created GitHub user repository");
                    }
                }
            }
        }
        ForgeKind::Forgejo | ForgeKind::Gitea => {
            let base = api_url
                .map(|u| {
                    let t = u.trim_end_matches('/');
                    t.strip_suffix("/api/v1").unwrap_or(t).to_string()
                })
                .unwrap_or_else(|| slug.origin_url.clone());
            let check = client
                .get(format!("{base}/api/v1/repos/{}/{}", slug.owner, slug.repo))
                .header(AUTHORIZATION, format!("token {token}"))
                .send()
                .await;
            if let Ok(resp) = check
                && resp.status() == StatusCode::NOT_FOUND
            {
                let _ = client
                    .post(format!("{base}/api/v1/user/repos"))
                    .header(AUTHORIZATION, format!("token {token}"))
                    .json(&serde_json::json!({ "name": slug.repo, "private": true }))
                    .send()
                    .await;
            }
        }
        ForgeKind::GitLab => {
            let base = api_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| slug.origin_url.clone());
            let encoded = slug.project_path.replace('/', "%2F");
            let check = client
                .get(format!("{base}/api/v4/projects/{encoded}"))
                .header("PRIVATE-TOKEN", token)
                .send()
                .await;
            if let Ok(resp) = check
                && resp.status() == StatusCode::NOT_FOUND
            {
                let _ = client
                    .post(format!("{base}/api/v4/projects"))
                    .header("PRIVATE-TOKEN", token)
                    .json(&serde_json::json!({ "name": slug.repo }))
                    .send()
                    .await;
            }
        }
        ForgeKind::Local => {}
    }
}

/// Initializes a local seed GitOps repository on `main` using [`GitRunner`] (zero shell calls).
pub async fn initialize_seed_git_repo(
    repo_dir: &Path,
    artifacts: &SeedControlPlaneArtifacts,
) -> Result<SeedRepoInitResult, CliError> {
    initialize_seed_git_repo_with_push(repo_dir, artifacts, None, None).await
}

async fn initialize_seed_git_repo_with_push(
    repo_dir: &Path,
    artifacts: &SeedControlPlaneArtifacts,
    push_url_override: Option<&str>,
    push_credentials: Option<GitCredentials>,
) -> Result<SeedRepoInitResult, CliError> {
    let git = match push_credentials {
        Some(creds) => GitRunner::new().with_credentials(creds),
        None => GitRunner::new(),
    };
    let repo_dir = absolutize_path(repo_dir)?;
    let repo_dir = repo_dir.as_path();
    let bare_repo_dir = default_bare_repo_for_workdir(repo_dir);

    if !bare_repo_dir.join("HEAD").exists() {
        git.init_bare(&bare_repo_dir).await?;
    }

    if !repo_dir.join(".git").exists() {
        if let Some(parent) = repo_dir.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bare_str = bare_repo_dir.to_str().ok_or_else(|| {
            CliError::Validation("bare repository path must be valid UTF-8".to_string())
        })?;
        git.clone(bare_str, repo_dir).await?;
    }

    let default_tenant_rbac =
        TenantManifestBundle::synthesize(&TenantIsolationConfig::new("default"))?
            .to_multi_doc_yaml()?;

    let mut default_tenant_git_repo = artifacts.flux_git_repository.clone();
    default_tenant_git_repo.metadata.name = "keel-gitops".to_string();
    default_tenant_git_repo.metadata.namespace = "tenant-default".to_string();
    let default_tenant_git_repo_yaml = format!(
        "{}---\n\
         apiVersion: networking.k8s.io/v1\n\
         kind: NetworkPolicy\n\
         metadata:\n\
         \x20 name: allow-pko-workspace\n\
         \x20 namespace: tenant-default\n\
         spec:\n\
         \x20 podSelector:\n\
         \x20   matchLabels:\n\
         \x20     auto.pulumi.com/component: workspace\n\
         \x20 policyTypes:\n\
         \x20 - Ingress\n\
         \x20 - Egress\n\
         \x20 ingress:\n\
         \x20 - from:\n\
         \x20   - namespaceSelector:\n\
         \x20       matchLabels:\n\
         \x20         kubernetes.io/metadata.name: {KEEL_SYSTEM_NAMESPACE}\n\
         \x20   ports:\n\
         \x20   - protocol: TCP\n\
         \x20     port: 50051\n\
         \x20 egress:\n\
         \x20 - {{}}\n\
         ---\n\
         apiVersion: v1\n\
         kind: LimitRange\n\
         metadata:\n\
         \x20 name: default-container-resources\n\
         \x20 namespace: tenant-default\n\
         spec:\n\
         \x20 limits:\n\
         \x20 - type: Container\n\
         \x20   default:\n\
         \x20     cpu: 500m\n\
         \x20     memory: 512Mi\n\
         \x20   defaultRequest:\n\
         \x20     cpu: 100m\n\
         \x20     memory: 128Mi\n\
         ---\n\
         apiVersion: rbac.authorization.k8s.io/v1\n\
         kind: ClusterRoleBinding\n\
         metadata:\n\
         \x20 name: tenant-default:tenant-default-sa:system:auth-delegator\n\
         roleRef:\n\
         \x20 apiGroup: rbac.authorization.k8s.io\n\
         \x20 kind: ClusterRole\n\
         \x20 name: system:auth-delegator\n\
         subjects:\n\
         - kind: ServiceAccount\n\
         \x20 namespace: tenant-default\n\
         \x20 name: tenant-default-sa\n\
         ---\n\
         apiVersion: rbac.authorization.k8s.io/v1\n\
         kind: RoleBinding\n\
         metadata:\n\
         \x20 name: tenant-default-admin\n\
         \x20 namespace: tenant-default\n\
         roleRef:\n\
         \x20 apiGroup: rbac.authorization.k8s.io\n\
         \x20 kind: ClusterRole\n\
         \x20 name: admin\n\
         subjects:\n\
         - kind: ServiceAccount\n\
         \x20 name: tenant-default-sa\n\
         \x20 namespace: tenant-default\n",
        default_tenant_git_repo.to_yaml()?
    );

    let tenants_kustomization_yaml = "apiVersion: kustomize.config.k8s.io/v1beta1\n\
        kind: Kustomization\n\
        resources:\n\
        - default/rbac.yaml\n\
        - default/git-repository.yaml\n"
        .to_string();

    let mut files_to_write: Vec<(&str, String)> = vec![
        (".keel/config.yaml", artifacts.keel_config.to_yaml()?),
        (
            "providers/registry.yaml",
            artifacts.provider_registry.to_yaml()?,
        ),
        (".sops.yaml", artifacts.sops_config.to_yaml()?),
        (
            "clusters/management/flux-system/git-repository.yaml",
            artifacts.flux_git_repository.to_yaml()?,
        ),
        (
            "clusters/management/core-addons/kyverno-policies.yaml",
            artifacts.kyverno_bundle.to_multi_doc_yaml()?,
        ),
        (
            "clusters/management/management-stack.yaml",
            artifacts.management_stack.to_yaml()?,
        ),
        (
            "clusters/management/Pulumi.yaml",
            "name: management-cluster-core\nruntime: yaml\ndescription: Keel Stage 0/1 protected management cluster core stack\n".to_string(),
        ),
        ("tenants/default/rbac.yaml", default_tenant_rbac),
        (
            "tenants/default/git-repository.yaml",
            default_tenant_git_repo_yaml,
        ),
        ("tenants/kustomization.yaml", tenants_kustomization_yaml),
    ];

    let apps_dir = repo_dir.join("catalog/apps");
    let infra_dir = repo_dir.join("catalog/infra");
    if is_dir_empty_or_missing(&infra_dir).await && is_dir_empty_or_missing(&apps_dir).await {
        files_to_write.push((
            "catalog/infra/kind-cluster/catalog.yaml",
            STARTER_KIND_CLUSTER_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/kind-cluster/Pulumi.yaml",
            STARTER_KIND_CLUSTER_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/apps/podinfo/catalog.yaml",
            STARTER_PODINFO_CATALOG_YAML.to_string(),
        ));
    }

    let mut written_files = Vec::with_capacity(files_to_write.len());
    for (rel_path, contents) in &files_to_write {
        let full_path = repo_dir.join(rel_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full_path, contents).await?;
        written_files.push((*rel_path).to_string());
    }

    // Ensure catalogs & tenants skeleton directories exist
    tokio::fs::create_dir_all(&apps_dir).await?;
    tokio::fs::create_dir_all(&infra_dir).await?;
    tokio::fs::create_dir_all(repo_dir.join("tenants")).await?;

    git.add(repo_dir, &written_files).await?;
    let author = CommitAuthor::new("Keel Bootstrap", "bootstrap@keel.io");
    let commit_sha = git
        .commit(
            repo_dir,
            "feat(bootstrap): initialize Stage 0 seed control-plane layout",
            &author,
        )
        .await?;

    if artifacts.keel_config.forge.provider == ForgeKind::Local {
        git.push(repo_dir, "origin", "HEAD:refs/heads/main").await?;
    } else {
        let remote_url = &artifacts.keel_config.forge.repo_url;
        if let Some(push_url) = push_url_override {
            git.set_remote_url(repo_dir, "origin", push_url).await?;
            let push_res = git.push(repo_dir, "origin", "HEAD:refs/heads/main").await;
            git.set_remote_url(repo_dir, "origin", remote_url).await?;
            push_res?;
        } else {
            git.set_remote_url(repo_dir, "origin", remote_url).await?;
            if let Err(err) = git.push(repo_dir, "origin", "HEAD:refs/heads/main").await {
                tracing::warn!(
                    remote_url = %remote_url,
                    error = %err,
                    "Seed commit created locally on `main`, but initial push to remote `origin` failed; push manually once the remote repository and credentials are ready"
                );
            }
        }
    }

    Ok(SeedRepoInitResult {
        repo_dir: repo_dir.to_path_buf(),
        bare_repo_dir,
        initial_commit_sha: commit_sha,
        written_files,
    })
}

const SEED_NAMESPACES_YAML: &str = include_str!("../../../deploy/seed/namespaces.yaml");

async fn run_kubectl_apply(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
    manifest_yaml: &str,
) -> Result<(), CliError> {
    use tokio::io::AsyncWriteExt;

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(kc) = kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        CliError::Validation(format!(
            "failed to invoke `kubectl` (is it installed and in PATH?): {e}"
        ))
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(manifest_yaml.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::Validation(format!(
            "`kubectl apply` failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

async fn run_helm_install(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
    args: &[&str],
) -> Result<(), CliError> {
    let mut cmd = tokio::process::Command::new("helm");
    if let Some(kc) = kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }
    if let Some(ctx) = context {
        cmd.arg("--kube-context").arg(ctx);
    }
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().await.map_err(|e| {
        CliError::Validation(format!(
            "failed to invoke `helm` (is it installed and in PATH?): {e}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::Validation(format!(
            "`helm {}` failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}

/// Infers `(ForgeKind, Option<api_url>)` from a remote `--git-url` and optional explicit overrides.
fn infer_forge_config(
    git_url: &str,
    explicit_kind: Option<ForgeKind>,
    explicit_api_url: Option<&str>,
) -> (ForgeKind, Option<String>) {
    let parsed_slug = parse_remote_repo_url(git_url).ok();
    let kind = explicit_kind.unwrap_or_else(|| {
        let lower = git_url.to_ascii_lowercase();
        if lower.contains("github.com") {
            ForgeKind::GitHub
        } else if lower.contains("gitlab") {
            ForgeKind::GitLab
        } else {
            ForgeKind::Forgejo
        }
    });

    let api_url = if let Some(explicit) = explicit_api_url {
        let trimmed = explicit.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else {
        match kind {
            ForgeKind::GitHub => parsed_slug.as_ref().map(|slug| {
                if slug.host.eq_ignore_ascii_case("github.com") {
                    "https://api.github.com".to_string()
                } else {
                    format!("{}/api/v3", slug.origin_url)
                }
            }),
            ForgeKind::GitLab | ForgeKind::Forgejo | ForgeKind::Gitea => {
                parsed_slug.map(|slug| slug.origin_url)
            }
            ForgeKind::Local => None,
        }
    };

    (kind, api_url)
}

/// Installs Stage 0 core control-plane controllers (Flux CD, Kyverno, PKO v2, and optional Forgejo)
/// and seed manifests onto the bootstrapped cluster unless `--skip-core-install` was specified.
async fn install_stage0_core_addons(
    connection: &ClusterConnection,
    artifacts: &SeedControlPlaneArtifacts,
    with_forge: bool,
    git_token: Option<&str>,
) -> Result<(), CliError> {
    let ctx = connection.context.as_deref();
    let kc = connection.kubeconfig_path.as_deref();

    tracing::info!(context = ?ctx, "Applying seed namespaces (`flux-system`, `keel-system`, `keel-previews`)");
    run_kubectl_apply(ctx, kc, SEED_NAMESPACES_YAML).await?;

    tracing::info!("Installing Flux CD (`flux2`) into `flux-system`");
    run_helm_install(
        ctx,
        kc,
        &[
            "upgrade",
            "--install",
            "flux2",
            "oci://ghcr.io/fluxcd-community/charts/flux2",
            "--namespace",
            "flux-system",
            "--create-namespace",
            "--wait",
            "--timeout",
            "5m",
        ],
    )
    .await?;

    tracing::info!("Installing Kyverno admission controller into `kyverno`");
    run_helm_install(
        ctx,
        kc,
        &[
            "upgrade",
            "--install",
            "kyverno",
            "kyverno",
            "--repo",
            "https://kyverno.github.io/kyverno/",
            "--namespace",
            "kyverno",
            "--create-namespace",
            "--set",
            "admissionController.replicas=1",
            "--wait",
            "--timeout",
            "5m",
        ],
    )
    .await?;

    tracing::info!("Installing Pulumi Kubernetes Operator (PKO v2) into `keel-system`");
    run_helm_install(
        ctx,
        kc,
        &[
            "upgrade",
            "--install",
            "pulumi-kubernetes-operator",
            "oci://ghcr.io/pulumi/helm-charts/pulumi-kubernetes-operator",
            "--namespace",
            "keel-system",
            "--create-namespace",
            "--wait",
            "--timeout",
            "5m",
        ],
    )
    .await?;

    if with_forge {
        tracing::info!("Installing in-cluster Forgejo Git forge into `keel-system`");
        run_helm_install(
            ctx,
            kc,
            &[
                "upgrade",
                "--install",
                "forgejo",
                "oci://code.forgejo.org/forgejo-helm/forgejo",
                "--namespace",
                "keel-system",
                "--create-namespace",
                "--set",
                "gitea.admin.username=keel",
                "--set",
                "gitea.admin.password=keel-admin-dev",
                "--set",
                "gitea.admin.email=admin@keel.local",
                "--set",
                "gitea.config.database.DB_TYPE=sqlite3",
                "--set",
                "persistence.enabled=false",
                "--set",
                "valkey-cluster.enabled=false",
                "--set",
                "valkey.enabled=false",
                "--set",
                "postgresql.enabled=false",
                "--set",
                "postgresql-ha.enabled=false",
                "--wait",
                "--timeout",
                "5m",
            ],
        )
        .await?;
    }

    tracing::info!("Applying Kyverno policies & management core Stack");
    let kyverno_yaml = artifacts.kyverno_bundle.to_multi_doc_yaml()?;
    run_kubectl_apply(ctx, kc, &kyverno_yaml).await?;

    if artifacts.kyverno_bundle.policy_exception.is_some() {
        tracing::info!("Deploying local-dev Docker socket proxy in flux-system for nested kind");
        let docker_proxy_yaml = format!(
            "apiVersion: apps/v1\n\
             kind: Deployment\n\
             metadata:\n\
             \x20 name: keel-docker-proxy\n\
             \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             \x20   keel.io/local-dev-only: \"true\"\n\
             spec:\n\
             \x20 replicas: 1\n\
             \x20 selector:\n\
             \x20   matchLabels:\n\
             \x20     app: keel-docker-proxy\n\
             \x20 template:\n\
             \x20   metadata:\n\
             \x20     labels:\n\
             \x20       app: keel-docker-proxy\n\
             \x20   spec:\n\
             \x20     containers:\n\
             \x20       - name: socat\n\
             \x20         image: alpine/socat:latest\n\
             \x20         args:\n\
             \x20           - TCP-LISTEN:2375,fork,reuseaddr\n\
             \x20           - UNIX-CONNECT:/var/run/docker.sock\n\
             \x20         ports:\n\
             \x20           - containerPort: 2375\n\
             \x20         volumeMounts:\n\
             \x20           - name: docker-sock\n\
             \x20             mountPath: /var/run/docker.sock\n\
             \x20     volumes:\n\
             \x20       - name: docker-sock\n\
             \x20         hostPath:\n\
             \x20           path: /var/run/docker.sock\n\
             \x20           type: Socket\n\
             ---\n\
             apiVersion: v1\n\
             kind: Service\n\
             metadata:\n\
             \x20 name: keel-docker-proxy\n\
             \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             \x20   keel.io/local-dev-only: \"true\"\n\
             spec:\n\
             \x20 selector:\n\
             \x20   app: keel-docker-proxy\n\
             \x20 ports:\n\
             \x20   - port: 2375\n\
             \x20     targetPort: 2375\n"
        );
        run_kubectl_apply(ctx, kc, &docker_proxy_yaml).await?;
    }

    let mgmt_sa_name = artifacts
        .management_stack
        .spec
        .service_account_name
        .as_deref()
        .unwrap_or("keel-management-pko");
    let mgmt_rbac_yaml = format!(
        "apiVersion: v1\n\
         kind: ServiceAccount\n\
         metadata:\n\
         \x20 name: {mgmt_sa_name}\n\
         \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
         ---\n\
         apiVersion: rbac.authorization.k8s.io/v1\n\
         kind: ClusterRoleBinding\n\
         metadata:\n\
         \x20 name: {mgmt_sa_name}\n\
         roleRef:\n\
         \x20 apiGroup: rbac.authorization.k8s.io\n\
         \x20 kind: ClusterRole\n\
         \x20 name: cluster-admin\n\
         subjects:\n\
         - kind: ServiceAccount\n\
         \x20 name: {mgmt_sa_name}\n\
         \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
         ---\n\
         apiVersion: networking.k8s.io/v1\n\
         kind: NetworkPolicy\n\
         metadata:\n\
         \x20 name: allow-pko-workspace\n\
         \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
         spec:\n\
         \x20 podSelector:\n\
         \x20   matchLabels:\n\
         \x20     auto.pulumi.com/component: workspace\n\
         \x20 policyTypes:\n\
         \x20 - Ingress\n\
         \x20 ingress:\n\
         \x20 - from:\n\
         \x20   - namespaceSelector:\n\
         \x20       matchLabels:\n\
         \x20         kubernetes.io/metadata.name: {KEEL_SYSTEM_NAMESPACE}\n\
         \x20   ports:\n\
         \x20   - protocol: TCP\n\
         \x20     port: 50051\n\
         ---\n\
         apiVersion: networking.k8s.io/v1\n\
         kind: NetworkPolicy\n\
         metadata:\n\
         \x20 name: allow-source-controller-artifacts\n\
         \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
         spec:\n\
         \x20 podSelector:\n\
         \x20   matchLabels:\n\
         \x20     app: source-controller\n\
         \x20 policyTypes:\n\
         \x20 - Ingress\n\
         \x20 ingress:\n\
         \x20 - from:\n\
         \x20   - namespaceSelector: {{}}\n\
         \x20   ports:\n\
         \x20   - protocol: TCP\n\
         \x20     port: 9090\n"
    );
    run_kubectl_apply(ctx, kc, &mgmt_rbac_yaml).await?;

    let mgmt_yaml = artifacts.management_stack.to_yaml()?;
    run_kubectl_apply(ctx, kc, &mgmt_yaml).await?;

    let repo_url = &artifacts.flux_git_repository.spec.url;
    if repo_url.starts_with("http://")
        || repo_url.starts_with("https://")
        || repo_url.starts_with("ssh://")
        || repo_url.starts_with("git://")
    {
        if let Some(token) = git_token.map(str::trim).filter(|t| !t.is_empty()) {
            let secret_doc = serde_json::json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {
                    "name": "git-credentials",
                    "namespace": FLUX_SYSTEM_NAMESPACE,
                },
                "type": "Opaque",
                "stringData": {
                    "username": "git",
                    "password": token,
                },
            });
            let secret_yaml = serde_yml::to_string(&secret_doc)?;
            run_kubectl_apply(ctx, kc, &secret_yaml).await?;
        }
        let git_repo_yaml = artifacts.flux_git_repository.to_yaml()?;
        run_kubectl_apply(ctx, kc, &git_repo_yaml).await?;

        let tenants_ks_yaml = format!(
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\n\
             kind: Kustomization\n\
             metadata:\n\
             \x20 name: keel-tenants\n\
             \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
             spec:\n\
             \x20 interval: 1m\n\
             \x20 path: ./tenants\n\
             \x20 prune: true\n\
             \x20 sourceRef:\n\
             \x20   kind: GitRepository\n\
             \x20   name: {FLUX_SYSTEM_NAMESPACE}\n"
        );
        run_kubectl_apply(ctx, kc, &tenants_ks_yaml).await?;
    } else {
        tracing::info!(
            url = %repo_url,
            "Skipping in-cluster Flux GitRepository apply for local filesystem path; configure a reachable HTTP/SSH Git remote for cluster pull-polling"
        );
    }

    Ok(())
}

/// Executes `keel bootstrap init` using an injected [`BootstrapTarget`] driver.
pub async fn execute_bootstrap_init_with_target(
    args: &BootstrapInitArgs,
    target: &dyn BootstrapTarget,
) -> Result<BootstrapInitOutcome, CliError> {
    let cfg = args.to_bootstrap_config();
    cfg.validate()?;

    tracing::info!(
        target = %cfg.target,
        cluster_name = %cfg.cluster_name,
        local_dev_docker_socket_exception = cfg.local_dev_docker_socket_exception,
        skip_core_install = args.skip_core_install,
        "Executing Stage 0 bootstrap initialization"
    );

    let connection = target.provision_cluster(&cfg).await?;

    let repo_url_override = if let Some(git_url) = args.git_url.as_deref() {
        Some(git_url.trim().to_string())
    } else if args.with_forge {
        None
    } else {
        args.repo_dir
            .as_ref()
            .map(|dir| absolutize_path(dir).map(|abs| default_bare_repo_for_workdir(&abs)))
            .transpose()?
            .map(|p| p.to_string_lossy().into_owned())
    };
    let mut artifacts = synthesize_seed_control_plane(&cfg, repo_url_override.as_deref()).await?;

    if let Some(git_url) = args.git_url.as_deref() {
        let (kind, api_url) = infer_forge_config(
            git_url,
            args.forge_type.map(Into::into),
            args.forge_api_url.as_deref(),
        );
        artifacts.keel_config.forge.provider = kind;
        artifacts.keel_config.forge.api_url = api_url;
        artifacts.keel_config.validate()?;
    }

    if args
        .git_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        artifacts.flux_git_repository.spec.secret_ref = Some(LocalObjectReference {
            name: "git-credentials".to_string(),
        });
    }

    if !args.skip_core_install {
        install_stage0_core_addons(
            &connection,
            &artifacts,
            cfg.with_forge,
            args.git_token.as_deref(),
        )
        .await?;
    }

    let mut _forgejo_pf: Option<TempPortForward> = None;
    let mut push_url_override: Option<String> = None;
    let mut push_creds: Option<GitCredentials> = None;

    if cfg.with_forge && !args.skip_core_install {
        tracing::info!("Provisioning `keel/infra-gitops` repository and API token in Forgejo");
        let pf = start_forgejo_port_forward(
            connection.context.as_deref(),
            connection.kubeconfig_path.as_deref(),
        )
        .await?;
        let token = provision_in_cluster_forgejo(
            connection.context.as_deref(),
            connection.kubeconfig_path.as_deref(),
            pf.local_port,
        )
        .await?;
        push_url_override = Some(format!(
            "http://127.0.0.1:{}/keel/infra-gitops.git",
            pf.local_port
        ));
        push_creds = Some(GitCredentials::HttpsBasic {
            username: "keel".to_string(),
            password_or_token: token,
        });
        _forgejo_pf = Some(pf);
    } else if args.git_url.is_some()
        && let Some(token) = args
            .git_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    {
        ensure_external_repo_exists(
            artifacts.keel_config.forge.provider,
            &artifacts.keel_config.forge.repo_url,
            artifacts.keel_config.forge.api_url.as_deref(),
            token,
        )
        .await;
        push_creds = Some(GitCredentials::HttpsBasic {
            username: "git".to_string(),
            password_or_token: token.to_string(),
        });
    }

    let seed_repo = if let Some(repo_dir) = &args.repo_dir {
        Some(
            initialize_seed_git_repo_with_push(
                repo_dir,
                &artifacts,
                push_url_override.as_deref(),
                push_creds,
            )
            .await?,
        )
    } else {
        None
    };

    let output = format_bootstrap_init_summary(&connection, &artifacts, seed_repo.as_ref());

    Ok(BootstrapInitOutcome {
        connection,
        artifacts,
        seed_repo,
        output,
    })
}

/// Executes `keel bootstrap init` resolving the [`BootstrapTarget`] driver from [`bootstrap_target_factory`].
pub async fn execute_bootstrap_init(
    args: &BootstrapInitArgs,
) -> Result<BootstrapInitOutcome, CliError> {
    let cfg = args.to_bootstrap_config();
    cfg.validate()?;
    let target = bootstrap_target_factory(cfg.target)?;
    execute_bootstrap_init_with_target(args, target.as_ref()).await
}

fn format_bootstrap_init_summary(
    conn: &ClusterConnection,
    artifacts: &SeedControlPlaneArtifacts,
    seed_repo: Option<&SeedRepoInitResult>,
) -> String {
    let mut out = String::new();
    out.push_str("Stage 0 Seed Control-Plane Bootstrap Complete\n");
    out.push_str("=============================================\n");
    out.push_str(&format!("Target:          {}\n", conn.target));
    out.push_str(&format!("Cluster:         {}\n", conn.cluster_name));
    if let Some(ctx) = &conn.context {
        out.push_str(&format!("Context:         {ctx}\n"));
    }
    if let Some(ep) = &conn.server_endpoint {
        out.push_str(&format!("API Endpoint:    {ep}\n"));
    }
    out.push_str(&format!(
        "Flux Polling:    interval={} (outbound pull-polling active)\n",
        artifacts.flux_git_repository.spec.interval
    ));
    out.push_str(&format!(
        "Kyverno Guard:   {} ClusterPolicies enforced (docker-socket exception: {})\n",
        artifacts.kyverno_bundle.cluster_policies.len(),
        artifacts.kyverno_bundle.policy_exception.is_some()
    ));
    out.push_str(&format!(
        "Anti-Lockout:    Stack '{}' (protect={})\n",
        artifacts.management_stack.metadata.name,
        artifacts.management_stack.is_protected()
    ));
    if let Some(repo) = seed_repo {
        out.push_str(&format!(
            "Seed Git Repo:   {} (origin: {}, commit: {})\n",
            repo.repo_dir.display(),
            repo.bare_repo_dir.display(),
            repo.initial_commit_sha
        ));
    }
    out.push_str(&format!(
        "Token Secret:    {}/{}\n",
        artifacts.bootstrap_secret_namespace, artifacts.bootstrap_secret_name
    ));
    out.push_str(&format!("Bootstrap Token: {}\n", artifacts.bootstrap_token));
    out.push_str("Portal URL:      http://127.0.0.1:8080\n");
    out
}

// ============================================================================
// 2. `keel bootstrap recover`
// ============================================================================

/// Outcome of `keel bootstrap recover` (`SDD.md` §2.2, `NFR-REL-01`).
#[derive(Debug, Clone)]
pub struct BootstrapRecoverOutcome {
    pub backend_url: String,
    pub recovery_stack: PkoStack,
    pub pulumi_commands: Vec<Vec<String>>,
    pub output: String,
}

fn validate_non_flag_input(value: &str, field: &str) -> Result<(), CliError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CliError::Validation(format!("{field} must not be empty")));
    }
    if trimmed.starts_with('-') {
        return Err(CliError::Validation(format!(
            "{field} must not start with '-' (CLI flag injection defense)"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(CliError::Validation(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Constructs explicit `pulumi` argument vectors (zero shell invocation per `NFR-SEC-06`)
/// for break-glass control-plane recovery against versioned S3 state.
pub fn build_pulumi_recover_argv(
    args: &BootstrapRecoverArgs,
) -> Result<(String, Vec<Vec<String>>), CliError> {
    let s3_endpoint = normalize_server_url(&args.s3_endpoint)?;
    validate_dns_label(&args.bucket, "bucket")?;
    validate_non_flag_input(&args.stack, "stack")?;

    let backend_url = format!(
        "s3://{}?endpoint={}&s3ForcePathStyle=true",
        args.bucket, s3_endpoint
    );

    let commands = vec![
        vec![
            "pulumi".to_string(),
            "login".to_string(),
            backend_url.clone(),
        ],
        vec![
            "pulumi".to_string(),
            "stack".to_string(),
            "select".to_string(),
            "--".to_string(),
            args.stack.trim().to_string(),
        ],
        vec![
            "pulumi".to_string(),
            "refresh".to_string(),
            "--yes".to_string(),
            "--stack".to_string(),
            args.stack.trim().to_string(),
        ],
        vec![
            "pulumi".to_string(),
            "up".to_string(),
            "--yes".to_string(),
            "--stack".to_string(),
            args.stack.trim().to_string(),
        ],
    ];

    Ok((backend_url, commands))
}

/// Executes `keel bootstrap recover` validating S3 recovery inputs and synthesizing the
/// anti-lockout `management-cluster-core` stack reconciliation plan.
pub async fn execute_bootstrap_recover(
    args: &BootstrapRecoverArgs,
) -> Result<BootstrapRecoverOutcome, CliError> {
    let (backend_url, pulumi_commands) = build_pulumi_recover_argv(args)?;

    let mut stack_params =
        ManagementCoreStackParams::new(&backend_url, FLUX_SYSTEM_NAMESPACE, "clusters/management");
    stack_params.pulumi_stack_name = args.stack.trim().to_string();
    let recovery_stack = synthesize_management_core_stack(&stack_params)?;

    let mut output = String::new();
    output.push_str("Break-Glass Control-Plane Recovery Plan\n");
    output.push_str("=======================================\n");
    output.push_str(&format!("S3 Endpoint:   {}\n", args.s3_endpoint.trim()));
    output.push_str(&format!("State Bucket:  {}\n", args.bucket));
    output.push_str(&format!("Pulumi Stack:  {}\n", args.stack.trim()));
    output.push_str(&format!("Backend URL:   {backend_url}\n"));
    output.push_str(&format!(
        "Anti-Lockout:  {} (protect={})\n\n",
        recovery_stack.metadata.name,
        recovery_stack.is_protected()
    ));
    output.push_str("Recovery Sequence:\n");
    for (idx, argv) in pulumi_commands.iter().enumerate() {
        output.push_str(&format!("  {}. {}\n", idx + 1, argv.join(" ")));
    }

    Ok(BootstrapRecoverOutcome {
        backend_url,
        recovery_stack,
        pulumi_commands,
        output,
    })
}

// ============================================================================
// 3. `keel auth login`
// ============================================================================

/// PKCE `S256` verifier/challenge pair (`NFR-SEC-01`).
#[derive(Clone, PartialEq, Eq)]
pub struct PkceChallenge {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

impl std::fmt::Debug for PkceChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkceChallenge")
            .field("code_verifier", &"[REDACTED]")
            .field("code_challenge", &self.code_challenge)
            .field("state", &self.state)
            .finish()
    }
}

/// Generates a fresh 256-bit OIDC PKCE `S256` challenge and CSRF state nonce.
pub fn generate_pkce_pair() -> PkceChallenge {
    let verifier_bytes = EnvelopeCrypto::generate_key();
    let state_bytes = EnvelopeCrypto::generate_key();
    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(digest);
    let state = URL_SAFE_NO_PAD.encode(&state_bytes[..16]);

    PkceChallenge {
        code_verifier,
        code_challenge,
        state,
    }
}

/// Outcome of `keel auth login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthLoginOutcome {
    BootstrapSession {
        sub: String,
        phase: AuthPhase,
        output: String,
    },
    OidcRedirect {
        authorize_url: String,
        output: String,
    },
}

impl AuthLoginOutcome {
    pub fn output(&self) -> &str {
        match self {
            Self::BootstrapSession { output, .. } | Self::OidcRedirect { output, .. } => output,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapLoginApiResponse {
    authenticated: bool,
    sub: String,
    phase: AuthPhase,
}

/// Executes `keel auth login` against `keel-server` via Stage 0 bootstrap token or OIDC PKCE (`SDD.md` §6.1).
pub async fn execute_auth_login(
    args: &AuthLoginArgs,
    ctx: &CliContext,
) -> Result<AuthLoginOutcome, CliError> {
    let base_url = normalize_server_url(&args.server)?;

    if let Some(raw_token) = &args.bootstrap_token {
        let trimmed = raw_token.trim();
        if trimmed.is_empty() {
            return Err(CliError::Validation(
                "bootstrap_token must not be empty".to_string(),
            ));
        }

        let endpoint = format!("{base_url}/api/v1/auth/bootstrap-login");
        let resp = ctx
            .http
            .post(&endpoint)
            .json(&json!({ "token": trimmed }))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }

        let session_cookie =
            extract_session_cookie_from_headers(resp.headers()).ok_or_else(|| {
                CliError::Execution(
                    "keel-server did not return a session Set-Cookie header".to_string(),
                )
            })?;

        let login_body: BootstrapLoginApiResponse = resp.json().await?;
        if !login_body.authenticated {
            return Err(CliError::Execution(
                "Bootstrap authentication rejected by server".to_string(),
            ));
        }

        ctx.save_session_token(&base_url, &session_cookie)?;

        let output = format!(
            "Authenticated with {base_url} as '{}' (phase: {:?}). Session saved.",
            login_body.sub, login_body.phase
        );
        Ok(AuthLoginOutcome::BootstrapSession {
            sub: login_body.sub,
            phase: login_body.phase,
            output,
        })
    } else {
        // OIDC Authorization Code + PKCE flow
        let pkce = generate_pkce_pair();
        let endpoint = format!("{base_url}/api/v1/auth/login");
        let resp = ctx.no_redirect_http.get(&endpoint).send().await?;

        if resp.status() == StatusCode::FOUND || resp.status() == StatusCode::SEE_OTHER {
            let authorize_url = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| {
                    CliError::Execution(
                        "OIDC redirect response missing Location header".to_string(),
                    )
                })?;

            let output = format!(
                "Open the following OIDC PKCE URL in your browser to authenticate (challenge S256={}):\n  {authorize_url}",
                pkce.code_challenge
            );
            Ok(AuthLoginOutcome::OidcRedirect {
                authorize_url,
                output,
            })
        } else {
            Err(extract_api_error(resp).await)
        }
    }
}

fn extract_session_cookie_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for val in headers.get_all(SET_COOKIE) {
        let raw = val.to_str().ok()?;
        for segment in raw.split(';') {
            let trimmed = segment.trim();
            if let Some((k, v)) = trimmed.split_once('=')
                && k.trim() == SESSION_COOKIE_NAME
                && !v.trim().is_empty()
            {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

async fn extract_api_error(resp: reqwest::Response) -> CliError {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if let Ok(parsed) = serde_json::from_str::<Value>(&body)
        && let Some(msg) = parsed
            .get("message")
            .or_else(|| parsed.get("error"))
            .and_then(Value::as_str)
    {
        return CliError::Api {
            status,
            message: msg.to_string(),
        };
    }
    CliError::Api {
        status,
        message: if body.trim().is_empty() {
            format!("HTTP {status}")
        } else {
            body.trim().to_string()
        },
    }
}

fn attach_auth(
    req: reqwest::RequestBuilder,
    ctx: &CliContext,
    server_url: &str,
) -> reqwest::RequestBuilder {
    if let Some(token) = ctx.load_session_token(server_url) {
        req.header(AUTHORIZATION, format!("Bearer {token}"))
            .header(COOKIE, format!("{SESSION_COOKIE_NAME}={token}"))
    } else {
        req
    }
}

// ============================================================================
// 4. `keel serve`
// ============================================================================

fn build_forge_provider(
    keel_cfg: Option<&KeelConfig>,
    resolved_token: Option<&str>,
    api_url_override: Option<&str>,
    bare_repo: &Path,
) -> Result<Arc<dyn ForgeProvider>, CliError> {
    let Some(cfg) = keel_cfg else {
        return Ok(Arc::new(LocalGitProvider::new(bare_repo)));
    };
    if cfg.forge.provider == ForgeKind::Local {
        return Ok(Arc::new(LocalGitProvider::new(bare_repo)));
    }

    let Some(token) = resolved_token.map(str::trim).filter(|t| !t.is_empty()) else {
        tracing::warn!(
            provider = ?cfg.forge.provider,
            repo_url = %cfg.forge.repo_url,
            "Remote forge configured in .keel/config.yaml, but no --git-token, KEEL_GIT_TOKEN, or keel-system/forge-token Secret was found; falling back to LocalGitProvider"
        );
        return Ok(Arc::new(LocalGitProvider::new(bare_repo)));
    };

    let slug = parse_remote_repo_url(&cfg.forge.repo_url).map_err(|e| {
        CliError::Validation(format!(
            "invalid forge.repo_url '{}': {e}",
            cfg.forge.repo_url
        ))
    })?;

    let effective_api = api_url_override.or(cfg.forge.api_url.as_deref());

    let forge: Arc<dyn ForgeProvider> = match cfg.forge.provider {
        ForgeKind::GitHub => {
            let api_base = effective_api
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| {
                    if slug.host.eq_ignore_ascii_case("github.com") {
                        "https://api.github.com".to_string()
                    } else {
                        format!("{}/api/v3", slug.origin_url)
                    }
                });
            Arc::new(
                GitHubProvider::new(api_base, &slug.owner, &slug.repo, token)
                    .map_err(|e| CliError::Validation(e.to_string()))?,
            )
        }
        ForgeKind::GitLab => {
            let api_base = effective_api
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| slug.origin_url.clone());
            Arc::new(
                GitLabProvider::new(api_base, &slug.project_path, token)
                    .map_err(|e| CliError::Validation(e.to_string()))?,
            )
        }
        ForgeKind::Forgejo | ForgeKind::Gitea => {
            let api_base = effective_api
                .map(|u| {
                    let trimmed = u.trim_end_matches('/');
                    trimmed
                        .strip_suffix("/api/v1")
                        .unwrap_or(trimmed)
                        .to_string()
                })
                .unwrap_or_else(|| slug.origin_url.clone());
            Arc::new(
                ForgejoGiteaProvider::new(api_base, &slug.owner, &slug.repo, token)
                    .map_err(|e| CliError::Validation(e.to_string()))?,
            )
        }
        ForgeKind::Local => Arc::new(LocalGitProvider::new(bare_repo)),
    };

    tracing::info!(
        provider = ?cfg.forge.provider,
        repo_url = %cfg.forge.repo_url,
        "Configured remote ForgeProvider"
    );
    Ok(forge)
}

/// Builds a configured stateless [`AppState`] from [`ServeArgs`] (`SDD.md` §6.3, §7.2).
pub async fn build_serve_state(args: &ServeArgs) -> Result<(AppState, Option<String>), CliError> {
    let (state, token, _pf) = build_serve_state_with_forward(args, false).await?;
    Ok((state, token))
}

async fn build_serve_state_with_forward(
    args: &ServeArgs,
    enable_in_cluster_port_forward: bool,
) -> Result<(AppState, Option<String>, Option<TempPortForward>), CliError> {
    if args.bind.trim().is_empty() || !args.bind.contains(':') {
        return Err(CliError::Validation(format!(
            "bind address must be in '<host>:<port>' format, got '{}'",
            args.bind
        )));
    }

    let base_temp = std::env::temp_dir().join(format!("keel-serve-{}", uuid::Uuid::new_v4()));
    let repo_workdir = match &args.repo_dir {
        Some(dir) => absolutize_path(dir)?,
        None => base_temp.join("workdir"),
    };
    let bare_repo = match &args.bare_repo {
        Some(dir) => absolutize_path(dir)?,
        None => default_bare_repo_for_workdir(&repo_workdir),
    };

    let git_runner = GitRunner::new();
    if !bare_repo.join("HEAD").exists() {
        git_runner.init_bare(&bare_repo).await?;
    }
    let bare_str = bare_repo
        .to_str()
        .ok_or_else(|| CliError::Validation("bare_repo path must be valid UTF-8".to_string()))?
        .to_string();

    if !repo_workdir.join(".git").exists() {
        if let Some(parent) = repo_workdir.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        git_runner.clone(&bare_str, &repo_workdir).await?;
    } else if bare_repo.join("HEAD").exists()
        && let Ok(existing_origin) = git_runner.get_remote_url(&repo_workdir, "origin").await
        && is_relative_local_remote(&existing_origin)
    {
        git_runner
            .set_remote_url(&repo_workdir, "origin", &bare_str)
            .await?;
    }

    // Ensure `main` branch exists so proposal branches can cleanly branch off `main`.
    if !git_runner
        .ref_exists(&repo_workdir, "refs/heads/main")
        .await?
    {
        let cfg_dir = repo_workdir.join(".keel");
        tokio::fs::create_dir_all(&cfg_dir).await?;
        let cfg_path = cfg_dir.join("config.yaml");
        if !tokio::fs::try_exists(&cfg_path).await.unwrap_or(false) {
            tokio::fs::write(&cfg_path, KeelConfig::default().to_yaml()?).await?;
        }
        let default_tenant_dir = repo_workdir.join("tenants/default");
        tokio::fs::create_dir_all(&default_tenant_dir).await?;
        let default_rbac_path = default_tenant_dir.join("rbac.yaml");
        if !tokio::fs::try_exists(&default_rbac_path)
            .await
            .unwrap_or(false)
        {
            let rbac_yaml =
                TenantManifestBundle::synthesize(&TenantIsolationConfig::new("default"))?
                    .to_multi_doc_yaml()?;
            tokio::fs::write(&default_rbac_path, rbac_yaml).await?;
        }
        let providers_dir = repo_workdir.join("providers");
        tokio::fs::create_dir_all(&providers_dir).await?;
        let registry_path = providers_dir.join("registry.yaml");
        if !tokio::fs::try_exists(&registry_path).await.unwrap_or(false) {
            tokio::fs::write(&registry_path, ProviderRegistry::default_day1().to_yaml()?).await?;
        }
        let mgmt_dir = repo_workdir.join("clusters/management");
        tokio::fs::create_dir_all(&mgmt_dir).await?;
        let mgmt_stack_path = mgmt_dir.join("management-stack.yaml");
        if !tokio::fs::try_exists(&mgmt_stack_path)
            .await
            .unwrap_or(false)
        {
            let mgmt_params = ManagementCoreStackParams::new(
                "s3://pulumi-state",
                FLUX_SYSTEM_NAMESPACE,
                "clusters/management",
            );
            let mgmt_stack = synthesize_management_core_stack(&mgmt_params)?;
            tokio::fs::write(&mgmt_stack_path, mgmt_stack.to_yaml()?).await?;
        }
        git_runner
            .add(
                &repo_workdir,
                [
                    ".keel/config.yaml",
                    "tenants/default/rbac.yaml",
                    "providers/registry.yaml",
                    "clusters/management/management-stack.yaml",
                ],
            )
            .await?;
        let author = CommitAuthor::new("Keel Server", "server@keel.io");
        git_runner
            .commit(&repo_workdir, "chore(bootstrap): seed main branch", &author)
            .await?;
        git_runner
            .push(&repo_workdir, "origin", "HEAD:refs/heads/main")
            .await?;
    }

    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| repo_workdir.join(".keel/config.yaml"));
    let parsed_keel_cfg = if tokio::fs::try_exists(&config_path).await.unwrap_or(false) {
        let raw = tokio::fs::read_to_string(&config_path).await?;
        Some(KeelConfig::from_yaml(&raw)?)
    } else {
        None
    };

    let auth_phase = parsed_keel_cfg
        .as_ref()
        .map(|c| c.auth_phase)
        .unwrap_or(AuthPhase::Bootstrap);
    let oidc_cfg = parsed_keel_cfg
        .as_ref()
        .and_then(|c| c.oidc.as_ref())
        .filter(|o| o.enabled)
        .cloned();

    let telemetry = TelemetryMux::default();
    let kube_reader = KubeClusterStatusReader::try_default().await.ok();
    if let Some(ref reader) = kube_reader {
        tracing::info!("Spawning background kube-rs CRD watchers for SSE telemetry");
        for kind in WatchedCrdKind::ALL {
            telemetry.spawn_crd_watcher(reader.client().clone(), kind, None);
        }
    }

    let env_token = std::env::var("KEEL_GIT_TOKEN").ok();
    let mut resolved_token: Option<String> = args
        .git_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            env_token
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        });

    if resolved_token.is_none()
        && parsed_keel_cfg
            .as_ref()
            .is_some_and(|c| c.forge.provider != ForgeKind::Local)
        && let Some(ref reader) = kube_reader
    {
        resolved_token = reader
            .read_secret_key(KEEL_SYSTEM_NAMESPACE, FORGE_TOKEN_SECRET_NAME, "token")
            .await;
    }

    let mut forge_pf: Option<TempPortForward> = None;
    let mut api_url_override: Option<String> = None;
    if enable_in_cluster_port_forward
        && kube_reader.is_some()
        && let Some(ref cfg) = parsed_keel_cfg
        && cfg
            .forge
            .api_url
            .as_deref()
            .is_some_and(|u| u.contains(".svc.cluster.local"))
        && let Ok(pf) = start_forgejo_port_forward(None, None).await
    {
        tracing::info!(
            local_port = pf.local_port,
            "Started automatic port-forward to in-cluster Forgejo"
        );
        api_url_override = Some(format!("http://127.0.0.1:{}", pf.local_port));
        if let Ok(slug) = parse_remote_repo_url(&cfg.forge.repo_url) {
            let local_remote = format!(
                "http://127.0.0.1:{}/{}/{}.git",
                pf.local_port, slug.owner, slug.repo
            );
            let _ = git_runner
                .set_remote_url(&repo_workdir, "origin", &local_remote)
                .await;
        }
        forge_pf = Some(pf);
    }

    let forge = build_forge_provider(
        parsed_keel_cfg.as_ref(),
        resolved_token.as_deref(),
        api_url_override.as_deref(),
        &bare_repo,
    )?;

    let git_runner = if let Some(ref tok) = resolved_token
        && parsed_keel_cfg
            .as_ref()
            .is_some_and(|c| c.forge.provider != ForgeKind::Local)
    {
        let username = match parsed_keel_cfg.as_ref().map(|c| c.forge.provider) {
            Some(ForgeKind::Forgejo | ForgeKind::Gitea) => "keel",
            _ => "git",
        };
        git_runner.with_credentials(GitCredentials::HttpsBasic {
            username: username.to_string(),
            password_or_token: tok.clone(),
        })
    } else {
        git_runner
    };

    let secret_store: Arc<dyn SecretStateStore> = Arc::new(InMemorySecretStore::new());
    let mut generated_token = None;

    if auth_phase != AuthPhase::OidcActive {
        let token = EnvelopeCrypto::generate_bootstrap_token();
        let hash = EnvelopeCrypto::sha256_hex(token.as_bytes());
        secret_store.store_bootstrap_token_hash(&hash).await?;
        generated_token = Some(token);
    }

    let live_mgmt_backend = if let Some(ref reader) = kube_reader {
        reader
            .read_stack_backend(FLUX_SYSTEM_NAMESPACE, "management-cluster-core")
            .await
    } else {
        None
    };
    let repo_state_provider = RepoClusterStateProvider::new(&repo_workdir);
    let mgmt_stack_backend = live_mgmt_backend.or_else(|| {
        std::fs::read_to_string(repo_workdir.join("clusters/management/management-stack.yaml"))
            .ok()
            .and_then(|raw| serde_yml::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("spec")
                    .and_then(|s| s.get("backend"))
                    .and_then(|b| b.as_str())
                    .map(str::to_string)
            })
    });
    let mut state = AppState::new(
        forge,
        secret_store,
        telemetry,
        git_runner,
        repo_workdir,
        bare_str,
        auth_phase,
    )
    .with_oidc_config(oidc_cfg);
    if let Some(backend) = mgmt_stack_backend {
        state.backend_url = backend;
    }

    if let Some(reader) = kube_reader {
        tracing::info!("Connected to Kubernetes cluster for live Flux/PKO status reconciliation");
        if reader.has_docker_proxy_service().await {
            tracing::info!(
                "Detected keel-docker-proxy in flux-system; enabling DOCKER_HOST for kind workspaces"
            );
            state = state.with_docker_proxy_url(Some(
                "tcp://keel-docker-proxy.flux-system.svc.cluster.local:2375".to_string(),
            ));
        }
        state = state.with_state_provider(Arc::new(KubeClusterStateProvider::new(
            repo_state_provider,
            Arc::new(reader),
        )));
    }

    Ok((state, generated_token, forge_pf))
}

/// Binds `keel-server` on `args.bind` and runs until `shutdown` resolves.
pub async fn execute_serve_until<F>(args: &ServeArgs, shutdown: F) -> Result<SocketAddr, CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (state, generated_token, _forge_pf) = build_serve_state_with_forward(args, true).await?;
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    let local_addr = listener.local_addr()?;

    if let Some(ref token) = generated_token {
        println!("Keel Control Plane listening on http://{local_addr}");
        println!("Stage 0 Bootstrap Token: {token}");
    }
    tracing::info!(%local_addr, "Listening for Keel HTTP/SSE connections");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });

    let mut graceful_rx = shutdown_rx.clone();
    let mut serve_fut = std::pin::pin!(std::future::IntoFuture::into_future(
        axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = graceful_rx.wait_for(|v| *v).await;
        })
    ));

    tokio::select! {
        res = &mut serve_fut => {
            res?;
        }
        _ = shutdown_rx.wait_for(|v| *v) => {
            // Bound graceful drain so long-lived SSE streams (`/api/v1/events/stream`)
            // cannot block Ctrl+C exit indefinitely.
            if let Ok(res) = tokio::time::timeout(Duration::from_millis(500), serve_fut).await {
                res?;
            }
        }
    }
    Ok(local_addr)
}

/// Runs `keel serve` until `SIGINT` (`Ctrl+C`) is received.
pub async fn execute_serve(args: &ServeArgs) -> Result<String, CliError> {
    let addr = execute_serve_until(args, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(format!("Keel server on {addr} shut down cleanly."))
}

// ============================================================================
// 5. `keel infra list`
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StacksApiResponse {
    stacks: Vec<StackSummary>,
}

/// Formats PKO v2 [`StackSummary`] inventory items as either a terminal table or pretty JSON.
pub fn format_stacks_output(
    stacks: &[StackSummary],
    format: OutputFormat,
) -> Result<String, CliError> {
    match format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(stacks)?),
        OutputFormat::Table => {
            if stacks.is_empty() {
                return Ok("No PKO stacks found.".to_string());
            }

            let headers = [
                "NAMESPACE",
                "NAME",
                "TENANT",
                "PROVIDER",
                "MODE",
                "PROTECT",
                "STATUS",
            ];
            let rows: Vec<[String; 7]> = stacks
                .iter()
                .map(|s| {
                    [
                        s.namespace.clone(),
                        s.name.clone(),
                        s.tenant.clone().unwrap_or_else(|| "-".to_string()),
                        s.provider.clone().unwrap_or_else(|| "-".to_string()),
                        if s.preview {
                            "preview".to_string()
                        } else {
                            "active".to_string()
                        },
                        if s.protect {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        },
                        s.status.clone(),
                    ]
                })
                .collect();

            let mut widths = headers.map(str::len);
            for row in &rows {
                for (i, cell) in row.iter().enumerate() {
                    widths[i] = widths[i].max(cell.len());
                }
            }

            let mut out = String::new();
            out.push_str(&format_fixed_row(&headers, &widths));
            out.push('\n');
            let dividers: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            let div_refs: [&str; 7] = [
                &dividers[0],
                &dividers[1],
                &dividers[2],
                &dividers[3],
                &dividers[4],
                &dividers[5],
                &dividers[6],
            ];
            out.push_str(&format_fixed_row(&div_refs, &widths));

            for row in &rows {
                out.push('\n');
                let refs: [&str; 7] = [
                    &row[0], &row[1], &row[2], &row[3], &row[4], &row[5], &row[6],
                ];
                out.push_str(&format_fixed_row(&refs, &widths));
            }
            Ok(out)
        }
    }
}

fn format_fixed_row<const N: usize>(cells: &[&str; N], widths: &[usize; N]) -> String {
    let mut line = String::new();
    for (idx, cell) in cells.iter().enumerate() {
        if idx + 1 == N {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{:<width$}  ", cell, width = widths[idx]));
        }
    }
    line
}

/// Executes `keel infra list` querying `/api/v1/stacks` and rendering table or JSON output.
pub async fn execute_infra_list(
    args: &InfraListArgs,
    ctx: &CliContext,
) -> Result<String, CliError> {
    if let Some(tenant) = &args.tenant {
        validate_dns_label(tenant, "tenant")?;
    }
    if let Some(provider) = &args.provider {
        validate_dns_label(provider, "provider")?;
    }

    let base_url = normalize_server_url(&args.server)?;
    let endpoint = format!("{base_url}/api/v1/stacks");

    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = args.tenant.as_deref() {
        query.push(("tenant", t));
    }
    if let Some(p) = args.provider.as_deref() {
        query.push(("provider", p));
    }

    let req = attach_auth(ctx.http.get(&endpoint).query(&query), ctx, &base_url);
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let payload: StacksApiResponse = resp.json().await?;
    format_stacks_output(&payload.stacks, args.output)
}

// ============================================================================
// 6 & 7. `keel infra provision` and `keel app install`
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogApiResponse {
    applications: Vec<CatalogApplication>,
    blueprints: Vec<CatalogInfraBlueprint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProposalApiResponse {
    proposal: PullRequestInfo,
    branch: String,
    preview_stack: PkoStack,
    report: PreviewReport,
}

/// Outcome of opening an infrastructure provisioning or application installation PR (`AGENTS.md` §3.6).
#[derive(Debug, Clone)]
pub struct ProposalCommandOutcome {
    pub proposal: PullRequestInfo,
    pub branch: String,
    pub preview_stack: PkoStack,
    pub report: PreviewReport,
    pub output: String,
}

/// Reads and parses a YAML or JSON values file into a JSON object (`serde_json::Value`).
pub async fn load_values_file(path: &Path) -> Result<Value, CliError> {
    let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
        CliError::Validation(format!(
            "Failed to read values file '{}': {e}",
            path.display()
        ))
    })?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let val: Value = serde_yml::from_str(&raw)?;
    if !val.is_object() {
        return Err(CliError::Validation(format!(
            "Values file '{}' must deserialize to a YAML/JSON mapping object",
            path.display()
        )));
    }
    Ok(val)
}

/// Validates input parameters against a [`CatalogInfraBlueprint`] JSONSchema (`SDD.md` §4, §7.2).
pub fn validate_blueprint_values(
    blueprint: &CatalogInfraBlueprint,
    values: &Value,
) -> Result<(), CliError> {
    blueprint.validate()?;
    validate_schema_values(&blueprint.spec.schema, values)?;
    Ok(())
}

/// Validates input values and progressive-delivery flags against a [`CatalogApplication`] JSONSchema (`SDD.md` §5.1, §5.2).
pub fn validate_application_values(
    app: &CatalogApplication,
    values: &Value,
    canary: bool,
) -> Result<PreparedHelmValues, CliError> {
    app.validate()?;
    if canary && !app.spec.progressive_delivery.flagger_supported {
        return Err(CliError::Validation(format!(
            "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
            app.metadata.id
        )));
    }
    Ok(prepare_helm_values(app, values)?)
}

/// Executes `keel infra provision`: validates values against the blueprint JSONSchema and opens a PR (`AGENTS.md` §3.6).
pub async fn execute_infra_provision(
    args: &InfraProvisionArgs,
    ctx: &CliContext,
) -> Result<ProposalCommandOutcome, CliError> {
    validate_dns_label(&args.blueprint, "blueprint")?;
    validate_dns_label(&args.tenant, "tenant")?;
    let values = load_values_file(&args.values_file).await?;
    let base_url = normalize_server_url(&args.server)?;

    // Fetch catalog to validate blueprint JSONSchema client-side before opening PR
    let catalog = fetch_catalog(&base_url, ctx).await?;
    let blueprint = catalog
        .blueprints
        .iter()
        .find(|bp| bp.metadata.id == args.blueprint)
        .ok_or_else(|| {
            CliError::Validation(format!(
                "Infrastructure blueprint '{}' not found or provider disabled in catalog",
                args.blueprint
            ))
        })?;
    validate_blueprint_values(blueprint, &values)?;

    submit_proposal_request(
        &base_url,
        ctx,
        &args.tenant,
        &args.blueprint,
        "infra",
        &values,
        false,
    )
    .await
}

/// Executes `keel app install`: validates Helm/Kustomize values against catalog JSONSchema and opens a PR (`AGENTS.md` §3.6).
pub async fn execute_app_install(
    args: &AppInstallArgs,
    ctx: &CliContext,
) -> Result<ProposalCommandOutcome, CliError> {
    validate_dns_label(&args.app, "app")?;
    validate_dns_label(&args.tenant, "tenant")?;
    let values = load_values_file(&args.values_file).await?;
    let base_url = normalize_server_url(&args.server)?;

    // Fetch catalog to validate application JSONSchema and canary capability client-side
    let catalog = fetch_catalog(&base_url, ctx).await?;
    let app = catalog
        .applications
        .iter()
        .find(|a| a.metadata.id == args.app)
        .ok_or_else(|| {
            CliError::Validation(format!(
                "Catalog application '{}' not found in catalog",
                args.app
            ))
        })?;
    let _prepared = validate_application_values(app, &values, args.canary)?;

    submit_proposal_request(
        &base_url,
        ctx,
        &args.tenant,
        &args.app,
        "application",
        &values,
        args.canary,
    )
    .await
}

async fn fetch_catalog(base_url: &str, ctx: &CliContext) -> Result<CatalogApiResponse, CliError> {
    let endpoint = format!("{base_url}/api/v1/catalog");
    let resp = attach_auth(ctx.http.get(&endpoint), ctx, base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }
    Ok(resp.json().await?)
}

async fn submit_proposal_request(
    base_url: &str,
    ctx: &CliContext,
    tenant: &str,
    catalog_id: &str,
    kind: &str,
    values: &Value,
    canary: bool,
) -> Result<ProposalCommandOutcome, CliError> {
    let endpoint = format!("{base_url}/api/v1/proposals");
    let req_body = json!({
        "tenant": tenant,
        "catalogId": catalog_id,
        "kind": kind,
        "values": values,
        "canary": canary,
    });

    let resp = attach_auth(ctx.http.post(&endpoint).json(&req_body), ctx, base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let created: CreateProposalApiResponse = resp.json().await?;
    let mut output = String::new();
    output.push_str(&format!(
        "Opened Proposal PR #{}: {}\n",
        created.proposal.id, created.proposal.title
    ));
    output.push_str(&format!(
        "Branch:        {} -> {}\n",
        created.branch, created.proposal.target_branch
    ));
    output.push_str(&format!("PR URL:        {}\n", created.proposal.url));
    output.push_str(&format!(
        "Preview Stack: {}/{} (preview={})\n\n",
        created.preview_stack.metadata.namespace,
        created.preview_stack.metadata.name,
        created.preview_stack.is_preview()
    ));
    output.push_str(&format_preview_table(&created.report));

    Ok(ProposalCommandOutcome {
        proposal: created.proposal,
        branch: created.branch,
        preview_stack: created.preview_stack,
        report: created.report,
        output,
    })
}

// ============================================================================
// 8. `keel preview status` & Terminal Diff Table Formatter
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProposalPreviewApiResponse {
    pr_id: String,
    url: String,
    state: PullRequestState,
    source_branch: String,
    target_branch: String,
    report: PreviewReport,
}

/// Outcome of `keel preview status`.
#[derive(Debug, Clone)]
pub struct PreviewStatusOutcome {
    pub pr_id: String,
    pub state: PullRequestState,
    pub url: String,
    pub report: PreviewReport,
    pub watched_events: Vec<CrdWatchEvent>,
    pub output: String,
}

/// Renders a PKO v2 [`PreviewReport`] as an aligned terminal diff table (`+ Create`, `~ Update`, `- Delete`, `! Replace`, `! Drift`).
pub fn format_preview_table(report: &PreviewReport) -> String {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str(&format!(
        "Summary: +{} create, ~{} update, -{} delete, !{} replace, !{} drift, ={} unchanged (total: {})\n",
        s.create,
        s.update,
        s.delete,
        s.replace,
        s.drift,
        s.same,
        s.total()
    ));

    if s.has_destructive_changes() {
        out.push_str("WARNING: Preview contains destructive resource changes (delete/replace).\n");
    }
    if s.drift > 0 {
        out.push_str(&format!(
            "DRIFT ALERT: {} resource(s) have drifted from desired Git state.\n",
            s.drift
        ));
    }

    if report.resources.is_empty() {
        out.push_str("No resource changes planned.");
        return out;
    }

    let headers = ["OPERATION", "RESOURCE TYPE", "NAME", "CHANGED FIELDS"];
    let rows: Vec<[String; 4]> = report
        .resources
        .iter()
        .map(|r| {
            let fields = if r.changed_fields.is_empty() {
                "-".to_string()
            } else {
                r.changed_fields.join(", ")
            };
            [
                r.op.badge().to_string(),
                r.resource_type.clone(),
                r.name.clone(),
                fields,
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    out.push_str(&format_fixed_row(&headers, &widths));
    out.push('\n');
    let dividers: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    let div_refs: [&str; 4] = [&dividers[0], &dividers[1], &dividers[2], &dividers[3]];
    out.push_str(&format_fixed_row(&div_refs, &widths));

    for row in &rows {
        out.push('\n');
        let refs: [&str; 4] = [&row[0], &row[1], &row[2], &row[3]];
        out.push_str(&format_fixed_row(&refs, &widths));
    }

    out
}

/// Executes `keel preview status` fetching `/api/v1/proposals/{pr}/preview` and optionally watching SSE updates.
pub async fn execute_preview_status(
    args: &PreviewStatusArgs,
    ctx: &CliContext,
) -> Result<PreviewStatusOutcome, CliError> {
    validate_non_flag_input(&args.pr, "pr")?;
    if args.pr.contains('/') || args.pr.contains("..") {
        return Err(CliError::Validation(format!(
            "invalid pull request identifier '{}'",
            args.pr
        )));
    }

    let base_url = normalize_server_url(&args.server)?;
    let mut watched_events = Vec::new();

    if args.watch {
        watched_events = watch_preview_sse(&base_url, &args.pr, ctx).await?;
    }

    let preview_endpoint = format!("{base_url}/api/v1/proposals/{}/preview", args.pr.trim());
    let resp = attach_auth(ctx.http.get(&preview_endpoint), ctx, &base_url)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let preview: ProposalPreviewApiResponse = resp.json().await?;

    let mut output = String::new();
    output.push_str(&format!(
        "PR #{} ({}) — {} -> {}\n",
        preview.pr_id, preview.state, preview.source_branch, preview.target_branch
    ));
    output.push_str(&format!("Forge URL: {}\n", preview.url));

    if !watched_events.is_empty() {
        output.push_str("Watch Events:\n");
        for ev in &watched_events {
            output.push_str(&format!(
                "  [{}] {}/{} -> {:?}: {}\n",
                ev.kind, ev.namespace, ev.name, ev.status, ev.message
            ));
        }
    }
    output.push('\n');
    output.push_str(&format_preview_table(&preview.report));

    Ok(PreviewStatusOutcome {
        pr_id: preview.pr_id,
        state: preview.state,
        url: preview.url,
        report: preview.report,
        watched_events,
        output,
    })
}

async fn watch_preview_sse(
    base_url: &str,
    pr_id: &str,
    ctx: &CliContext,
) -> Result<Vec<CrdWatchEvent>, CliError> {
    let sse_url = format!("{base_url}/api/v1/events/stream");
    let mut resp = attach_auth(
        ctx.http
            .get(&sse_url)
            .query(&[("namespace", KEEL_PREVIEWS_NAMESPACE)]),
        ctx,
        base_url,
    )
    .send()
    .await?;

    if !resp.status().is_success() {
        return Err(extract_api_error(resp).await);
    }

    let mut collected = Vec::new();
    let mut buffer = String::new();
    let pr_marker = format!("pr-{pr_id}");

    while let Some(chunk) = resp.chunk().await? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(newline_idx) = buffer.find('\n') {
            let line = buffer[..newline_idx].trim().to_string();
            buffer.drain(..=newline_idx);

            let Some(json_str) = line.strip_prefix("data:") else {
                continue;
            };
            let json_trimmed = json_str.trim();
            if json_trimmed.is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<CrdWatchEvent>(json_trimmed) {
                let matches_pr =
                    ev.name.contains(&pr_marker) || ev.message.contains(&format!("#{pr_id}"));
                let is_terminal = matches!(
                    ev.status,
                    CrdReconcileStatus::Ready
                        | CrdReconcileStatus::Failed
                        | CrdReconcileStatus::Deleted
                );
                collected.push(ev);
                if matches_pr && is_terminal {
                    return Ok(collected);
                }
            }
        }
    }

    Ok(collected)
}

// ============================================================================
// Top-Level CLI Dispatcher
// ============================================================================

/// Dispatches any parsed [`Cli`] command and returns formatted stdout text.
pub async fn dispatch(cli: &Cli, ctx: &CliContext) -> Result<String, CliError> {
    match &cli.command {
        Commands::Bootstrap(b) => match &b.command {
            BootstrapCommands::Init(args) => Ok(execute_bootstrap_init(args).await?.output),
            BootstrapCommands::Recover(args) => Ok(execute_bootstrap_recover(args).await?.output),
        },
        Commands::Auth(a) => match &a.command {
            AuthCommands::Login(args) => {
                Ok(execute_auth_login(args, ctx).await?.output().to_string())
            }
        },
        Commands::Serve(args) => execute_serve(args).await,
        Commands::Infra(i) => match &i.command {
            InfraCommands::List(args) => execute_infra_list(args, ctx).await,
            InfraCommands::Provision(args) => Ok(execute_infra_provision(args, ctx).await?.output),
        },
        Commands::App(a) => match &a.command {
            AppCommands::Install(args) => Ok(execute_app_install(args, ctx).await?.output),
        },
        Commands::Preview(p) => match &p.command {
            PreviewCommands::Status(args) => Ok(execute_preview_status(args, ctx).await?.output),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use keel_core::{BootstrapTargetKind, parse_catalog_application, parse_catalog_blueprint};
    use keel_kube::WatchedCrdKind;
    use keel_pko::{ChangeOp, PreviewSummary, ResourceDiff};
    use tokio::sync::oneshot;

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("keel-cli-test-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct MockBootstrapTarget {
        kind: BootstrapTargetKind,
    }

    #[async_trait::async_trait]
    impl BootstrapTarget for MockBootstrapTarget {
        fn kind(&self) -> BootstrapTargetKind {
            self.kind
        }

        async fn provision_cluster(
            &self,
            cfg: &BootstrapConfig,
        ) -> Result<ClusterConnection, CoreError> {
            Ok(ClusterConnection {
                target: self.kind,
                cluster_name: cfg.cluster_name.clone(),
                kubeconfig_yaml: "apiVersion: v1\nkind: Config\n".to_string(),
                kubeconfig_path: cfg.kubeconfig.clone(),
                context: Some(format!("kind-{}", cfg.cluster_name)),
                server_endpoint: Some("https://127.0.0.1:6443".to_string()),
            })
        }

        async fn teardown(&self, _cfg: &BootstrapConfig) -> Result<(), CoreError> {
            Ok(())
        }
    }

    // Minimal async_trait re-export via keel-core/keel-kube isn't direct in keel-cli Cargo.toml,
    // wait: let's check if we need async_trait or can test with real KubeconfigBootstrapTarget!
    // Even better: KubeconfigBootstrapTarget IS in keel_core and implements BootstrapTarget with zero mocks needed!
    const VALID_KUBECONFIG_YAML: &str = r#"apiVersion: v1
kind: Config
current-context: seed-ctx
clusters:
  - name: seed-cluster
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: seed-ctx
    context:
      cluster: seed-cluster
      user: seed-admin
users:
  - name: seed-admin
    user:
      token: dummy
"#;

    const SAMPLE_APP_CATALOG_YAML: &str = r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: authentik
  name: Authentik Identity Provider
  category: identity
  runtime: helm
spec:
  upstream:
    repoURL: https://charts.goauthentik.io
    chart: authentik
    version: "2026.10.1"
  progressiveDelivery:
    flaggerSupported: true
    defaultCanary: false
  schema:
    type: object
    required: [domain, adminEmail]
    properties:
      domain:
        type: string
      adminEmail:
        type: string
      replicas:
        type: integer
        minimum: 1
        maximum: 10
"#;

    const SAMPLE_APP_NO_CANARY_YAML: &str = r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: redis
  name: Redis Cache
  category: database
  runtime: helm
spec:
  upstream:
    repoURL: https://charts.bitnami.com/bitnami
    chart: redis
    version: "20.1.0"
  progressiveDelivery:
    flaggerSupported: false
    defaultCanary: false
  schema:
    type: object
    required: [replicas]
    properties:
      replicas:
        type: integer
"#;

    const SAMPLE_BLUEPRINT_CATALOG_YAML: &str = r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: postgres-managed
  name: Managed PostgreSQL
  category: database
  provider: aws
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: true
  schema:
    type: object
    required: [instanceSize, storageGb]
    properties:
      instanceSize:
        type: string
      storageGb:
        type: integer
        minimum: 10
"#;

    #[test]
    fn kyverno_policy_generator_respects_docker_socket_exception_flag() {
        let prod_gen = KyvernoPolicyGenerator::with_default_providers(false);
        let prod_bundle = prod_gen.generate().unwrap();
        assert_eq!(prod_bundle.cluster_policies.len(), 3);
        assert!(prod_bundle.policy_exception.is_none());

        let kind_dev_gen = KyvernoPolicyGenerator::with_default_providers(true);
        let kind_dev_bundle = kind_dev_gen.generate().unwrap();
        assert_eq!(kind_dev_bundle.cluster_policies.len(), 3);
        assert!(kind_dev_bundle.policy_exception.is_some());
        let yaml = kind_dev_bundle.to_multi_doc_yaml().unwrap();
        assert!(yaml.contains("local-kind-docker-socket-exception"));
    }

    #[tokio::test]
    async fn bootstrap_init_kubeconfig_and_seed_git_repo_end_to_end() {
        let tmp = TempTestDir::new("bootstrap-init");
        let kubeconfig_path = tmp.path().join("kubeconfig.yaml");
        tokio::fs::write(&kubeconfig_path, VALID_KUBECONFIG_YAML)
            .await
            .unwrap();
        let repo_dir = tmp.path().join("seed-gitops");

        let cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "init",
            "--target",
            "kubeconfig",
            "--cluster-name",
            "keel-seed",
            "--kubeconfig",
            kubeconfig_path.to_str().unwrap(),
            "--context",
            "seed-ctx",
            "--with-forge",
            "--skip-core-install",
            "--repo-dir",
            repo_dir.to_str().unwrap(),
        ])
        .unwrap();

        let ctx = CliContext::with_credentials_path(tmp.path().join("creds.json"));
        let output = dispatch(&cli, &ctx).await.unwrap();

        assert!(output.contains("Stage 0 Seed Control-Plane Bootstrap Complete"));
        assert!(output.contains("Target:          kubeconfig"));
        assert!(output.contains("Cluster:         keel-seed"));
        assert!(output.contains("interval=1m"));
        assert!(output.contains("docker-socket exception: false"));
        assert!(output.contains("protect=true"));

        // Verify Git seed repository files and `main` branch commit
        assert!(repo_dir.join(".keel/config.yaml").exists());
        assert!(repo_dir.join("providers/registry.yaml").exists());
        assert!(repo_dir.join(".sops.yaml").exists());
        assert!(
            repo_dir
                .join("clusters/management/flux-system/git-repository.yaml")
                .exists()
        );
        assert!(
            repo_dir
                .join("clusters/management/core-addons/kyverno-policies.yaml")
                .exists()
        );
        assert!(
            repo_dir
                .join("clusters/management/management-stack.yaml")
                .exists()
        );
        assert!(repo_dir.join("tenants/default/rbac.yaml").exists());
        assert!(
            repo_dir
                .join("catalog/infra/kind-cluster/catalog.yaml")
                .exists()
        );
        assert!(repo_dir.join("catalog/apps/podinfo/catalog.yaml").exists());

        let git = GitRunner::new();
        assert!(git.ref_exists(&repo_dir, "refs/heads/main").await.unwrap());
    }

    #[tokio::test]
    async fn bootstrap_init_kind_with_docker_socket_exception_and_guardrails() {
        // 1. Non-kind target with `--local-dev-docker-socket-exception` MUST be rejected
        let bad_args = BootstrapInitArgs {
            target: BootstrapTargetKind::Aws,
            cluster_name: "prod-aws".to_string(),
            kubeconfig: None,
            context: None,
            proxmox_url: None,
            region: Some("us-east-1".to_string()),
            distro: None,
            with_forge: false,
            with_oidc: false,
            local_dev_docker_socket_exception: true,
            repo_dir: None,
            skip_core_install: true,
            git_url: None,
            forge_type: None,
            forge_api_url: None,
            git_token: None,
        };
        let err = execute_bootstrap_init(&bad_args).await.unwrap_err();
        assert!(err.to_string().contains("only permitted for the 'kind'"));

        // 2. Cloud target resolves through `bootstrap_target_factory` without Proxmox hardcoding
        let cloud_args = BootstrapInitArgs {
            local_dev_docker_socket_exception: false,
            ..bad_args.clone()
        };
        let cloud_err = execute_bootstrap_init(&cloud_args).await.unwrap_err();
        assert!(cloud_err.to_string().contains("Stage 0 Pulumi runner"));

        // 3. `kind` target with `--local-dev-docker-socket-exception` + `--with-oidc` synthesizes PolicyException
        let kind_args = BootstrapInitArgs {
            target: BootstrapTargetKind::Kind,
            cluster_name: "local-kind".to_string(),
            kubeconfig: None,
            context: None,
            proxmox_url: None,
            region: None,
            distro: None,
            with_forge: false,
            with_oidc: true,
            local_dev_docker_socket_exception: true,
            repo_dir: None,
            skip_core_install: true,
            git_url: None,
            forge_type: None,
            forge_api_url: None,
            git_token: None,
        };
        let mock_kind = MockBootstrapTarget {
            kind: BootstrapTargetKind::Kind,
        };
        let outcome = execute_bootstrap_init_with_target(&kind_args, &mock_kind)
            .await
            .unwrap();
        assert!(outcome.artifacts.kyverno_bundle.policy_exception.is_some());
        assert_eq!(
            outcome.artifacts.keel_config.auth_phase,
            AuthPhase::OidcActive
        );
        assert_eq!(outcome.artifacts.bootstrap_token.len(), 64);
        assert!(!format!("{:?}", outcome.artifacts).contains(&outcome.artifacts.bootstrap_token));
    }

    #[tokio::test]
    async fn bootstrap_recover_validates_inputs_and_builds_protected_plan() {
        let tmp = TempTestDir::new("recover");
        let ctx = CliContext::with_credentials_path(tmp.path().join("creds.json"));

        let cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "recover",
            "--s3-endpoint",
            "https://s3.garage.internal:3900",
            "--bucket",
            "pulumi-state",
            "--stack",
            "keel/management-cluster-core/prod",
        ])
        .unwrap();

        let out = dispatch(&cli, &ctx).await.unwrap();
        assert!(out.contains("Break-Glass Control-Plane Recovery Plan"));
        assert!(out.contains(
            "s3://pulumi-state?endpoint=https://s3.garage.internal:3900&s3ForcePathStyle=true"
        ));
        assert!(out.contains("protect=true"));
        assert!(out.contains("pulumi refresh --yes --stack keel/management-cluster-core/prod"));

        // Reject flag injection in stack or invalid URL
        let bad_stack = BootstrapRecoverArgs {
            s3_endpoint: "https://s3.local".to_string(),
            bucket: "pulumi-state".to_string(),
            stack: "--malicious-flag".to_string(),
        };
        assert!(execute_bootstrap_recover(&bad_stack).await.is_err());

        let bad_url = BootstrapRecoverArgs {
            s3_endpoint: "ftp://s3.local".to_string(),
            bucket: "pulumi-state".to_string(),
            stack: "management-cluster-core".to_string(),
        };
        assert!(execute_bootstrap_recover(&bad_url).await.is_err());
    }

    #[test]
    fn preview_diff_table_and_stack_inventory_formatting() {
        let empty = PreviewReport::default();
        let empty_out = format_preview_table(&empty);
        assert!(empty_out.contains("No resource changes planned."));

        let populated = PreviewReport {
            resources: vec![
                ResourceDiff {
                    urn: "urn:pulumi:pr-7::acme::kubernetes:apps/v1:Deployment::web".to_string(),
                    resource_type: "kubernetes:apps/v1:Deployment".to_string(),
                    name: "web".to_string(),
                    op: ChangeOp::Create,
                    changed_fields: vec!["spec".to_string()],
                },
                ResourceDiff {
                    urn: "urn:pulumi:pr-7::acme::aws:rds:Instance::db".to_string(),
                    resource_type: "aws:rds:Instance".to_string(),
                    name: "db".to_string(),
                    op: ChangeOp::Replace,
                    changed_fields: vec![
                        "engineVersion".to_string(),
                        "allocatedStorage".to_string(),
                    ],
                },
                ResourceDiff {
                    urn: "urn:pulumi:pr-7::acme::kubernetes:core/v1:ConfigMap::cfg".to_string(),
                    resource_type: "kubernetes:core/v1:ConfigMap".to_string(),
                    name: "cfg".to_string(),
                    op: ChangeOp::Drift,
                    changed_fields: vec![],
                },
            ],
            summary: PreviewSummary {
                create: 1,
                update: 0,
                delete: 0,
                replace: 1,
                same: 2,
                drift: 1,
            },
        };

        let table = format_preview_table(&populated);
        assert!(table.contains("+1 create"));
        assert!(table.contains("!1 replace"));
        assert!(table.contains("!1 drift"));
        assert!(table.contains("WARNING: Preview contains destructive"));
        assert!(table.contains("DRIFT ALERT: 1 resource(s)"));
        assert!(table.contains("+ Create"));
        assert!(table.contains("! Replace"));
        assert!(table.contains("engineVersion, allocatedStorage"));

        let stacks = vec![StackSummary {
            name: "postgres-main".to_string(),
            namespace: "tenant-acme".to_string(),
            tenant: Some("acme".to_string()),
            stack: "keel/acme/postgres-main".to_string(),
            provider: Some("aws".to_string()),
            preview: false,
            protect: true,
            bundle: None,
            bundle_item: None,
            status: "Ready".to_string(),
        }];

        let stack_table = format_stacks_output(&stacks, OutputFormat::Table).unwrap();
        assert!(stack_table.contains("NAMESPACE"));
        assert!(stack_table.contains("tenant-acme"));
        assert!(stack_table.contains("postgres-main"));
        assert!(stack_table.contains("aws"));

        let stack_json = format_stacks_output(&stacks, OutputFormat::Json).unwrap();
        assert!(stack_json.contains("\"postgres-main\""));
        assert_eq!(
            format_stacks_output(&[], OutputFormat::Table).unwrap(),
            "No PKO stacks found."
        );
    }

    #[test]
    fn catalog_validation_helpers_enforce_schema_and_canary_support() {
        let app = parse_catalog_application(SAMPLE_APP_CATALOG_YAML).unwrap();
        let valid_vals = json!({
            "domain": "auth.acme.io",
            "adminEmail": "ops@acme.io",
            "replicas": 3
        });
        assert!(validate_application_values(&app, &valid_vals, true).is_ok());

        let invalid_vals = json!({ "domain": "auth.acme.io" });
        assert!(validate_application_values(&app, &invalid_vals, false).is_err());

        let no_canary_app = parse_catalog_application(SAMPLE_APP_NO_CANARY_YAML).unwrap();
        let redis_vals = json!({ "replicas": 2 });
        let canary_err =
            validate_application_values(&no_canary_app, &redis_vals, true).unwrap_err();
        assert!(canary_err.to_string().contains("flaggerSupported=false"));

        let bp = parse_catalog_blueprint(SAMPLE_BLUEPRINT_CATALOG_YAML).unwrap();
        assert!(
            validate_blueprint_values(
                &bp,
                &json!({ "instanceSize": "db.t4g.medium", "storageGb": 20 })
            )
            .is_ok()
        );
        assert!(
            validate_blueprint_values(
                &bp,
                &json!({ "instanceSize": "db.t4g.medium", "storageGb": 2 })
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn full_cli_server_integration_auth_infra_app_preview_watch_and_serve() {
        let tmp = TempTestDir::new("e2e-cli");
        let work_dir = tmp.path().join("work");
        let bare_dir = tmp.path().join("work.git");

        // Build server state using `ServeArgs`
        let serve_args = ServeArgs {
            bind: "127.0.0.1:0".to_string(),
            repo_dir: Some(work_dir.clone()),
            bare_repo: Some(bare_dir.clone()),
            config: None,
            git_token: None,
        };
        let (state, bootstrap_token) = build_serve_state(&serve_args).await.unwrap();
        let raw_token = bootstrap_token.expect("Stage 0 token should be generated");

        // Commit enabled provider registry + catalog items onto `main`
        let mut reg = ProviderRegistry::default_day1();
        assert!(reg.set_enabled("aws", true));
        let reg_dir = work_dir.join("providers");
        let app_cat_dir = work_dir.join("catalog/apps/authentik");
        let bp_cat_dir = work_dir.join("catalog/infra/postgres-managed");
        tokio::fs::create_dir_all(&reg_dir).await.unwrap();
        tokio::fs::create_dir_all(&app_cat_dir).await.unwrap();
        tokio::fs::create_dir_all(&bp_cat_dir).await.unwrap();
        tokio::fs::write(reg_dir.join("registry.yaml"), reg.to_yaml().unwrap())
            .await
            .unwrap();
        tokio::fs::write(app_cat_dir.join("catalog.yaml"), SAMPLE_APP_CATALOG_YAML)
            .await
            .unwrap();
        tokio::fs::write(
            bp_cat_dir.join("catalog.yaml"),
            SAMPLE_BLUEPRINT_CATALOG_YAML,
        )
        .await
        .unwrap();

        let git = GitRunner::new();
        git.add(
            &work_dir,
            [
                "providers/registry.yaml",
                "catalog/apps/authentik/catalog.yaml",
                "catalog/infra/postgres-managed/catalog.yaml",
            ],
        )
        .await
        .unwrap();
        git.commit(
            &work_dir,
            "feat(catalog): add authentik app and postgres blueprint",
            &CommitAuthor::new("Test Admin", "admin@keel.io"),
        )
        .await
        .unwrap();
        git.push(&work_dir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        let telemetry = state.telemetry.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        let server_url = format!("http://{bound_addr}");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server_task = tokio::spawn(async move {
            axum::serve(listener, build_router(state))
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let ctx = CliContext::with_credentials_path(tmp.path().join("cli-creds.json"));

        // 1. OIDC login while OIDC is unconfigured returns HTTP 503 error
        let oidc_unconfigured_cli =
            Cli::try_parse_from(["keel", "auth", "login", "--server", &server_url]).unwrap();
        let unconf_err = dispatch(&oidc_unconfigured_cli, &ctx).await.unwrap_err();
        assert!(unconf_err.to_string().contains("503"));

        // 2. Stage 0 bootstrap login succeeds and persists session token
        let login_cli = Cli::try_parse_from([
            "keel",
            "auth",
            "login",
            "--server",
            &server_url,
            "--bootstrap-token",
            &raw_token,
        ])
        .unwrap();
        let login_out = dispatch(&login_cli, &ctx).await.unwrap();
        assert!(login_out.contains("bootstrap-admin"));

        // 3. `keel infra provision` opens PR #1 and prints PKO preview diff table
        let db_values_file = tmp.path().join("db-values.yaml");
        tokio::fs::write(
            &db_values_file,
            "instanceSize: db.t4g.medium\nstorageGb: 50\n",
        )
        .await
        .unwrap();

        let prov_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "provision",
            "--blueprint",
            "postgres-managed",
            "--tenant",
            "acme",
            "--values-file",
            db_values_file.to_str().unwrap(),
            "--server",
            &server_url,
        ])
        .unwrap();
        let prov_out = dispatch(&prov_cli, &ctx).await.unwrap();
        assert!(prov_out.contains("Opened Proposal PR #1"));
        assert!(prov_out.contains("+ Create"));

        // 4. `keel app install --canary` opens PR #2
        let app_values_file = tmp.path().join("app-values.yaml");
        tokio::fs::write(
            &app_values_file,
            "domain: auth.acme.io\nadminEmail: admin@acme.io\nreplicas: 2\n",
        )
        .await
        .unwrap();

        let app_cli = Cli::try_parse_from([
            "keel",
            "app",
            "install",
            "--app",
            "authentik",
            "--tenant",
            "acme",
            "--values-file",
            app_values_file.to_str().unwrap(),
            "--canary",
            "--server",
            &server_url,
        ])
        .unwrap();
        let app_out = dispatch(&app_cli, &ctx).await.unwrap();
        assert!(app_out.contains("Opened Proposal PR #2"));
        assert!(app_out.contains("helm.toolkit.fluxcd.io/v2:HelmRelease"));

        // 5. `keel preview status --pr 1` without watch
        let prev_cli = Cli::try_parse_from([
            "keel",
            "preview",
            "status",
            "--pr",
            "1",
            "--server",
            &server_url,
        ])
        .unwrap();
        let prev_out = dispatch(&prev_cli, &ctx).await.unwrap();
        assert!(prev_out.contains("PR #1 (open)"));
        assert!(prev_out.contains("+1 create"));

        // 6. `keel preview status --pr 1 --watch` streams SSE event and terminates on Ready
        let telemetry_pub = telemetry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            telemetry_pub.publish(CrdWatchEvent::new(
                WatchedCrdKind::Stack,
                KEEL_PREVIEWS_NAMESPACE,
                "preview-pr-1-acme",
                CrdReconcileStatus::Ready,
                "Preview finished for PR #1",
                None,
            ));
        });

        let watch_cli = Cli::try_parse_from([
            "keel",
            "preview",
            "status",
            "--pr",
            "1",
            "--watch",
            "--server",
            &server_url,
        ])
        .unwrap();
        let watch_out = dispatch(&watch_cli, &ctx).await.unwrap();
        assert!(watch_out.contains("Watch Events:"));
        assert!(watch_out.contains("Preview finished for PR #1"));

        // 7. Merge PR #1 and verify `keel infra list` shows the merged stack in Table & JSON modes
        let merge_resp = attach_auth(
            ctx.http
                .post(format!("{server_url}/api/v1/proposals/1/merge")),
            &ctx,
            &server_url,
        )
        .send()
        .await
        .unwrap();
        assert!(merge_resp.status().is_success());
        // Check out latest `main` in work_dir so RepoClusterStateProvider sees merged files
        git.fetch(&work_dir, "origin").await.unwrap();
        git.fast_forward_merge(&work_dir, "main", "origin/main")
            .await
            .unwrap();

        let list_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "list",
            "--tenant",
            "acme",
            "--output",
            "table",
            "--server",
            &server_url,
        ])
        .unwrap();
        let list_out = dispatch(&list_cli, &ctx).await.unwrap();
        assert!(list_out.contains("postgres-managed"));

        let list_json_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "list",
            "--output",
            "json",
            "--server",
            &server_url,
        ])
        .unwrap();
        let list_json_out = dispatch(&list_json_cli, &ctx).await.unwrap();
        assert!(list_json_out.contains("\"postgres-managed\""));

        let _ = shutdown_tx.send(());
        server_task.await.unwrap();

        // 8. Also verify `execute_serve_until` ephemeral startup + immediate graceful shutdown
        let (ephemeral_tx, ephemeral_rx) = oneshot::channel::<()>();
        let _ = ephemeral_tx.send(());
        let bound_ephemeral = execute_serve_until(
            &ServeArgs {
                bind: "127.0.0.1:0".to_string(),
                repo_dir: None,
                bare_repo: None,
                config: None,
                git_token: None,
            },
            async move {
                let _ = ephemeral_rx.await;
            },
        )
        .await
        .unwrap();
        assert!(bound_ephemeral.port() > 0);
    }

    #[tokio::test]
    async fn oidc_login_redirect_flow_returns_pkce_authorize_url() {
        let tmp = TempTestDir::new("oidc-redirect");
        let (mut state, _) = build_serve_state(&ServeArgs {
            bind: "127.0.0.1:0".to_string(),
            repo_dir: None,
            bare_repo: None,
            config: None,
            git_token: None,
        })
        .await
        .unwrap();
        state = state.with_oidc_config(Some(OidcConfig {
            enabled: true,
            issuer_url: "https://idp.acme.io".to_string(),
            client_id: "keel-cli".to_string(),
            redirect_url: "http://127.0.0.1:8080/api/v1/auth/callback".to_string(),
            scopes: vec!["openid".to_string(), "profile".to_string()],
        }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, build_router(state))
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });

        let ctx = CliContext::with_credentials_path(tmp.path().join("creds.json"));
        let args = AuthLoginArgs {
            server: format!("http://{addr}"),
            bootstrap_token: None,
        };
        let outcome = execute_auth_login(&args, &ctx).await.unwrap();
        match outcome {
            AuthLoginOutcome::OidcRedirect {
                authorize_url,
                output,
            } => {
                assert!(
                    authorize_url.starts_with("https://idp.acme.io/protocol/openid-connect/auth")
                );
                assert!(authorize_url.contains("code_challenge_method=S256"));
                assert!(output.contains("OIDC PKCE URL"));
            }
            other => panic!("expected OidcRedirect, got {other:?}"),
        }

        let _ = tx.send(());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn validation_guards_and_secret_redaction_edge_cases() {
        let tmp = TempTestDir::new("edge-cases");

        // CliContext & PkceChallenge Debug secret redaction + session override
        let default_ctx = CliContext::default();
        let overridden_ctx = default_ctx.with_session_token("super-secret-session");
        let ctx_dbg = format!("{overridden_ctx:?}");
        assert!(!ctx_dbg.contains("super-secret-session"));
        assert!(ctx_dbg.contains("[REDACTED]"));
        assert_eq!(
            overridden_ctx.load_session_token("http://127.0.0.1:8080"),
            Some("super-secret-session".to_string())
        );

        let pkce = generate_pkce_pair();
        let pkce_dbg = format!("{pkce:?}");
        assert!(!pkce_dbg.contains(&pkce.code_verifier));
        assert!(pkce_dbg.contains("[REDACTED]"));

        // load_values_file: missing, empty, and non-mapping YAML
        assert!(
            load_values_file(&tmp.path().join("missing.yaml"))
                .await
                .is_err()
        );

        let empty_path = tmp.path().join("empty.yaml");
        tokio::fs::write(&empty_path, "   \n").await.unwrap();
        assert_eq!(load_values_file(&empty_path).await.unwrap(), json!({}));

        let list_path = tmp.path().join("array.yaml");
        tokio::fs::write(&list_path, "- one\n- two\n")
            .await
            .unwrap();
        assert!(load_values_file(&list_path).await.is_err());

        // Invalid bind address in `build_serve_state`
        assert!(
            build_serve_state(&ServeArgs {
                bind: "invalid-bind-without-port".to_string(),
                repo_dir: None,
                bare_repo: None,
                config: None,
                git_token: None,
            })
            .await
            .is_err()
        );

        // Path traversal / flag injection rejection in `execute_preview_status` & `execute_auth_login`
        let err_pr = execute_preview_status(
            &PreviewStatusArgs {
                pr: "../etc".to_string(),
                watch: false,
                server: "http://127.0.0.1:8080".to_string(),
            },
            &overridden_ctx,
        )
        .await
        .unwrap_err();
        assert!(
            err_pr
                .to_string()
                .contains("invalid pull request identifier")
        );

        let err_empty_tok = execute_auth_login(
            &AuthLoginArgs {
                server: "http://127.0.0.1:8080".to_string(),
                bootstrap_token: Some("   ".to_string()),
            },
            &overridden_ctx,
        )
        .await
        .unwrap_err();
        assert!(err_empty_tok.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn relative_repo_dir_resolves_to_absolute_bare_origin_and_repairs_existing_remote() {
        let abs = absolutize_path(Path::new("./keel-gitops")).unwrap();
        assert!(abs.is_absolute());
        assert_eq!(
            abs.file_name().and_then(|n| n.to_str()),
            Some("keel-gitops")
        );
        assert!(is_relative_local_remote("./keel-gitops.git"));
        assert!(!is_relative_local_remote("/tmp/keel-gitops.git"));
        assert!(!is_relative_local_remote(
            "https://forge.local/org/repo.git"
        ));
        assert!(!is_relative_local_remote("git@github.com:org/repo.git"));

        // Simulate an existing repo whose `origin` was previously set to a relative path
        let tmp = TempTestDir::new("repair-origin");
        let work_dir = tmp.path().join("keel-gitops");
        let bare_dir = tmp.path().join("keel-gitops.git");
        let git = GitRunner::new();
        git.init_bare(&bare_dir).await.unwrap();
        git.clone(bare_dir.to_str().unwrap(), &work_dir)
            .await
            .unwrap();
        git.set_remote_url(&work_dir, "origin", "./keel-gitops.git")
            .await
            .unwrap();
        assert_eq!(
            git.get_remote_url(&work_dir, "origin").await.unwrap(),
            "./keel-gitops.git"
        );

        let (_state, _token) = build_serve_state(&ServeArgs {
            bind: "127.0.0.1:0".to_string(),
            repo_dir: Some(work_dir.clone()),
            bare_repo: None,
            config: None,
            git_token: None,
        })
        .await
        .unwrap();

        let repaired_origin = git.get_remote_url(&work_dir, "origin").await.unwrap();
        assert_eq!(repaired_origin, bare_dir.to_string_lossy());
    }

    #[tokio::test]
    async fn external_git_url_infers_forge_and_synthesizes_secret_ref() {
        let (gh_kind, gh_api) =
            infer_forge_config("https://github.com/acme/infra-gitops.git", None, None);
        assert_eq!(gh_kind, ForgeKind::GitHub);
        assert_eq!(gh_api.as_deref(), Some("https://api.github.com"));

        let (gl_kind, gl_api) =
            infer_forge_config("https://gitlab.example.com/team/infra.git", None, None);
        assert_eq!(gl_kind, ForgeKind::GitLab);
        assert_eq!(gl_api.as_deref(), Some("https://gitlab.example.com"));

        let tmp = TempTestDir::new("ext-git");
        let repo_dir = tmp.path().join("ext-repo");
        let args = BootstrapInitArgs {
            target: BootstrapTargetKind::Kind,
            cluster_name: "ext-kind".to_string(),
            kubeconfig: None,
            context: None,
            proxmox_url: None,
            region: None,
            distro: None,
            with_forge: false,
            with_oidc: false,
            local_dev_docker_socket_exception: true,
            repo_dir: Some(repo_dir.clone()),
            skip_core_install: true,
            git_url: Some("https://github.com/acme/infra-gitops.git".to_string()),
            forge_type: None,
            forge_api_url: None,
            git_token: Some("ghp_test_token_123".to_string()),
        };
        let mock_kind = MockBootstrapTarget {
            kind: BootstrapTargetKind::Kind,
        };
        let outcome = execute_bootstrap_init_with_target(&args, &mock_kind)
            .await
            .unwrap();
        assert_eq!(
            outcome.artifacts.keel_config.forge.provider,
            ForgeKind::GitHub
        );
        assert_eq!(
            outcome.artifacts.keel_config.forge.api_url.as_deref(),
            Some("https://api.github.com")
        );
        assert_eq!(
            outcome
                .artifacts
                .flux_git_repository
                .spec
                .secret_ref
                .as_ref()
                .map(|r| r.name.as_str()),
            Some("git-credentials")
        );
        let origin = GitRunner::new()
            .get_remote_url(&repo_dir, "origin")
            .await
            .unwrap();
        assert_eq!(origin, "https://github.com/acme/infra-gitops.git");
    }
}
