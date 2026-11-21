//! Core domain models for Keel configuration, multi-tenancy, Stage 0 bootstrapping,
//! provider registries, and `keel.io/v1alpha1` catalog schemas.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{CoreError, validate_dns_label};

/// Git-stored catalog and configuration schema API version.
pub const CATALOG_API_VERSION: &str = "keel.io/v1alpha1";

/// `kind` discriminator for application catalog definitions.
pub const CATALOG_APPLICATION_KIND: &str = "CatalogApplication";

/// `kind` discriminator for infrastructure blueprint catalog definitions.
pub const CATALOG_INFRA_BLUEPRINT_KIND: &str = "CatalogInfraBlueprint";

/// `kind` discriminator for multi-item bundle catalog definitions.
pub const CATALOG_BUNDLE_KIND: &str = "CatalogBundle";

/// Default seed management cluster name.
pub const DEFAULT_CLUSTER_NAME: &str = "keel-mgmt";

/// Default outbound Flux GitRepository polling interval (`NFR` NAT/firewall resilience).
pub const DEFAULT_POLL_INTERVAL: &str = "1m";

/// Supported Stage 0 bootstrap target providers (`SDD.md` §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootstrapTargetKind {
    Kind,
    Kubeconfig,
    Proxmox,
    Aws,
    Gcp,
    Azure,
    Hetzner,
}

impl BootstrapTargetKind {
    /// Returns all supported Stage 0 bootstrap target kinds.
    pub const fn all() -> &'static [Self] {
        &[
            Self::Kind,
            Self::Kubeconfig,
            Self::Proxmox,
            Self::Aws,
            Self::Gcp,
            Self::Azure,
            Self::Hetzner,
        ]
    }

    /// Canonical lowercase identifier used in CLI flags and YAML configurations.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Kubeconfig => "kubeconfig",
            Self::Proxmox => "proxmox",
            Self::Aws => "aws",
            Self::Gcp => "gcp",
            Self::Azure => "azure",
            Self::Hetzner => "hetzner",
        }
    }

    /// Returns `true` when Stage 0 provisions underlying compute/cluster infrastructure via Pulumi
    /// and uploads protected state (`management-cluster-core` with `protect: true`).
    pub const fn requires_stage0_infra(&self) -> bool {
        matches!(
            self,
            Self::Proxmox | Self::Aws | Self::Gcp | Self::Azure | Self::Hetzner
        )
    }
}

impl fmt::Display for BootstrapTargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BootstrapTargetKind {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kind" => Ok(Self::Kind),
            "kubeconfig" => Ok(Self::Kubeconfig),
            "proxmox" => Ok(Self::Proxmox),
            "aws" => Ok(Self::Aws),
            "gcp" => Ok(Self::Gcp),
            "azure" => Ok(Self::Azure),
            "hetzner" => Ok(Self::Hetzner),
            other => Err(CoreError::Validation(format!(
                "Unsupported bootstrap target '{other}'; expected one of: kind, kubeconfig, proxmox, aws, gcp, azure, hetzner"
            ))),
        }
    }
}

/// Kubernetes distribution used when provisioning VM-backed Stage 0 clusters (`SDD.md` §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootstrapDistro {
    #[default]
    K3s,
    Talos,
}

impl BootstrapDistro {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::K3s => "k3s",
            Self::Talos => "talos",
        }
    }
}

impl fmt::Display for BootstrapDistro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BootstrapDistro {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "k3s" => Ok(Self::K3s),
            "talos" => Ok(Self::Talos),
            other => Err(CoreError::Validation(format!(
                "Unsupported Kubernetes distro '{other}'; expected 'k3s' or 'talos'"
            ))),
        }
    }
}

/// Configuration for Stage 0 cluster bootstrapping (`keel bootstrap init`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapConfig {
    pub target: BootstrapTargetKind,
    #[serde(default = "default_cluster_name")]
    pub cluster_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default)]
    pub with_forge: bool,
    /// Deploy in-cluster Rauthy OIDC provider during Stage 0 (default `true`; skipped when `oidc_issuer` is set or `--oidc-provider none`).
    #[serde(default = "default_true")]
    pub with_oidc: bool,
    /// Optional external OIDC issuer URL (disables in-cluster Rauthy deployment when provided).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_issuer: Option<String>,
    /// Optional OIDC client ID (defaults to `keel-server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_client_id: Option<String>,
    /// Optional initial admin username/email for Stage 0 Rauthy seeding (defaults to `admin@keel.local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_username: Option<String>,
    /// Optional initial admin password for Stage 0 Rauthy seeding (auto-generated if omitted).
    /// Accepted on input but never serialized back out (`NFR-SEC-05`); redacted in `Debug`.
    #[serde(default, skip_serializing)]
    pub admin_password: Option<String>,
    /// Explicit opt-in for generating a Kyverno `PolicyException` allowing local container/Docker
    /// socket mounts strictly on local `kind` management clusters. Rejected on all non-`kind` targets.
    #[serde(default)]
    pub local_dev_docker_socket_exception: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxmox_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro: Option<BootstrapDistro>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulumi_program_dir: Option<PathBuf>,
}

impl std::fmt::Debug for BootstrapConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapConfig")
            .field("target", &self.target)
            .field("cluster_name", &self.cluster_name)
            .field("kubeconfig", &self.kubeconfig)
            .field("context", &self.context)
            .field("with_forge", &self.with_forge)
            .field("with_oidc", &self.with_oidc)
            .field("oidc_issuer", &self.oidc_issuer)
            .field("oidc_client_id", &self.oidc_client_id)
            .field("admin_username", &self.admin_username)
            .field(
                "admin_password",
                &self.admin_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "local_dev_docker_socket_exception",
                &self.local_dev_docker_socket_exception,
            )
            .field("proxmox_url", &self.proxmox_url)
            .field("region", &self.region)
            .field("distro", &self.distro)
            .field("pulumi_program_dir", &self.pulumi_program_dir)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

fn default_cluster_name() -> String {
    DEFAULT_CLUSTER_NAME.to_string()
}

impl BootstrapConfig {
    /// Creates a new `BootstrapConfig` for the given target with safe defaults.
    pub fn new(target: BootstrapTargetKind) -> Self {
        Self {
            target,
            cluster_name: default_cluster_name(),
            kubeconfig: None,
            context: None,
            with_forge: false,
            with_oidc: true,
            oidc_issuer: None,
            oidc_client_id: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: false,
            proxmox_url: None,
            region: None,
            distro: None,
            pulumi_program_dir: None,
        }
    }

    /// Validates configuration invariants and prevents CLI argument injection (`NFR-SEC-06`).
    pub fn validate(&self) -> Result<(), CoreError> {
        validate_dns_label(&self.cluster_name, "cluster_name")?;

        if self.local_dev_docker_socket_exception && self.target != BootstrapTargetKind::Kind {
            return Err(CoreError::Validation(format!(
                "local_dev_docker_socket_exception is only permitted for the 'kind' bootstrap target, not '{}'",
                self.target
            )));
        }

        if let Some(ctx) = &self.context {
            let trimmed = ctx.trim();
            if trimmed.is_empty() || trimmed.starts_with('-') {
                return Err(CoreError::Validation(
                    "kubeconfig context must be non-empty and must not start with '-'".to_string(),
                ));
            }
        }

        if let Some(path) = &self.kubeconfig {
            let path_str = path.to_string_lossy();
            if path_str.trim().is_empty() || path_str.starts_with('-') {
                return Err(CoreError::Validation(
                    "kubeconfig path must be non-empty and must not start with '-'".to_string(),
                ));
            }
        }

        if let Some(path) = &self.pulumi_program_dir {
            let path_str = path.to_string_lossy();
            if path_str.trim().is_empty() || path_str.starts_with('-') {
                return Err(CoreError::Validation(
                    "pulumi_program_dir must be non-empty and must not start with '-'".to_string(),
                ));
            }
        }

        if let Some(url) = &self.proxmox_url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(CoreError::Validation(
                "proxmox_url must start with 'https://' or 'http://'".to_string(),
            ));
        }

        if let Some(issuer) = &self.oidc_issuer {
            let trimmed = issuer.trim();
            if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
                return Err(CoreError::Validation(
                    "oidc_issuer must start with 'https://' or 'http://'".to_string(),
                ));
            }
        }

        if let Some(client_id) = &self.oidc_client_id
            && client_id.trim().is_empty()
        {
            return Err(CoreError::Validation(
                "oidc_client_id must not be empty".to_string(),
            ));
        }

        if let Some(username) = &self.admin_username
            && username.trim().is_empty()
        {
            return Err(CoreError::Validation(
                "admin_username must not be empty".to_string(),
            ));
        }

        if let Some(password) = &self.admin_password
            && password.trim().is_empty()
        {
            return Err(CoreError::Validation(
                "admin_password must not be empty".to_string(),
            ));
        }

        Ok(())
    }
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self::new(BootstrapTargetKind::Kind)
    }
}

/// Authentication state machine (`SDD.md` §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthPhase {
    #[default]
    OidcActive,
    BreakGlass,
}

/// Supported Git forge provider kinds (`SDD.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Forgejo,
    Gitea,
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
    #[default]
    Local,
}

/// Git repository and forge configuration inside `.keel/config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeConfig {
    pub provider: ForgeKind,
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    /// Outbound Flux polling interval (defaults to `1m` for NAT/firewall resilience).
    #[serde(default = "default_poll_interval")]
    pub poll_interval: String,
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_poll_interval() -> String {
    DEFAULT_POLL_INTERVAL.to_string()
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            provider: ForgeKind::Local,
            repo_url: String::new(),
            api_url: None,
            default_branch: default_branch(),
            poll_interval: default_poll_interval(),
        }
    }
}

/// OIDC PKCE provider configuration inside `.keel/config.yaml` (`NFR-SEC-01`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OidcConfig {
    #[serde(default)]
    pub enabled: bool,
    pub issuer_url: String,
    pub client_id: String,
    pub redirect_url: String,
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".to_string(),
        "profile".to_string(),
        "email".to_string(),
        "groups".to_string(),
    ]
}

/// S3-compatible Pulumi state backend configuration inside `.keel/config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S3BackendConfig {
    pub endpoint: String,
    #[serde(default = "default_s3_bucket")]
    pub bucket: String,
    #[serde(default = "default_s3_region")]
    pub region: String,
    #[serde(default)]
    pub force_path_style: bool,
}

fn default_s3_bucket() -> String {
    "pulumi-state".to_string()
}

fn default_s3_region() -> String {
    "us-east-1".to_string()
}

/// Repository-level control plane configuration stored at `.keel/config.yaml` (`SDD.md` §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeelConfig {
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default = "default_keel_config_kind")]
    pub kind: String,
    #[serde(default = "default_cluster_name")]
    pub cluster_name: String,
    #[serde(default = "default_bootstrap_target")]
    pub bootstrap_target: BootstrapTargetKind,
    #[serde(default)]
    pub auth_phase: AuthPhase,
    #[serde(default)]
    pub enabled_providers: Vec<String>,
    #[serde(default)]
    pub forge: ForgeConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3BackendConfig>,
}

fn default_api_version() -> String {
    CATALOG_API_VERSION.to_string()
}

fn default_keel_config_kind() -> String {
    "KeelConfig".to_string()
}

fn default_bootstrap_target() -> BootstrapTargetKind {
    BootstrapTargetKind::Kind
}

impl KeelConfig {
    /// Parses `.keel/config.yaml` and validates required invariants.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let cfg: Self = serde_yml::from_str(yaml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Serializes this `KeelConfig` to YAML.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        self.validate()?;
        Ok(serde_yml::to_string(self)?)
    }

    /// Validates `KeelConfig` fields.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.api_version != CATALOG_API_VERSION {
            return Err(CoreError::Validation(format!(
                "Unsupported KeelConfig apiVersion '{}', expected '{}'",
                self.api_version, CATALOG_API_VERSION
            )));
        }
        if self.kind != "KeelConfig" {
            return Err(CoreError::Validation(format!(
                "Invalid config kind '{}', expected 'KeelConfig'",
                self.kind
            )));
        }
        validate_dns_label(&self.cluster_name, "cluster_name")?;
        if self.forge.poll_interval.trim().is_empty() {
            return Err(CoreError::Validation(
                "forge.pollInterval must not be empty".to_string(),
            ));
        }
        if self.forge.default_branch.trim().is_empty() {
            return Err(CoreError::Validation(
                "forge.defaultBranch must not be empty".to_string(),
            ));
        }
        if let Some(oidc) = &self.oidc
            && oidc.enabled
            && (oidc.issuer_url.trim().is_empty() || oidc.client_id.trim().is_empty())
        {
            return Err(CoreError::Validation(
                "Enabled OIDC configuration requires non-empty issuerUrl and clientId".to_string(),
            ));
        }
        Ok(())
    }

    /// Checks whether a given provider ID is enabled in `.keel/config.yaml`.
    pub fn is_provider_enabled(&self, provider_id: &str) -> bool {
        self.enabled_providers
            .iter()
            .any(|p| p.eq_ignore_ascii_case(provider_id))
    }
}

impl Default for KeelConfig {
    fn default() -> Self {
        Self {
            api_version: default_api_version(),
            kind: default_keel_config_kind(),
            cluster_name: default_cluster_name(),
            bootstrap_target: BootstrapTargetKind::Kind,
            auth_phase: AuthPhase::OidcActive,
            enabled_providers: Vec::new(),
            forge: ForgeConfig::default(),
            oidc: None,
            s3: None,
        }
    }
}

/// Entry for an infrastructure provider inside `providers/registry.yaml` (`SDD.md` §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub version: String,
    /// Pulumi native plugin or OpenTofu/Terraform provider reference (e.g. `bpg/proxmox`, `pulumi/aws`).
    pub plugin: String,
    /// Whether this provider uses `pulumi package add terraform-provider`.
    #[serde(default)]
    pub terraform_bridge: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Declarative provider registry stored at `providers/registry.yaml` (`SDD.md` §3.1, §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRegistry {
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default = "default_provider_registry_kind")]
    pub kind: String,
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
}

fn default_provider_registry_kind() -> String {
    "ProviderRegistry".to_string()
}

impl ProviderRegistry {
    /// Returns the built-in Day-1 provider registry (`SDD.md` §4.1).
    pub fn default_day1() -> Self {
        Self {
            api_version: default_api_version(),
            kind: default_provider_registry_kind(),
            providers: vec![
                ProviderEntry {
                    id: "proxmox".to_string(),
                    name: "Proxmox VE".to_string(),
                    enabled: false,
                    version: "0.71.0".to_string(),
                    plugin: "bpg/proxmox".to_string(),
                    terraform_bridge: true,
                    description: Some(
                        "Proxmox VE VMs, LXC containers, and SDN networks".to_string(),
                    ),
                },
                ProviderEntry {
                    id: "aws".to_string(),
                    name: "Amazon Web Services".to_string(),
                    enabled: false,
                    version: "6.68.0".to_string(),
                    plugin: "pulumi/aws".to_string(),
                    terraform_bridge: false,
                    description: Some("AWS compute, EKS, RDS, and S3 infrastructure".to_string()),
                },
                ProviderEntry {
                    id: "gcp".to_string(),
                    name: "Google Cloud Platform".to_string(),
                    enabled: false,
                    version: "8.18.0".to_string(),
                    plugin: "pulumi/gcp".to_string(),
                    terraform_bridge: false,
                    description: Some("GCP Compute Engine, GKE, Cloud SQL, and GCS".to_string()),
                },
                ProviderEntry {
                    id: "azure".to_string(),
                    name: "Microsoft Azure".to_string(),
                    enabled: false,
                    version: "2.86.0".to_string(),
                    plugin: "pulumi/azure-native".to_string(),
                    terraform_bridge: false,
                    description: Some("Azure VMs, AKS, PostgreSQL, and Blob Storage".to_string()),
                },
                ProviderEntry {
                    id: "hetzner".to_string(),
                    name: "Hetzner Cloud".to_string(),
                    enabled: false,
                    version: "1.21.0".to_string(),
                    plugin: "pulumi/hcloud".to_string(),
                    terraform_bridge: false,
                    description: Some(
                        "Hetzner Cloud servers, networks, volumes, and firewalls".to_string(),
                    ),
                },
                ProviderEntry {
                    id: "cloudflare".to_string(),
                    name: "Cloudflare".to_string(),
                    enabled: false,
                    version: "5.49.0".to_string(),
                    plugin: "pulumi/cloudflare".to_string(),
                    terraform_bridge: false,
                    description: Some("Cloudflare DNS, Tunnels, and edge security".to_string()),
                },
                ProviderEntry {
                    id: "kind".to_string(),
                    name: "Kubernetes IN Docker (kind)".to_string(),
                    enabled: true,
                    version: "0.9.0".to_string(),
                    plugin: "tehcyx/kind".to_string(),
                    terraform_bridge: true,
                    description: Some(
                        "Local Docker-backed Kubernetes workload clusters via tehcyx/kind OpenTofu/Terraform bridge"
                            .to_string(),
                    ),
                },
            ],
        }
    }

    /// Parses and validates `providers/registry.yaml`.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let reg: Self = serde_yml::from_str(yaml)?;
        reg.validate()?;
        Ok(reg)
    }

    /// Serializes `ProviderRegistry` to YAML.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        self.validate()?;
        Ok(serde_yml::to_string(self)?)
    }

    /// Validates registry schema and uniqueness of provider IDs.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.api_version != CATALOG_API_VERSION {
            return Err(CoreError::Validation(format!(
                "Unsupported ProviderRegistry apiVersion '{}', expected '{}'",
                self.api_version, CATALOG_API_VERSION
            )));
        }
        if self.kind != "ProviderRegistry" {
            return Err(CoreError::Validation(format!(
                "Invalid registry kind '{}', expected 'ProviderRegistry'",
                self.kind
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &self.providers {
            validate_dns_label(&entry.id, "provider.id")?;
            if entry.plugin.trim().is_empty() || entry.version.trim().is_empty() {
                return Err(CoreError::Validation(format!(
                    "Provider '{}' must specify non-empty plugin and version",
                    entry.id
                )));
            }
            if !seen.insert(entry.id.as_str()) {
                return Err(CoreError::Validation(format!(
                    "Duplicate provider id '{}' in ProviderRegistry",
                    entry.id
                )));
            }
        }
        Ok(())
    }

    /// Returns all currently enabled providers.
    pub fn enabled_providers(&self) -> Vec<&ProviderEntry> {
        self.providers.iter().filter(|p| p.enabled).collect()
    }

    /// Returns `true` if `provider_id` exists and is enabled.
    pub fn is_enabled(&self, provider_id: &str) -> bool {
        self.providers
            .iter()
            .any(|p| p.id == provider_id && p.enabled)
    }

    /// Enables or disables an existing provider by ID. Returns `true` if found.
    pub fn set_enabled(&mut self, provider_id: &str, enabled: bool) -> bool {
        if let Some(entry) = self.providers.iter_mut().find(|p| p.id == provider_id) {
            entry.enabled = enabled;
            true
        } else {
            false
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::default_day1()
    }
}

/// Namespace resource quota specification for a tenant (`SDD.md` §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceQuotaSpec {
    pub cpu_requests: String,
    pub cpu_limits: String,
    pub memory_requests: String,
    pub memory_limits: String,
    pub max_pods: u32,
}

impl Default for ResourceQuotaSpec {
    fn default() -> Self {
        Self {
            cpu_requests: "4".to_string(),
            cpu_limits: "8".to_string(),
            memory_requests: "8Gi".to_string(),
            memory_limits: "16Gi".to_string(),
            max_pods: 50,
        }
    }
}

/// Multi-tenant isolation and policy configuration (`SDD.md` §6.2, §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantConfig {
    pub id: String,
    pub display_name: String,
    /// Dedicated Kubernetes namespace (`tenant-<id>`).
    pub namespace: String,
    /// Per-tenant SOPS `age` public key (`age1...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_public_key: Option<String>,
    /// Cloud / hypervisor providers permitted by Kyverno `restrict-pulumi-providers`.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// OIDC groups mapped to this tenant's RBAC bindings.
    #[serde(default)]
    pub oidc_groups: Vec<String>,
    /// Optional workload cluster `kubeconfig` Secret name in `tenant-<id>` (`spec.kubeConfig.secretRef`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_cluster_kubeconfig_secret: Option<String>,
    #[serde(default)]
    pub quotas: ResourceQuotaSpec,
}

impl TenantConfig {
    /// Constructs and validates a new `TenantConfig` mapping `id` to `tenant-<id>`.
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Result<Self, CoreError> {
        let id = id.into();
        let namespace = crate::tenant_namespace(&id)?;
        let cfg = Self {
            id,
            display_name: display_name.into(),
            namespace,
            age_public_key: None,
            allowed_providers: Vec::new(),
            oidc_groups: Vec::new(),
            target_cluster_kubeconfig_secret: None,
            quotas: ResourceQuotaSpec::default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validates strict namespace naming (`tenant-<id>`) and key hygiene (`NFR-SEC-02`, `NFR-SEC-05`).
    pub fn validate(&self) -> Result<(), CoreError> {
        let expected_ns = crate::tenant_namespace(&self.id)?;
        if self.namespace != expected_ns {
            return Err(CoreError::Validation(format!(
                "Tenant namespace '{}' does not match required isolation boundary '{}'",
                self.namespace, expected_ns
            )));
        }
        if self.display_name.trim().is_empty() {
            return Err(CoreError::Validation(
                "Tenant display_name must not be empty".to_string(),
            ));
        }
        if let Some(pub_key) = &self.age_public_key
            && (!pub_key.starts_with("age1") || pub_key.chars().any(char::is_whitespace))
        {
            return Err(CoreError::Validation(
                "Tenant age_public_key must be a valid 'age1...' recipient without whitespace"
                    .to_string(),
            ));
        }
        for provider in &self.allowed_providers {
            validate_dns_label(provider, "allowed_provider")?;
        }
        Ok(())
    }

    /// Returns the `.sops.yaml` `path_regex` scoping encrypted secrets for this tenant.
    pub fn sops_path_regex(&self) -> String {
        format!(r"^tenants/{}/.*\.enc\.yaml$", self.id)
    }

    /// Checks whether this tenant is permitted to instantiate the specified Pulumi provider.
    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        self.allowed_providers
            .iter()
            .any(|p| p.eq_ignore_ascii_case(provider_id))
    }

    /// Parses and validates a `TenantConfig` from YAML.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let cfg: Self = serde_yml::from_str(yaml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Serializes this `TenantConfig` to YAML.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        self.validate()?;
        Ok(serde_yml::to_string(self)?)
    }
}

/// Infrastructure stack resource classification used for dashboard grouping,
/// SSH capability gating, and deployment target cluster filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InfraResourceKind {
    #[serde(
        alias = "VM",
        alias = "Vm",
        alias = "compute",
        alias = "virtual-machine"
    )]
    Vm,
    #[serde(
        alias = "Cluster",
        alias = "CLUSTER",
        alias = "kubernetes-cluster",
        alias = "kubernetes",
        alias = "k8s"
    )]
    Cluster,
    #[serde(
        alias = "Service",
        alias = "SERVICE",
        alias = "database",
        alias = "config",
        alias = "progressive-delivery"
    )]
    Service,
}

impl InfraResourceKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Vm => "vm",
            Self::Cluster => "cluster",
            Self::Service => "service",
        }
    }
}

impl fmt::Display for InfraResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InfraResourceKind {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vm" | "virtual-machine" | "compute" => Ok(Self::Vm),
            "cluster" | "kubernetes-cluster" | "kubernetes" | "k8s" => Ok(Self::Cluster),
            "service" | "database" | "config" | "progressive-delivery" => Ok(Self::Service),
            other => Err(CoreError::Validation(format!(
                "Unsupported InfraResourceKind '{other}'"
            ))),
        }
    }
}

/// Application packaging runtime supported by Flux CD and PKO (`SDD.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppRuntime {
    #[serde(alias = "Helm", alias = "HELM")]
    Helm,
    #[serde(alias = "Kustomize", alias = "KUSTOMIZE")]
    Kustomize,
    #[serde(alias = "Manifest", alias = "MANIFEST")]
    Manifest,
}

impl AppRuntime {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Helm => "helm",
            Self::Kustomize => "kustomize",
            Self::Manifest => "manifest",
        }
    }
}

impl fmt::Display for AppRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AppRuntime {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "helm" => Ok(Self::Helm),
            "kustomize" => Ok(Self::Kustomize),
            "manifest" => Ok(Self::Manifest),
            other => Err(CoreError::Validation(format!(
                "Unsupported application runtime '{other}'; expected 'helm', 'kustomize', or 'manifest'"
            ))),
        }
    }
}

/// Reconciliation engine for raw Kubernetes manifest catalog applications (`SDD.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestEngine {
    #[default]
    #[serde(alias = "Flux", alias = "FLUX")]
    Flux,
    #[serde(alias = "Pulumi", alias = "PULUMI")]
    Pulumi,
}

impl ManifestEngine {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Flux => "flux",
            Self::Pulumi => "pulumi",
        }
    }
}

impl fmt::Display for ManifestEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ManifestEngine {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "flux" => Ok(Self::Flux),
            "pulumi" => Ok(Self::Pulumi),
            other => Err(CoreError::Validation(format!(
                "Unsupported manifest engine '{other}'; expected 'flux' or 'pulumi'"
            ))),
        }
    }
}

/// Metadata block for `CatalogApplication` (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogAppMetadata {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub category: String,
    pub runtime: AppRuntime,
    #[serde(default, skip_serializing_if = "is_false")]
    pub protected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Unmodified upstream Helm chart or Kustomize repository reference (`NFR-EXT-01`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpstreamSource {
    #[serde(default, rename = "repoURL", alias = "repoUrl", alias = "repo_url")]
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Container image repository (no tag/digest) of an image-based `runtime: kustomize`
    /// add-on, e.g. `ghcr.io/sebadob/rauthy`; `version` is then the image tag pinned through the
    /// Flux Kustomization `spec.images[].newTag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// `metadata.category` of management-plane base add-ons (`catalog/core/<id>`), reconciled by one
/// Flux object in `clusters/management/core-addons/` rather than installed into a tenant.
pub const MANAGEMENT_CORE_CATEGORY: &str = "management-core";

/// Validates an upstream image repository reference: non-empty, no whitespace/control
/// characters, and no tag or digest (the tag is carried by `spec.upstream.version`).
fn validate_upstream_image(image: &str) -> Result<(), CoreError> {
    let invalid = |why: &str| {
        Err(CoreError::Validation(format!(
            "spec.upstream.image '{}' is invalid: {why}",
            image.escape_debug()
        )))
    };
    if image.is_empty() || image.len() > 255 {
        return invalid("must be 1-255 characters");
    }
    if image
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c.is_ascii_uppercase())
    {
        return invalid("must be a lowercase image repository without whitespace");
    }
    if image.contains('@') {
        return invalid("must not contain a digest");
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    if last.contains(':') {
        return invalid("must not contain a tag (use spec.upstream.version)");
    }
    if image.starts_with('/') || image.ends_with('/') || image.contains("//") {
        return invalid("must be a repository path such as ghcr.io/org/name");
    }
    Ok(())
}

/// Flagger + Envoy Gateway progressive delivery capability settings (`SDD.md` §5.1, §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressiveDeliverySpec {
    #[serde(default)]
    pub flagger_supported: bool,
    #[serde(default)]
    pub default_canary: bool,
}

/// Companion PKO infrastructure stack hook bound atomically in the same PR (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionInfraHook {
    pub blueprint: String,
    #[serde(default)]
    pub bind_values: BTreeMap<String, serde_json::Value>,
}

/// Specification block for `CatalogApplication`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogAppSpec {
    #[serde(default)]
    pub upstream: UpstreamSource,
    #[serde(default)]
    pub progressive_delivery: ProgressiveDeliverySpec,
    #[serde(default)]
    pub companion_infrastructure: Vec<CompanionInfraHook>,
    pub schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_schema: Option<serde_json::Value>,
    /// Optional external/ingress URL for accessing this service (e.g., admin UI on Management Plane Base Addons).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url: Option<String>,
    /// Optional default reconciliation engine (`flux` or `pulumi`) when `metadata.runtime == "manifest"`.
    #[serde(
        default,
        alias = "default_engine",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_engine: Option<ManifestEngine>,
}

/// Git-stored `keel.io/v1alpha1` `CatalogApplication` definition (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogApplication {
    pub api_version: String,
    pub kind: String,
    pub metadata: CatalogAppMetadata,
    pub spec: CatalogAppSpec,
}

impl CatalogApplication {
    /// Validates `CatalogApplication` envelope, metadata, upstream contract, and progressive delivery invariants.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.api_version != CATALOG_API_VERSION {
            return Err(CoreError::Validation(format!(
                "Invalid apiVersion '{}', expected '{}'",
                self.api_version, CATALOG_API_VERSION
            )));
        }
        if self.kind != CATALOG_APPLICATION_KIND {
            return Err(CoreError::Validation(format!(
                "Invalid kind '{}', expected '{}'",
                self.kind, CATALOG_APPLICATION_KIND
            )));
        }
        validate_dns_label(&self.metadata.id, "metadata.id")?;
        if self.metadata.name.trim().is_empty() {
            return Err(CoreError::Validation(
                "metadata.name must not be empty".to_string(),
            ));
        }
        if self.metadata.category.trim().is_empty() {
            return Err(CoreError::Validation(
                "metadata.category must not be empty".to_string(),
            ));
        }
        if let Some(ref t) = self.metadata.tenant {
            validate_dns_label(t, "metadata.tenant")?;
        }
        if let Some(image) = self.spec.upstream.image.as_deref() {
            // Only management base add-ons are image-based (a Flux Kustomization pinning
            // `spec.images[].newTag`); the self-service tenant install path synthesizes
            // HelmRelease/HelmRepository objects from `repoURL` and cannot install them.
            if self.metadata.category != MANAGEMENT_CORE_CATEGORY {
                return Err(CoreError::Validation(format!(
                    "spec.upstream.image is only supported for metadata.category: \
                     {MANAGEMENT_CORE_CATEGORY} add-ons"
                )));
            }
            if self.metadata.runtime != AppRuntime::Kustomize {
                return Err(CoreError::Validation(
                    "spec.upstream.image is only supported for runtime: kustomize".to_string(),
                ));
            }
            validate_upstream_image(image)?;
            if self
                .spec
                .upstream
                .version
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
            {
                return Err(CoreError::Validation(
                    "spec.upstream.image requires spec.upstream.version (the image tag)"
                        .to_string(),
                ));
            }
        }
        // Image-based kustomize add-ons are sourced from the image registry, not a repository.
        let image_based = self.spec.upstream.image.is_some();
        if self.metadata.runtime != AppRuntime::Manifest
            && !image_based
            && self.spec.upstream.repo_url.trim().is_empty()
        {
            return Err(CoreError::Validation(
                "spec.upstream.repoURL must not be empty".to_string(),
            ));
        }
        if self.metadata.runtime == AppRuntime::Helm
            && self
                .spec
                .upstream
                .chart
                .as_deref()
                .is_none_or(|c| c.trim().is_empty())
        {
            return Err(CoreError::Validation(
                "Helm CatalogApplication requires spec.upstream.chart".to_string(),
            ));
        }
        if self.spec.progressive_delivery.default_canary
            && !self.spec.progressive_delivery.flagger_supported
        {
            return Err(CoreError::Validation(
                "spec.progressiveDelivery.defaultCanary cannot be true when flaggerSupported is false"
                    .to_string(),
            ));
        }
        if !self.spec.schema.is_object() {
            return Err(CoreError::Validation(
                "spec.schema must be a JSON Schema object".to_string(),
            ));
        }
        for hook in &self.spec.companion_infrastructure {
            validate_dns_label(&hook.blueprint, "companionInfrastructure.blueprint")?;
        }
        Ok(())
    }
}

/// Metadata block for `CatalogInfraBlueprint` (`SDD.md` §3.1, §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBlueprintMetadata {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub category: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_kind: Option<InfraResourceKind>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub protected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

fn default_pulumi_runtime() -> String {
    "yaml".to_string()
}

fn default_pulumi_entrypoint() -> String {
    "Pulumi.yaml".to_string()
}

/// Specification block for `CatalogInfraBlueprint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBlueprintSpec {
    #[serde(default = "default_pulumi_runtime")]
    pub pulumi_runtime: String,
    #[serde(default = "default_pulumi_entrypoint")]
    pub entrypoint: String,
    #[serde(default)]
    pub protect_by_default: bool,
    #[serde(default)]
    pub outputs: Vec<String>,
    pub schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_schema: Option<serde_json::Value>,
}

/// Git-stored `keel.io/v1alpha1` `CatalogInfraBlueprint` definition (`SDD.md` §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogInfraBlueprint {
    pub api_version: String,
    pub kind: String,
    pub metadata: CatalogBlueprintMetadata,
    pub spec: CatalogBlueprintSpec,
}

impl CatalogInfraBlueprint {
    /// Validates `CatalogInfraBlueprint` envelope, metadata, and schema object invariants.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.api_version != CATALOG_API_VERSION {
            return Err(CoreError::Validation(format!(
                "Invalid apiVersion '{}', expected '{}'",
                self.api_version, CATALOG_API_VERSION
            )));
        }
        if self.kind != CATALOG_INFRA_BLUEPRINT_KIND {
            return Err(CoreError::Validation(format!(
                "Invalid kind '{}', expected '{}'",
                self.kind, CATALOG_INFRA_BLUEPRINT_KIND
            )));
        }
        validate_dns_label(&self.metadata.id, "metadata.id")?;
        validate_dns_label(&self.metadata.provider, "metadata.provider")?;
        if self.metadata.name.trim().is_empty() {
            return Err(CoreError::Validation(
                "metadata.name must not be empty".to_string(),
            ));
        }
        if self.metadata.category.trim().is_empty() {
            return Err(CoreError::Validation(
                "metadata.category must not be empty".to_string(),
            ));
        }
        if let Some(ref t) = self.metadata.tenant {
            validate_dns_label(t, "metadata.tenant")?;
        }
        if self.spec.entrypoint.trim().is_empty() {
            return Err(CoreError::Validation(
                "spec.entrypoint must not be empty".to_string(),
            ));
        }
        if !self.spec.schema.is_object() {
            return Err(CoreError::Validation(
                "spec.schema must be a JSON Schema object".to_string(),
            ));
        }
        Ok(())
    }

    pub fn effective_resource_kind(&self) -> InfraResourceKind {
        if let Some(k) = self.metadata.resource_kind {
            return k;
        }
        if self.spec.outputs.iter().any(|o| o == "kubeconfig")
            || matches!(
                self.metadata.category.to_ascii_lowercase().as_str(),
                "kubernetes-cluster" | "kubernetes" | "cluster" | "k8s"
            )
            || self.metadata.provider.eq_ignore_ascii_case("kind")
        {
            InfraResourceKind::Cluster
        } else if self.spec.outputs.iter().any(|o| o == "sshHost")
            || matches!(
                self.metadata.category.to_ascii_lowercase().as_str(),
                "compute" | "vm" | "virtual-machine"
            )
        {
            InfraResourceKind::Vm
        } else {
            InfraResourceKind::Service
        }
    }
}

/// Item kind inside a `CatalogBundle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleItemKind {
    Infra,
    App,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Single step/item inside a `CatalogBundle` (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBundleItem {
    pub id: String,
    pub kind: BundleItemKind,
    pub catalog_ref: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_cluster_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_namespace: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub canary: bool,
    #[serde(default)]
    pub bind_values: BTreeMap<String, serde_json::Value>,
}

/// Metadata block for `CatalogBundle` (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBundleMetadata {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub category: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub protected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Specification block for `CatalogBundle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBundleSpec {
    pub items: Vec<CatalogBundleItem>,
    pub schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_schema: Option<serde_json::Value>,
}

/// Maximum allowed number of items inside a `CatalogBundle`.
pub const MAX_BUNDLE_ITEMS: usize = 1000;

/// Git-stored `keel.io/v1alpha1` `CatalogBundle` definition composing infra + app items with DAG ordering (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBundle {
    pub api_version: String,
    pub kind: String,
    pub metadata: CatalogBundleMetadata,
    pub spec: CatalogBundleSpec,
}

impl CatalogBundle {
    /// Validates `CatalogBundle` envelope, metadata, item references, `targetClusterFrom` rules, and acyclic DAG.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.api_version != CATALOG_API_VERSION {
            return Err(CoreError::Validation(format!(
                "Invalid apiVersion '{}', expected '{}'",
                self.api_version, CATALOG_API_VERSION
            )));
        }
        if self.kind != CATALOG_BUNDLE_KIND {
            return Err(CoreError::Validation(format!(
                "Invalid kind '{}', expected '{}'",
                self.kind, CATALOG_BUNDLE_KIND
            )));
        }
        validate_dns_label(&self.metadata.id, "metadata.id")?;
        if self.metadata.name.trim().is_empty() {
            return Err(CoreError::Validation(
                "metadata.name must not be empty".to_string(),
            ));
        }
        if self.metadata.category.trim().is_empty() {
            return Err(CoreError::Validation(
                "metadata.category must not be empty".to_string(),
            ));
        }
        if let Some(ref t) = self.metadata.tenant {
            validate_dns_label(t, "metadata.tenant")?;
        }
        if !self.spec.schema.is_object() {
            return Err(CoreError::Validation(
                "spec.schema must be a JSON Schema object".to_string(),
            ));
        }
        if self.spec.items.is_empty() {
            return Err(CoreError::Validation(
                "spec.items must contain at least one item".to_string(),
            ));
        }
        if self.spec.items.len() > MAX_BUNDLE_ITEMS {
            return Err(CoreError::Validation(format!(
                "spec.items exceeds maximum allowed limit of {MAX_BUNDLE_ITEMS} items"
            )));
        }

        let mut by_id = BTreeMap::new();
        for item in &self.spec.items {
            validate_dns_label(&item.id, "spec.items[].id")?;
            validate_dns_label(&item.catalog_ref, "spec.items[].catalogRef")?;
            if let Some(ns) = &item.target_namespace {
                validate_dns_label(ns, "spec.items[].targetNamespace")?;
            }
            if item.canary && item.kind != BundleItemKind::App {
                return Err(CoreError::Validation(format!(
                    "Bundle item '{}' sets canary: true, which is only valid for kind: app",
                    item.id
                )));
            }
            if by_id.insert(item.id.as_str(), item).is_some() {
                return Err(CoreError::Validation(format!(
                    "Duplicate bundle item id '{}'",
                    item.id
                )));
            }
        }

        for item in &self.spec.items {
            for dep in &item.depends_on {
                if dep == &item.id {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' cannot depend on itself",
                        item.id
                    )));
                }
                if !by_id.contains_key(dep.as_str()) {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' depends on unknown item '{}'",
                        item.id, dep
                    )));
                }
            }
            if let Some(cluster_item_id) = &item.target_cluster_from {
                if cluster_item_id == &item.id {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' cannot reference itself in targetClusterFrom",
                        item.id
                    )));
                }
                let Some(cluster_item) = by_id.get(cluster_item_id.as_str()) else {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' targetClusterFrom references unknown item '{}'",
                        item.id, cluster_item_id
                    )));
                };
                if cluster_item.kind != BundleItemKind::Infra {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' targetClusterFrom '{}' must reference an item of kind: infra",
                        item.id, cluster_item_id
                    )));
                }
                if !item.depends_on.iter().any(|d| d == cluster_item_id) {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' must include '{}' in dependsOn when targetClusterFrom is set",
                        item.id, cluster_item_id
                    )));
                }
            }
        }

        let _ = self.topological_order()?;
        Ok(())
    }

    /// Returns bundle items in deterministic topological order (dependencies before dependents),
    /// returning a validation error if a dependency cycle exists.
    pub fn topological_order(&self) -> Result<Vec<&CatalogBundleItem>, CoreError> {
        let mut in_degree: BTreeMap<&str, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut by_id: BTreeMap<&str, &CatalogBundleItem> = BTreeMap::new();

        for item in &self.spec.items {
            by_id.insert(item.id.as_str(), item);
            in_degree.entry(item.id.as_str()).or_insert(0);
            dependents.entry(item.id.as_str()).or_default();
        }

        for item in &self.spec.items {
            for dep in &item.depends_on {
                if let Some(deg) = in_degree.get_mut(item.id.as_str()) {
                    *deg += 1;
                }
                if let Some(list) = dependents.get_mut(dep.as_str()) {
                    list.push(item.id.as_str());
                }
            }
        }

        let mut queue: std::collections::VecDeque<&str> = in_degree
            .iter()
            .filter_map(|(id, deg)| if *deg == 0 { Some(*id) } else { None })
            .collect();
        let items_capacity = self.spec.items.len().min(MAX_BUNDLE_ITEMS);
        let mut ordered = Vec::with_capacity(items_capacity);

        while let Some(id) = queue.pop_front() {
            if let Some(item) = by_id.get(id) {
                ordered.push(*item);
            }
            if let Some(nexts) = dependents.get(id) {
                for next in nexts {
                    if let Some(deg) = in_degree.get_mut(next) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(next);
                        }
                    }
                }
            }
        }

        if ordered.len() != self.spec.items.len() {
            return Err(CoreError::Validation(
                "CatalogBundle spec.items contains a dependency cycle".to_string(),
            ));
        }

        Ok(ordered)
    }
}

/// Sum type representing a `CatalogApplication`, `CatalogInfraBlueprint`, or `CatalogBundle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogManifest {
    Application(Box<CatalogApplication>),
    InfraBlueprint(Box<CatalogInfraBlueprint>),
    Bundle(Box<CatalogBundle>),
}

impl CatalogManifest {
    pub fn is_protected(&self) -> bool {
        match self {
            Self::Application(app) => app.metadata.protected,
            Self::InfraBlueprint(bp) => bp.metadata.protected,
            Self::Bundle(b) => b.metadata.protected,
        }
    }

    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::Application(app) => app.metadata.tenant.as_deref(),
            Self::InfraBlueprint(bp) => bp.metadata.tenant.as_deref(),
            Self::Bundle(b) => b.metadata.tenant.as_deref(),
        }
    }
}

/// Resolved SSH connection metadata for a provisioned VM infrastructure stack (`GET /api/v1/stacks/{namespace}/{name}/ssh-info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackSshInfoResponse {
    pub stack_name: String,
    pub namespace: String,
    pub tenant: String,
    pub provider: String,
    pub ssh_host: String,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub ssh_command: String,
    pub cli_command: String,
    pub cli_exec_command: String,
    pub has_saved_ssh_key: bool,
}

/// Returns the default SSH login user for a cloud/virtualization provider.
pub fn default_ssh_user_for_provider(provider: &str) -> &'static str {
    match provider.trim().to_ascii_lowercase().as_str() {
        "aws" | "proxmox" => "ubuntu",
        "azure" => "azureuser",
        "gcp" => "debian",
        "hetzner" => "root",
        _ => "ubuntu",
    }
}

/// Returns `true` if `ip` is not a publicly routable unicast address: loopback, unspecified,
/// link-local (`169.254.0.0/16` incl. the `169.254.169.254` cloud metadata endpoint,
/// `fe80::/10`), RFC 1918 private (`10/8`, `172.16/12`, `192.168/16`), CGNAT (`100.64/10`),
/// IPv6 unique-local (`fc00::/7`), multicast, broadcast, or "this network" (`0.0.0.0/8`).
/// IPv4-mapped IPv6 addresses are checked as IPv4.
///
/// Used to vet **client-supplied** SSH targets (and their DNS resolutions) so the SSH proxy cannot
/// be abused to reach the control-plane's internal networks. Stack-resolved targets from Pulumi
/// outputs / GitOps manifests are trusted and are *not* subject to this check (Proxmox/kind VMs
/// legitimately live on RFC 1918 networks).
pub fn is_private_or_internal_ip(ip: std::net::IpAddr) -> bool {
    fn v4_internal(v4: std::net::Ipv4Addr) -> bool {
        let o = v4.octets();
        v4.is_loopback()
            || v4.is_unspecified()
            || v4.is_link_local()
            || v4.is_private()
            || v4.is_multicast()
            || v4.is_broadcast()
            || o[0] == 0
            || (o[0] == 100 && (o[1] & 0xc0) == 64)
    }
    match ip {
        std::net::IpAddr::V4(v4) => v4_internal(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_internal(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
                || v6.is_multicast()
        }
    }
}

/// Returns `true` if `host` is a name or address that must never be targeted by a
/// **client-supplied** SSH host override: `localhost`/`*.localhost`, mDNS/internal suffixes
/// (`.local`, `.internal`, `.cluster.local`, `.svc`, `.svc.cluster.local`), single-label names
/// (resolved via cluster search domains, e.g. `kubernetes`), or any IP literal rejected by
/// [`is_private_or_internal_ip`].
///
/// DNS names that pass this check must additionally be resolved and every resolved address
/// re-checked with [`is_private_or_internal_ip`] immediately before connecting (DNS rebinding).
pub fn is_private_or_internal_ssh_host(host: &str) -> bool {
    let lower = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if lower.is_empty()
        || lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower.ends_with(".cluster.local")
        || lower.ends_with(".svc")
    {
        return true;
    }
    if let Ok(ip) = lower.parse::<std::net::IpAddr>() {
        return is_private_or_internal_ip(ip);
    }
    // Single-label hostnames resolve through the pod's cluster search domains
    // (`<svc>.<ns>.svc.cluster.local`), so they are treated as cluster-internal.
    !lower.contains('.')
}

/// Returns `true` when `info.ssh_host` is the synthetic `<stack>.<tenant>.local` fallback produced
/// by [`resolve_ssh_target_from_sources`] because no host could be resolved from Pulumi outputs or
/// GitOps manifests.
pub fn is_unresolved_ssh_target(info: &StackSshInfoResponse) -> bool {
    info.ssh_host == format!("{}.{}.local", info.stack_name, info.tenant)
}

/// Output / config keys that may carry the expected SSH host key (an OpenSSH public key line such
/// as `ssh-ed25519 AAAA...`, or a `SHA256:<base64>` fingerprint).
const SSH_HOST_KEY_KEYS: &[&str] = &[
    "sshHostKeyFingerprint",
    "ssh_host_key_fingerprint",
    "sshHostKey",
    "ssh_host_key",
    "hostKeyFingerprint",
    "hostKey",
];

/// Resolves the expected SSH host key / fingerprint for a stack from live Pulumi outputs, then
/// `values.yaml`, then `stack.yaml` `spec.config`. Returns the raw trimmed value (validation and
/// fingerprint normalization happen in the SSH transport layer).
pub fn resolve_ssh_host_key_from_sources(
    live_outputs: &std::collections::HashMap<String, String>,
    values_doc: Option<&serde_json::Value>,
    stack_doc: Option<&serde_json::Value>,
) -> Option<String> {
    fn clean(raw: &str) -> Option<String> {
        let t = raw.trim().trim_matches('"').trim_matches('\'').trim();
        if t.is_empty() || t.starts_with("${") || t.eq_ignore_ascii_case("null") {
            None
        } else {
            Some(t.to_string())
        }
    }
    for key in SSH_HOST_KEY_KEYS {
        if let Some(v) = live_outputs.get(*key).and_then(|v| clean(v)) {
            return Some(v);
        }
    }
    let objects = [
        values_doc.and_then(serde_json::Value::as_object),
        stack_doc
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(serde_json::Value::as_object),
    ];
    for obj in objects.into_iter().flatten() {
        for key in SSH_HOST_KEY_KEYS {
            if let Some(v) = obj
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .and_then(clean)
            {
                return Some(v);
            }
        }
    }
    None
}

fn is_loopback_or_link_local(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower == "localhost"
        || lower == "::1"
        || lower == "0.0.0.0"
        || lower.starts_with("127.")
        || lower.starts_with("169.254.")
}

fn clean_single_ssh_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("dhcp")
        || trimmed.eq_ignore_ascii_case("auto")
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("undefined")
        || trimmed.starts_with("${")
        || trimmed.starts_with('-')
    {
        return None;
    }
    let host_part = match trimmed.split_once('/') {
        Some((ip, prefix)) if prefix.chars().all(|c| c.is_ascii_digit()) => {
            if ip.ends_with(".0") {
                return None;
            }
            ip.trim()
        }
        Some(_) => return None,
        None => trimmed,
    };
    if host_part.is_empty()
        || host_part.contains(char::is_whitespace)
        || host_part.contains(['[', ']', '{', '}'])
    {
        return None;
    }
    Some(host_part.to_string())
}

fn collect_hosts_from_json_value(val: &serde_json::Value, out: &mut Vec<String>) {
    match val {
        serde_json::Value::String(s) => {
            if let Some(cleaned) = clean_single_ssh_host(s) {
                out.push(cleaned);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_hosts_from_json_value(item, out);
            }
        }
        _ => {}
    }
}

fn pick_preferred_host(candidates: Vec<String>) -> Option<String> {
    candidates
        .iter()
        .find(|h| !is_loopback_or_link_local(h))
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

/// Sanitizes and extracts a candidate SSH hostname or IP address from a raw string or JSON array string.
pub fn clean_candidate_ssh_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if trimmed.starts_with('[')
        && let Ok(json_val) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let mut candidates = Vec::new();
        collect_hosts_from_json_value(&json_val, &mut candidates);
        return pick_preferred_host(candidates);
    }
    clean_single_ssh_host(trimmed)
}

fn extract_ssh_host_from_yaml_value(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => clean_candidate_ssh_host(s),
        serde_json::Value::Array(_) => {
            let mut candidates = Vec::new();
            collect_hosts_from_json_value(val, &mut candidates);
            pick_preferred_host(candidates)
        }
        _ => None,
    }
}

/// Sanitizes a candidate SSH username, rejecting empty strings, template expressions, flags, or whitespace.
pub fn clean_candidate_ssh_user(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if trimmed.is_empty()
        || trimmed.starts_with("${")
        || trimmed.starts_with('-')
        || trimmed.contains(char::is_whitespace)
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn parse_candidate_ssh_port(val: &serde_json::Value) -> Option<u16> {
    match val {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0),
        serde_json::Value::String(s) => s
            .trim()
            .trim_matches('"')
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0),
        _ => None,
    }
}

/// Resolves SSH target connection details (`StackSshInfoResponse`) from live Pulumi outputs
/// and GitOps manifests (`values.yaml`, `stack.yaml`, `Pulumi.yaml`).
#[allow(clippy::too_many_arguments)]
pub fn resolve_ssh_target_from_sources(
    stack_name: &str,
    namespace: &str,
    tenant: &str,
    provider_override: Option<String>,
    live_outputs: &std::collections::HashMap<String, String>,
    values_doc: Option<&serde_json::Value>,
    stack_doc: Option<&serde_json::Value>,
    pulumi_doc: Option<&serde_json::Value>,
    has_saved_ssh_key: bool,
) -> StackSshInfoResponse {
    let provider = stack_doc
        .and_then(|d| {
            d.pointer("/metadata/labels/keel.io~1provider")
                .or_else(|| d.pointer("/spec/config/keel:provider"))
        })
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| provider_override.filter(|p| !p.trim().is_empty()))
        .unwrap_or_else(|| {
            let lower = stack_name.to_ascii_lowercase();
            for candidate in ["proxmox", "aws", "gcp", "azure", "hetzner", "kind"] {
                if lower.contains(candidate) {
                    return candidate.to_string();
                }
            }
            "proxmox".to_string()
        });

    const HOST_KEYS: &[&str] = &[
        "sshHost",
        "ssh_host",
        "externalIp",
        "externalHostname",
        "publicIp",
        "public_ip",
        "controlPlanePublicIp",
        "controlPlaneIp",
        "ipv4Address",
        "ipv4Addresses",
        "ipAddress",
        "ip_address",
        "hostname",
        "host",
        "ip",
    ];
    let mut ssh_host: Option<String> = None;
    for key in HOST_KEYS {
        if let Some(val) = live_outputs.get(*key)
            && let Some(cleaned) = clean_candidate_ssh_host(val)
        {
            ssh_host = Some(cleaned);
            break;
        }
    }
    if ssh_host.is_none()
        && let Some(obj) = values_doc.and_then(serde_json::Value::as_object)
    {
        for key in HOST_KEYS {
            if let Some(val) = obj.get(*key)
                && let Some(cleaned) = extract_ssh_host_from_yaml_value(val)
            {
                ssh_host = Some(cleaned);
                break;
            }
        }
    }
    if ssh_host.is_none()
        && let Some(obj) = stack_doc
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(serde_json::Value::as_object)
    {
        for key in HOST_KEYS {
            if let Some(val) = obj.get(*key)
                && let Some(cleaned) = extract_ssh_host_from_yaml_value(val)
            {
                ssh_host = Some(cleaned);
                break;
            }
        }
    }
    if ssh_host.is_none()
        && let Some(obj) = pulumi_doc
            .and_then(|d| d.get("outputs"))
            .and_then(serde_json::Value::as_object)
    {
        for key in HOST_KEYS {
            if let Some(val) = obj.get(*key)
                && let Some(cleaned) = extract_ssh_host_from_yaml_value(val)
            {
                ssh_host = Some(cleaned);
                break;
            }
        }
    }
    let ssh_host = ssh_host.unwrap_or_else(|| format!("{stack_name}.{tenant}.local"));

    const USER_KEYS: &[&str] = &["sshUser", "ciuser", "username", "user", "adminUsername"];
    let mut ssh_user: Option<String> = None;
    for key in USER_KEYS {
        if let Some(val) = live_outputs.get(*key)
            && let Some(cleaned) = clean_candidate_ssh_user(val)
        {
            ssh_user = Some(cleaned);
            break;
        }
    }
    if ssh_user.is_none()
        && let Some(obj) = values_doc.and_then(serde_json::Value::as_object)
    {
        for key in USER_KEYS {
            if let Some(val) = obj.get(*key).and_then(serde_json::Value::as_str)
                && let Some(cleaned) = clean_candidate_ssh_user(val)
            {
                ssh_user = Some(cleaned);
                break;
            }
        }
    }
    if ssh_user.is_none()
        && let Some(obj) = stack_doc
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(serde_json::Value::as_object)
    {
        for key in USER_KEYS {
            if let Some(val) = obj.get(*key).and_then(serde_json::Value::as_str)
                && let Some(cleaned) = clean_candidate_ssh_user(val)
            {
                ssh_user = Some(cleaned);
                break;
            }
        }
    }
    let ssh_user = ssh_user.unwrap_or_else(|| default_ssh_user_for_provider(&provider).to_string());

    const PORT_KEYS: &[&str] = &["sshPort", "port"];
    let mut ssh_port: Option<u16> = None;
    for key in PORT_KEYS {
        if let Some(val) = live_outputs.get(*key)
            && let Some(p) = parse_candidate_ssh_port(&serde_json::Value::String(val.clone()))
        {
            ssh_port = Some(p);
            break;
        }
    }
    if ssh_port.is_none()
        && let Some(obj) = values_doc.and_then(serde_json::Value::as_object)
    {
        for key in PORT_KEYS {
            if let Some(val) = obj.get(*key)
                && let Some(p) = parse_candidate_ssh_port(val)
            {
                ssh_port = Some(p);
                break;
            }
        }
    }
    if ssh_port.is_none()
        && let Some(obj) = stack_doc
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(serde_json::Value::as_object)
    {
        for key in PORT_KEYS {
            if let Some(val) = obj.get(*key)
                && let Some(p) = parse_candidate_ssh_port(val)
            {
                ssh_port = Some(p);
                break;
            }
        }
    }
    let ssh_port = ssh_port.unwrap_or(22);

    let ssh_command = if ssh_port == 22 {
        format!("ssh {ssh_user}@{ssh_host}")
    } else {
        format!("ssh -p {ssh_port} {ssh_user}@{ssh_host}")
    };
    let cli_command = format!("keel infra ssh {stack_name} --tenant {tenant}");
    let cli_exec_command = format!("keel infra ssh {stack_name} --tenant {tenant} --exec");

    StackSshInfoResponse {
        stack_name: stack_name.to_string(),
        namespace: namespace.to_string(),
        tenant: tenant.to_string(),
        provider,
        ssh_host,
        ssh_user,
        ssh_port,
        ssh_command,
        cli_command,
        cli_exec_command,
        has_saved_ssh_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_target_kind_roundtrip_and_traits() {
        for kind in BootstrapTargetKind::all() {
            let s = kind.to_string();
            let parsed = BootstrapTargetKind::from_str(&s).unwrap();
            assert_eq!(*kind, parsed);
            let upper = BootstrapTargetKind::from_str(&s.to_uppercase()).unwrap();
            assert_eq!(*kind, upper);
        }

        assert!(!BootstrapTargetKind::Kind.requires_stage0_infra());
        assert!(!BootstrapTargetKind::Kubeconfig.requires_stage0_infra());
        assert!(BootstrapTargetKind::Proxmox.requires_stage0_infra());
        assert!(BootstrapTargetKind::Aws.requires_stage0_infra());
        assert!(BootstrapTargetKind::Gcp.requires_stage0_infra());
        assert!(BootstrapTargetKind::Azure.requires_stage0_infra());
        assert!(BootstrapTargetKind::Hetzner.requires_stage0_infra());

        assert!(BootstrapTargetKind::from_str("digitalocean").is_err());
    }

    #[test]
    fn test_bootstrap_distro_parsing() {
        assert_eq!(BootstrapDistro::default(), BootstrapDistro::K3s);
        assert_eq!(
            BootstrapDistro::from_str("k3s").unwrap(),
            BootstrapDistro::K3s
        );
        assert_eq!(
            BootstrapDistro::from_str("TALOS").unwrap(),
            BootstrapDistro::Talos
        );
        assert_eq!(BootstrapDistro::Talos.to_string(), "talos");
        assert!(BootstrapDistro::from_str("ubuntu").is_err());
    }

    #[test]
    fn test_bootstrap_config_validation_and_docker_socket_guard() {
        let mut cfg = BootstrapConfig::default();
        assert_eq!(cfg.target, BootstrapTargetKind::Kind);
        assert_eq!(cfg.cluster_name, "keel-mgmt");
        assert!(!cfg.local_dev_docker_socket_exception);
        assert!(cfg.validate().is_ok());

        // Allowed on kind when explicitly opted in
        cfg.local_dev_docker_socket_exception = true;
        assert!(cfg.validate().is_ok());

        // Strictly rejected on non-kind targets
        let mut aws_cfg = BootstrapConfig::new(BootstrapTargetKind::Aws);
        aws_cfg.local_dev_docker_socket_exception = true;
        let err = aws_cfg.validate().unwrap_err().to_string();
        assert!(err.contains("only permitted for the 'kind' bootstrap target"));

        // Reject flag smuggling in cluster_name, context, kubeconfig, pulumi_program_dir
        let bad_name = BootstrapConfig {
            cluster_name: "--evil".to_string(),
            ..Default::default()
        };
        assert!(bad_name.validate().is_err());

        let mut bad_ctx = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        bad_ctx.context = Some("-bad-ctx".to_string());
        assert!(bad_ctx.validate().is_err());

        let mut bad_kc = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        bad_kc.kubeconfig = Some(PathBuf::from("-bad-path"));
        assert!(bad_kc.validate().is_err());

        let bad_pulumi_dir = BootstrapConfig {
            pulumi_program_dir: Some(PathBuf::from("-inject")),
            ..Default::default()
        };
        assert!(bad_pulumi_dir.validate().is_err());

        let empty_pulumi_dir = BootstrapConfig {
            pulumi_program_dir: Some(PathBuf::from("   ")),
            ..Default::default()
        };
        assert!(empty_pulumi_dir.validate().is_err());

        let mut bad_pve = BootstrapConfig::new(BootstrapTargetKind::Proxmox);
        bad_pve.proxmox_url = Some("ftp://pve.local".to_string());
        assert!(bad_pve.validate().is_err());
        bad_pve.proxmox_url = Some("https://pve.local:8006".to_string());
        assert!(bad_pve.validate().is_ok());
    }

    #[test]
    fn test_keel_config_yaml_roundtrip_and_validation() {
        let mut cfg = KeelConfig {
            cluster_name: "keel-prod".to_string(),
            bootstrap_target: BootstrapTargetKind::Proxmox,
            auth_phase: AuthPhase::OidcActive,
            enabled_providers: vec!["proxmox".to_string(), "cloudflare".to_string()],
            forge: ForgeConfig {
                provider: ForgeKind::Forgejo,
                repo_url: "https://git.example.com/platform/infra-gitops.git".to_string(),
                api_url: Some("https://git.example.com/api/v1".to_string()),
                default_branch: "main".to_string(),
                poll_interval: "1m".to_string(),
            },
            oidc: Some(OidcConfig {
                enabled: true,
                issuer_url: "https://id.example.com".to_string(),
                client_id: "keel-portal".to_string(),
                redirect_url: "https://keel.example.com/api/v1/auth/callback".to_string(),
                scopes: default_oidc_scopes(),
            }),
            s3: Some(S3BackendConfig {
                endpoint: "https://s3.example.com".to_string(),
                bucket: "pulumi-state".to_string(),
                region: "us-east-1".to_string(),
                force_path_style: true,
            }),
            ..KeelConfig::default()
        };

        assert!(cfg.is_provider_enabled("PROXMOX"));
        assert!(!cfg.is_provider_enabled("aws"));

        let yaml = cfg.to_yaml().unwrap();
        let parsed = KeelConfig::from_yaml(&yaml).unwrap();
        assert_eq!(cfg, parsed);

        // Invalid OIDC missing issuer
        cfg.oidc.as_mut().unwrap().issuer_url.clear();
        assert!(cfg.validate().is_err());

        // Invalid apiVersion & kind & forge fields
        let bad_ver = KeelConfig {
            api_version: "v1".to_string(),
            ..Default::default()
        };
        assert!(bad_ver.validate().is_err());

        let bad_kind = KeelConfig {
            kind: "OtherKind".to_string(),
            ..Default::default()
        };
        assert!(bad_kind.validate().is_err());

        let mut empty_poll = KeelConfig::default();
        empty_poll.forge.poll_interval = "   ".to_string();
        assert!(empty_poll.validate().is_err());

        let mut empty_branch = KeelConfig::default();
        empty_branch.forge.default_branch = "".to_string();
        assert!(empty_branch.validate().is_err());
    }

    #[test]
    fn test_provider_registry_day1_and_mutation() {
        let mut reg = ProviderRegistry::default();
        assert_eq!(reg.providers.len(), 7);
        assert_eq!(reg.enabled_providers().len(), 1);
        assert!(reg.is_enabled("kind"));

        assert!(reg.set_enabled("proxmox", true));
        assert!(reg.set_enabled("aws", true));
        assert!(!reg.set_enabled("nonexistent", true));
        assert!(reg.is_enabled("proxmox"));
        assert_eq!(reg.enabled_providers().len(), 3);

        let yaml = reg.to_yaml().unwrap();
        let roundtrip = ProviderRegistry::from_yaml(&yaml).unwrap();
        assert_eq!(reg, roundtrip);

        // Duplicate provider ID, bad header, and empty plugin checks
        let mut dup = reg.clone();
        dup.providers.push(dup.providers[0].clone());
        assert!(dup.validate().is_err());

        let mut bad_reg_ver = reg.clone();
        bad_reg_ver.api_version = "v2".to_string();
        assert!(bad_reg_ver.validate().is_err());

        let mut bad_reg_kind = reg.clone();
        bad_reg_kind.kind = "Wrong".to_string();
        assert!(bad_reg_kind.validate().is_err());

        let mut empty_plugin = reg;
        empty_plugin.providers[0].plugin.clear();
        assert!(empty_plugin.validate().is_err());
    }

    #[test]
    fn test_tenant_config_isolation_and_sops_regex() {
        let mut tenant = TenantConfig::new("acme-corp", "Acme Corp").unwrap();
        assert_eq!(tenant.namespace, "tenant-acme-corp");
        assert_eq!(
            tenant.sops_path_regex(),
            r"^tenants/acme-corp/.*\.enc\.yaml$"
        );

        tenant.age_public_key =
            Some("age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p".to_string());
        tenant.allowed_providers = vec!["proxmox".to_string(), "aws".to_string()];
        assert!(tenant.is_provider_allowed("Proxmox"));
        assert!(!tenant.is_provider_allowed("gcp"));

        let yaml = tenant.to_yaml().unwrap();
        let roundtrip = TenantConfig::from_yaml(&yaml).unwrap();
        assert_eq!(tenant, roundtrip);

        // Reject path traversal / uppercase in tenant id
        assert!(TenantConfig::new("../evil", "Evil").is_err());
        assert!(TenantConfig::new("UpperCase", "Upper").is_err());

        // Reject mismatched namespace
        let mut tampered = tenant.clone();
        tampered.namespace = "flux-system".to_string();
        assert!(tampered.validate().is_err());

        // Reject malformed age key, empty display name, and invalid allowed provider
        let mut bad_key = tenant.clone();
        bad_key.age_public_key = Some("not-an-age-key".to_string());
        assert!(bad_key.validate().is_err());

        assert!(TenantConfig::new("acme-empty", "   ").is_err());

        let mut bad_prov = tenant;
        bad_prov.allowed_providers = vec!["INVALID_PROVIDER".to_string()];
        assert!(bad_prov.validate().is_err());
    }

    #[test]
    fn test_app_runtime_variants() {
        assert_eq!(AppRuntime::from_str("helm").unwrap(), AppRuntime::Helm);
        assert_eq!(
            AppRuntime::from_str("Kustomize").unwrap(),
            AppRuntime::Kustomize
        );
        assert_eq!(
            AppRuntime::from_str("manifest").unwrap(),
            AppRuntime::Manifest
        );
        assert_eq!(
            AppRuntime::from_str("MANIFEST").unwrap(),
            AppRuntime::Manifest
        );
        assert_eq!(AppRuntime::Manifest.as_str(), "manifest");
        assert_eq!(AppRuntime::Manifest.to_string(), "manifest");
        assert!(AppRuntime::from_str("docker-compose").is_err());

        let h: AppRuntime = serde_yml::from_str("Helm").unwrap();
        assert_eq!(h, AppRuntime::Helm);
        let k: AppRuntime = serde_yml::from_str("kustomize").unwrap();
        assert_eq!(k, AppRuntime::Kustomize);
        let m: AppRuntime = serde_yml::from_str("Manifest").unwrap();
        assert_eq!(m, AppRuntime::Manifest);
    }

    #[test]
    fn test_manifest_engine_variants_and_catalog_validation() {
        assert_eq!(ManifestEngine::default(), ManifestEngine::Flux);
        assert_eq!(
            ManifestEngine::from_str("flux").unwrap(),
            ManifestEngine::Flux
        );
        assert_eq!(
            ManifestEngine::from_str("PULUMI").unwrap(),
            ManifestEngine::Pulumi
        );
        assert_eq!(ManifestEngine::Flux.as_str(), "flux");
        assert_eq!(ManifestEngine::Pulumi.as_str(), "pulumi");
        assert_eq!(ManifestEngine::Pulumi.to_string(), "pulumi");
        assert!(ManifestEngine::from_str("argocd").is_err());

        let app = CatalogApplication {
            api_version: CATALOG_API_VERSION.to_string(),
            kind: CATALOG_APPLICATION_KIND.to_string(),
            metadata: CatalogAppMetadata {
                id: "nginx-raw".to_string(),
                name: "Nginx Raw Manifest".to_string(),
                description: None,
                category: "Web".to_string(),
                runtime: AppRuntime::Manifest,
                protected: false,
                tenant: None,
            },
            spec: CatalogAppSpec {
                upstream: UpstreamSource::default(),
                progressive_delivery: ProgressiveDeliverySpec::default(),
                companion_infrastructure: Vec::new(),
                schema: serde_json::json!({ "type": "object", "properties": {} }),
                ui_schema: None,
                service_url: None,
                default_engine: Some(ManifestEngine::Pulumi),
            },
        };
        assert!(app.validate().is_ok());

        let mut helm_app = app;
        helm_app.metadata.runtime = AppRuntime::Helm;
        assert!(helm_app.validate().is_err());
    }

    #[test]
    fn test_ssh_target_resolution_and_private_host_detection() {
        assert!(is_private_or_internal_ssh_host("localhost"));
        assert!(is_private_or_internal_ssh_host("vm.acme.local"));
        assert!(is_private_or_internal_ssh_host("db.internal"));
        assert!(is_private_or_internal_ssh_host(
            "svc.default.svc.cluster.local"
        ));
        assert!(is_private_or_internal_ssh_host("127.0.0.1"));
        assert!(is_private_or_internal_ssh_host("0.0.0.0"));
        assert!(is_private_or_internal_ssh_host("169.254.169.254"));
        assert!(is_private_or_internal_ssh_host("::1"));
        assert!(is_private_or_internal_ssh_host("fe80::1"));
        assert!(is_private_or_internal_ssh_host("224.0.0.1"));
        assert!(is_private_or_internal_ssh_host("255.255.255.255"));
        // RFC 1918, CGNAT, IPv6 ULA, cluster service names, and mapped addresses are internal.
        for host in [
            "192.168.1.50",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "100.64.0.1",
            "100.127.255.254",
            "fc00::1",
            "fd12:3456::1",
            "[fd00::1]",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "0.1.2.3",
            "kubernetes.default.svc",
            "keel-server.keel-system.svc.cluster.local",
            "kubernetes",
            "metadata",
        ] {
            assert!(is_private_or_internal_ssh_host(host), "{host}");
        }
        for host in [
            "203.0.113.10",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2001:db8::1",
            "vm.example.com",
            "svc.example.com",
        ] {
            assert!(!is_private_or_internal_ssh_host(host), "{host}");
        }

        assert_eq!(
            clean_candidate_ssh_host(r#"["127.0.0.1", "10.10.20.55/24"]"#).as_deref(),
            Some("10.10.20.55")
        );
        assert_eq!(clean_candidate_ssh_host("dhcp"), None);
        assert_eq!(clean_candidate_ssh_host("-oProxyCommand=evil"), None);
        assert_eq!(
            clean_candidate_ssh_user("ubuntu"),
            Some("ubuntu".to_string())
        );
        assert_eq!(clean_candidate_ssh_user("-oEvil"), None);

        let mut live_outputs = std::collections::HashMap::new();
        live_outputs.insert(
            "ipv4Addresses".to_string(),
            r#"["127.0.0.1","10.20.30.40"]"#.to_string(),
        );
        let values_doc = serde_json::json!({
            "sshUser": "adminuser",
            "sshPort": 2222
        });
        let stack_doc = serde_json::json!({
            "metadata": {
                "labels": {
                    "keel.io/provider": "hetzner"
                }
            }
        });
        let info = resolve_ssh_target_from_sources(
            "web-vm",
            "tenant-acme",
            "acme",
            None,
            &live_outputs,
            Some(&values_doc),
            Some(&stack_doc),
            None,
            true,
        );
        assert_eq!(info.provider, "hetzner");
        assert_eq!(info.ssh_host, "10.20.30.40");
        assert_eq!(info.ssh_user, "adminuser");
        assert_eq!(info.ssh_port, 2222);
        assert_eq!(info.ssh_command, "ssh -p 2222 adminuser@10.20.30.40");
        assert!(info.has_saved_ssh_key);
    }

    #[test]
    fn test_unresolved_ssh_target_and_host_key_resolution() {
        let empty = std::collections::HashMap::new();
        let info = resolve_ssh_target_from_sources(
            "vm",
            "tenant-acme",
            "acme",
            None,
            &empty,
            None,
            None,
            None,
            false,
        );
        assert!(is_unresolved_ssh_target(&info));

        let mut outputs = std::collections::HashMap::new();
        outputs.insert("publicIp".to_string(), "203.0.113.7".to_string());
        let info = resolve_ssh_target_from_sources(
            "vm",
            "tenant-acme",
            "acme",
            None,
            &outputs,
            None,
            None,
            None,
            false,
        );
        assert!(!is_unresolved_ssh_target(&info));

        assert_eq!(resolve_ssh_host_key_from_sources(&empty, None, None), None);
        outputs.insert(
            "sshHostKeyFingerprint".to_string(),
            " SHA256:abc ".to_string(),
        );
        let values = serde_json::json!({ "sshHostKey": "ssh-ed25519 AAAAvalues" });
        assert_eq!(
            resolve_ssh_host_key_from_sources(&outputs, Some(&values), None).as_deref(),
            Some("SHA256:abc")
        );
        assert_eq!(
            resolve_ssh_host_key_from_sources(&empty, Some(&values), None).as_deref(),
            Some("ssh-ed25519 AAAAvalues")
        );
        let stack = serde_json::json!({ "spec": { "config": { "ssh_host_key": "${unset}" } } });
        assert_eq!(
            resolve_ssh_host_key_from_sources(&empty, None, Some(&stack)),
            None
        );
        let stack = serde_json::json!({ "spec": { "config": { "hostKey": "ssh-rsa AAAAstack" } } });
        assert_eq!(
            resolve_ssh_host_key_from_sources(&empty, None, Some(&stack)).as_deref(),
            Some("ssh-rsa AAAAstack")
        );
    }

    #[test]
    fn test_is_private_or_internal_ip() {
        use std::net::IpAddr;
        for ip in [
            "169.254.169.254",
            "10.1.2.3",
            "100.64.0.0",
            "fd00::1",
            "::1",
            "127.0.0.1",
        ] {
            assert!(
                is_private_or_internal_ip(ip.parse::<IpAddr>().unwrap()),
                "{ip}"
            );
        }
        for ip in ["1.1.1.1", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(
                !is_private_or_internal_ip(ip.parse::<IpAddr>().unwrap()),
                "{ip}"
            );
        }
    }
}
