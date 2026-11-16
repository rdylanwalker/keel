//! `clap` v4 CLI command contract for `keel` (`SDD.md` §7.2, `AGENTS.md` §1, §3.1, §3.6).

use std::fmt;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use keel_core::{BootstrapConfig, BootstrapDistro, BootstrapTargetKind, DEFAULT_CLUSTER_NAME};
use keel_pko::MANAGEMENT_CORE_STACK_NAME;
use serde::{Deserialize, Serialize};

/// Default local address bound by `keel serve`.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";

/// Default `keel-server` URL targeted by CLI subcommands.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";

/// Default S3 bucket name storing versioned Pulumi state (`SDD.md` §2.1).
pub const DEFAULT_S3_BUCKET: &str = "pulumi-state";

/// Top-level `keel` CLI entrypoint.
#[derive(Parser, Debug, Clone, PartialEq, Eq)]
#[command(
    name = "keel",
    version,
    propagate_version = true,
    about = "Keel — GitOps-Driven, Self-Bootstrapping Control Plane CLI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level subcommand groups for `keel`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Stage 0 seed cluster bootstrap and break-glass S3 recovery
    Bootstrap(BootstrapArgs),
    /// Authenticate the CLI against keel-server (Stage 0 token or OIDC PKCE)
    Auth(AuthArgs),
    /// Start the stateless Axum API/SSE server and embedded React SPA
    Serve(ServeArgs),
    /// List and provision multi-provider Pulumi Kubernetes Operator (PKO v2) stacks
    Infra(InfraArgs),
    /// Install upstream Helm/Kustomize catalog applications via Pull Requests
    App(AppArgs),
    /// Inspect ephemeral PKO v2 PR preview diffs and status
    Preview(PreviewArgs),
}

/// `keel bootstrap` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BootstrapArgs {
    #[command(subcommand)]
    pub command: BootstrapCommands,
}

/// Subcommands under `keel bootstrap`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum BootstrapCommands {
    /// Execute Stage 0 seed cluster bootstrap across local kind, kubeconfig, Proxmox, or cloud targets
    Init(BootstrapInitArgs),
    /// Break-glass recovery pulling versioned Pulumi state from S3 to repair an unreachable control plane
    Recover(BootstrapRecoverArgs),
}

/// Arguments for `keel bootstrap init` (`SDD.md` §2.1, §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BootstrapInitArgs {
    /// Stage 0 bootstrap target (`kind|kubeconfig|proxmox|aws|gcp|azure|hetzner`)
    #[arg(long)]
    pub target: BootstrapTargetKind,

    /// Seed management cluster name (DNS-1123 label)
    #[arg(long, default_value = DEFAULT_CLUSTER_NAME)]
    pub cluster_name: String,

    /// Path to an existing kubeconfig file (used with `--target kubeconfig`)
    #[arg(long)]
    pub kubeconfig: Option<PathBuf>,

    /// Explicit kubeconfig context to select
    #[arg(long)]
    pub context: Option<String>,

    /// Proxmox VE API endpoint URL (used with `--target proxmox`)
    #[arg(long)]
    pub proxmox_url: Option<String>,

    /// Target cloud region or availability zone
    #[arg(long)]
    pub region: Option<String>,

    /// Kubernetes distribution for VM-backed targets (`k3s|talos`)
    #[arg(long)]
    pub distro: Option<BootstrapDistro>,

    /// Deploy an in-cluster Git forge (Forgejo/Gitea) during Stage 0
    #[arg(long, default_value_t = false)]
    pub with_forge: bool,

    /// External OIDC issuer URL (disables in-cluster Dex OIDC deployment when provided)
    #[arg(long)]
    pub oidc_issuer: Option<String>,

    /// OIDC client ID (defaults to `keel-server`)
    #[arg(long)]
    pub oidc_client_id: Option<String>,

    /// Initial admin username/email for Stage 0 Dex seeding (defaults to `admin@keel.local`)
    #[arg(long)]
    pub admin_username: Option<String>,

    /// Initial admin password for Stage 0 Dex seeding (auto-generated if omitted)
    #[arg(long, env = "KEEL_ADMIN_PASSWORD")]
    pub admin_password: Option<String>,

    /// Explicitly enable Kyverno `PolicyException` allowing Docker socket access on local `kind` clusters
    #[arg(long, default_value_t = false)]
    pub local_dev_docker_socket_exception: bool,

    /// Local directory path to initialize the seed GitOps repository
    #[arg(long)]
    pub repo_dir: Option<PathBuf>,

    /// Skip installing core control-plane controllers (Flux CD, PKO v2, Kyverno) onto the cluster
    #[arg(long, default_value_t = false)]
    pub skip_core_install: bool,

    /// External Git repository URL (e.g. `https://github.com/org/infra-gitops.git`)
    #[arg(long)]
    pub git_url: Option<String>,

    /// Git forge provider type (`local|github|gitlab|forgejo|gitea`); inferred from `--git-url` if omitted
    #[arg(long)]
    pub forge_type: Option<ForgeTypeArg>,

    /// Optional Forge REST API base URL override (e.g. for GitHub Enterprise or self-hosted GitLab/Forgejo)
    #[arg(long)]
    pub forge_api_url: Option<String>,

    /// Personal access token for external/private Git repository & Forge API access (or set `KEEL_GIT_TOKEN`)
    #[arg(long, env = "KEEL_GIT_TOKEN")]
    pub git_token: Option<String>,

    /// Optional override directory containing the Stage 0 Pulumi.yaml program
    #[arg(long)]
    pub pulumi_program_dir: Option<PathBuf>,
}

impl BootstrapInitArgs {
    /// Converts parsed CLI flags into a validated-ready [`BootstrapConfig`].
    pub fn to_bootstrap_config(&self) -> BootstrapConfig {
        BootstrapConfig {
            target: self.target,
            cluster_name: self.cluster_name.clone(),
            kubeconfig: self.kubeconfig.clone(),
            context: self.context.clone(),
            with_forge: self.with_forge,
            with_oidc: self.oidc_issuer.is_none(),
            oidc_issuer: self.oidc_issuer.clone(),
            oidc_client_id: self.oidc_client_id.clone(),
            admin_username: self.admin_username.clone(),
            admin_password: self.admin_password.clone(),
            local_dev_docker_socket_exception: self.local_dev_docker_socket_exception,
            proxmox_url: self.proxmox_url.clone(),
            region: self.region.clone(),
            distro: self.distro,
            pulumi_program_dir: self.pulumi_program_dir.clone(),
        }
    }
}

/// Arguments for `keel bootstrap recover` (`SDD.md` §2.2, §7.2, `NFR-REL-01`).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BootstrapRecoverArgs {
    /// S3-compatible state backend endpoint URL
    #[arg(long)]
    pub s3_endpoint: String,

    /// S3 bucket containing versioned Pulumi state snapshots
    #[arg(long, default_value = DEFAULT_S3_BUCKET)]
    pub bucket: String,

    /// Protected control-plane Pulumi stack name to recover
    #[arg(long, default_value = MANAGEMENT_CORE_STACK_NAME)]
    pub stack: String,
}

/// `keel auth` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommands,
}

/// Subcommands under `keel auth`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AuthCommands {
    /// Authenticate the CLI against keel-server via OIDC Authorization Code + PKCE
    Login(AuthLoginArgs),
}

/// Arguments for `keel auth login` (`SDD.md` §6.1, §7.2, `NFR-SEC-01`).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AuthLoginArgs {
    /// Keel server base URL
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    pub server: String,
}

/// Arguments for `keel serve` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ServeArgs {
    /// Address and port to bind the HTTP/SSE server
    #[arg(long, default_value = DEFAULT_BIND_ADDR)]
    pub bind: String,

    /// Path to the local Git working tree containing `.keel/`, `catalog/`, and `tenants/`
    #[arg(long)]
    pub repo_dir: Option<PathBuf>,

    /// Path to the local bare Git repository used by `LocalGitProvider`
    #[arg(long)]
    pub bare_repo: Option<PathBuf>,

    /// Optional explicit path to `.keel/config.yaml`
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Personal access token for remote Forge API access (GitHub/GitLab/Forgejo/Gitea; or set `KEEL_GIT_TOKEN`)
    #[arg(long, env = "KEEL_GIT_TOKEN")]
    pub git_token: Option<String>,
}

/// Supported CLI `--forge-type` values.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeTypeArg {
    Local,
    Github,
    Gitlab,
    Forgejo,
    Gitea,
}

impl From<ForgeTypeArg> for keel_core::ForgeKind {
    fn from(arg: ForgeTypeArg) -> Self {
        match arg {
            ForgeTypeArg::Local => Self::Local,
            ForgeTypeArg::Github => Self::GitHub,
            ForgeTypeArg::Gitlab => Self::GitLab,
            ForgeTypeArg::Forgejo => Self::Forgejo,
            ForgeTypeArg::Gitea => Self::Gitea,
        }
    }
}

/// Output format for CLI inventory tables (`SDD.md` §7.2).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table => f.write_str("table"),
            Self::Json => f.write_str("json"),
        }
    }
}

/// `keel infra` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraArgs {
    #[command(subcommand)]
    pub command: InfraCommands,
}

/// Subcommands under `keel infra`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum InfraCommands {
    /// List PKO Stack resources across providers with live status
    List(InfraListArgs),
    /// Validate parameters against a blueprint JSONSchema and open a provisioning PR
    Provision(InfraProvisionArgs),
}

/// Arguments for `keel infra list` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraListArgs {
    /// Filter stacks by tenant ID
    #[arg(long)]
    pub tenant: Option<String>,

    /// Filter stacks by provider ID (e.g. `proxmox`, `aws`, `gcp`, `azure`, `hetzner`)
    #[arg(long)]
    pub provider: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    pub server: String,
}

/// Arguments for `keel infra provision` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraProvisionArgs {
    /// Infrastructure blueprint catalog ID
    #[arg(long)]
    pub blueprint: String,

    /// Target tenant ID (`tenant-<id>`)
    #[arg(long)]
    pub tenant: String,

    /// Path to YAML or JSON input values file
    #[arg(long)]
    pub values_file: PathBuf,

    /// Keel server base URL
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    pub server: String,
}

/// `keel app` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AppArgs {
    #[command(subcommand)]
    pub command: AppCommands,
}

/// Subcommands under `keel app`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum AppCommands {
    /// Validate Helm/Kustomize values against catalog JSONSchema and open a deployment PR
    Install(AppInstallArgs),
}

/// Arguments for `keel app install` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AppInstallArgs {
    /// Catalog application ID
    #[arg(long)]
    pub app: String,

    /// Target tenant ID (`tenant-<id>`)
    #[arg(long)]
    pub tenant: String,

    /// Path to YAML or JSON Helm/Kustomize values override file
    #[arg(long)]
    pub values_file: PathBuf,

    /// Enable Flagger + Envoy Gateway progressive canary rollout
    #[arg(long, default_value_t = false)]
    pub canary: bool,

    /// Keel server base URL
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    pub server: String,
}

/// `keel preview` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct PreviewArgs {
    #[command(subcommand)]
    pub command: PreviewCommands,
}

/// Subcommands under `keel preview`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum PreviewCommands {
    /// Fetch and render the PKO v2 preview diff (`+`/`~`/`-`/`!`) for an open PR
    Status(PreviewStatusArgs),
}

/// Arguments for `keel preview status` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct PreviewStatusArgs {
    /// Pull Request / proposal identifier
    #[arg(long)]
    pub pr: String,

    /// Stream real-time SSE reconciliation events until preview completion
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Keel server base URL
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    pub server: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bootstrap_init_all_targets_and_defaults() {
        for target in BootstrapTargetKind::all() {
            let cli =
                Cli::try_parse_from(["keel", "bootstrap", "init", "--target", target.as_str()])
                    .expect("target should parse");
            match cli.command {
                Commands::Bootstrap(BootstrapArgs {
                    command: BootstrapCommands::Init(args),
                }) => {
                    assert_eq!(args.target, *target);
                    assert_eq!(args.cluster_name, "keel-mgmt");
                    assert!(args.kubeconfig.is_none());
                    assert!(args.context.is_none());
                    assert!(args.proxmox_url.is_none());
                    assert!(args.region.is_none());
                    assert!(args.distro.is_none());
                    assert!(!args.with_forge);
                    assert!(args.oidc_issuer.is_none());
                    assert!(args.oidc_client_id.is_none());
                    assert!(args.admin_username.is_none());
                    assert!(args.admin_password.is_none());
                    assert!(!args.local_dev_docker_socket_exception);
                    assert!(args.repo_dir.is_none());
                    assert!(!args.skip_core_install);
                    assert!(args.pulumi_program_dir.is_none());
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn parse_bootstrap_init_full_flags_and_to_bootstrap_config() {
        let cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "init",
            "--target",
            "kind",
            "--cluster-name",
            "dev-seed",
            "--kubeconfig",
            "/tmp/kubeconfig.yaml",
            "--context",
            "kind-dev-seed",
            "--proxmox-url",
            "https://pve.local:8006",
            "--region",
            "us-east-1",
            "--distro",
            "talos",
            "--with-forge",
            "--oidc-issuer",
            "https://idp.example.com",
            "--oidc-client-id",
            "keel-custom",
            "--admin-username",
            "ops@example.com",
            "--local-dev-docker-socket-exception",
            "--skip-core-install",
            "--repo-dir",
            "/tmp/keel-gitops",
            "--pulumi-program-dir",
            "/tmp/pulumi-kind",
        ])
        .unwrap();

        let Commands::Bootstrap(BootstrapArgs {
            command: BootstrapCommands::Init(args),
        }) = cli.command
        else {
            panic!("expected bootstrap init");
        };

        assert_eq!(args.target, BootstrapTargetKind::Kind);
        assert_eq!(args.cluster_name, "dev-seed");
        assert_eq!(args.kubeconfig, Some(PathBuf::from("/tmp/kubeconfig.yaml")));
        assert_eq!(args.context.as_deref(), Some("kind-dev-seed"));
        assert_eq!(args.proxmox_url.as_deref(), Some("https://pve.local:8006"));
        assert_eq!(args.region.as_deref(), Some("us-east-1"));
        assert_eq!(args.distro, Some(BootstrapDistro::Talos));
        assert!(args.with_forge);
        assert_eq!(args.oidc_issuer.as_deref(), Some("https://idp.example.com"));
        assert_eq!(args.oidc_client_id.as_deref(), Some("keel-custom"));
        assert_eq!(args.admin_username.as_deref(), Some("ops@example.com"));
        assert!(args.local_dev_docker_socket_exception);
        assert!(args.skip_core_install);
        assert_eq!(args.repo_dir, Some(PathBuf::from("/tmp/keel-gitops")));
        assert_eq!(
            args.pulumi_program_dir,
            Some(PathBuf::from("/tmp/pulumi-kind"))
        );

        let cfg = args.to_bootstrap_config();
        assert_eq!(cfg.target, BootstrapTargetKind::Kind);
        assert_eq!(cfg.cluster_name, "dev-seed");
        assert!(cfg.local_dev_docker_socket_exception);
        assert_eq!(cfg.distro, Some(BootstrapDistro::Talos));
        assert_eq!(
            cfg.pulumi_program_dir,
            Some(PathBuf::from("/tmp/pulumi-kind"))
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parse_bootstrap_init_rejects_invalid_target_and_distro() {
        assert!(
            Cli::try_parse_from(["keel", "bootstrap", "init", "--target", "digitalocean"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "keel",
                "bootstrap",
                "init",
                "--target",
                "proxmox",
                "--distro",
                "openshift",
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_bootstrap_recover_flags_and_defaults() {
        let default_cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "recover",
            "--s3-endpoint",
            "https://s3.example.internal:9000",
        ])
        .unwrap();
        let Commands::Bootstrap(BootstrapArgs {
            command: BootstrapCommands::Recover(def_args),
        }) = default_cli.command
        else {
            panic!("expected bootstrap recover");
        };
        assert_eq!(def_args.s3_endpoint, "https://s3.example.internal:9000");
        assert_eq!(def_args.bucket, "pulumi-state");
        assert_eq!(def_args.stack, "management-cluster-core");

        let explicit_cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "recover",
            "--s3-endpoint",
            "https://garage.local:3900",
            "--bucket",
            "custom-state",
            "--stack",
            "keel/management-cluster-core/prod",
        ])
        .unwrap();
        let Commands::Bootstrap(BootstrapArgs {
            command: BootstrapCommands::Recover(exp_args),
        }) = explicit_cli.command
        else {
            panic!("expected bootstrap recover");
        };
        assert_eq!(exp_args.bucket, "custom-state");
        assert_eq!(exp_args.stack, "keel/management-cluster-core/prod");

        assert!(Cli::try_parse_from(["keel", "bootstrap", "recover"]).is_err());
    }

    #[test]
    fn parse_auth_login() {
        let oidc_cli = Cli::try_parse_from(["keel", "auth", "login"]).unwrap();
        let Commands::Auth(AuthArgs {
            command: AuthCommands::Login(oidc_args),
        }) = oidc_cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(oidc_args.server, "http://127.0.0.1:8080");

        let custom_cli =
            Cli::try_parse_from(["keel", "auth", "login", "--server", "https://keel.internal"])
                .unwrap();
        let Commands::Auth(AuthArgs {
            command: AuthCommands::Login(custom_args),
        }) = custom_cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(custom_args.server, "https://keel.internal");
    }

    #[test]
    fn parse_serve_defaults_and_custom_paths() {
        let def_cli = Cli::try_parse_from(["keel", "serve"]).unwrap();
        let Commands::Serve(def_args) = def_cli.command else {
            panic!("expected serve");
        };
        assert_eq!(def_args.bind, "127.0.0.1:8080");
        assert!(def_args.repo_dir.is_none());
        assert!(def_args.bare_repo.is_none());
        assert!(def_args.config.is_none());

        let custom_cli = Cli::try_parse_from([
            "keel",
            "serve",
            "--bind",
            "0.0.0.0:9090",
            "--repo-dir",
            "/var/lib/keel/work",
            "--bare-repo",
            "/var/lib/keel/origin.git",
            "--config",
            "/etc/keel/config.yaml",
        ])
        .unwrap();
        let Commands::Serve(custom_args) = custom_cli.command else {
            panic!("expected serve");
        };
        assert_eq!(custom_args.bind, "0.0.0.0:9090");
        assert_eq!(
            custom_args.repo_dir,
            Some(PathBuf::from("/var/lib/keel/work"))
        );
        assert_eq!(
            custom_args.bare_repo,
            Some(PathBuf::from("/var/lib/keel/origin.git"))
        );
        assert_eq!(
            custom_args.config,
            Some(PathBuf::from("/etc/keel/config.yaml"))
        );
    }

    #[test]
    fn parse_infra_list_and_provision() {
        let list_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "list",
            "--tenant",
            "acme",
            "--provider",
            "proxmox",
            "--output",
            "json",
        ])
        .unwrap();
        let Commands::Infra(InfraArgs {
            command: InfraCommands::List(list_args),
        }) = list_cli.command
        else {
            panic!("expected infra list");
        };
        assert_eq!(list_args.tenant.as_deref(), Some("acme"));
        assert_eq!(list_args.provider.as_deref(), Some("proxmox"));
        assert_eq!(list_args.output, OutputFormat::Json);
        assert_eq!(list_args.output.to_string(), "json");
        assert_eq!(OutputFormat::Table.to_string(), "table");

        assert!(Cli::try_parse_from(["keel", "infra", "list", "--output", "yaml"]).is_err());

        let prov_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "provision",
            "--blueprint",
            "postgres-managed",
            "--tenant",
            "acme",
            "--values-file",
            "db-values.yaml",
            "--server",
            "http://127.0.0.1:8081",
        ])
        .unwrap();
        let Commands::Infra(InfraArgs {
            command: InfraCommands::Provision(prov_args),
        }) = prov_cli.command
        else {
            panic!("expected infra provision");
        };
        assert_eq!(prov_args.blueprint, "postgres-managed");
        assert_eq!(prov_args.tenant, "acme");
        assert_eq!(prov_args.values_file, PathBuf::from("db-values.yaml"));
        assert_eq!(prov_args.server, "http://127.0.0.1:8081");
    }

    #[test]
    fn parse_app_install_and_preview_status() {
        let app_cli = Cli::try_parse_from([
            "keel",
            "app",
            "install",
            "--app",
            "authentik",
            "--tenant",
            "acme",
            "--values-file",
            "authentik.yaml",
            "--canary",
            "--server",
            "http://127.0.0.1:8080",
        ])
        .unwrap();
        let Commands::App(AppArgs {
            command: AppCommands::Install(app_args),
        }) = app_cli.command
        else {
            panic!("expected app install");
        };
        assert_eq!(app_args.app, "authentik");
        assert_eq!(app_args.tenant, "acme");
        assert_eq!(app_args.values_file, PathBuf::from("authentik.yaml"));
        assert!(app_args.canary);

        let prev_cli = Cli::try_parse_from([
            "keel",
            "preview",
            "status",
            "--pr",
            "42",
            "--watch",
            "--server",
            "http://127.0.0.1:8080",
        ])
        .unwrap();
        let Commands::Preview(PreviewArgs {
            command: PreviewCommands::Status(prev_args),
        }) = prev_cli.command
        else {
            panic!("expected preview status");
        };
        assert_eq!(prev_args.pr, "42");
        assert!(prev_args.watch);
        assert_eq!(prev_args.server, "http://127.0.0.1:8080");
    }
}
