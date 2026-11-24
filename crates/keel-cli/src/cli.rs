//! `clap` v4 CLI command contract for `keel` (`SDD.md` §7.2, `AGENTS.md` §1, §3.1, §3.6).

use std::fmt;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use keel_core::{BootstrapConfig, BootstrapDistro, BootstrapTargetKind, DEFAULT_CLUSTER_NAME};
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
    about = "Keel — GitOps-Driven, Self-Bootstrapping Control Plane CLI"
)]
pub struct Cli {
    /// Keel server base URL for API commands (overrides per-command defaults)
    #[arg(
        long = "server",
        global = true,
        env = "KEEL_SERVER",
        value_name = "URL"
    )]
    pub server: Option<String>,

    /// Verbose errors: print the full error cause chain (repeat for more detail)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Server URL from `--server` / `KEEL_SERVER`, or `DEFAULT_SERVER_URL`.
    pub fn server_url(&self) -> String {
        self.server
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
    }
}

/// Help text shared by every `--tenant` flag.
pub const TENANT_HELP: &str =
    "Tenant ID as a bare id (e.g. `default`); a `tenant-` prefix is stripped";

/// Help text for the proposal scope (`--tenant` of `keel proposal show|merge|close` and
/// `keel preview status`).
pub const PROPOSAL_SCOPE_HELP: &str = "Tenant whose repository holds the proposal (bare id, e.g. `default`), or `management` for platform-repository proposals; PR ids are per repository";

/// Top-level subcommand groups for `keel`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Stage 0 seed cluster bootstrap and break-glass S3 recovery
    Bootstrap(BootstrapArgs),
    /// Log in / out of keel-server (browser OIDC + PKCE loopback) and show session status
    Auth(AuthArgs),
    /// Start the stateless Axum API/SSE server and embedded React SPA
    Serve(ServeArgs),
    /// List, provision, and destroy multi-provider Pulumi Kubernetes Operator (PKO v2) stacks
    Infra(InfraArgs),
    /// Install and delete catalog applications via Pull Requests and list deployed releases
    App(AppArgs),
    /// Inspect ephemeral PKO v2 PR preview diffs and status
    Preview(PreviewArgs),
    /// List, inspect, merge, or close change proposals (pull requests)
    Proposal(ProposalArgs),
    /// Browse the application / infrastructure catalog
    Catalog(CatalogArgs),
    /// Tenant operations (credential repair)
    #[command(visible_alias = "tenants")]
    Tenant(TenantArgs),
    /// Work with the local GitOps working copy (`--repo-dir` of `keel bootstrap init`)
    Gitops(GitopsArgs),
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
    /// Break-glass: re-apply escrowed Stage 0 Pulumi state from S3 (refresh + up) to repair the control plane
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

    /// Kubernetes distribution for VM-backed targets (`k3s`; `talos` only with a custom
    /// `--pulumi-program-dir`)
    #[arg(long)]
    pub distro: Option<BootstrapDistro>,

    /// Deploy an in-cluster Git forge (Forgejo/Gitea) during Stage 0
    #[arg(long, default_value_t = false)]
    pub with_forge: bool,

    /// Run keel-server in-cluster as a Flux-managed add-on (`clusters/management/addons/keel-server`,
    /// image pinned to this CLI's release); requires `--repo-dir` plus either `--with-forge` or an
    /// HTTP(S) `--git-url` (not both)
    #[arg(long, default_value_t = false)]
    pub with_keel_server: bool,

    /// External OIDC issuer URL (disables in-cluster Rauthy OIDC deployment when provided)
    #[arg(long)]
    pub oidc_issuer: Option<String>,

    /// In-cluster OIDC provider to deploy during Stage 0 when `--oidc-issuer` is omitted
    #[arg(long, value_enum, default_value_t = OidcProviderArg::Rauthy)]
    pub oidc_provider: OidcProviderArg,

    /// OIDC client ID (defaults to `keel-server`)
    #[arg(long)]
    pub oidc_client_id: Option<String>,

    /// Public HTTPS URL of the in-cluster Rauthy IdP; the issuer becomes `<url>/auth/v1` and the
    /// Forgejo/portal hosts are derived from its parent domain (default `https://idp.keel.local:8888`)
    #[arg(long, value_name = "URL")]
    pub oidc_public_url: Option<String>,

    /// Initial admin username/email for Stage 0 Rauthy seeding (defaults to `admin@keel.local`)
    #[arg(long)]
    pub admin_username: Option<String>,

    /// Initial admin password for Stage 0 Rauthy seeding (auto-generated if omitted)
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

    /// External Git repository URL (e.g. `https://github.com/org/infra-gitops.git`); must not
    /// embed credentials (use `--git-token` / `KEEL_GIT_TOKEN`)
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

    /// Override directory with the Stage 0 Pulumi.yaml program (default: the embedded program)
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
            with_keel_server: self.with_keel_server,
            with_oidc: self.oidc_issuer.is_none() && self.oidc_provider != OidcProviderArg::None,
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
///
/// Prerequisites: `pulumi` on PATH; S3 credentials in `AWS_ACCESS_KEY_ID` /
/// `AWS_SECRET_ACCESS_KEY`; the Stage 0 passphrase in `PULUMI_CONFIG_PASSPHRASE`,
/// `--passphrase-file`, or the local `~/.config/keel/stage0/<cluster>/pulumi-passphrase`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BootstrapRecoverArgs {
    /// S3-compatible endpoint holding the escrowed state (e.g. `http://127.0.0.1:8333` via
    /// `kubectl -n keel-system port-forward svc/s3-state-backend-seaweedfs-s3 8333:8333`)
    #[arg(long, value_name = "URL")]
    pub s3_endpoint: String,

    /// S3 bucket containing the versioned Pulumi state
    #[arg(long, default_value = DEFAULT_S3_BUCKET)]
    pub bucket: String,

    /// S3 region (must match the region used when the state was written)
    #[arg(long, default_value = "us-east-1")]
    pub region: String,

    /// Seed management cluster name whose Stage 0 state (`stage0/<cluster>/`) is recovered
    #[arg(long, default_value = DEFAULT_CLUSTER_NAME)]
    pub cluster_name: String,

    /// Stage 0 stack(s) to recover (repeatable; default: all stacks in the escrow manifest, or
    /// `stage0-control-plane-<cluster>`)
    #[arg(long = "stack", value_name = "STACK")]
    pub stacks: Vec<String>,

    /// Kubeconfig file for the (possibly new) cluster; overrides the escrowed `kubeconfig` config
    #[arg(long)]
    pub kubeconfig: Option<PathBuf>,

    /// Kubeconfig context to target (overrides the escrowed `kubeContext` config)
    #[arg(long)]
    pub context: Option<String>,

    /// File containing the Stage 0 Pulumi passphrase (instead of `PULUMI_CONFIG_PASSPHRASE`)
    #[arg(long)]
    pub passphrase_file: Option<PathBuf>,

    /// Restore a specific S3 object version of the stack checkpoint before refresh/up
    #[arg(long, conflicts_with = "snapshot")]
    pub state_version: Option<String>,

    /// Restore a timestamped snapshot (`stage0/<cluster>/snapshots/<stack>/<SNAPSHOT>.json`)
    #[arg(long)]
    pub snapshot: Option<String>,

    /// List available state versions and snapshots, then exit
    #[arg(long, default_value_t = false)]
    pub list_versions: bool,

    /// Skip `pulumi refresh` before `pulumi up`
    #[arg(long, default_value_t = false)]
    pub skip_refresh: bool,

    /// Print the recovery plan without executing anything
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
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
    /// Log in via the browser (OIDC + PKCE, RFC 8252 loopback redirect) and store a session token
    Login(AuthLoginArgs),
    /// Show the stored session and the identity reported by the server
    Status(AuthServerArgs),
    /// Remove the stored session token for the server
    Logout(AuthServerArgs),
}

/// Arguments for `keel auth login` (`SDD.md` §6.1, §7.2, `NFR-SEC-01`).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AuthLoginArgs {
    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,

    /// Do not try to open a browser; only print the login URL
    #[arg(long, default_value_t = false)]
    pub no_browser: bool,

    /// Force the IdP to re-prompt for credentials
    #[arg(long, default_value_t = false)]
    pub force_prompt: bool,

    /// Seconds to wait for the browser callback
    #[arg(long, default_value_t = 300)]
    pub timeout_secs: u64,
}

/// Arguments for `keel auth status|logout`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AuthServerArgs {
    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
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
    #[arg(long, env = "KEEL_GIT_TOKEN", hide_env_values = true)]
    pub git_token: Option<String>,

    /// Remote GitOps repository to bootstrap `--repo-dir` from (cloned when the directory is
    /// missing/empty, otherwise fetched). Credentials come from `KEEL_GIT_TOKEN`, never the URL.
    #[arg(long, env = "KEEL_GIT_URL")]
    pub git_url: Option<String>,

    /// HTTPS username paired with `KEEL_GIT_TOKEN` for `--git-url` (default: `keel`)
    #[arg(long, env = "KEEL_GIT_USERNAME")]
    pub git_username: Option<String>,
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

/// In-cluster OIDC provider selection for Stage 0 bootstrap.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OidcProviderArg {
    /// Lightweight Rust OIDC provider with built-in Admin UI for users & groups (default)
    #[default]
    Rauthy,
    /// Skip in-cluster OIDC provider installation
    None,
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
    /// Open a removal PR for a provisioned stack (PKO runs `pulumi destroy` once it is merged)
    Destroy(InfraDestroyArgs),
    /// Display SSH connection info or open an interactive SSH session to a provisioned VM stack
    Ssh(InfraSshArgs),
    /// Clear finalizers and force-delete a stuck or terminating PKO v2 Stack
    ForceDelete(InfraForceDeleteArgs),
    /// Cancel a stale Pulumi state lock for a PKO v2 Stack and request reconciliation
    Unlock(InfraUnlockArgs),
}

/// Arguments for `keel infra list` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraListArgs {
    /// Filter stacks by tenant (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: Option<String>,

    /// Filter stacks by provider ID (e.g. `proxmox`, `aws`, `gcp`, `azure`, `hetzner`)
    #[arg(long)]
    pub provider: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel infra provision` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraProvisionArgs {
    /// Infrastructure blueprint catalog ID
    #[arg(long)]
    pub blueprint: String,

    /// Target tenant (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: String,

    /// Path to YAML or JSON input values file. On an existing stack it is merged over the
    /// committed values (only the keys it sets change; `null` removes a key)
    #[arg(long)]
    pub values_file: PathBuf,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel infra destroy` (`SDD.md` §7.2): `DELETE /api/v1/stacks/tenant-<id>/<stack>`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraDestroyArgs {
    /// Infrastructure stack name (as shown by `keel infra list`)
    pub stack_name: String,

    /// Tenant owning the stack (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: String,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel infra ssh` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraSshArgs {
    /// Infrastructure stack name
    pub stack_name: String,

    /// Target tenant (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, default_value = "default", help = TENANT_HELP)]
    pub tenant: String,

    /// Override SSH username
    #[arg(long)]
    pub user: Option<String>,

    /// Override SSH port
    #[arg(long)]
    pub port: Option<u16>,

    /// Path to SSH private key identity file (-i)
    #[arg(long, short = 'i')]
    pub identity_file: Option<PathBuf>,

    /// Execute the SSH command interactively in the current terminal instead of printing
    #[arg(long)]
    pub exec: bool,

    /// Output format when printing (`table|json|command`)
    #[arg(long, default_value = "table", value_parser = ["table", "json", "command"])]
    pub output: String,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel infra force-delete`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraForceDeleteArgs {
    /// Infrastructure stack name
    pub stack_name: String,

    /// Target tenant identifier (resolves to `tenant-<tenant>` unless `--namespace` is specified)
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Explicit Kubernetes namespace override
    #[arg(long)]
    pub namespace: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel infra unlock`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct InfraUnlockArgs {
    /// Infrastructure stack name
    pub stack_name: String,

    /// Target tenant identifier (resolves to `tenant-<tenant>` unless `--namespace` is specified)
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Explicit Kubernetes namespace override
    #[arg(long)]
    pub namespace: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
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
    /// List deployed application releases (Flux HelmReleases / Kustomizations)
    List(AppListArgs),
    /// Open a removal PR for an installed application release (Flux prunes it once merged)
    #[command(visible_alias = "uninstall")]
    Delete(AppDeleteArgs),
}

/// Arguments for `keel app delete`: `DELETE /api/v1/releases/tenant-<id>/<name>`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AppDeleteArgs {
    /// Release name (as shown by `keel app list`)
    pub name: String,

    /// Tenant owning the release (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: String,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel app list`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AppListArgs {
    /// Filter by tenant (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// `keel proposal` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ProposalArgs {
    #[command(subcommand)]
    pub command: ProposalCommands,
}

/// Subcommands under `keel proposal`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ProposalCommands {
    /// List proposals (pull requests)
    List(ProposalListArgs),
    /// Show one proposal
    Show(ProposalRefArgs),
    /// Close (reject) an open proposal; ephemeral previews are garbage-collected
    Close(ProposalRefArgs),
    /// Merge an open proposal (pinned to the reviewed head SHA by the server)
    Merge(ProposalRefArgs),
}

/// Proposal status filter for `keel proposal list`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProposalStatusArg {
    #[default]
    Open,
    Merged,
    Closed,
    All,
}

impl ProposalStatusArg {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

/// Arguments for `keel proposal list`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ProposalListArgs {
    /// Only this tenant's repository (or `management` for the platform repository); without it,
    /// every repository you can see
    #[arg(long, help = PROPOSAL_SCOPE_HELP)]
    pub tenant: Option<String>,

    /// Status filter
    #[arg(long, value_enum, default_value_t = ProposalStatusArg::Open)]
    pub status: ProposalStatusArg,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel proposal show|close|merge`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ProposalRefArgs {
    /// Pull request / proposal id (within the `--tenant` repository)
    pub pr: String,

    /// Repository scope of the proposal
    #[arg(long, help = PROPOSAL_SCOPE_HELP)]
    pub tenant: String,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// `keel catalog` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CatalogArgs {
    #[command(subcommand)]
    pub command: CatalogCommands,
}

/// Subcommands under `keel catalog`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum CatalogCommands {
    /// List catalog applications, infrastructure blueprints, and bundles
    List(CatalogListArgs),
    /// Show one catalog entry (including its values JSONSchema)
    Show(CatalogShowArgs),
}

/// Arguments for `keel catalog list`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CatalogListArgs {
    /// Tenant whose view of the catalog to show (bare id; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel catalog show`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CatalogShowArgs {
    /// Catalog entry id (application, blueprint, or bundle)
    pub id: String,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// `keel tenant` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct TenantArgs {
    #[command(subcommand)]
    pub command: TenantCommands,
}

/// Subcommands under `keel tenant`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum TenantCommands {
    /// Re-mint a tenant's repository read credential and its cluster Secret (all tenants when no
    /// id is given; platform admin only)
    RepairCredentials(TenantRepairCredentialsArgs),
}

/// `keel gitops` subcommands.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct GitopsArgs {
    #[command(subcommand)]
    pub command: GitopsCommands,
}

/// Subcommands under `keel gitops`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum GitopsCommands {
    /// Fast-forward the working copy from its origin (`git pull --ff-only`). With the in-cluster
    /// Forgejo and no `keel serve` running, opens a temporary port-forward and Git credential for
    /// the pull and removes both afterwards
    Pull(GitopsPullArgs),
}

/// Arguments for `keel gitops pull`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct GitopsPullArgs {
    /// Local GitOps working copy (the `--repo-dir` of `keel bootstrap init`)
    #[arg(long, default_value = "keel-gitops")]
    pub repo_dir: PathBuf,

    /// Kubeconfig context of the management cluster (default: the current context)
    #[arg(long)]
    pub context: Option<String>,

    /// Kubeconfig file of the management cluster (default: `KUBECONFIG` / `~/.kube/config`)
    #[arg(long)]
    pub kubeconfig: Option<PathBuf>,
}

/// Arguments for `keel tenant repair-credentials` (calls keel-server; platform admin only).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct TenantRepairCredentialsArgs {
    /// Tenant (bare id, e.g. `acme`; a `tenant-` prefix is stripped). Omit to repair every tenant
    pub tenant: Option<String>,

    /// Output format (`table|json`)
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
    pub server: String,
}

/// Arguments for `keel app install` (`SDD.md` §7.2).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AppInstallArgs {
    /// Catalog application ID
    #[arg(long)]
    pub app: String,

    /// Target tenant (bare id, e.g. `default`; a `tenant-` prefix is stripped)
    #[arg(long, help = TENANT_HELP)]
    pub tenant: String,

    /// Path to YAML or JSON Helm/Kustomize values override file. Optional: without it the
    /// catalog schema defaults apply (required fields without a default must be set here). On an
    /// existing instance it is merged over the committed values (only the keys it sets change;
    /// `null` removes a key)
    #[arg(long)]
    pub values_file: Option<PathBuf>,

    /// Enable Flagger + Envoy Gateway progressive canary rollout. The canary serves the host in
    /// the app's `ingressHost` value, which it requires (a concrete DNS name that is neither a
    /// platform host nor routed by another app)
    #[arg(long, default_value_t = false)]
    pub canary: bool,

    /// Reconciliation engine for raw Kubernetes manifest catalog apps (`flux` or `pulumi`)
    #[arg(long)]
    pub engine: Option<String>,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
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
    /// Pull Request / proposal identifier (within the `--tenant` repository)
    #[arg(long)]
    pub pr: String,

    /// Repository scope of the proposal
    #[arg(long, help = PROPOSAL_SCOPE_HELP)]
    pub tenant: String,

    /// Stream real-time SSE reconciliation events until preview completion
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Keel server base URL (set via global `--server` / `KEEL_SERVER`)
    #[arg(skip = DEFAULT_SERVER_URL.to_string())]
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
                    assert!(!args.with_keel_server);
                    assert!(!args.to_bootstrap_config().with_keel_server);
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
            "--with-keel-server",
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
        assert!(args.with_keel_server);
        assert!(args.to_bootstrap_config().with_keel_server);
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
        assert!(def_args.stacks.is_empty());
        assert_eq!(def_args.cluster_name, "keel-mgmt");
        assert_eq!(def_args.region, "us-east-1");
        assert!(!def_args.dry_run);

        let explicit_cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "recover",
            "--s3-endpoint",
            "https://garage.local:3900",
            "--bucket",
            "custom-state",
            "--stack",
            "stage0-control-plane-keel-mgmt",
            "--stack",
            "stage0-control-plane-rauthy-keel-mgmt",
            "--dry-run",
            "--snapshot",
            "20261120T000000Z",
        ])
        .unwrap();
        let Commands::Bootstrap(BootstrapArgs {
            command: BootstrapCommands::Recover(exp_args),
        }) = explicit_cli.command
        else {
            panic!("expected bootstrap recover");
        };
        assert_eq!(exp_args.bucket, "custom-state");
        assert_eq!(exp_args.stacks.len(), 2);
        assert!(exp_args.dry_run);
        assert_eq!(exp_args.snapshot.as_deref(), Some("20261120T000000Z"));

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
        assert_eq!(custom_cli.server_url(), "https://keel.internal");
        let Commands::Auth(AuthArgs {
            command: AuthCommands::Login(custom_args),
        }) = custom_cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(custom_cli.server.as_deref(), Some("https://keel.internal"));
        assert!(!custom_args.no_browser);
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
        assert!(def_args.git_url.is_none());

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
            "--git-url",
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
            "--git-username",
            "keel",
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
        assert_eq!(
            custom_args.git_url.as_deref(),
            Some("http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git")
        );
        assert_eq!(custom_args.git_username.as_deref(), Some("keel"));
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
        assert_eq!(prov_cli.server.as_deref(), Some("http://127.0.0.1:8081"));
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
        assert_eq!(app_args.values_file, Some(PathBuf::from("authentik.yaml")));
        assert!(app_args.canary);
        assert_eq!(app_args.engine, None);

        let app_engine_cli = Cli::try_parse_from([
            "keel",
            "app",
            "install",
            "--app",
            "raw-nginx",
            "--tenant",
            "acme",
            "--values-file",
            "nginx.yaml",
            "--engine",
            "pulumi",
        ])
        .unwrap();
        let Commands::App(AppArgs {
            command: AppCommands::Install(app_engine_args),
        }) = app_engine_cli.command
        else {
            panic!("expected app install with engine");
        };
        assert_eq!(app_engine_args.engine.as_deref(), Some("pulumi"));

        // `--values-file` is optional: schema defaults apply (FU-7).
        let app_defaults_cli = Cli::try_parse_from([
            "keel", "app", "install", "--app", "podinfo", "--tenant", "acme",
        ])
        .unwrap();
        let Commands::App(AppArgs {
            command: AppCommands::Install(app_defaults_args),
        }) = app_defaults_cli.command
        else {
            panic!("expected app install without values file");
        };
        assert_eq!(app_defaults_args.values_file, None);

        let delete_cli = Cli::try_parse_from([
            "keel", "app", "delete", "podinfo", "--tenant", "acme", "--output", "json",
        ])
        .unwrap();
        let Commands::App(AppArgs {
            command: AppCommands::Delete(delete_args),
        }) = delete_cli.command
        else {
            panic!("expected app delete");
        };
        assert_eq!(delete_args.name, "podinfo");
        assert_eq!(delete_args.tenant, "acme");
        assert_eq!(delete_args.output, OutputFormat::Json);
        let alias_cli =
            Cli::try_parse_from(["keel", "app", "uninstall", "podinfo", "--tenant", "acme"])
                .unwrap();
        assert!(matches!(
            alias_cli.command,
            Commands::App(AppArgs {
                command: AppCommands::Delete(_)
            })
        ));
        // `--tenant` is required: a removal PR never guesses the tenant.
        assert!(Cli::try_parse_from(["keel", "app", "delete", "podinfo"]).is_err());

        let destroy_cli =
            Cli::try_parse_from(["keel", "infra", "destroy", "dev-vm", "--tenant", "acme"])
                .unwrap();
        let Commands::Infra(InfraArgs {
            command: InfraCommands::Destroy(destroy_args),
        }) = destroy_cli.command
        else {
            panic!("expected infra destroy");
        };
        assert_eq!(destroy_args.stack_name, "dev-vm");
        assert_eq!(destroy_args.tenant, "acme");
        assert_eq!(destroy_args.output, OutputFormat::Table);
        assert!(Cli::try_parse_from(["keel", "infra", "destroy", "dev-vm"]).is_err());

        let ssh_cli = Cli::try_parse_from([
            "keel",
            "infra",
            "ssh",
            "proxmox-vm",
            "--tenant",
            "acme",
            "--user",
            "debian",
            "--port",
            "2222",
            "-i",
            "/tmp/id_ed25519",
            "--output",
            "command",
            "--exec",
        ])
        .unwrap();
        let Commands::Infra(InfraArgs {
            command: InfraCommands::Ssh(ssh_args),
        }) = ssh_cli.command
        else {
            panic!("expected infra ssh");
        };
        assert_eq!(ssh_args.stack_name, "proxmox-vm");
        assert_eq!(ssh_args.tenant, "acme");
        assert_eq!(ssh_args.user.as_deref(), Some("debian"));
        assert_eq!(ssh_args.port, Some(2222));
        assert_eq!(
            ssh_args.identity_file,
            Some(PathBuf::from("/tmp/id_ed25519"))
        );
        assert_eq!(ssh_args.output, "command");
        assert!(ssh_args.exec);

        let prev_cli = Cli::try_parse_from([
            "keel",
            "preview",
            "status",
            "--pr",
            "42",
            "--tenant",
            "acme",
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
        assert_eq!(prev_args.tenant, "acme");
        assert!(prev_args.watch);
        // PR ids are per repository: the scope is mandatory.
        assert!(Cli::try_parse_from(["keel", "preview", "status", "--pr", "42"]).is_err());
        assert!(Cli::try_parse_from(["keel", "proposal", "merge", "42"]).is_err());
        let merge =
            Cli::try_parse_from(["keel", "proposal", "merge", "42", "--tenant", "management"])
                .unwrap();
        let Commands::Proposal(ProposalArgs {
            command: ProposalCommands::Merge(merge_args),
        }) = merge.command
        else {
            panic!("expected proposal merge");
        };
        assert_eq!(merge_args.tenant, "management");
        assert_eq!(prev_cli.server.as_deref(), Some("http://127.0.0.1:8080"));
    }
}
