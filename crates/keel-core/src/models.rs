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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub with_oidc: bool,
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
    pub kind_config_path: Option<PathBuf>,
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
            with_oidc: false,
            local_dev_docker_socket_exception: false,
            proxmox_url: None,
            region: None,
            distro: None,
            kind_config_path: None,
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

        if let Some(path) = &self.kind_config_path {
            let path_str = path.to_string_lossy();
            if path_str.trim().is_empty() || path_str.starts_with('-') {
                return Err(CoreError::Validation(
                    "kind_config_path must be non-empty and must not start with '-'".to_string(),
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

        Ok(())
    }
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self::new(BootstrapTargetKind::Kind)
    }
}

/// Two-phase authentication state machine (`SDD.md` §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthPhase {
    #[default]
    Bootstrap,
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
            auth_phase: AuthPhase::Bootstrap,
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

/// Application packaging runtime supported by Flux CD (`SDD.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppRuntime {
    #[serde(alias = "Helm", alias = "HELM")]
    Helm,
    #[serde(alias = "Kustomize", alias = "KUSTOMIZE")]
    Kustomize,
}

impl AppRuntime {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Helm => "helm",
            Self::Kustomize => "kustomize",
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
            other => Err(CoreError::Validation(format!(
                "Unsupported application runtime '{other}'; expected 'helm' or 'kustomize'"
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
}

/// Unmodified upstream Helm chart or Kustomize repository reference (`NFR-EXT-01`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamSource {
    #[serde(rename = "repoURL", alias = "repoUrl", alias = "repo_url")]
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
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
    pub upstream: UpstreamSource,
    #[serde(default)]
    pub progressive_delivery: ProgressiveDeliverySpec,
    #[serde(default)]
    pub companion_infrastructure: Vec<CompanionInfraHook>,
    pub schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_schema: Option<serde_json::Value>,
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
        if self.spec.upstream.repo_url.trim().is_empty() {
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
}

/// Item kind inside a `CatalogBundle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleItemKind {
    Infra,
    App,
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

        let mut by_id = BTreeMap::new();
        for item in &self.spec.items {
            validate_dns_label(&item.id, "spec.items[].id")?;
            validate_dns_label(&item.catalog_ref, "spec.items[].catalogRef")?;
            if let Some(ns) = &item.target_namespace {
                validate_dns_label(ns, "spec.items[].targetNamespace")?;
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
                if item.kind != BundleItemKind::App {
                    return Err(CoreError::Validation(format!(
                        "Bundle item '{}' sets targetClusterFrom, which is only valid for kind: app",
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
        let mut ordered = Vec::with_capacity(self.spec.items.len());

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

        // Reject flag smuggling in cluster_name, context, kubeconfig, kind_config_path
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

        let bad_kind_cfg = BootstrapConfig {
            kind_config_path: Some(PathBuf::from("-inject")),
            ..Default::default()
        };
        assert!(bad_kind_cfg.validate().is_err());

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
        assert!(AppRuntime::from_str("docker-compose").is_err());

        let h: AppRuntime = serde_yml::from_str("Helm").unwrap();
        assert_eq!(h, AppRuntime::Helm);
        let k: AppRuntime = serde_yml::from_str("kustomize").unwrap();
        assert_eq!(k, AppRuntime::Kustomize);
    }
}
