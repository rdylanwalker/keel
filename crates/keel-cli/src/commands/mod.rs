//! Command execution handlers for `keel` (`SDD.md` §2.1, §2.2, §4.2, §6.1, §7.2).

pub mod auth;
pub mod bootstrap;
pub mod recover;
pub mod serve;
pub mod workloads;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use keel_core::CoreError;
use keel_git::{ForgeError, GitError};
use keel_kube::KubeError;
use keel_pko::PkoError;
use keel_server::SESSION_COOKIE_NAME;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, COOKIE};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{
    AppCommands, AuthCommands, BootstrapCommands, Cli, Commands, InfraCommands, PreviewCommands,
};

pub use auth::{AuthLoginOutcome, PkceChallenge, execute_auth_login, generate_pkce_pair};
pub use bootstrap::{
    BootstrapInitOutcome, KyvernoPolicyGenerator, RauthySeedConfig, SeedControlPlaneArtifacts,
    SeedRepoInitResult, default_bare_repo_for_workdir, execute_bootstrap_init,
    execute_bootstrap_init_with_target, initialize_seed_git_repo, synthesize_seed_control_plane,
};
pub use recover::{BootstrapRecoverOutcome, build_pulumi_recover_argv, execute_bootstrap_recover};
pub use serve::{build_serve_state, execute_serve, execute_serve_until};
pub use workloads::{
    PreviewStatusOutcome, ProposalCommandOutcome, build_ssh_argv, execute_app_install,
    execute_infra_force_delete, execute_infra_list, execute_infra_provision, execute_infra_ssh,
    execute_infra_unlock, execute_preview_status, format_preview_table, format_ssh_info_output,
    format_stacks_output, load_values_file, validate_application_values, validate_blueprint_values,
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

/// Stored CLI session map keyed by normalized server URL (never logged).
#[derive(Default, Serialize, Deserialize)]
struct StoredCredentialsFile {
    #[serde(default)]
    sessions: BTreeMap<String, String>,
}

/// Runtime context for CLI command execution (HTTP client + session credential store).
#[derive(Clone)]
pub struct CliContext {
    pub(crate) http: Client,
    pub(crate) no_redirect_http: Client,
    pub(crate) stream_http: Client,
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
        let stream_http = Client::builder()
            .user_agent("keel-cli/0.1.0")
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http,
            no_redirect_http,
            stream_http,
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

/// Resolves a path to an absolute, lexically normalized [`PathBuf`] (without requiring it to exist yet).
pub(crate) fn absolutize_path(path: &Path) -> Result<PathBuf, CliError> {
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

pub(crate) fn is_relative_local_remote(url: &str) -> bool {
    !url.is_empty() && !url.starts_with('/') && !url.contains("://") && !url.contains(':')
}

pub(crate) fn validate_non_flag_input(value: &str, field: &str) -> Result<(), CliError> {
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

pub(crate) async fn extract_api_error(resp: reqwest::Response) -> CliError {
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

pub(crate) fn attach_auth(
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
            InfraCommands::Ssh(args) => execute_infra_ssh(args, ctx).await,
            InfraCommands::ForceDelete(args) => execute_infra_force_delete(args, ctx).await,
            InfraCommands::Unlock(args) => execute_infra_unlock(args, ctx).await,
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
    use super::bootstrap::{infer_forge_config, render_default_tenant_git_repository_yaml};
    use super::workloads::resolve_local_fallback_ssh_info;
    use super::*;
    use crate::cli::{
        AuthLoginArgs, BootstrapInitArgs, BootstrapRecoverArgs, InfraSshArgs, OutputFormat,
        PreviewStatusArgs, ServeArgs,
    };
    use clap::Parser;
    use keel_core::{
        AuthPhase, BootstrapConfig, BootstrapTarget, BootstrapTargetKind, ClusterConnection,
        ForgeKind, KEEL_PREVIEWS_NAMESPACE, OidcConfig, ProviderRegistry, bootstrap_target_factory,
        parse_catalog_application, parse_catalog_blueprint,
    };
    use keel_git::{CommitAuthor, GitRunner};
    use keel_kube::{
        CrdReconcileStatus, CrdWatchEvent, S3_CREDENTIALS_SECRET_NAME, WatchedCrdKind,
    };
    use keel_pko::{
        ChangeOp, CrObjectMeta, DEFAULT_FLUX_POLL_INTERVAL, FLUX_GIT_REPO_API_VERSION,
        FLUX_GIT_REPO_KIND, FluxGitRepository, FluxGitRepositoryRef, FluxGitRepositorySpec,
        IN_CLUSTER_S3_BACKEND_URL, LocalObjectReference, PreviewReport, PreviewSummary,
        ResourceDiff,
    };
    use keel_server::{StackSummary, build_router};
    use serde_json::json;
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
                .join("clusters/management/core-addons/s3-seaweedfs.yaml")
                .exists()
        );
        assert!(
            repo_dir
                .join("catalog/core/s3-state-backend/catalog.yaml")
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
        assert!(repo_dir.join("catalog/infra/aws-vm/catalog.yaml").exists());
        assert!(repo_dir.join("catalog/infra/aws-vm/Pulumi.yaml").exists());
        assert!(
            repo_dir
                .join("catalog/infra/azure-vm/catalog.yaml")
                .exists()
        );
        assert!(repo_dir.join("catalog/infra/azure-vm/Pulumi.yaml").exists());
        assert!(repo_dir.join("catalog/infra/gcp-vm/catalog.yaml").exists());
        assert!(repo_dir.join("catalog/infra/gcp-vm/Pulumi.yaml").exists());
        assert!(repo_dir.join("catalog/apps/podinfo/catalog.yaml").exists());

        let git = GitRunner::new();
        assert!(git.ref_exists(&repo_dir, "refs/heads/main").await.unwrap());
    }

    #[tokio::test]
    async fn rauthy_forgejo_client_is_confidential_with_s256_pkce() {
        let args = BootstrapInitArgs {
            target: BootstrapTargetKind::Kind,
            cluster_name: "test-oidc".to_string(),
            kubeconfig: None,
            context: None,
            proxmox_url: None,
            region: None,
            distro: None,
            with_forge: true,
            oidc_issuer: None,
            oidc_provider: crate::cli::OidcProviderArg::Rauthy,
            oidc_client_id: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: false,
            repo_dir: None,
            skip_core_install: true,
            git_url: None,
            forge_type: None,
            forge_api_url: None,
            git_token: None,
            pulumi_program_dir: None,
        };
        let cfg = args.to_bootstrap_config();
        let artifacts = synthesize_seed_control_plane(&cfg, None).await.unwrap();
        let rauthy = artifacts.rauthy_config.expect("rauthy config");
        let clients: serde_json::Value = serde_json::from_str(&rauthy.clients_json).unwrap();
        let clients_arr = clients.as_array().unwrap();
        let forgejo = clients_arr
            .iter()
            .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("forgejo"))
            .expect("forgejo client");
        assert_eq!(
            forgejo.get("confidential").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            forgejo.get("challenges"),
            Some(&serde_json::json!(["S256"]))
        );
        assert_eq!(
            forgejo
                .get("secret")
                .and_then(|s| s.get("Plain"))
                .and_then(|v| v.as_str())
                .map(str::len),
            Some(64)
        );
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
            oidc_issuer: None,
            oidc_provider: crate::cli::OidcProviderArg::Rauthy,
            oidc_client_id: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: true,
            repo_dir: None,
            skip_core_install: true,
            git_url: None,
            forge_type: None,
            forge_api_url: None,
            git_token: None,
            pulumi_program_dir: None,
        };
        let err = execute_bootstrap_init(&bad_args).await.unwrap_err();
        assert!(err.to_string().contains("only permitted for the 'kind'"));

        // 2. Cloud target resolves through `bootstrap_target_factory` without Proxmox hardcoding
        let cloud_args = BootstrapInitArgs {
            local_dev_docker_socket_exception: false,
            ..bad_args.clone()
        };
        let cloud_cfg = cloud_args.to_bootstrap_config();
        cloud_cfg.validate().unwrap();
        let driver = bootstrap_target_factory(BootstrapTargetKind::Aws).unwrap();
        assert_eq!(driver.kind(), BootstrapTargetKind::Aws);

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
            oidc_issuer: None,
            oidc_provider: crate::cli::OidcProviderArg::Rauthy,
            oidc_client_id: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: true,
            repo_dir: None,
            skip_core_install: true,
            git_url: None,
            forge_type: None,
            forge_api_url: None,
            git_token: None,
            pulumi_program_dir: None,
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
        assert_eq!(outcome.artifacts.admin_username, "admin@keel.local");
        assert_eq!(outcome.artifacts.admin_password.len(), 20);
        assert!(outcome.artifacts.admin_password_generated);
        assert!(
            outcome
                .artifacts
                .rauthy_config
                .as_ref()
                .is_some_and(|r| r.config_toml.contains("idp.keel.local:8888")
                    && r.groups_json.contains("keel-admins"))
        );
        assert!(!format!("{:?}", outcome.artifacts).contains(&outcome.artifacts.admin_password));
        assert_eq!(
            outcome.artifacts.management_stack.spec.backend.as_deref(),
            Some(IN_CLUSTER_S3_BACKEND_URL)
        );
        assert_eq!(
            outcome
                .artifacts
                .management_stack
                .spec
                .env_refs
                .get("AWS_ACCESS_KEY_ID")
                .map(|r| (r.secret.name.as_str(), r.secret.key.as_str())),
            Some((S3_CREDENTIALS_SECRET_NAME, "accessKey"))
        );
        assert_eq!(
            outcome
                .artifacts
                .management_stack
                .spec
                .env_refs
                .get("AWS_SECRET_ACCESS_KEY")
                .map(|r| (r.secret.name.as_str(), r.secret.key.as_str())),
            Some((S3_CREDENTIALS_SECRET_NAME, "secretKey"))
        );
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
            stack_type: "cloud".to_string(),
            preview: false,
            protect: true,
            bundle: None,
            bundle_item: None,
            resource_count: Some(12),
            last_synced: Some("2026-11-15T10:00:00Z".to_string()),
            status: "Ready".to_string(),
            catalog_id: Some("postgres-managed".to_string()),
            resource_kind: Some(keel_core::models::InfraResourceKind::Service),
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
        let (state, _) = build_serve_state(&serve_args).await.unwrap();

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

        let session_key = state
            .secret_store
            .get_or_create_session_key()
            .await
            .unwrap();
        let claims = keel_server::SessionClaims {
            sub: "admin@keel.local".to_string(),
            email: Some("admin@keel.local".to_string()),
            name: Some("Keel Admin".to_string()),
            groups: vec!["keel-admins".to_string()],
            auth_phase: AuthPhase::OidcActive,
            exp: chrono::Utc::now().timestamp() + 3600,
        };
        let session_token = keel_server::encrypt_session_token(&session_key, &claims).unwrap();

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

        // 2. Save OIDC session token in CLI context
        ctx.save_session_token(&server_url, &session_token).unwrap();

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

        // 7b. `keel infra ssh` against live server (table, json, command, overrides, and flag-injection rejection)
        for (output_fmt, expected_substr) in [
            ("table", "Stack:       postgres-managed"),
            ("json", "\"stackName\": \"postgres-managed\""),
            ("command", "ssh -p 2222 -i /tmp/id_key debian@"),
        ] {
            let ssh_cli = Cli::try_parse_from([
                "keel",
                "infra",
                "ssh",
                "postgres-managed",
                "--tenant",
                "acme",
                "--user",
                "debian",
                "--port",
                "2222",
                "-i",
                "/tmp/id_key",
                "--output",
                output_fmt,
                "--server",
                &server_url,
            ])
            .unwrap();
            let ssh_out = dispatch(&ssh_cli, &ctx).await.unwrap();
            assert!(
                ssh_out.contains(expected_substr),
                "expected '{expected_substr}' in '{ssh_out}'"
            );
        }

        // Flag injection rejection on `execute_infra_ssh`
        let bad_user_ssh = InfraSshArgs {
            stack_name: "postgres-managed".to_string(),
            tenant: "acme".to_string(),
            user: Some("-oProxyCommand=evil".to_string()),
            port: None,
            identity_file: None,
            exec: false,
            output: "command".to_string(),
            server: server_url.clone(),
        };
        assert!(execute_infra_ssh(&bad_user_ssh, &ctx).await.is_err());

        // 7c. `keel infra force-delete` CLI parsing & protected stack rejection
        let force_delete_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "force-delete",
            "management-cluster-core",
            "--namespace",
            "flux-system",
            "--server",
            &server_url,
        ])
        .unwrap();
        let force_delete_err = dispatch(&force_delete_cli, &ctx).await.unwrap_err();
        assert!(
            force_delete_err
                .to_string()
                .contains("management-cluster-core"),
            "unexpected error: {force_delete_err}"
        );

        let json_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "force-delete",
            "azure-free-tier-vm",
            "--tenant",
            "default",
            "--output",
            "json",
            "--server",
            &server_url,
        ])
        .unwrap();
        assert!(matches!(
            json_cli.command,
            Commands::Infra(crate::cli::InfraArgs {
                command: InfraCommands::ForceDelete(_)
            })
        ));

        let unlock_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "unlock",
            "azure-free-tier-vm",
            "--tenant",
            "default",
            "--output",
            "json",
            "--server",
            &server_url,
        ])
        .unwrap();
        assert!(matches!(
            unlock_cli.command,
            Commands::Infra(crate::cli::InfraArgs {
                command: InfraCommands::Unlock(_)
            })
        ));

        // `keel app install --engine flux` and `--engine pulumi`
        for eng in ["flux", "pulumi"] {
            let eng_cli = Cli::try_parse_from([
                "keel",
                "app",
                "install",
                "--app",
                "authentik",
                "--tenant",
                "acme",
                "--values-file",
                app_values_file.to_str().unwrap(),
                "--engine",
                eng,
                "--server",
                &server_url,
            ])
            .unwrap();
            let eng_out = dispatch(&eng_cli, &ctx).await.unwrap();
            assert!(eng_out.contains("Opened Proposal PR #"));
        }

        // Invalid `--engine` rejected client-side
        let bad_eng_cli = Cli::try_parse_from([
            "keel",
            "app",
            "install",
            "--app",
            "authentik",
            "--tenant",
            "acme",
            "--values-file",
            app_values_file.to_str().unwrap(),
            "--engine",
            "argocd",
            "--server",
            &server_url,
        ])
        .unwrap();
        assert!(dispatch(&bad_eng_cli, &ctx).await.is_err());

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
        };
        let outcome = execute_auth_login(&args, &ctx).await.unwrap();
        let AuthLoginOutcome::OidcRedirect {
            authorize_url,
            output,
        } = outcome;
        assert!(authorize_url.starts_with("https://idp.acme.io/auth"));
        assert!(authorize_url.contains("code_challenge_method=S256"));
        assert!(output.contains("OIDC PKCE URL"));
        let server_challenge = authorize_url
            .split('&')
            .find_map(|p| p.strip_prefix("code_challenge="))
            .expect("code_challenge query parameter in authorize_url");
        assert!(
            output.contains(&format!("(challenge S256={server_challenge})")),
            "expected challenge S256={server_challenge} in '{output}'"
        );

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
            oidc_issuer: None,
            oidc_provider: crate::cli::OidcProviderArg::Rauthy,
            oidc_client_id: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: true,
            repo_dir: Some(repo_dir.clone()),
            skip_core_install: true,
            git_url: Some("https://github.com/acme/infra-gitops.git".to_string()),
            forge_type: None,
            forge_api_url: None,
            git_token: Some("ghp_test_token_123".to_string()),
            pulumi_program_dir: None,
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

    #[test]
    fn default_tenant_git_repository_yaml_is_valid_multi_doc() {
        let repo = FluxGitRepository {
            api_version: FLUX_GIT_REPO_API_VERSION.to_string(),
            kind: FLUX_GIT_REPO_KIND.to_string(),
            metadata: CrObjectMeta {
                name: "keel-gitops".to_string(),
                namespace: "tenant-default".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
            },
            spec: FluxGitRepositorySpec {
                interval: DEFAULT_FLUX_POLL_INTERVAL.to_string(),
                url: "https://forgejo.keel.local/keel/infra-gitops.git".to_string(),
                git_ref: FluxGitRepositoryRef {
                    branch: Some("main".to_string()),
                    tag: None,
                    commit: None,
                },
                secret_ref: Some(LocalObjectReference {
                    name: "git-credentials".to_string(),
                }),
                timeout: Some("60s".to_string()),
            },
        };
        let yaml = render_default_tenant_git_repository_yaml(&repo).unwrap();
        assert!(yaml.ends_with('\n'));
        assert!(yaml.contains("\n---\n"));
        for line in yaml.lines() {
            if line.contains("---") {
                assert_eq!(line, "---", "document separator must be on its own line");
            }
        }
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 2);
        let first: serde_json::Value = serde_yml::from_str(docs[0]).unwrap();
        let second: serde_json::Value = serde_yml::from_str(docs[1]).unwrap();
        assert_eq!(first["kind"], "GitRepository");
        assert_eq!(second["kind"], "NetworkPolicy");
    }

    #[test]
    fn infra_ssh_argv_and_output_formatting_reject_flag_injection() {
        let info = keel_core::StackSshInfoResponse {
            stack_name: "proxmox-vm".to_string(),
            namespace: "tenant-default".to_string(),
            tenant: "default".to_string(),
            provider: "proxmox".to_string(),
            ssh_host: "192.168.10.25".to_string(),
            ssh_user: "ubuntu".to_string(),
            ssh_port: 2222,
            ssh_command: "ssh -p 2222 ubuntu@192.168.10.25".to_string(),
            cli_command: "keel infra ssh proxmox-vm --tenant default".to_string(),
            cli_exec_command: "keel infra ssh proxmox-vm --tenant default --exec".to_string(),
            has_saved_ssh_key: true,
        };

        let argv = build_ssh_argv(&info, Some(Path::new("/home/user/.ssh/id_ed25519"))).unwrap();
        assert_eq!(
            argv,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "-i".to_string(),
                "/home/user/.ssh/id_ed25519".to_string(),
                "ubuntu@192.168.10.25".to_string()
            ]
        );

        let cmd_out = format_ssh_info_output(&info, "command").unwrap();
        assert_eq!(cmd_out, "ssh -p 2222 ubuntu@192.168.10.25");

        let table_out = format_ssh_info_output(&info, "table").unwrap();
        assert!(table_out.contains("Stack:       proxmox-vm"));
        assert!(table_out.contains("Host:        192.168.10.25"));
        assert!(table_out.contains("SSH Command: ssh -p 2222 ubuntu@192.168.10.25"));

        let json_out = format_ssh_info_output(&info, "json").unwrap();
        let parsed: keel_core::StackSshInfoResponse = serde_json::from_str(&json_out).unwrap();
        assert_eq!(parsed, info);

        // Reject option injection in user/host/identity_file
        let mut injected = info.clone();
        injected.ssh_user = "-oProxyCommand=evil".to_string();
        assert!(build_ssh_argv(&injected, None).is_err());

        let mut injected_host = info.clone();
        injected_host.ssh_host = "-oLocalCommand=evil".to_string();
        assert!(build_ssh_argv(&injected_host, None).is_err());

        assert!(build_ssh_argv(&info, Some(Path::new("-oProxyCommand=evil"))).is_err());
    }

    #[tokio::test]
    async fn resolve_local_fallback_ssh_info_reads_gitops_stack_and_values() {
        let tmp = TempTestDir::new("local-ssh-fallback");
        let stack_dir = tmp.path().join("tenants/acme/infra/proxmox-node");
        tokio::fs::create_dir_all(&stack_dir).await.unwrap();
        tokio::fs::write(
            stack_dir.join("values.yaml"),
            "ipAddress: 10.10.20.55/24\nsshUser: adminuser\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            stack_dir.join("stack.yaml"),
            r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: proxmox-node
  namespace: tenant-acme
  labels:
    keel.io/provider: proxmox
spec:
  stack: organization/proxmox/acme-proxmox-node
"#,
        )
        .await
        .unwrap();

        let fallback = resolve_local_fallback_ssh_info(tmp.path(), "acme", "proxmox-node")
            .await
            .expect("should resolve local fallback ssh info");
        assert_eq!(fallback.stack_name, "proxmox-node");
        assert_eq!(fallback.tenant, "acme");
        assert_eq!(fallback.provider, "proxmox");
        assert_eq!(fallback.ssh_host, "10.10.20.55");
        assert_eq!(fallback.ssh_user, "adminuser");
        assert_eq!(fallback.ssh_command, "ssh adminuser@10.10.20.55");
        assert!(
            resolve_local_fallback_ssh_info(tmp.path(), "acme", "nonexistent")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cli_context_token_persistence_and_url_and_values_helpers() {
        let tmp = TempTestDir::new("cli-helpers");
        let creds_path = tmp.path().join("nested/credentials.json");
        let ctx = CliContext::with_credentials_path(&creds_path);

        assert_eq!(
            normalize_server_url("https://keel.local:8080/").unwrap(),
            "https://keel.local:8080"
        );
        assert!(normalize_server_url("ftp://keel.local").is_err());
        assert!(normalize_server_url("").is_err());

        ctx.save_session_token("https://keel.local:8080/", "tok-secret-123")
            .unwrap();
        assert_eq!(
            ctx.load_session_token("https://keel.local:8080").as_deref(),
            Some("tok-secret-123")
        );
        assert_eq!(ctx.load_session_token("https://other.local"), None);

        let override_ctx =
            CliContext::with_credentials_path(&creds_path).with_session_token("mem-tok");
        assert_eq!(
            override_ctx
                .load_session_token("https://other.local")
                .as_deref(),
            Some("mem-tok")
        );

        assert_eq!(
            default_bare_repo_for_workdir(Path::new("/tmp/keel-gitops")),
            PathBuf::from("/tmp/keel-gitops.git")
        );
        assert!(is_relative_local_remote("origin-dir"));
        assert!(!is_relative_local_remote("/abs/path"));
        assert!(!is_relative_local_remote("https://github.com/org/repo"));

        // load_values_file cases
        let empty_file = tmp.path().join("empty.yaml");
        tokio::fs::write(&empty_file, "   \n").await.unwrap();
        assert_eq!(load_values_file(&empty_file).await.unwrap(), json!({}));

        let valid_file = tmp.path().join("vals.yaml");
        tokio::fs::write(&valid_file, "replicaCount: 2\n")
            .await
            .unwrap();
        assert_eq!(
            load_values_file(&valid_file).await.unwrap()["replicaCount"],
            2
        );

        assert!(
            load_values_file(&tmp.path().join("missing.yaml"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dispatch_and_infra_force_delete_and_unlock_against_mock_server() {
        use axum::Router;
        use axum::routing::post;
        use clap::Parser;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_url = format!("http://127.0.0.1:{}", addr.port());

        let app = Router::new()
            .route(
                "/api/v1/stacks/tenant-default/my-vm/force-delete",
                post(|| async {
                    axum::Json(json!({ "message": "Force-deleted tenant-default/my-vm" }))
                }),
            )
            .route(
                "/api/v1/stacks/tenant-default/my-vm/unlock",
                post(|| async {
                    axum::Json(json!({ "message": "Unlocked tenant-default/my-vm" }))
                }),
            )
            .route(
                "/api/v1/stacks",
                axum::routing::get(|| async {
                    axum::Json(json!({
                        "stacks": [{
                            "name": "my-vm",
                            "tenant": "default",
                            "namespace": "tenant-default",
                            "provider": "proxmox",
                            "stack": "org/proj/my-vm",
                            "preview": false,
                            "protect": false,
                            "status": "Ready",
                            "stackType": "vm"
                        }]
                    }))
                }),
            )
            .route(
                "/api/v1/proposals/42/preview",
                axum::routing::get(|| async {
                    axum::Json(json!({
                        "prId": "42",
                        "url": "http://local/pr/42",
                        "state": "open",
                        "sourceBranch": "keel/test",
                        "targetBranch": "main",
                        "proposal": {
                            "id": "42",
                            "title": "feat: test",
                            "branch": "keel/test",
                            "base": "main",
                            "state": "open",
                            "url": "http://local/pr/42",
                            "author": "keel"
                        },
                        "report": PreviewReport::default()
                    }))
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tmp = TempTestDir::new("cli-dispatch");
        let ctx = CliContext::with_credentials_path(tmp.path().join("creds.json"))
            .with_session_token("test-bearer");

        let cli = Cli::try_parse_from([
            "keel",
            "infra",
            "force-delete",
            "my-vm",
            "--server",
            &server_url,
        ])
        .unwrap();
        let out = dispatch(&cli, &ctx).await.unwrap();
        assert!(out.contains("Force-deleted tenant-default/my-vm"));

        let cli =
            Cli::try_parse_from(["keel", "infra", "unlock", "my-vm", "--server", &server_url])
                .unwrap();
        let out = dispatch(&cli, &ctx).await.unwrap();
        assert!(out.contains("Unlocked tenant-default/my-vm"));

        let cli = Cli::try_parse_from(["keel", "infra", "list", "--server", &server_url]).unwrap();
        let out = dispatch(&cli, &ctx).await.unwrap();
        assert!(out.contains("No infrastructure stacks found") || out.contains("NAME"));

        let cli = Cli::try_parse_from([
            "keel",
            "preview",
            "status",
            "--pr",
            "42",
            "--server",
            &server_url,
        ])
        .unwrap();
        let out = dispatch(&cli, &ctx).await.unwrap();
        assert!(out.contains("PR #42") || out.contains("my-vm"));
    }
}
