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
    /// Run `keel-server` in-cluster as the Flux-managed `keel-server` core add-on
    /// (`--with-keel-server`, default `false`). Needs a cluster-reachable HTTP(S) Git remote.
    #[serde(default)]
    pub with_keel_server: bool,
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
            .field("with_keel_server", &self.with_keel_server)
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
            with_keel_server: false,
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

        // The embedded Stage 0 programs install single-node k3s; only a custom
        // `--pulumi-program-dir` (or `--target kubeconfig`) can bring another distribution.
        if self.distro == Some(BootstrapDistro::Talos)
            && self.target.requires_stage0_infra()
            && self.pulumi_program_dir.is_none()
        {
            return Err(CoreError::Validation(format!(
                "distro 'talos' is not supported by the built-in '{}' Stage 0 program (it installs \
                 k3s); install Talos yourself and use --target kubeconfig, or pass \
                 --pulumi-program-dir",
                self.target
            )));
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

/// Checks that `origin` is an `https://<host>[:port]` origin (no path, query, fragment,
/// credentials or whitespace; a trailing `/` is tolerated). Returns the reason on failure.
pub fn validate_https_origin(origin: &str) -> Result<(), &'static str> {
    let Some(authority) = origin.strip_prefix("https://") else {
        return Err("must use the https:// scheme");
    };
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty() {
        return Err("has no host");
    }
    if authority
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | '?' | '#'))
    {
        return Err("must not contain a path, query, fragment, credentials or whitespace");
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: `[addr]` or `[addr]:port`.
        match rest.split_once(']') {
            Some((addr, "")) => (addr, None),
            Some((addr, tail)) => match tail.strip_prefix(':') {
                Some(p) => (addr, Some(p)),
                None => return Err("has an invalid host"),
            },
            None => return Err("has an invalid host"),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("has no host");
    }
    if let Some(port) = port
        && port.parse::<u16>().map_or(true, |p| p == 0)
    {
        return Err("has an invalid port");
    }
    Ok(())
}

/// Git repository and forge configuration inside `.keel/config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeConfig {
    pub provider: ForgeKind,
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// HTTPS origin of the forge (`https://<host>[:port]`, no path), e.g.
    /// `https://forgejo.keel.local:8888` for the Stage 0 in-cluster Forgejo behind Envoy Gateway.
    /// Flux pulls the in-cluster Forgejo over its plain `http://` Service (`repoUrl`), which PKO
    /// cannot clone, so tenant and preview PKO `Stack`s clone `<publicUrl>/<org>/<repo>.git`
    /// instead (`keel_pko::pko_project_repo_url`). Written by `keel bootstrap init --with-forge`
    /// (derived from `--oidc-public-url`); platform configuration, never tenant input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
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
            public_url: None,
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

/// Environment variables that override the `.keel/config.yaml` `oidc` block of `keel serve`
/// (e.g. a catalog-installed keel-server whose OIDC client is configured in its Deployment rather
/// than in the GitOps repository it serves). The client secret (`KEEL_OIDC_CLIENT_SECRET`) is
/// deliberately not part of [`OidcConfig`] (never serialized into Git); `keel serve` reads it
/// separately.
pub const OIDC_ISSUER_URL_ENV: &str = "KEEL_OIDC_ISSUER_URL";
/// See [`OIDC_ISSUER_URL_ENV`].
pub const OIDC_CLIENT_ID_ENV: &str = "KEEL_OIDC_CLIENT_ID";
/// See [`OIDC_ISSUER_URL_ENV`].
pub const OIDC_REDIRECT_URL_ENV: &str = "KEEL_OIDC_REDIRECT_URL";
/// Confidential-client secret sent on the OIDC token exchange (`client_secret_post`).
pub const OIDC_CLIENT_SECRET_ENV: &str = "KEEL_OIDC_CLIENT_SECRET";

impl OidcConfig {
    /// Applies the `KEEL_OIDC_*` environment overrides ([`OIDC_ISSUER_URL_ENV`],
    /// [`OIDC_CLIENT_ID_ENV`], [`OIDC_REDIRECT_URL_ENV`]) on top of the repository `oidc` block.
    ///
    /// Blank variables are ignored. Without any override `base` is returned unchanged. With at
    /// least one, the result is an enabled config whose issuer, client id and redirect URL come
    /// from the environment or (field by field) from `base`; all three must then be non-empty
    /// `http(s)` URLs / ids, otherwise a validation error is returned (fail closed at startup
    /// instead of silently serving without login).
    pub fn with_env_overrides(
        base: Option<Self>,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, CoreError> {
        let var = |name: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let issuer = var(OIDC_ISSUER_URL_ENV);
        let client_id = var(OIDC_CLIENT_ID_ENV);
        let redirect = var(OIDC_REDIRECT_URL_ENV);
        if issuer.is_none() && client_id.is_none() && redirect.is_none() {
            return Ok(base);
        }
        let base_ref = base.as_ref();
        let pick = |over: Option<String>, from_base: Option<&String>| {
            over.or_else(|| from_base.filter(|v| !v.trim().is_empty()).cloned())
        };
        let issuer_url = pick(issuer, base_ref.map(|b| &b.issuer_url));
        let client_id = pick(client_id, base_ref.map(|b| &b.client_id));
        let redirect_url = pick(redirect, base_ref.map(|b| &b.redirect_url));
        let (Some(issuer_url), Some(client_id), Some(redirect_url)) =
            (issuer_url, client_id, redirect_url)
        else {
            return Err(CoreError::Validation(format!(
                "OIDC environment overrides need an issuer URL, client id and redirect URL \
                 ({OIDC_ISSUER_URL_ENV}, {OIDC_CLIENT_ID_ENV}, {OIDC_REDIRECT_URL_ENV}, or the \
                 .keel/config.yaml oidc block)"
            )));
        };
        for (name, url) in [
            (OIDC_ISSUER_URL_ENV, &issuer_url),
            (OIDC_REDIRECT_URL_ENV, &redirect_url),
        ] {
            let valid = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .and_then(|rest| rest.split(['/', '?', '#']).next())
                .is_some_and(|authority| {
                    !authority.is_empty()
                        && !authority.contains('@')
                        && !url.chars().any(char::is_whitespace)
                });
            if !valid {
                return Err(CoreError::Validation(format!(
                    "{name} must be an absolute http(s) URL"
                )));
            }
        }
        Ok(Some(Self {
            enabled: true,
            issuer_url: issuer_url.trim_end_matches('/').to_string(),
            client_id,
            redirect_url,
            scopes: base.map(|b| b.scopes).unwrap_or_else(default_oidc_scopes),
        }))
    }
}

fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".to_string(),
        "profile".to_string(),
        "email".to_string(),
        "groups".to_string(),
    ]
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
        let cfg: Self = noyalib::from_str(yaml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Serializes this `KeelConfig` to YAML.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        self.validate()?;
        Ok(crate::yaml::to_string(self)?)
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
        if let Some(public_url) = &self.forge.public_url {
            validate_https_origin(public_url).map_err(|why| {
                CoreError::Validation(format!(
                    "forge.publicUrl '{public_url}' {why} (expected e.g. `https://forgejo.example.com`)"
                ))
            })?;
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
        let reg: Self = noyalib::from_str(yaml)?;
        reg.validate()?;
        Ok(reg)
    }

    /// Serializes `ProviderRegistry` to YAML.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        self.validate()?;
        Ok(crate::yaml::to_string(self)?)
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

/// Where a `CatalogApplication` may be installed (`SDD.md` §5.1 `spec.placement.scope`).
///
/// `namespaced` (the default) apps only create namespaced objects and may be installed into the
/// tenant namespace of the management cluster or into a remote workload cluster. `cluster` apps
/// install cluster-scoped resources (CRDs, ClusterRoles/Bindings, admission webhooks) that would
/// collide with — or escalate past — the management plane's own add-ons and the tenant's
/// namespace-scoped RBAC, so they may only target a remote workload cluster (`targetCluster`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlacementScope {
    #[default]
    #[serde(alias = "Namespaced", alias = "NAMESPACED")]
    Namespaced,
    #[serde(alias = "Cluster", alias = "CLUSTER")]
    Cluster,
}

impl PlacementScope {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Namespaced => "namespaced",
            Self::Cluster => "cluster",
        }
    }
}

impl fmt::Display for PlacementScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PlacementScope {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "namespaced" => Ok(Self::Namespaced),
            "cluster" => Ok(Self::Cluster),
            other => Err(CoreError::Validation(format!(
                "Unsupported placement scope '{other}'; expected 'namespaced' or 'cluster'"
            ))),
        }
    }
}

/// Flux `HelmRelease` CRD lifecycle policy for a chart's `crds/` directory
/// (`spec.crdPolicy`, `runtime: helm` only), rendered into `spec.install.crds` and
/// `spec.upgrade.crds`. Omitted means Flux's defaults (`Create` on install, `Skip` on upgrade),
/// which never update CRDs on a chart upgrade; charts shipping their CRDs in `crds/` (e.g. the
/// Pulumi Kubernetes Operator) declare `CreateReplace` so a version bump also updates them.
/// Charts that template their CRDs are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HelmCrdPolicy {
    Skip,
    Create,
    CreateReplace,
}

impl HelmCrdPolicy {
    /// The Flux `HelmRelease` `install.crds` / `upgrade.crds` value.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Skip => "Skip",
            Self::Create => "Create",
            Self::CreateReplace => "CreateReplace",
        }
    }
}

impl fmt::Display for HelmCrdPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Declarative placement rule of a `CatalogApplication` (`spec.placement`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementSpec {
    #[serde(default)]
    pub scope: PlacementScope,
    /// Dedicated install namespace on the remote workload cluster (`scope: cluster` only), e.g.
    /// `flux-system` for a cluster-wide operator. A proposal without `targetNamespace` installs
    /// there, and any other `targetNamespace` is rejected, so the operator never lands in the
    /// tenant's shared `tenant-<id>` namespace (where namespace-wide chart NetworkPolicies or
    /// webhook namespace exclusions would affect the tenant's other apps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Further namespaces a `runtime: manifest` app with a dedicated [`Self::namespace`] spans on
    /// the remote workload cluster (e.g. keel-server's `keel-previews` and `flux-system` Roles).
    /// Keel then does not force a Flux `targetNamespace` (which would collapse every object into
    /// one namespace): instead every namespaced object of the rendered manifests must name one of
    /// `namespace` + `additionalNamespaces` explicitly, and Keel creates all of them first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_namespaces: Vec<String>,
}

impl PlacementSpec {
    /// `true` when the spec is the implicit default (omitted from serialized `catalog.yaml`).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// `true` when the app may only be installed into a remote workload cluster.
    pub const fn requires_remote_cluster(&self) -> bool {
        matches!(self.scope, PlacementScope::Cluster)
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

/// A Gateway API route an upstream Helm chart renders from its own values
/// (`spec.gatewayRoutes[]`, `runtime: helm` only). Keel never patches the chart: it reads the
/// hostnames from the submitted values, applies the tenant route guards (no platform host, no
/// hostname routed by another app, no catch-all route) and pins the route to the shared
/// `keel-system/keel-ingress` HTTPS listeners (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelmGatewayRouteSpec {
    /// Dot path of the boolean value that turns the route on (e.g. `httpRoute.enabled`); when
    /// absent the route is always rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_value: Option<String>,
    /// Dot path of the route hostnames (a string or a list of strings), e.g. `httpRoute.hostnames`.
    pub hostnames_value: String,
    /// Dot path Keel overwrites with the `keel-ingress` HTTPS listener `parentRefs` (e.g.
    /// `httpRoute.parentRefs`), so a tenant route can only attach to the shared Gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_refs_value: Option<String>,
}

/// Keel-generated tenant `NetworkPolicy` for a Helm app (`spec.networkPolicy`, `runtime: helm`
/// only). Tenant namespaces are `default-deny-all`; upstream charts rarely ship policies, so
/// Keel commits one `NetworkPolicy` selecting the release's pods
/// (`app.kubernetes.io/instance: <release>`). Cluster DNS egress is always allowed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelmNetworkPolicySpec {
    /// TCP ports the Envoy Gateway proxies (`keel-system`) may reach (the route backends).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gateway_ingress_ports: Vec<u16>,
    /// TCP ports any pod of the tenant namespace may reach (e.g. an S3 endpoint consumed by
    /// other tenant apps). Consumer pods still need their own egress allowance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespace_ingress_ports: Vec<u16>,
    /// Pods of the release may reach each other on any port (multi-component charts).
    #[serde(default)]
    pub release_traffic: bool,
    /// Egress to the `keel-ingress` Envoy proxy pods in `keel-system` on their HTTP(S) listener
    /// ports only (e.g. an OIDC issuer routed through the shared Gateway); never to other
    /// `keel-system` workloads.
    #[serde(default)]
    pub gateway_egress: bool,
    /// Egress to public IPv4 addresses on TCP 80/443 (RFC 1918, CGNAT, link-local, loopback,
    /// `0.0.0.0/8`, multicast and reserved ranges excluded), e.g. repository mirrors and webhooks.
    #[serde(default)]
    pub public_egress: bool,
}

/// Specification block for `CatalogApplication`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogAppSpec {
    #[serde(default)]
    pub upstream: UpstreamSource,
    #[serde(default)]
    pub progressive_delivery: ProgressiveDeliverySpec,
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
    /// Placement rule; absent means `scope: namespaced`.
    #[serde(default, skip_serializing_if = "PlacementSpec::is_default")]
    pub placement: PlacementSpec,
    /// Gateway API routes the Helm chart renders from its values (`runtime: helm` only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gateway_routes: Vec<HelmGatewayRouteSpec>,
    /// Keel-generated tenant `NetworkPolicy` for the release (`runtime: helm` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<HelmNetworkPolicySpec>,
    /// Chart values Keel always deep-merges over the submitted values into
    /// `HelmRelease.spec.values` (`runtime: helm` only): the known-good tenant install config
    /// (restricted Pod Security contexts, no cluster-scoped RBAC, bundled databases off) that the
    /// form must not be able to drop or change. Never shown in the form; never secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_values: Option<serde_json::Value>,
    /// Create-only value paths (`runtime: helm` only), e.g. PersistentVolumeClaim sizes and
    /// storage classes rendered into immutable StatefulSet `volumeClaimTemplates`: an Edit that
    /// changes one of them is rejected instead of committing a Helm upgrade that can never
    /// succeed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub immutable_values: Vec<String>,
    /// Flux CRD lifecycle for the chart's `crds/` directory (`runtime: helm` only); see
    /// [`HelmCrdPolicy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crd_policy: Option<HelmCrdPolicy>,
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
        // Fail closed on secret fields the partitioner could not find (`NFR-SEC-05`).
        crate::secret_schema::validate_secret_schema_support(&self.spec.schema)?;
        if let Some(ns) = &self.spec.placement.namespace {
            validate_dns_label(ns, "spec.placement.namespace")?;
            if !self.spec.placement.requires_remote_cluster() {
                return Err(CoreError::Validation(
                    "spec.placement.namespace requires spec.placement.scope: cluster (management \
                     cluster installs are confined to the tenant namespace)"
                        .to_string(),
                ));
            }
            if ns.starts_with("tenant-") {
                return Err(CoreError::Validation(format!(
                    "spec.placement.namespace '{ns}' must be a dedicated namespace, not a tenant \
                     namespace"
                )));
            }
        }
        let extra = &self.spec.placement.additional_namespaces;
        if !extra.is_empty() {
            let Some(primary) = self.spec.placement.namespace.as_deref() else {
                return Err(CoreError::Validation(
                    "spec.placement.additionalNamespaces requires spec.placement.namespace"
                        .to_string(),
                ));
            };
            if self.metadata.runtime != AppRuntime::Manifest {
                return Err(CoreError::Validation(
                    "spec.placement.additionalNamespaces is only supported for runtime: manifest \
                     (a HelmRelease installs into a single namespace)"
                        .to_string(),
                ));
            }
            for (i, ns) in extra.iter().enumerate() {
                validate_dns_label(ns, "spec.placement.additionalNamespaces[]")?;
                if ns.starts_with("tenant-") {
                    return Err(CoreError::Validation(format!(
                        "spec.placement.additionalNamespaces entry '{ns}' must be a dedicated \
                         namespace, not a tenant namespace"
                    )));
                }
                if ns == primary || extra[..i].contains(ns) {
                    return Err(CoreError::Validation(format!(
                        "spec.placement.additionalNamespaces entry '{ns}' is listed twice"
                    )));
                }
            }
        }
        if (!self.spec.gateway_routes.is_empty()
            || self.spec.network_policy.is_some()
            || self.spec.locked_values.is_some()
            || !self.spec.immutable_values.is_empty()
            || self.spec.crd_policy.is_some())
            && self.metadata.runtime != AppRuntime::Helm
        {
            return Err(CoreError::Validation(
                "spec.gatewayRoutes, spec.networkPolicy, spec.lockedValues, \
                 spec.immutableValues and spec.crdPolicy are only supported for runtime: helm \
                 (manifest apps declare their HTTPRoute/NetworkPolicy and fixed settings in the \
                 template)"
                    .to_string(),
            ));
        }
        if self
            .spec
            .locked_values
            .as_ref()
            .is_some_and(|v| !v.is_object())
        {
            return Err(CoreError::Validation(
                "spec.lockedValues must be a Helm values object".to_string(),
            ));
        }
        for route in &self.spec.gateway_routes {
            validate_value_path(&route.hostnames_value, "gatewayRoutes[].hostnamesValue")?;
            if let Some(path) = &route.enabled_value {
                validate_value_path(path, "gatewayRoutes[].enabledValue")?;
            }
            if let Some(path) = &route.parent_refs_value {
                validate_value_path(path, "gatewayRoutes[].parentRefsValue")?;
            }
        }
        for path in &self.spec.immutable_values {
            validate_value_path(path, "immutableValues[]")?;
        }
        if let Some(np) = &self.spec.network_policy
            && (np.gateway_ingress_ports.contains(&0) || np.namespace_ingress_ports.contains(&0))
        {
            return Err(CoreError::Validation(
                "spec.networkPolicy.gatewayIngressPorts / namespaceIngressPorts must be TCP \
                 ports 1-65535"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Whether submitted values must stay inside `spec.schema` (fail closed on undeclared
    /// keys). Implied by `spec.lockedValues` / `spec.gatewayRoutes`: their guarantees (locked
    /// security contexts, guarded routes) only hold when no other chart value can be set, e.g.
    /// a second chart route, `extraDeploy`, sidecars or another ServiceAccount.
    pub fn strict_values(&self) -> bool {
        self.metadata.runtime == AppRuntime::Helm
            && (self.spec.locked_values.is_some() || !self.spec.gateway_routes.is_empty())
    }

    /// Enforces `spec.placement`: a `scope: cluster` app may only be installed into a remote
    /// workload cluster. `remote_target` is the remote kubeconfig Secret reference the install
    /// targets, or `None` for the management cluster (tenant namespace).
    pub fn ensure_placement_allows(&self, remote_target: Option<&str>) -> Result<(), CoreError> {
        let remote = remote_target.map(str::trim).is_some_and(|t| !t.is_empty());
        if self.spec.placement.requires_remote_cluster() && !remote {
            return Err(CoreError::Validation(format!(
                "Catalog application '{}' has spec.placement.scope: cluster (it installs \
                 cluster-scoped resources such as CRDs, ClusterRoles or admission webhooks) and \
                 can only be deployed to a remote workload cluster; select a targetCluster other \
                 than the management cluster",
                self.metadata.id
            )));
        }
        Ok(())
    }

    /// Resolves the remote install namespace of a proposal under `spec.placement.namespace`:
    /// without a dedicated namespace the trimmed, non-empty `requested` namespace is returned
    /// unchanged; with one, an absent/empty request resolves to it and any other namespace is
    /// rejected.
    pub fn resolve_target_namespace(
        &self,
        requested: Option<&str>,
    ) -> Result<Option<String>, CoreError> {
        let requested = requested.map(str::trim).filter(|s| !s.is_empty());
        match (self.spec.placement.namespace.as_deref(), requested) {
            (None, requested) => Ok(requested.map(str::to_string)),
            (Some(dedicated), None) => Ok(Some(dedicated.to_string())),
            (Some(dedicated), Some(requested)) if requested == dedicated => {
                Ok(Some(dedicated.to_string()))
            }
            (Some(dedicated), Some(requested)) => Err(CoreError::Validation(format!(
                "Catalog application '{}' installs into the dedicated namespace '{dedicated}' \
                 (spec.placement.namespace); targetNamespace '{requested}' is not allowed",
                self.metadata.id
            ))),
        }
    }

    /// Namespaces a multi-namespace manifest app (`spec.placement.additionalNamespaces`) spans on
    /// the remote cluster: the dedicated namespace first, then the additional ones. Empty for
    /// every other app (whose Flux `Kustomization` pins a single `targetNamespace`).
    pub fn multi_namespace_placement(&self) -> Vec<String> {
        match self.spec.placement.namespace.as_deref() {
            Some(primary) if !self.spec.placement.additional_namespaces.is_empty() => {
                std::iter::once(primary.to_string())
                    .chain(self.spec.placement.additional_namespaces.iter().cloned())
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// For a multi-namespace manifest app ([`Self::multi_namespace_placement`]), rejects rendered
    /// manifests with a namespaced object (any kind outside
    /// `crate::catalog::CLUSTER_SCOPED_KINDS`) that does not name one of the placement
    /// namespaces in `metadata.namespace`: without a Flux `targetNamespace` such an object would
    /// land in an arbitrary namespace of the workload cluster. A no-op for other apps.
    pub fn ensure_manifest_namespaces(&self, rendered_yaml: &str) -> Result<(), CoreError> {
        let allowed = self.multi_namespace_placement();
        if allowed.is_empty() {
            return Ok(());
        }
        let offenders =
            crate::catalog::manifest_objects_outside_namespaces(rendered_yaml, &allowed);
        if offenders.is_empty() {
            return Ok(());
        }
        Err(CoreError::Validation(format!(
            "Catalog application '{}' spans the namespaces {} (spec.placement.namespace + \
             additionalNamespaces), so every namespaced object must set one of them as \
             metadata.namespace; offending objects: {}",
            self.metadata.id,
            allowed.join(", "),
            offenders.join(", ")
        )))
    }

    /// Rejects a `scope: namespaced` `runtime: manifest` app whose manifests declare well-known
    /// cluster-scoped kinds (`Namespace`, CRDs, `ClusterRole`s, webhooks, ...); such an app must
    /// declare `spec.placement.scope: cluster`. A declarative authoring guard: the hard boundary on
    /// the management cluster is the namespace-scoped RBAC of `tenant-<id>-sa`.
    pub fn ensure_manifest_matches_placement(&self, manifests_yaml: &str) -> Result<(), CoreError> {
        if self.spec.placement.requires_remote_cluster() {
            return Ok(());
        }
        let kinds = crate::catalog::manifest_cluster_scoped_kinds(manifests_yaml);
        if kinds.is_empty() {
            return Ok(());
        }
        Err(CoreError::Validation(format!(
            "Catalog application '{}' is scope: namespaced but its manifests declare cluster-scoped \
             kinds ({}); set placementScope: cluster (spec.placement.scope: cluster) so it can only \
             target a remote workload cluster",
            self.metadata.id,
            kinds.join(", ")
        )))
    }
}

/// Validates a dot-separated Helm value path (`a.b.c`): non-empty segments of
/// `[A-Za-z0-9_-]` (no list indices).
fn validate_value_path(path: &str, field: &str) -> Result<(), CoreError> {
    let valid = !path.is_empty()
        && path.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    if valid {
        Ok(())
    } else {
        Err(CoreError::Validation(format!(
            "spec.{field} '{}' must be a dot-separated value path such as httpRoute.hostnames",
            path.escape_debug()
        )))
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
        // Fail closed on secret fields the partitioner could not find (`NFR-SEC-05`).
        crate::secret_schema::validate_secret_schema_support(&self.spec.schema)?;
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
    /// The self-contained schema of item `item_id` declared in the bundle schema
    /// (`spec.schema.properties.<item_id>`, with the bundle's `$defs`/`definitions`, see
    /// [`crate::standalone_subschema`]), or `None` when the bundle schema does not declare it.
    pub fn item_schema(&self, item_id: &str) -> Option<serde_json::Value> {
        self.spec
            .schema
            .get("properties")
            .and_then(|p| p.get(item_id))
            .map(|sub| crate::secret_schema::standalone_subschema(&self.spec.schema, sub))
    }

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
        // Fail closed on secret fields the partitioner could not find (`NFR-SEC-05`).
        crate::secret_schema::validate_secret_schema_support(&self.spec.schema)?;
        // Each item's values are partitioned and masked with its own self-contained schema
        // ([`Self::item_schema`]): a `$ref` it cannot resolve would hide a secret there.
        for item in &self.spec.items {
            if let Some(sub) = self
                .spec
                .schema
                .get("properties")
                .and_then(|p| p.get(&item.id))
            {
                crate::secret_schema::validate_standalone_subschema(&self.spec.schema, sub)
                    .map_err(|e| {
                        let e = match e {
                            CoreError::Validation(msg) => msg,
                            other => other.to_string(),
                        };
                        CoreError::Validation(format!(
                            "spec.schema.properties.{}: {e}; a bundle item's schema may only \
                             reference the bundle schema's $defs/definitions",
                            item.id
                        ))
                    })?;
            }
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
    fn test_oidc_env_overrides() {
        fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        }
        let base = OidcConfig {
            enabled: true,
            issuer_url: "https://idp.keel.local:8888/auth/v1".to_string(),
            client_id: "keel-server".to_string(),
            redirect_url: "https://keel.keel.local:8888/api/v1/auth/callback".to_string(),
            scopes: vec!["openid".to_string()],
        };
        // No (or only blank) overrides: unchanged, including "no OIDC at all".
        assert_eq!(
            OidcConfig::with_env_overrides(Some(base.clone()), env(&[])).unwrap(),
            Some(base.clone())
        );
        assert_eq!(
            OidcConfig::with_env_overrides(None, env(&[(OIDC_CLIENT_ID_ENV, "  ")])).unwrap(),
            None
        );

        // Full override without a repository block (catalog-installed keel-server).
        let full = OidcConfig::with_env_overrides(
            None,
            env(&[
                (
                    OIDC_ISSUER_URL_ENV,
                    " https://login.team-a.example/realms/a/ ",
                ),
                (OIDC_CLIENT_ID_ENV, "team-a-keel"),
                (
                    OIDC_REDIRECT_URL_ENV,
                    "https://keel.team-a.example/api/v1/auth/callback",
                ),
                // Never part of the config (read separately by `keel serve`).
                (OIDC_CLIENT_SECRET_ENV, "s3cr3t"),
            ]),
        )
        .unwrap()
        .unwrap();
        assert!(full.enabled);
        assert_eq!(full.issuer_url, "https://login.team-a.example/realms/a");
        assert_eq!(full.client_id, "team-a-keel");
        assert_eq!(full.scopes, default_oidc_scopes());
        assert!(!crate::yaml::to_string(&full).unwrap().contains("s3cr3t"));

        // Partial override merges field by field with the repository block.
        let merged = OidcConfig::with_env_overrides(
            Some(OidcConfig {
                enabled: false,
                ..base.clone()
            }),
            env(&[(OIDC_CLIENT_ID_ENV, "other")]),
        )
        .unwrap()
        .unwrap();
        assert!(merged.enabled);
        assert_eq!(merged.client_id, "other");
        assert_eq!(merged.issuer_url, base.issuer_url);
        assert_eq!(merged.scopes, base.scopes);

        // Incomplete or malformed overrides fail closed.
        for pairs in [
            &[(OIDC_ISSUER_URL_ENV, "https://idp.example")][..],
            &[
                (OIDC_ISSUER_URL_ENV, "idp.example"),
                (OIDC_CLIENT_ID_ENV, "c"),
                (OIDC_REDIRECT_URL_ENV, "https://keel.example/cb"),
            ][..],
            &[
                (OIDC_ISSUER_URL_ENV, "https://user@idp.example"),
                (OIDC_CLIENT_ID_ENV, "c"),
                (OIDC_REDIRECT_URL_ENV, "https://keel.example/cb"),
            ][..],
            &[
                (OIDC_ISSUER_URL_ENV, "https://idp.example"),
                (OIDC_CLIENT_ID_ENV, "c"),
                (OIDC_REDIRECT_URL_ENV, "ftp://keel.example/cb"),
            ][..],
        ] {
            assert!(
                OidcConfig::with_env_overrides(None, env(pairs)).is_err(),
                "{pairs:?}"
            );
        }
    }

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
        assert!(!cfg.with_keel_server, "in-cluster keel-server is opt-in");
        assert!(cfg.validate().is_ok());

        // `withKeelServer` is optional on input and round-trips.
        let parsed: BootstrapConfig = serde_json::from_str(r#"{"target":"kind"}"#).unwrap();
        assert!(!parsed.with_keel_server);
        let parsed: BootstrapConfig =
            serde_json::from_str(r#"{"target":"kind","withKeelServer":true}"#).unwrap();
        assert!(parsed.with_keel_server);
        assert!(format!("{parsed:?}").contains("with_keel_server: true"));

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

        // The embedded VM programs install k3s: Talos needs a custom program or `kubeconfig`.
        for target in [BootstrapTargetKind::Proxmox, BootstrapTargetKind::Hetzner] {
            let mut talos = BootstrapConfig::new(target);
            talos.distro = Some(BootstrapDistro::Talos);
            let err = talos.validate().unwrap_err().to_string();
            assert!(err.contains("--target kubeconfig"), "{err}");
            talos.pulumi_program_dir = Some(PathBuf::from("/opt/talos-program"));
            assert!(talos.validate().is_ok());
            talos.pulumi_program_dir = None;
            talos.distro = Some(BootstrapDistro::K3s);
            assert!(talos.validate().is_ok());
        }
        let mut talos_kc = BootstrapConfig::new(BootstrapTargetKind::Kubeconfig);
        talos_kc.distro = Some(BootstrapDistro::Talos);
        assert!(talos_kc.validate().is_ok());
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
                public_url: Some("https://git.example.com".to_string()),
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
            ..KeelConfig::default()
        };

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

        for ok in [
            "https://forgejo.keel.local:8888",
            "https://forgejo.example.com/",
            "https://forgejo.example.com",
        ] {
            let mut c = KeelConfig::default();
            c.forge.public_url = Some(ok.to_string());
            assert!(c.validate().is_ok(), "{ok}");
        }
        for bad in [
            "http://forgejo.keel.local:8888",
            "https://forgejo.keel.local/sub",
            "https://user:pw@forgejo.keel.local",
            "https://",
            "https://forgejo.keel.local:0",
            "https://forgejo.keel.local:http",
            "forgejo.keel.local",
        ] {
            let mut c = KeelConfig::default();
            c.forge.public_url = Some(bad.to_string());
            assert!(c.validate().is_err(), "{bad}");
        }
        // `publicUrl` is optional and omitted from YAML when unset (older configs still parse).
        assert!(
            !KeelConfig::default()
                .to_yaml()
                .unwrap()
                .contains("publicUrl")
        );
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

        let h: AppRuntime = noyalib::from_str("Helm").unwrap();
        assert_eq!(h, AppRuntime::Helm);
        let k: AppRuntime = noyalib::from_str("kustomize").unwrap();
        assert_eq!(k, AppRuntime::Kustomize);
        let m: AppRuntime = noyalib::from_str("Manifest").unwrap();
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
                schema: serde_json::json!({ "type": "object", "properties": {} }),
                ui_schema: None,
                service_url: None,
                default_engine: Some(ManifestEngine::Pulumi),
                placement: PlacementSpec::default(),
                gateway_routes: Vec::new(),
                network_policy: None,
                locked_values: None,
                immutable_values: Vec::new(),
                crd_policy: None,
            },
        };
        assert!(app.validate().is_ok());

        // Route/NetworkPolicy declarations are Helm-only.
        let mut manifest_with_routes = app.clone();
        manifest_with_routes.spec.network_policy = Some(HelmNetworkPolicySpec::default());
        assert!(manifest_with_routes.validate().is_err());
        let mut manifest_immutable = app.clone();
        manifest_immutable.spec.immutable_values = vec!["persistence.size".to_string()];
        assert!(manifest_immutable.validate().is_err());
        let mut manifest_crds = app.clone();
        manifest_crds.spec.crd_policy = Some(HelmCrdPolicy::CreateReplace);
        assert!(manifest_crds.validate().is_err());
        assert!(!app.strict_values());

        let mut helm_app = app;
        helm_app.metadata.runtime = AppRuntime::Helm;
        assert!(helm_app.validate().is_err());

        helm_app.spec.upstream.repo_url = "https://charts.example.com".to_string();
        helm_app.spec.upstream.chart = Some("demo".to_string());
        helm_app.spec.default_engine = None;
        helm_app.spec.gateway_routes = vec![HelmGatewayRouteSpec {
            enabled_value: Some("httpRoute.enabled".to_string()),
            hostnames_value: "httpRoute.hostnames".to_string(),
            parent_refs_value: Some("httpRoute.parentRefs".to_string()),
        }];
        helm_app.spec.network_policy = Some(HelmNetworkPolicySpec {
            gateway_ingress_ports: vec![3000],
            ..HelmNetworkPolicySpec::default()
        });
        assert!(helm_app.validate().is_ok());
        for bad in ["", "a..b", "a[0].b", "a b", ".a"] {
            let mut invalid = helm_app.clone();
            invalid.spec.gateway_routes[0].hostnames_value = bad.to_string();
            assert!(invalid.validate().is_err(), "{bad:?} must be rejected");
        }
        let mut zero_port = helm_app.clone();
        zero_port.spec.network_policy = Some(HelmNetworkPolicySpec {
            gateway_ingress_ports: vec![0],
            ..HelmNetworkPolicySpec::default()
        });
        assert!(zero_port.validate().is_err());
        let mut zero_ns_port = helm_app.clone();
        zero_ns_port.spec.network_policy = Some(HelmNetworkPolicySpec {
            namespace_ingress_ports: vec![0],
            ..HelmNetworkPolicySpec::default()
        });
        assert!(zero_ns_port.validate().is_err());
        let mut immutable = helm_app.clone();
        immutable.spec.immutable_values = vec!["persistence.size".to_string()];
        assert!(immutable.validate().is_ok());
        immutable.spec.immutable_values = vec!["volume.dataDirs[0].size".to_string()];
        assert!(immutable.validate().is_err());
        // Declared routes imply strict values.
        assert!(helm_app.strict_values());
        let mut no_routes = helm_app.clone();
        no_routes.spec.gateway_routes.clear();
        assert!(!no_routes.strict_values());
        let mut locked = helm_app.clone();
        locked.spec.locked_values = Some(serde_json::json!({ "test": { "enabled": false } }));
        assert!(locked.validate().is_ok());
        locked.spec.gateway_routes.clear();
        assert!(locked.strict_values());
        locked.spec.locked_values = Some(serde_json::json!(["not", "an", "object"]));
        assert!(locked.validate().is_err());

        // `spec.crdPolicy` parses Flux's exact spelling only and is omitted when unset.
        let mut crds = helm_app.clone();
        crds.spec.crd_policy = Some(HelmCrdPolicy::CreateReplace);
        assert!(crds.validate().is_ok());
        let yaml = crate::yaml::to_string(&crds).unwrap();
        assert!(yaml.contains("crdPolicy: CreateReplace"), "{yaml}");
        assert_eq!(
            noyalib::from_str::<CatalogApplication>(&yaml).unwrap(),
            crds
        );
        assert!(
            !crate::yaml::to_string(&helm_app)
                .unwrap()
                .contains("crdPolicy")
        );
        for (policy, flux) in [
            (HelmCrdPolicy::Skip, "Skip"),
            (HelmCrdPolicy::Create, "Create"),
            (HelmCrdPolicy::CreateReplace, "CreateReplace"),
        ] {
            assert_eq!(policy.as_str(), flux);
            assert_eq!(policy.to_string(), flux);
        }
        assert!(noyalib::from_str::<HelmCrdPolicy>("replace").is_err());
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
