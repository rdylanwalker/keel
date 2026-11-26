//! Stage 0 seed control-plane synthesis and cluster bootstrap handlers (`keel bootstrap init`).
//!
//! Submodules: [`kube`] (`kubectl` helpers), [`s3`] (SeaweedFS + SigV4), [`state`] (durable
//! Stage 0 Pulumi state, passphrase, S3 escrow), [`forge`] (Forgejo and
//! external repository provisioning) and [`addons`] (core add-on installation).

pub(crate) mod addons;
pub(crate) mod forge;
pub(crate) mod kube;
pub(crate) mod s3;
pub(crate) mod state;
pub(crate) mod tenant_repo;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use keel_core::{
    AuthPhase, BootstrapConfig, BootstrapTarget, ClusterConnection, DEFAULT_POLL_INTERVAL,
    FLUX_SYSTEM_NAMESPACE, ForgeConfig, ForgeKind, KEEL_SYSTEM_NAMESPACE, KeelConfig, OidcConfig,
    ProviderRegistry, SopsConfig, SopsManager, bootstrap_target_factory, generate_admin_password,
    stage0_state_root,
};
use keel_git::{CommitAuthor, GitCredentials, GitRunner, parse_remote_repo_url};
use keel_kube::{
    DockerSocketExceptionConfig, KyvernoManifestBundle, KyvernoPolicyConfig,
    S3_CREDENTIALS_SECRET_NAME, TenantIsolationConfig, TenantManifestBundle,
};
use keel_pko::{
    CrObjectMeta, DEFAULT_FLUX_POLL_INTERVAL, FLUX_GIT_REPO_API_VERSION, FLUX_GIT_REPO_KIND,
    FluxGitRepository, FluxGitRepositoryRef, FluxGitRepositorySpec, IN_CLUSTER_S3_BACKEND_URL,
    LocalObjectReference, ManagementCoreStackParams, PkoStack, attach_s3_credentials_env_refs,
    synthesize_management_core_stack,
};
use serde_json::json;

use crate::cli::BootstrapInitArgs;

use super::{CliError, absolutize_path};
use kube::KubeTarget;

pub(crate) use forge::FORGE_TOKEN_SECRET_NAME;
pub(crate) use kube::{TempPortForward, start_forgejo_port_forward, start_gateway_port_forward};

/// Default public URL of the in-cluster Rauthy IdP on local `kind` (Envoy Gateway port 8888).
pub const DEFAULT_OIDC_PUBLIC_URL: &str = "https://idp.keel.local:8888";

/// Public ingress coordinates for Stage 0 IdP / Forgejo / portal hosts, derived from
/// `--oidc-public-url` (default [`DEFAULT_OIDC_PUBLIC_URL`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage0Ingress {
    /// `https://<idp_host>[:port]` (no trailing slash).
    pub idp_origin: String,
    pub idp_host: String,
    /// Domain shared by sibling hosts (`idp.keel.local` -> `keel.local`).
    pub base_domain: String,
    pub forgejo_host: String,
    pub keel_host: String,
    /// `:<port>` suffix (empty for the scheme default port).
    pub port_suffix: String,
}

fn is_dns_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

impl Stage0Ingress {
    /// Parses and validates `--oidc-public-url` (`https://<host>[:port]`, DNS host, no path).
    pub fn from_public_url(raw: &str) -> Result<Self, CliError> {
        let invalid = |why: &str| {
            CliError::Validation(format!(
                "invalid --oidc-public-url '{}': {why} (expected e.g. `https://idp.example.com`)",
                raw.trim()
            ))
        };
        let parsed = url::Url::parse(raw.trim()).map_err(|_| invalid("not a URL"))?;
        if parsed.scheme() != "https" {
            return Err(invalid("scheme must be https"));
        }
        if !(parsed.path().is_empty() || parsed.path() == "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
        {
            return Err(invalid(
                "must not contain a path, query, fragment or credentials",
            ));
        }
        let host = parsed
            .host_str()
            .map(str::to_ascii_lowercase)
            .filter(|h| is_dns_hostname(h) && h.contains('.'))
            .ok_or_else(|| invalid("host must be a DNS name with at least two labels"))?;
        let port_suffix = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
        let base_domain = host
            .split_once('.')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
        Ok(Self {
            idp_origin: format!("https://{host}{port_suffix}"),
            forgejo_host: format!("forgejo.{base_domain}"),
            keel_host: format!("keel.{base_domain}"),
            idp_host: host,
            base_domain,
            port_suffix,
        })
    }

    /// OIDC issuer served by Rauthy (`<origin>/auth/v1`).
    pub fn issuer(&self) -> String {
        format!("{}/auth/v1", self.idp_origin)
    }

    /// OIDC discovery document URL.
    pub fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.issuer())
    }

    pub fn forgejo_origin(&self) -> String {
        format!("https://{}{}", self.forgejo_host, self.port_suffix)
    }

    pub fn keel_origin(&self) -> String {
        format!("https://{}{}", self.keel_host, self.port_suffix)
    }

    /// `${VAR}` substitutions for `deploy/bootstrap/manifests/*.yaml`.
    pub(crate) fn template_vars(&self) -> Vec<(&'static str, &str)> {
        vec![
            ("IDP_HOST", self.idp_host.as_str()),
            ("FORGEJO_HOST", self.forgejo_host.as_str()),
            ("KEEL_HOST", self.keel_host.as_str()),
            ("BASE_DOMAIN", self.base_domain.as_str()),
        ]
    }
}

impl Default for Stage0Ingress {
    fn default() -> Self {
        Self::from_public_url(DEFAULT_OIDC_PUBLIC_URL).expect("default OIDC public URL is valid")
    }
}

/// Substitutes `${NAME}` placeholders with DNS-safe values (rejecting anything else so templates
/// can never be used for YAML injection) and fails on unknown placeholders.
pub(crate) fn render_manifest(template: &str, vars: &[(&str, &str)]) -> Result<String, CliError> {
    let mut out = template.to_string();
    for (name, value) in vars {
        if value.is_empty()
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        {
            return Err(CliError::Validation(format!(
                "refusing to render manifest placeholder {name}={value:?}: not a DNS-safe value"
            )));
        }
        out = out.replace(&format!("${{{name}}}"), value);
    }
    if let Some(idx) = out.find("${") {
        let rest: String = out[idx..].chars().take(32).collect();
        return Err(CliError::Validation(format!(
            "unrendered manifest placeholder near `{rest}`"
        )));
    }
    Ok(out)
}

/// Kyverno policy bundle generator with explicit control over the local-dev Docker socket
/// `PolicyException` (`SDD.md` §6.2, `NFR-SEC-03`, `NFR-SEC-04`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KyvernoPolicyGenerator {
    pub allowed_providers: Vec<String>,
    pub local_dev_docker_socket_exception: bool,
    /// Platform hosts tenant Gateway API routes may never claim
    /// (`restrict-tenant-route-hostnames`); defaults to the `keel.local` hosts.
    pub reserved_route_hostnames: Vec<String>,
}

impl KyvernoPolicyGenerator {
    /// Creates a generator for the specified enabled Pulumi/OpenTofu providers and explicit
    /// `local_dev_docker_socket_exception` flag (`false` in production; opt-in on local `kind`).
    pub fn new(allowed_providers: Vec<String>, local_dev_docker_socket_exception: bool) -> Self {
        Self {
            allowed_providers,
            local_dev_docker_socket_exception,
            reserved_route_hostnames: keel_kube::default_reserved_route_hostnames(),
        }
    }

    /// Reserves the Stage 0 platform hosts (IdP, keel-server, Forgejo) of `ingress`, in addition
    /// to the default `keel.local` hosts, against tenant route hostnames.
    pub fn with_reserved_ingress_hosts(mut self, ingress: &Stage0Ingress) -> Self {
        let mut reserved = keel_core::platform_reserved_hostnames([ingress.idp_origin.as_str()]);
        reserved.extend([
            ingress.idp_host.clone(),
            ingress.keel_host.clone(),
            ingress.forgejo_host.clone(),
        ]);
        self.reserved_route_hostnames = reserved.into_iter().collect();
        self
    }

    /// Synthesizes the baseline `ValidatingPolicy` manifests and optional local-dev `PolicyException`.
    pub fn generate(&self) -> Result<KyvernoManifestBundle, CliError> {
        let cfg = KyvernoPolicyConfig {
            allowed_providers: self.allowed_providers.clone(),
            local_dev_docker_socket_exception: self.local_dev_docker_socket_exception,
            docker_socket_exception: DockerSocketExceptionConfig::default(),
            reserved_route_hostnames: self.reserved_route_hostnames.clone(),
        };
        Ok(KyvernoManifestBundle::generate(&cfg)?)
    }
}

/// Secret name holding the Stage 0 Rauthy OIDC configuration & bootstrap JSON in `keel-system`.
const RAUTHY_CONFIG_SECRET_NAME: &str = "rauthy-config";

/// Rendered Stage 0 Rauthy OIDC configuration and first-boot JSON seed artifacts.
#[derive(Clone)]
pub struct RauthySeedConfig {
    pub config_toml: String,
    pub groups_json: String,
    pub users_json: String,
    pub clients_json: String,
    pub forgejo_oidc_secret: Option<String>,
    pub secret_namespace: String,
    pub secret_name: String,
}

/// Seed control-plane artifacts synthesized identically across all Stage 0 `BootstrapTarget`s.
#[derive(Clone)]
pub struct SeedControlPlaneArtifacts {
    pub keel_config: KeelConfig,
    pub provider_registry: ProviderRegistry,
    pub sops_config: SopsConfig,
    pub flux_git_repository: FluxGitRepository,
    pub kyverno_bundle: KyvernoManifestBundle,
    pub management_stack: PkoStack,
    /// Initial admin username/email for Stage 0 Rauthy seeding.
    pub admin_username: String,
    /// Initial admin password for Stage 0 Rauthy seeding (never logged in `Debug`).
    pub admin_password: String,
    /// `true` if `admin_password` was auto-generated rather than supplied via CLI flags.
    pub admin_password_generated: bool,
    /// Rendered Rauthy config & bootstrap JSON (`None` when using external OIDC or `--oidc-provider none`).
    pub rauthy_config: Option<RauthySeedConfig>,
    /// Optional generated `age` keyfile contents for `flux-system/sops-age` (never logged).
    pub sops_age_keyfile: Option<String>,
    /// Public ingress hosts for the IdP / Forgejo / portal (`--oidc-public-url`).
    pub ingress: Stage0Ingress,
    /// `--with-forge`: the in-cluster Forgejo is installed and managed as a Flux add-on.
    pub with_forge: bool,
    /// `--with-keel-server`: keel-server runs in-cluster as the Flux-managed `keel-server` add-on.
    pub with_keel_server: bool,
}

impl std::fmt::Debug for SeedControlPlaneArtifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeedControlPlaneArtifacts")
            .field("keel_config", &self.keel_config)
            .field("admin_username", &self.admin_username)
            .field("admin_password", &"[REDACTED]")
            .field("admin_password_generated", &self.admin_password_generated)
            .field("ingress", &self.ingress)
            .field("with_forge", &self.with_forge)
            .field("with_keel_server", &self.with_keel_server)
            .field(
                "rauthy_secret",
                &self
                    .rauthy_config
                    .as_ref()
                    .map(|r| format!("{}/{}", r.secret_namespace, r.secret_name)),
            )
            .finish()
    }
}

/// Result of initializing the Stage 0 local Git repository (`--repo-dir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRepoInitResult {
    pub repo_dir: PathBuf,
    pub bare_repo_dir: PathBuf,
    /// The working copy's `origin` after bootstrap: the local bare repository (Local Git
    /// provider) or the forge's repository URL (`forge.repoUrl`).
    pub origin_url: String,
    /// Whether the seed commit reached `origin`.
    pub pushed: bool,
    pub initial_commit_sha: String,
    pub written_files: Vec<String>,
}

/// Outcome of `keel bootstrap init`.
#[derive(Debug, Clone)]
pub struct BootstrapInitOutcome {
    /// The synthesized seed control plane; only inspected by tests.
    #[cfg(test)]
    pub artifacts: SeedControlPlaneArtifacts,
    pub output: String,
}

/// Synthesizes all target-agnostic Stage 0 seed control-plane manifests (`SDD.md` §2.1,
/// `AGENTS.md` §3.1) for the given Stage 0 ingress hosts (`--oidc-public-url`).
pub async fn synthesize_seed_control_plane(
    cfg: &BootstrapConfig,
    repo_url_override: Option<&str>,
    ingress: Stage0Ingress,
) -> Result<SeedControlPlaneArtifacts, CliError> {
    cfg.validate()?;

    let mut provider_registry = ProviderRegistry::default_day1();
    if cfg.target.requires_stage0_infra() {
        let _ = provider_registry.set_enabled(cfg.target.as_str(), true);
    }
    provider_registry.validate()?;
    // Shared allowlist helper (also used by `keel serve` / proposals) always includes the
    // built-in providers such as `kubernetes` (OPEN-3).
    let enabled_ids: Vec<String> = provider_registry
        .enabled_providers()
        .into_iter()
        .map(|p| p.id.clone())
        .collect();
    let enabled_provider_ids = keel_kube::pulumi_provider_allowlist(&enabled_ids);

    let repo_url = repo_url_override.map(str::to_owned).unwrap_or_else(|| {
        if cfg.with_forge {
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git"
                .to_string()
        } else {
            "https://forge.internal/keel/infra-gitops.git".to_string()
        }
    });

    let external_oidc_issuer = cfg
        .oidc_issuer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let use_in_cluster_rauthy = external_oidc_issuer.is_none() && cfg.with_oidc;
    let oidc_issuer = external_oidc_issuer
        .clone()
        .or_else(|| use_in_cluster_rauthy.then(|| ingress.issuer()));
    let oidc_client_id = cfg
        .oidc_client_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "keel-server".to_string());

    let keel_config = KeelConfig {
        cluster_name: cfg.cluster_name.clone(),
        bootstrap_target: cfg.target,
        auth_phase: AuthPhase::OidcActive,
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
            // PKO cannot clone the in-cluster `http://` Forgejo Service Flux uses; tenant and
            // preview Stacks clone it through the Envoy Gateway HTTPS ingress instead (Keel CA,
            // trusted through `keel-ca-bundle`).
            public_url: cfg.with_forge.then(|| ingress.forgejo_origin()),
            default_branch: "main".to_string(),
            // Mandatory outbound pull-polling interval (`1m`) for NAT/firewall resilience (`AGENTS.md` §3.1)
            poll_interval: DEFAULT_POLL_INTERVAL.to_string(),
        },
        oidc: oidc_issuer.as_ref().map(|issuer_url| OidcConfig {
            enabled: true,
            issuer_url: issuer_url.clone(),
            client_id: oidc_client_id.clone(),
            redirect_url: "http://127.0.0.1:8080/api/v1/auth/callback".to_string(),
            scopes: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
                "groups".to_string(),
            ],
        }),
        ..KeelConfig::default()
    };
    keel_config.validate()?;

    // Generate root SOPS config (populate root age rule if `age-keygen` is installed on PATH)
    let mut sops_config = SopsConfig::default();
    let mut sops_age_keyfile = None;
    if let Ok(keypair) = SopsManager::new().generate_age_keypair().await {
        sops_age_keyfile = Some(keypair.to_keyfile_contents());
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
        KyvernoPolicyGenerator::new(enabled_provider_ids, cfg.local_dev_docker_socket_exception)
            .with_reserved_ingress_hosts(&ingress);
    let kyverno_bundle = kyverno_gen.generate()?;

    // Protected anti-lockout `management-cluster-core` PKO v2 Stack (`protect: true`)
    let mut mgmt_params = ManagementCoreStackParams::new(
        IN_CLUSTER_S3_BACKEND_URL,
        FLUX_SYSTEM_NAMESPACE,
        "clusters/management",
    );
    attach_s3_credentials_env_refs(&mut mgmt_params.env_refs, S3_CREDENTIALS_SECRET_NAME);
    let management_stack = synthesize_management_core_stack(&mgmt_params)?;

    // Stage 0 Rauthy OIDC admin credentials & bootstrap config (`SDD.md` §2.1, §6.1, `NFR-SEC-01`)
    let admin_username = cfg
        .admin_username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "admin@keel.local".to_string());

    let (admin_password, admin_password_generated) = match cfg
        .admin_password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(provided) => (provided.to_string(), false),
        None => (generate_admin_password(), true),
    };

    let rauthy_config = if use_in_cluster_rauthy {
        let secret_raft = format!(
            "{}{}{}",
            generate_admin_password(),
            generate_admin_password(),
            generate_admin_password()
        );
        let secret_api = format!(
            "{}{}{}",
            generate_admin_password(),
            generate_admin_password(),
            generate_admin_password()
        );
        let enc_bytes = keel_core::EnvelopeCrypto::generate_key();
        let enc_key_id = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            enc_bytes[0], enc_bytes[1], enc_bytes[2], enc_bytes[3]
        );
        let enc_key_b64 = STANDARD.encode(enc_bytes);

        let pub_url = format!("{}{}", ingress.idp_host, ingress.port_suffix);
        let config_toml = format!(
            "[access]\ncookie_mode = 'secure'\n\n\
             [bootstrap]\nbootstrap_dir = '/app/bootstrap'\n\n\
             [cluster]\nnode_id = 1\nnodes = ['1 127.0.0.1:8100 127.0.0.1:8200']\nsecret_raft = '{secret_raft}'\nsecret_api = '{secret_api}'\n\n\
             [database]\nhiqlite = true\n\n\
             [encryption]\nkeys = ['{enc_key_id}/{enc_key_b64}']\nkey_active = '{enc_key_id}'\n\n\
             [logging]\nlevel = 'info'\n\n\
             [mfa]\nadmin_force_mfa = false\n\n\
             [server]\nlisten_address = '0.0.0.0'\nport_http = 8080\nport_https = 8443\nscheme = 'http_https'\npub_url = '{pub_url}'\nproxy_mode = true\ntrusted_proxies = ['10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '127.0.0.0/8']\n\n\
             [tls]\ngenerate_self_signed = true\n\n\
             [webauthn]\nrp_id = '{rp_id}'\nrp_origin = '{rp_origin}'\nrp_name = 'Keel IAM'\n",
            rp_id = ingress.idp_host,
            rp_origin = ingress.idp_origin,
        );

        let groups_json = serde_json::to_string_pretty(&json!([
            { "name": "keel-admins" },
            { "name": "tenant-default" }
        ]))?;
        let users_json = serde_json::to_string_pretty(&json!([
            {
                "email": admin_username,
                "preferred_username": "admin",
                "given_name": "Keel",
                "family_name": "Admin",
                "password": { "Plain": admin_password },
                "roles": ["rauthy_admin"],
                "groups": ["keel-admins", "tenant-default"],
                "enabled": true,
                "email_verified": true
            }
        ]))?;
        let forgejo_oidc_secret = cfg.with_forge.then(|| {
            let mut secret = format!(
                "{}{}{}{}",
                generate_admin_password(),
                generate_admin_password(),
                generate_admin_password(),
                generate_admin_password()
            );
            secret.truncate(64);
            secret
        });
        let mut clients = vec![json!({
            "id": oidc_client_id,
            "name": "Keel Control Plane",
            "secret": null,
            "redirect_uris": [
                "http://127.0.0.1:8080/api/v1/auth/callback",
                "http://localhost:8080/api/v1/auth/callback",
                format!("{}/api/v1/auth/callback", ingress.keel_origin())
            ],
            "allowed_origins": [
                "http://127.0.0.1:8080",
                "http://localhost:8080",
                ingress.keel_origin()
            ],
            "enabled": true,
            "flows_enabled": ["authorization_code", "refresh_token"],
            "access_token_alg": "RS256",
            "id_token_alg": "RS256",
            "auth_code_lifetime": 300,
            "access_token_lifetime": 3600,
            "scopes": ["openid", "profile", "email", "groups"],
            "default_scopes": ["openid", "profile", "email", "groups"],
            "challenges": ["S256"],
            "force_mfa": false
        })];
        if let Some(ref forgejo_secret) = forgejo_oidc_secret {
            clients.push(json!({
                "id": "forgejo",
                "name": "Forgejo Git Forge",
                "confidential": true,
                "secret": { "Plain": forgejo_secret },
                "redirect_uris": [
                    format!("{}/user/oauth2/rauthy/callback", ingress.forgejo_origin())
                ],
                "allowed_origins": [
                    ingress.forgejo_origin()
                ],
                "enabled": true,
                "flows_enabled": ["authorization_code", "refresh_token"],
                "access_token_alg": "RS256",
                "id_token_alg": "RS256",
                "auth_code_lifetime": 300,
                "access_token_lifetime": 3600,
                "scopes": ["openid", "profile", "email", "groups"],
                "default_scopes": ["openid", "profile", "email", "groups"],
                "challenges": ["S256"],
                "force_mfa": false
            }));
        }
        let clients_json = serde_json::to_string_pretty(&clients)?;

        Some(RauthySeedConfig {
            config_toml,
            groups_json,
            users_json,
            clients_json,
            forgejo_oidc_secret,
            secret_namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            secret_name: RAUTHY_CONFIG_SECRET_NAME.to_string(),
        })
    } else {
        None
    };

    Ok(SeedControlPlaneArtifacts {
        keel_config,
        provider_registry,
        sops_config,
        flux_git_repository,
        kyverno_bundle,
        management_stack,
        admin_username,
        admin_password,
        admin_password_generated,
        rauthy_config,
        sops_age_keyfile,
        ingress,
        with_forge: cfg.with_forge,
        with_keel_server: cfg.with_keel_server,
    })
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

// `BUILT_IN_CATALOG_FILES`: every `deploy/catalog/{apps,infra,bundles}` file (`build.rs`).
include!(concat!(env!("OUT_DIR"), "/built_in_catalog.rs"));

/// Contents of the built-in catalog file at repository path `path`; a missing path fails to compile.
#[cfg(test)]
const fn built_in_catalog_file(path: &str) -> &'static str {
    const fn str_eq(a: &str, b: &str) -> bool {
        let (a, b) = (a.as_bytes(), b.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }
    let mut i = 0;
    while i < BUILT_IN_CATALOG_FILES.len() {
        if str_eq(BUILT_IN_CATALOG_FILES[i].0, path) {
            return BUILT_IN_CATALOG_FILES[i].1;
        }
        i += 1;
    }
    panic!("not a built-in catalog file")
}

/// Self-service twins of the management add-ons and starter apps checked by the drift tests.
#[cfg(test)]
const STARTER_DEX_APP_YAML: &str = built_in_catalog_file("catalog/apps/dex/catalog.yaml");
#[cfg(test)]
const STARTER_CERT_MANAGER_APP_YAML: &str =
    built_in_catalog_file("catalog/apps/cert-manager/catalog.yaml");
#[cfg(test)]
const STARTER_RAUTHY_APP_YAML: &str = built_in_catalog_file("catalog/apps/rauthy/catalog.yaml");
#[cfg(test)]
const STARTER_RAUTHY_APP_MANIFESTS_YAML: &str =
    built_in_catalog_file("catalog/apps/rauthy/manifests.yaml");
#[cfg(test)]
const STARTER_FORGEJO_APP_YAML: &str = built_in_catalog_file("catalog/apps/forgejo/catalog.yaml");
#[cfg(test)]
const STARTER_SEAWEEDFS_APP_YAML: &str =
    built_in_catalog_file("catalog/apps/seaweedfs/catalog.yaml");
#[cfg(test)]
const STARTER_KYVERNO_APP_YAML: &str = built_in_catalog_file("catalog/apps/kyverno/catalog.yaml");
#[cfg(test)]
const STARTER_PKO_APP_YAML: &str =
    built_in_catalog_file("catalog/apps/pulumi-kubernetes-operator/catalog.yaml");
#[cfg(test)]
const STARTER_FLUX2_APP_YAML: &str = built_in_catalog_file("catalog/apps/flux2/catalog.yaml");
#[cfg(test)]
const STARTER_KEEL_SERVER_APP_YAML: &str =
    built_in_catalog_file("catalog/apps/keel-server/catalog.yaml");
#[cfg(test)]
const STARTER_KEEL_SERVER_APP_MANIFESTS_YAML: &str =
    built_in_catalog_file("catalog/apps/keel-server/manifests.yaml");

const SEED_CORE_ADDON_KUSTOMIZATION_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/kustomization.yaml");
const SEED_CORE_ADDON_HELM_REPOSITORIES_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/helm-repositories.yaml");
const SEED_CORE_ADDON_S3_SEAWEEDFS_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/s3-seaweedfs.yaml");
const SEED_CORE_ADDON_PKO_YAML: &str = include_str!("../../../../deploy/seed/core-addons/pko.yaml");
/// Flux self-management: protected `flux-system/flux2` HelmRelease adopting the Stage 0 release.
const SEED_CORE_ADDON_FLUX2_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/flux2.yaml");
const SEED_CORE_ADDON_KYVERNO_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/kyverno.yaml");
const SEED_CORE_ADDON_ENVOY_GATEWAY_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/envoy-gateway.yaml");
const SEED_CORE_ADDON_PROMETHEUS_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/prometheus.yaml");
const SEED_CORE_ADDON_FLAGGER_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/flagger.yaml");
const SEED_CORE_ADDON_CERT_MANAGER_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/cert-manager.yaml");
const SEED_CORE_ADDON_RAUTHY_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/rauthy.yaml");
/// Optional Forgejo `HelmRepository` + `HelmRelease` + `forgejo-ingress` HTTPRoute
/// (`--with-forge`); `${FORGEJO_ROOT_URL}` / `${FORGEJO_HOST}` are rendered by
/// [`render_seed_forgejo_helmrelease`].
const SEED_CORE_ADDON_FORGEJO_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/forgejo.yaml");
/// Plain manifests of the Rauthy add-on (`clusters/management/addons/rauthy/`), reconciled by the
/// Flux Kustomization in [`SEED_CORE_ADDON_RAUTHY_YAML`].
const SEED_ADDON_RAUTHY_KUSTOMIZATION_YAML: &str =
    include_str!("../../../../deploy/seed/addons/rauthy/kustomization.yaml");
const SEED_ADDON_RAUTHY_SERVICES_YAML: &str =
    include_str!("../../../../deploy/seed/addons/rauthy/services.yaml");
const SEED_ADDON_RAUTHY_STATEFULSET_YAML: &str =
    include_str!("../../../../deploy/seed/addons/rauthy/statefulset.yaml");
/// `${IDP_HOST}` template of the Rauthy HTTPRoute (see [`render_rauthy_httproute`]).
const SEED_ADDON_RAUTHY_HTTPROUTE_YAML: &str =
    include_str!("../../../../deploy/seed/addons/rauthy/httproute.yaml");
/// Repository-relative directory of the Rauthy plain-manifest add-on.
const RAUTHY_ADDON_DIR: &str = "clusters/management/addons/rauthy";

/// Optional in-cluster keel-server (`--with-keel-server`): Flux Kustomization `flux-system/keel-server`
/// pinning [`KEEL_SERVER_IMAGE`] to [`KEEL_SERVER_IMAGE_TAG`].
const SEED_CORE_ADDON_KEEL_SERVER_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/keel-server.yaml");
/// keel-server RBAC and Deployment/Service, shared verbatim with the manual
/// `kubectl apply -k deploy/bootstrap/keel-server` bundle.
const KEEL_SERVER_RBAC_YAML: &str =
    include_str!("../../../../deploy/bootstrap/keel-server/rbac.yaml");
const KEEL_SERVER_DEPLOYMENT_YAML: &str =
    include_str!("../../../../deploy/bootstrap/keel-server/deployment.yaml");
/// `${KEEL_HOST}` template of the keel-server HTTPRoute (see [`render_keel_server_httproute`]).
const SEED_ADDON_KEEL_SERVER_HTTPROUTE_YAML: &str =
    include_str!("../../../../deploy/seed/addons/keel-server/httproute.yaml");
const CORE_CATALOG_KEEL_SERVER_YAML: &str =
    include_str!("../../../../deploy/catalog/core/keel-server/catalog.yaml");
/// Repository-relative directory of the keel-server plain-manifest add-on.
const KEEL_SERVER_ADDON_DIR: &str = "clusters/management/addons/keel-server";
/// Release image of `keel` / `keel-server` (`.github/workflows/release.yml`, job `image`).
pub(crate) const KEEL_SERVER_IMAGE: &str = "ghcr.io/rdylanwalker/keel";
/// Image tag the release workflow publishes for this build: the `{{version}}` semver tag (no `v`
/// prefix), i.e. the workspace crate version that semantic-release sets. Never `latest`.
pub(crate) const KEEL_SERVER_IMAGE_TAG: &str = env!("CARGO_PKG_VERSION");

/// `clusters/management/flux-system/kustomization.yaml`.
const SEED_FLUX_SYSTEM_KUSTOMIZATION_YAML: &str =
    include_str!("../../../../deploy/seed/flux-system/kustomization.yaml");
/// Canonical root Flux Kustomizations (`flux-system`, `core-addons`, `keel-tenants`): written to
/// `clusters/management/flux-system/gotk-sync.yaml` and applied verbatim by Stage 0.
pub(crate) const SEED_FLUX_GOTK_SYNC_YAML: &str =
    include_str!("../../../../deploy/seed/flux-system/gotk-sync.yaml");

const CORE_CATALOG_S3_STATE_BACKEND_YAML: &str =
    include_str!("../../../../deploy/catalog/core/s3-state-backend/catalog.yaml");
const CORE_CATALOG_KYVERNO_YAML: &str =
    include_str!("../../../../deploy/catalog/core/kyverno/catalog.yaml");
const CORE_CATALOG_FLUX2_YAML: &str =
    include_str!("../../../../deploy/catalog/core/flux2/catalog.yaml");
const CORE_CATALOG_CERT_MANAGER_YAML: &str =
    include_str!("../../../../deploy/catalog/core/cert-manager/catalog.yaml");
const CORE_CATALOG_RAUTHY_YAML: &str =
    include_str!("../../../../deploy/catalog/core/rauthy/catalog.yaml");
const CORE_CATALOG_FORGEJO_YAML: &str =
    include_str!("../../../../deploy/catalog/core/forgejo/catalog.yaml");
const CORE_CATALOG_ENVOY_GATEWAY_YAML: &str =
    include_str!("../../../../deploy/catalog/core/envoy-gateway/catalog.yaml");
const CORE_CATALOG_PROMETHEUS_YAML: &str =
    include_str!("../../../../deploy/catalog/core/prometheus/catalog.yaml");
const CORE_CATALOG_FLAGGER_YAML: &str =
    include_str!("../../../../deploy/catalog/core/flagger/catalog.yaml");
const CORE_CATALOG_PKO_YAML: &str =
    include_str!("../../../../deploy/catalog/core/pulumi-kubernetes-operator/catalog.yaml");

async fn is_dir_empty_or_missing(dir: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return true;
    };
    !matches!(entries.next_entry().await, Ok(Some(_)))
}

/// Default tenant provisioned during Stage 0.
pub(crate) const DEFAULT_TENANT_ID: &str = "default";

/// Applies the control-plane `flux-system/git-credentials` Secret (used only by the
/// `flux-system` `GitRepository`). It is never cloned into tenant namespaces; every tenant reads
/// its own repository with a repository-scoped credential (`tenant_repo`).
pub(crate) async fn apply_git_credentials_secret(
    target: KubeTarget<'_>,
    username: &str,
    password: &str,
) -> Result<(), CliError> {
    let creds_doc = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": "git-credentials",
            "namespace": FLUX_SYSTEM_NAMESPACE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/secret-kind": "control-plane-git-credentials",
            },
        },
        "type": "Opaque",
        "stringData": {
            "username": username,
            "password": password,
        },
    });
    target.apply(&keel_core::yaml::to_string(&creds_doc)?).await
}

/// The `keel.io/platform-owned` tenant baseline (`tenants/<id>/rbac.yaml`: Namespace,
/// ServiceAccount, quota, LimitRange, NetworkPolicy, RoleBindings, auth-delegator binding).
pub(crate) fn tenant_baseline_yaml(tenant_id: &str) -> Result<String, CliError> {
    Ok(
        TenantManifestBundle::synthesize(&TenantIsolationConfig::new(tenant_id))?
            .to_multi_doc_yaml()?,
    )
}

/// How Stage 0 handles the `keel.io/platform-owned` baseline of a tenant
/// ([`tenant_baseline_action`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TenantBaselineAction {
    /// The tenant namespace exists: never re-apply the baseline. `keel-tenants`
    /// (`keel-tenant-reconciler`) owns it from Git (`tenants/<id>/rbac.yaml`), and an operator
    /// re-apply would be denied by Kyverno `protect-platform-owned-tenant-objects` for managed
    /// cluster admins outside `system:masters` / `kubeadm:cluster-admins` (EKS access entries,
    /// GKE IAM, AKS Entra ID).
    Skip,
    /// First bootstrap: the namespace is missing and the policy is not installed yet, so the
    /// operator's kubeconfig identity creates the baseline.
    ApplyAsOperator,
    /// The namespace is missing but the policy already exists (e.g. the tenant namespace was
    /// deleted): create the baseline impersonating this identity, the same confined
    /// `keel-tenant-reconciler` that `keel-tenants` uses for these objects.
    ApplyAs(String),
}

/// Pure decision for [`ensure_tenant_baseline`]: Stage 0 never writes a platform-owned object as
/// the operator once `protect-platform-owned-tenant-objects` is installed, and never re-applies an
/// existing baseline.
pub(crate) fn tenant_baseline_action(
    namespace_exists: bool,
    policy_installed: bool,
) -> TenantBaselineAction {
    match (namespace_exists, policy_installed) {
        (true, _) => TenantBaselineAction::Skip,
        (false, false) => TenantBaselineAction::ApplyAsOperator,
        (false, true) => TenantBaselineAction::ApplyAs(format!(
            "system:serviceaccount:{FLUX_SYSTEM_NAMESPACE}:{}",
            keel_kube::TENANT_RECONCILER_SERVICE_ACCOUNT
        )),
    }
}

/// `true` when `kubectl get <args> --ignore-not-found -o name` finds the object; a missing
/// resource type (CRD not installed yet) counts as absent, any other error is returned.
async fn kube_object_exists(
    target: KubeTarget<'_>,
    kind: &str,
    name: &str,
) -> Result<bool, CliError> {
    match target
        .output(&["get", kind, name, "--ignore-not-found", "-o", "name"])
        .await
    {
        Ok(out) => Ok(!out.trim().is_empty()),
        Err(err)
            if err
                .to_string()
                .contains("the server doesn't have a resource type") =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// Creates the `keel.io/platform-owned` baseline of `tenant-<id>` only when the tenant namespace
/// does not exist yet ([`tenant_baseline_action`]). Stage 0 calls this before applying the Kyverno
/// bundle, so on a first bootstrap the operator creates it before the policy exists; afterwards
/// only the unlabelled tenant Secrets are written by Stage 0.
pub(crate) async fn ensure_tenant_baseline(
    target: KubeTarget<'_>,
    tenant_id: &str,
) -> Result<TenantBaselineAction, CliError> {
    let namespace = keel_kube::tenant_namespace(tenant_id)?;
    let namespace_exists = kube_object_exists(target, "namespace", &namespace).await?;
    let policy_installed = !namespace_exists
        && kube_object_exists(
            target,
            "validatingpolicies.policies.kyverno.io",
            keel_kube::POLICY_PROTECT_PLATFORM_OWNED_TENANT_OBJECTS,
        )
        .await?;
    let action = tenant_baseline_action(namespace_exists, policy_installed);
    match &action {
        TenantBaselineAction::Skip => {}
        TenantBaselineAction::ApplyAsOperator => {
            target.apply(&tenant_baseline_yaml(tenant_id)?).await?;
        }
        TenantBaselineAction::ApplyAs(identity) => {
            target
                .apply_as(&tenant_baseline_yaml(tenant_id)?, Some(identity))
                .await
                .map_err(|err| {
                    CliError::Execution(format!(
                        "creating the `{namespace}` baseline as `{identity}` failed ({err}); \
                         Kyverno `{}` only admits keel-tenant-reconciler, keel-server and cluster \
                         admins in `system:masters`/`kubeadm:cluster-admins`, and the current \
                         kubeconfig identity needs `impersonate` on that ServiceAccount",
                        keel_kube::POLICY_PROTECT_PLATFORM_OWNED_TENANT_OBJECTS
                    ))
                })?;
        }
    }
    Ok(action)
}

/// Renders the Flux `HelmRelease` that adopts the Stage 0 Kyverno release in place, keeping
/// `features.policyExceptions.enabled` identical to the Stage 0 install (no value drift).
pub(crate) fn render_seed_kyverno_helmrelease(policy_exceptions: bool) -> String {
    SEED_CORE_ADDON_KYVERNO_YAML.replace(
        "enabled: true # keel:policy-exceptions",
        if policy_exceptions {
            "enabled: true # keel:policy-exceptions"
        } else {
            "enabled: false # keel:policy-exceptions"
        },
    )
}

/// Renders the Rauthy HTTPRoute (`addons/rauthy/httproute.yaml`) for the Stage 0 ingress host (both `keel-ingress` HTTPS listeners). The same document is written to Git and
/// applied by Stage 0, so Flux never rewrites the live route.
pub(crate) fn render_rauthy_httproute(ingress: &Stage0Ingress) -> Result<String, CliError> {
    render_manifest(
        SEED_ADDON_RAUTHY_HTTPROUTE_YAML,
        &[("IDP_HOST", ingress.idp_host.as_str())],
    )
}

/// Public Forgejo `ROOT_URL` (`https://forgejo.<domain>[:port]/`), identical for the Stage 0
/// Pulumi release (`forgejoRootUrl`) and the seed Flux HelmRelease.
pub(crate) fn forgejo_root_url(ingress: &Stage0Ingress) -> String {
    format!("{}/", ingress.forgejo_origin())
}

/// Non-secret Pulumi config of the `control-plane-forgejo` Stage 0 stack (besides the kubeconfig
/// connection). `forgejoRootUrl` must match a `config:` key of
/// `deploy/bootstrap/control-plane-forgejo/Pulumi.yaml`, otherwise Stage 0 silently falls back to
/// the default `ROOT_URL` and Flux's in-place adoption replaces the (ephemeral) Forgejo pod.
pub(crate) fn forgejo_stack_config(ingress: &Stage0Ingress) -> Vec<(&'static str, String)> {
    vec![("forgejoRootUrl", forgejo_root_url(ingress))]
}

/// Renders `core-addons/forgejo.yaml`: the Flux `HelmRelease` that adopts the Stage 0 Forgejo
/// release in place, with `ROOT_URL` from `--oidc-public-url` (host and port are validated by
/// [`Stage0Ingress`]), plus its `HelmRepository` and `forgejo-ingress` HTTPRoute.
pub(crate) fn render_seed_forgejo_helmrelease(ingress: &Stage0Ingress) -> Result<String, CliError> {
    // `ROOT_URL` is not DNS-safe (scheme, port, path), so it is substituted before the
    // DNS-validated host placeholders.
    let out =
        SEED_CORE_ADDON_FORGEJO_YAML.replace("${FORGEJO_ROOT_URL}", &forgejo_root_url(ingress));
    render_manifest(&out, &ingress.template_vars())
}

/// Renders `clusters/management/core-addons/kustomization.yaml`, listing `rauthy.yaml` only when
/// the built-in Rauthy IdP is enabled, `forgejo.yaml` only with `--with-forge` and
/// `keel-server.yaml` only with `--with-keel-server` (exactly the files [`seed_core_addon_files`]
/// writes).
fn render_seed_core_addons_kustomization(
    with_rauthy: bool,
    with_forge: bool,
    with_keel_server: bool,
) -> String {
    let mut out = SEED_CORE_ADDON_KUSTOMIZATION_YAML.to_string();
    if !with_rauthy {
        out = out.replace("  - rauthy.yaml\n", "");
    }
    if !with_forge {
        out = out.replace("  - forgejo.yaml\n", "");
    }
    if !with_keel_server {
        out = out.replace("  - keel-server.yaml\n", "");
    }
    out
}

/// Validates the Git remote an in-cluster keel-server (`--with-keel-server`) clones on start
/// (`KEEL_GIT_URL`): it must be reachable from pods (HTTP(S), not a host path or SSH, since the
/// Deployment only carries an HTTPS token) and must not embed credentials (`keel serve` rejects
/// them; the token comes from `keel-system/forge-token`).
pub(crate) fn validate_keel_server_git_url(raw: &str) -> Result<(), CliError> {
    let invalid = |why: &str| {
        CliError::Validation(format!(
            "--with-keel-server needs a cluster-reachable HTTP(S) Git remote: {why} \
             (use --with-forge or --git-url https://...)"
        ))
    };
    let parsed = url::Url::parse(raw.trim()).map_err(|_| invalid("not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(invalid("scheme must be http or https"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "the URL must not embed credentials (pass --git-token / KEEL_GIT_TOKEN instead)",
        ));
    }
    Ok(())
}

/// Rejects a `--git-url` that embeds credentials, in every bootstrap mode: the URL is written
/// to `keel.yaml` and the Flux `GitRepository`, logged on push failures and printed in the
/// summary, so a token in it would leak (`NFR-SEC-05`). HTTP(S) userinfo of any kind is
/// refused (a bare `https://<token>@host` is a credential too); other URL schemes may name a
/// user (`ssh://git@host/...`) but never a password. Local paths and scp-style remotes are
/// not URLs and pass through.
pub(crate) fn validate_git_url_has_no_credentials(
    args: &BootstrapInitArgs,
) -> Result<(), CliError> {
    let Some(raw) = args.git_url.as_deref().map(str::trim) else {
        return Ok(());
    };
    let Ok(parsed) = url::Url::parse(raw) else {
        return Ok(());
    };
    let http = matches!(parsed.scheme(), "http" | "https");
    if parsed.password().is_some() || (http && !parsed.username().is_empty()) {
        return Err(CliError::Validation(
            "--git-url must not embed credentials (they would be committed to keel.yaml and              printed); pass --git-token / KEEL_GIT_TOKEN instead"
                .to_string(),
        ));
    }
    Ok(())
}

/// Validates `--with-keel-server` against the Git mode before anything is provisioned.
/// `tenant-default` gets its own repository on every remote forge, which needs forge API access:
/// with an external `--git-url` (not the in-cluster Forgejo, not a local path), `--git-token`
/// (or `KEEL_GIT_TOKEN`) is mandatory. Checked before any infrastructure is created.
pub(crate) fn validate_tenant_default_forge_args(args: &BootstrapInitArgs) -> Result<(), CliError> {
    if args.skip_core_install || args.with_forge {
        return Ok(());
    }
    let Some(git_url) = args.git_url.as_deref().map(str::trim) else {
        return Ok(());
    };
    let (kind, _) = infer_forge_config(
        git_url,
        args.forge_type.map(Into::into),
        args.forge_api_url.as_deref(),
    );
    let has_token = args
        .git_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    if kind != ForgeKind::Local && !has_token {
        return Err(CliError::Validation(format!(
            "--git-url {git_url} is a {kind:?} forge: tenant-default's own repository needs forge API access; pass --git-token (or set KEEL_GIT_TOKEN)"
        )));
    }
    Ok(())
}

pub(crate) fn validate_keel_server_args(args: &BootstrapInitArgs) -> Result<(), CliError> {
    if !args.with_keel_server {
        return Ok(());
    }
    if args.repo_dir.is_none() {
        // The add-on only reaches Git through the seed push of `--repo-dir`; without it Flux
        // never creates `flux-system/keel-server`.
        return Err(CliError::Validation(
            "--with-keel-server needs --repo-dir: the keel-server add-on is delivered through \
             the seeded GitOps repository"
                .to_string(),
        ));
    }
    match args.git_url.as_deref() {
        // The in-cluster server would clone the external remote with the Forgejo admin username
        // and the in-cluster Forgejo token (`keel-system/forge-token`): reject the mismatch.
        Some(_) if args.with_forge => Err(CliError::Validation(
            "--with-keel-server cannot combine --with-forge and --git-url: the in-cluster \
             server's credentials would belong to Forgejo while its remote is the external \
             --git-url; pass only one of them"
                .to_string(),
        )),
        Some(url) => validate_keel_server_git_url(url),
        None if args.with_forge => Ok(()),
        None => Err(CliError::Validation(
            "--with-keel-server needs a cluster-reachable Git remote: pass --with-forge or \
             --git-url https://... (a local --repo-dir cannot be cloned from a pod)"
                .to_string(),
        )),
    }
}

/// Where the in-cluster keel-server's `keel-system/forge-token` Secret comes from at Stage 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeelServerTokenSource {
    /// `--with-keel-server` is off: no Secret is needed.
    NotRequired,
    /// `--with-forge` provisioning writes the in-cluster Forgejo token.
    Forgejo,
    /// The external `--git-url` remote's `--git-token` (the same token the seed push uses).
    GitToken,
    /// Nothing creates it (`--skip-core-install` or no `--git-token`); the operator must.
    Missing,
}

/// Decides how `keel-system/forge-token` is created for `--with-keel-server` (pure, so every
/// flag combination is unit-tested). Assumes [`validate_keel_server_args`] passed.
pub(crate) fn keel_server_token_source(args: &BootstrapInitArgs) -> KeelServerTokenSource {
    if !args.with_keel_server {
        return KeelServerTokenSource::NotRequired;
    }
    if args.skip_core_install {
        return KeelServerTokenSource::Missing;
    }
    if args.with_forge {
        // `--with-forge` + `--git-url` is rejected by validation; never hand the external remote
        // the Forgejo token.
        return if args.git_url.is_none() {
            KeelServerTokenSource::Forgejo
        } else {
            KeelServerTokenSource::Missing
        };
    }
    if args
        .git_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        KeelServerTokenSource::GitToken
    } else {
        KeelServerTokenSource::Missing
    }
}

/// HTTPS username paired with `KEEL_GIT_TOKEN` for the in-cluster keel-server: the Stage 0 Forgejo
/// admin with `--with-forge`, otherwise `git` (the same username the seed push uses).
fn keel_server_git_username(artifacts: &SeedControlPlaneArtifacts) -> &'static str {
    if artifacts.with_forge {
        forge::FORGEJO_ADMIN_USERNAME
    } else {
        "git"
    }
}

/// Renders `clusters/management/addons/keel-server/kustomization.yaml`: the shared RBAC +
/// Deployment, the ingress HTTPRoute and the `keel-server-config` ConfigMap (Git remote + HTTPS
/// username; never secrets). Serialized with `noyalib`, so the user-supplied Git URL can never
/// inject YAML. No `images:` override: the Flux Kustomization `spec.images` is the single version
/// source.
fn render_keel_server_addon_kustomization(
    artifacts: &SeedControlPlaneArtifacts,
) -> Result<String, CliError> {
    let repo_url = artifacts.keel_config.forge.repo_url.trim();
    validate_keel_server_git_url(repo_url)?;
    let doc = serde_json::json!({
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": ["rbac.yaml", "deployment.yaml", "httproute.yaml"],
        "configMapGenerator": [{
            "name": "keel-server-config",
            "namespace": KEEL_SYSTEM_NAMESPACE,
            "options": {
                "labels": {
                    "app.kubernetes.io/name": "keel-server",
                    "app.kubernetes.io/managed-by": "keel",
                },
            },
            "literals": [
                format!("KEEL_GIT_URL={repo_url}"),
                format!("KEEL_GIT_USERNAME={}", keel_server_git_username(artifacts)),
            ],
        }],
    });
    Ok(format!(
        "# keel-server plain manifests (`keel bootstrap init --with-keel-server`, `SDD.md` §6.3),\n\
         # reconciled by the Flux Kustomization `flux-system/keel-server`\n\
         # (`clusters/management/core-addons/keel-server.yaml`), which pins the image tag through\n\
         # `spec.images[].newTag`. `KEEL_GIT_TOKEN` comes from the `keel-system/forge-token` Secret,\n\
         # which is never stored in Git.\n{}",
        keel_core::yaml::to_string(&doc)?
    ))
}

/// Renders the keel-server HTTPRoute (`addons/keel-server/httproute.yaml`) for the portal host
/// `keel.<domain>` derived from `--oidc-public-url` (both `keel-ingress` HTTPS listeners).
pub(crate) fn render_keel_server_httproute(ingress: &Stage0Ingress) -> Result<String, CliError> {
    render_manifest(
        SEED_ADDON_KEEL_SERVER_HTTPROUTE_YAML,
        &[("KEEL_HOST", ingress.keel_host.as_str())],
    )
}

/// Files of the keel-server plain-manifest add-on (`clusters/management/addons/keel-server/`).
fn keel_server_addon_files(
    artifacts: &SeedControlPlaneArtifacts,
) -> Result<Vec<(String, String)>, CliError> {
    Ok([
        (
            "kustomization.yaml",
            render_keel_server_addon_kustomization(artifacts)?,
        ),
        ("rbac.yaml", KEEL_SERVER_RBAC_YAML.to_string()),
        ("deployment.yaml", KEEL_SERVER_DEPLOYMENT_YAML.to_string()),
        (
            "httproute.yaml",
            render_keel_server_httproute(&artifacts.ingress)?,
        ),
    ]
    .into_iter()
    .map(|(name, contents)| (format!("{KEEL_SERVER_ADDON_DIR}/{name}"), contents))
    .collect())
}

/// Post-bootstrap notes for `--with-keel-server`: where the server is reachable and which
/// out-of-Git prerequisites (never written to Git) still have to be created.
pub(crate) fn keel_server_addon_notes(
    artifacts: &SeedControlPlaneArtifacts,
    forge_token_created: bool,
) -> Vec<String> {
    if !artifacts.with_keel_server {
        return Vec::new();
    }
    let ingress = &artifacts.ingress;
    let mut notes = vec![format!(
        "keel-server runs in-cluster as the Flux add-on `flux-system/keel-server` \
         ({KEEL_SERVER_IMAGE}:{KEEL_SERVER_IMAGE_TAG}): open {} (make `{}` resolve to the ingress) \
         or `kubectl -n {KEEL_SYSTEM_NAMESPACE} port-forward svc/keel-server 8080:8080`",
        ingress.keel_origin(),
        ingress.keel_host
    )];
    if !forge_token_created {
        notes.push(format!(
            "keel-server needs the `{KEEL_SYSTEM_NAMESPACE}/{FORGE_TOKEN_SECRET_NAME}` Secret (key \
             `token`: a forge token with repository write and pull-request access); it is never \
             stored in Git. Create it before the Deployment can start, e.g. \
             `kubectl -n {KEEL_SYSTEM_NAMESPACE} create secret generic {FORGE_TOKEN_SECRET_NAME} \
             --from-file=token=<path-to-token-file>`"
        ));
    }
    if artifacts.rauthy_config.is_none()
        && let Some(oidc) = artifacts.keel_config.oidc.as_ref()
    {
        notes.push(format!(
            "register `{}/api/v1/auth/callback` (and the port-forward origin \
             `http://127.0.0.1:8080/api/v1/auth/callback`) as redirect URIs of OIDC client `{}` \
             at {}",
            ingress.keel_origin(),
            oidc.client_id,
            oidc.issuer_url
        ));
    }
    notes
}

/// Management-plane files written to `clusters/management/{flux-system,core-addons,addons}/`.
/// Everything Flux reconciles there is rendered here, so Git and the Stage 0 install always agree:
/// the root Flux Kustomizations ([`SEED_FLUX_GOTK_SYNC_YAML`]), the cert-manager wildcard hosts and
/// the Rauthy HTTPRoute (from `--oidc-public-url`), the Kyverno bundle and the default-tenant
/// GitRepository. Rauthy files are only included when the built-in Rauthy IdP is enabled; its
/// `rauthy-config` Secret is never written to Git.
fn seed_management_files(
    artifacts: &SeedControlPlaneArtifacts,
) -> Result<Vec<(String, String)>, CliError> {
    let ingress = &artifacts.ingress;
    let with_rauthy = artifacts.rauthy_config.is_some();
    let flux = "clusters/management/flux-system";
    let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;
    let mut files: Vec<(String, String)> = vec![
        (
            format!("{flux}/kustomization.yaml"),
            SEED_FLUX_SYSTEM_KUSTOMIZATION_YAML.to_string(),
        ),
        (
            format!("{flux}/git-repository.yaml"),
            artifacts.flux_git_repository.to_yaml()?,
        ),
        (
            format!("{flux}/tenant-reconciler-rbac.yaml"),
            keel_kube::build_tenant_reconciler_rbac_yaml()?,
        ),
        (
            format!("{flux}/gotk-sync.yaml"),
            SEED_FLUX_GOTK_SYNC_YAML.to_string(),
        ),
        (
            format!("{core}/kustomization.yaml"),
            render_seed_core_addons_kustomization(
                with_rauthy,
                artifacts.with_forge,
                artifacts.with_keel_server,
            ),
        ),
    ];
    files.extend(seed_core_addon_files(artifacts)?);
    if artifacts.with_keel_server {
        files.extend(keel_server_addon_files(artifacts)?);
    }
    if with_rauthy {
        for (name, contents) in [
            (
                "kustomization.yaml",
                SEED_ADDON_RAUTHY_KUSTOMIZATION_YAML.to_string(),
            ),
            ("services.yaml", SEED_ADDON_RAUTHY_SERVICES_YAML.to_string()),
            (
                "statefulset.yaml",
                SEED_ADDON_RAUTHY_STATEFULSET_YAML.to_string(),
            ),
            ("httproute.yaml", render_rauthy_httproute(ingress)?),
        ] {
            files.push((format!("{RAUTHY_ADDON_DIR}/{name}"), contents));
        }
    }
    Ok(files)
}

/// Files under `clusters/management/core-addons/` other than its `kustomization.yaml`.
fn seed_core_addon_files(
    artifacts: &SeedControlPlaneArtifacts,
) -> Result<Vec<(String, String)>, CliError> {
    let ingress_vars = artifacts.ingress.template_vars();
    let mut files = vec![
        (
            "helm-repositories.yaml",
            SEED_CORE_ADDON_HELM_REPOSITORIES_YAML.to_string(),
        ),
        ("flux2.yaml", SEED_CORE_ADDON_FLUX2_YAML.to_string()),
        (
            "s3-seaweedfs.yaml",
            SEED_CORE_ADDON_S3_SEAWEEDFS_YAML.to_string(),
        ),
        ("pko.yaml", SEED_CORE_ADDON_PKO_YAML.to_string()),
        (
            "kyverno.yaml",
            render_seed_kyverno_helmrelease(artifacts.kyverno_bundle.policy_exception.is_some()),
        ),
        (
            "kyverno-policies.yaml",
            artifacts.kyverno_bundle.to_multi_doc_yaml()?,
        ),
        (
            "envoy-gateway.yaml",
            SEED_CORE_ADDON_ENVOY_GATEWAY_YAML.to_string(),
        ),
        (
            "prometheus.yaml",
            SEED_CORE_ADDON_PROMETHEUS_YAML.to_string(),
        ),
        ("flagger.yaml", SEED_CORE_ADDON_FLAGGER_YAML.to_string()),
        (
            "cert-manager.yaml",
            render_manifest(SEED_CORE_ADDON_CERT_MANAGER_YAML, &ingress_vars)?,
        ),
    ];
    if artifacts.rauthy_config.is_some() {
        files.push(("rauthy.yaml", SEED_CORE_ADDON_RAUTHY_YAML.to_string()));
    }
    if artifacts.with_forge {
        files.push((
            "forgejo.yaml",
            render_seed_forgejo_helmrelease(&artifacts.ingress)?,
        ));
    }
    if artifacts.with_keel_server {
        files.push((
            "keel-server.yaml",
            SEED_CORE_ADDON_KEEL_SERVER_YAML.to_string(),
        ));
    }
    Ok(files
        .into_iter()
        .map(|(name, contents)| {
            (
                format!("{}/{name}", keel_core::MANAGEMENT_CORE_ADDONS_DIR),
                contents,
            )
        })
        .collect())
}

/// `catalog/core/<id>/catalog.yaml` files; Rauthy only when the built-in Rauthy IdP is enabled,
/// Forgejo only with `--with-forge`, keel-server only with `--with-keel-server`.
fn seed_core_catalog_files(artifacts: &SeedControlPlaneArtifacts) -> Vec<(String, String)> {
    let mut files = vec![
        (
            "s3-state-backend",
            CORE_CATALOG_S3_STATE_BACKEND_YAML.to_string(),
        ),
        ("flux2", CORE_CATALOG_FLUX2_YAML.to_string()),
        ("kyverno", CORE_CATALOG_KYVERNO_YAML.to_string()),
        ("cert-manager", CORE_CATALOG_CERT_MANAGER_YAML.to_string()),
        ("envoy-gateway", CORE_CATALOG_ENVOY_GATEWAY_YAML.to_string()),
        ("prometheus", CORE_CATALOG_PROMETHEUS_YAML.to_string()),
        ("flagger", CORE_CATALOG_FLAGGER_YAML.to_string()),
        (
            "pulumi-kubernetes-operator",
            CORE_CATALOG_PKO_YAML.to_string(),
        ),
    ];
    if artifacts.rauthy_config.is_some() {
        // The admin UI link follows `--oidc-public-url`.
        files.push((
            "rauthy",
            CORE_CATALOG_RAUTHY_YAML
                .replace(DEFAULT_OIDC_PUBLIC_URL, &artifacts.ingress.idp_origin),
        ));
    }
    if artifacts.with_forge {
        // The service link follows `--oidc-public-url` (`https://forgejo.<domain>[:port]`).
        files.push((
            "forgejo",
            CORE_CATALOG_FORGEJO_YAML.replace(
                &Stage0Ingress::default().forgejo_origin(),
                &artifacts.ingress.forgejo_origin(),
            ),
        ));
    }
    if artifacts.with_keel_server {
        // The portal link follows `--oidc-public-url` (`https://keel.<domain>[:port]`).
        files.push((
            "keel-server",
            CORE_CATALOG_KEEL_SERVER_YAML.replace(
                &Stage0Ingress::default().keel_origin(),
                &artifacts.ingress.keel_origin(),
            ),
        ));
    }
    files
        .into_iter()
        .map(|(id, contents)| (format!("catalog/core/{id}/catalog.yaml"), contents))
        .collect()
}

pub(crate) fn render_default_tenant_git_repository_yaml(
    repo: &FluxGitRepository,
) -> Result<String, CliError> {
    // Platform-written tenant source: only keel-tenant-reconciler / keel-server may change it
    // (Kyverno `protect-platform-owned-tenant-objects`).
    let mut repo = repo.clone();
    repo.metadata.labels.insert(
        keel_kube::PLATFORM_OWNED_LABEL.to_string(),
        "true".to_string(),
    );
    // The tenant's `allow-pko-workspace` NetworkPolicy is part of the baseline
    // (`tenants/<id>/rbac.yaml`, `keel_kube::build_tenant_pko_workspace_network_policy`) for every
    // tenant, so it is not repeated here.
    let mut repo_yaml = repo.to_yaml()?;
    if !repo_yaml.ends_with('\n') {
        repo_yaml.push('\n');
    }
    Ok(repo_yaml)
}

/// Initializes a local seed GitOps repository on `main` using [`GitRunner`] (zero shell calls),
/// optionally pushing to `push_url_override` with `push_credentials`.
async fn initialize_seed_git_repo(
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

    // tenant-default: the dedicated-repository stub on every remote forge (its content lives in
    // `<owner>/<monorepo>-tenant-default`, provisioned by `tenant_repo`); only the
    // development-only Local Git provider keeps it in this repository (D8).
    let (default_tenant_files, default_tenant_entries) =
        match tenant_repo::default_tenant_stub(&artifacts.keel_config.forge)? {
            Some(stub) => stub,
            None => {
                let default_tenant_rbac =
                    TenantManifestBundle::synthesize(&TenantIsolationConfig::new("default"))?
                        .to_multi_doc_yaml()?;
                let mut default_tenant_git_repo = artifacts.flux_git_repository.clone();
                default_tenant_git_repo.metadata.name = "keel-gitops".to_string();
                default_tenant_git_repo.metadata.namespace = "tenant-default".to_string();
                (
                    vec![
                        ("tenants/default/rbac.yaml".to_string(), default_tenant_rbac),
                        (
                            "tenants/default/git-repository.yaml".to_string(),
                            render_default_tenant_git_repository_yaml(&default_tenant_git_repo)?,
                        ),
                    ],
                    vec![
                        "default/rbac.yaml".to_string(),
                        "default/git-repository.yaml".to_string(),
                    ],
                )
            }
        };
    for (path, yaml) in &default_tenant_files {
        keel_kube::ensure_git_safe_manifests(path, yaml)?;
    }
    let tenants_kustomization_yaml = keel_core::yaml::to_string(&serde_json::json!({
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": default_tenant_entries,
    }))?;

    let mut files_to_write: Vec<(String, String)> = vec![
        (
            ".keel/config.yaml".to_string(),
            artifacts.keel_config.to_yaml()?,
        ),
        (
            "providers/registry.yaml".to_string(),
            artifacts.provider_registry.to_yaml()?,
        ),
        (".sops.yaml".to_string(), artifacts.sops_config.to_yaml()?),
    ];
    files_to_write.extend(seed_management_files(artifacts)?);
    files_to_write.extend(seed_core_catalog_files(artifacts));
    files_to_write.extend(
        [
            (
                "clusters/management/management-stack.yaml",
                artifacts.management_stack.to_yaml()?,
            ),
            (
                "clusters/management/Pulumi.yaml",
                "name: management-cluster-core\nruntime: yaml\ndescription: Keel Stage 0/1 protected management cluster core stack\n".to_string(),
            ),
            ("tenants/kustomization.yaml", tenants_kustomization_yaml),
        ]
        .map(|(path, contents)| (path.to_string(), contents)),
    );
    files_to_write.extend(default_tenant_files);

    let apps_dir = repo_dir.join("catalog/apps");
    let infra_dir = repo_dir.join("catalog/infra");
    let bundles_dir = repo_dir.join("catalog/bundles");
    // The whole built-in self-service catalog (apps, infra blueprints, bundles), unless the
    // repository already carries its own catalog.
    if is_dir_empty_or_missing(&infra_dir).await
        && is_dir_empty_or_missing(&apps_dir).await
        && is_dir_empty_or_missing(&bundles_dir).await
    {
        files_to_write.extend(
            BUILT_IN_CATALOG_FILES
                .iter()
                .map(|(path, contents)| (path.to_string(), contents.to_string())),
        );
    }

    let mut written_files = Vec::with_capacity(files_to_write.len());
    for (rel_path, contents) in &files_to_write {
        let full_path = repo_dir.join(rel_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full_path, contents).await?;
        written_files.push(rel_path.clone());
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

    let mut pushed = true;
    let origin_url = if artifacts.keel_config.forge.provider == ForgeKind::Local {
        git.push(repo_dir, "origin", "HEAD:refs/heads/main").await?;
        bare_repo_dir.to_string_lossy().into_owned()
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
                pushed = false;
            }
        }
        remote_url.clone()
    };

    Ok(SeedRepoInitResult {
        repo_dir: repo_dir.to_path_buf(),
        bare_repo_dir,
        origin_url,
        pushed,
        initial_commit_sha: commit_sha,
        written_files,
    })
}

/// Infers `(ForgeKind, Option<api_url>)` from a remote `--git-url` and optional explicit overrides
/// (a local path is the development-only Local Git provider).
pub(crate) fn infer_forge_config(
    git_url: &str,
    explicit_kind: Option<ForgeKind>,
    explicit_api_url: Option<&str>,
) -> (ForgeKind, Option<String>) {
    let parsed_slug = parse_remote_repo_url(git_url).ok();
    let kind = explicit_kind.unwrap_or_else(|| {
        let lower = git_url.to_ascii_lowercase();
        if !git_url.contains("://") && !git_url.contains('@') {
            // A local path (bare repository): the development-only Local Git provider (D8).
            ForgeKind::Local
        } else if lower.contains("github.com") {
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

#[cfg(debug_assertions)]
/// Best-effort DX helper: if a local `keel-zot-cache` Docker container is running (via `just zot-up`),
/// wire containerd `certs.d` mirrors on the newly created `kind` nodes before Stage 0 addons pull images.
/// Has zero effect when `keel-zot-cache` is not running or on non-`kind` targets.
async fn maybe_wire_local_zot_mirror(cluster_name: &str) {
    let inspect = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", "keel-zot-cache"])
        .output()
        .await;
    let Ok(out) = inspect else {
        return;
    };
    if !out.status.success() || String::from_utf8_lossy(&out.stdout).trim() != "true" {
        return;
    }

    let _ = tokio::process::Command::new("docker")
        .args(["network", "connect", "kind", "keel-zot-cache"])
        .output()
        .await;

    let nodes_out = tokio::process::Command::new("kind")
        .args(["get", "nodes", "--name", cluster_name])
        .output()
        .await;
    let Ok(nodes_out) = nodes_out else {
        return;
    };
    if !nodes_out.status.success() {
        return;
    }

    let nodes: Vec<String> = String::from_utf8_lossy(&nodes_out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if nodes.is_empty() {
        return;
    }

    const MIRRORS: &[(&str, &str)] = &[
        ("docker.io", "https://registry-1.docker.io"),
        ("ghcr.io", "https://ghcr.io"),
        ("quay.io", "https://quay.io"),
        ("registry.k8s.io", "https://registry.k8s.io"),
    ];

    for node in &nodes {
        for (reg, upstream) in MIRRORS {
            let dir = format!("/etc/containerd/certs.d/{reg}");
            let file = format!("{dir}/hosts.toml");
            let mkdir_ok = tokio::process::Command::new("docker")
                .args(["exec", node, "mkdir", "-p", &dir])
                .output()
                .await
                .is_ok_and(|o| o.status.success());
            if !mkdir_ok {
                continue;
            }
            let content = format!(
                "server = \"{upstream}\"\n\n[host.\"http://keel-zot-cache:5000\"]\n  capabilities = [\"pull\", \"resolve\"]\n"
            );
            if let Ok(mut child) = tokio::process::Command::new("docker")
                .args(["exec", "-i", node, "tee", &file])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(content.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                }
                let _ = child.wait().await;
            }
        }
        tracing::info!(
            node = %node,
            "Wired local Zot pull-through cache (`keel-zot-cache:5000`) into kind containerd"
        );
    }
}

/// Executes `keel bootstrap init` using an injected [`BootstrapTarget`] driver.
pub async fn execute_bootstrap_init_with_target(
    args: &BootstrapInitArgs,
    target: &dyn BootstrapTarget,
) -> Result<BootstrapInitOutcome, CliError> {
    execute_bootstrap_init_with_target_in(&stage0_state_root(), args, target).await
}

/// [`execute_bootstrap_init_with_target`] against an explicit Stage 0 state root (tests).
async fn execute_bootstrap_init_with_target_in(
    stage0_root: &Path,
    args: &BootstrapInitArgs,
    target: &dyn BootstrapTarget,
) -> Result<BootstrapInitOutcome, CliError> {
    let cfg = args.to_bootstrap_config();
    cfg.validate()?;
    validate_git_url_has_no_credentials(args)?;
    validate_keel_server_args(args)?;
    validate_tenant_default_forge_args(args)?;
    let ingress = match args.oidc_public_url.as_deref() {
        Some(url) => Stage0Ingress::from_public_url(url)?,
        None => Stage0Ingress::default(),
    };

    tracing::info!(
        target = %cfg.target,
        cluster_name = %cfg.cluster_name,
        local_dev_docker_socket_exception = cfg.local_dev_docker_socket_exception,
        skip_core_install = args.skip_core_install,
        "Executing Stage 0 bootstrap initialization"
    );

    if !args.skip_core_install && cfg.target == keel_core::BootstrapTargetKind::Kind {
        keel_core::clear_stale_kind_stage0_state_in(stage0_root, &cfg.cluster_name).await;
    }
    let stage0 = if !args.skip_core_install {
        // Resolves/generates the passphrase before the target
        // driver runs Pulumi, so both use the same durable state root and passphrase.
        Some(state::Stage0Context::prepare_in(
            stage0_root,
            &cfg.cluster_name,
        )?)
    } else {
        None
    };

    let result = bootstrap_init_after_prepare(args, target, &cfg, ingress, stage0.as_ref()).await;
    if let Some(notice) = settle_escrow_notice(stage0.as_ref(), &result) {
        // The failure path never reaches the summary: the passphrase file already exists, so
        // the operator must learn where it is now (path only, never the value).
        eprintln!("{notice}");
    }
    result
}

/// Decides what happens to a pending passphrase escrow notice once `keel bootstrap init` ends:
/// a run whose summary carries the notice (success, or [`CliError::Incomplete`], whose output is
/// still printed) acknowledges it; any other failure returns it for stderr and keeps it pending
/// so the next run repeats it.
fn settle_escrow_notice(
    stage0: Option<&state::Stage0Context>,
    result: &Result<BootstrapInitOutcome, CliError>,
) -> Option<String> {
    let stage0 = stage0?;
    match result {
        Ok(_) | Err(CliError::Incomplete { .. }) => {
            stage0.acknowledge_escrow_notice();
            None
        }
        Err(_) => stage0.escrow_notice(false),
    }
}

/// Everything after the Stage 0 context is prepared: provisioning, add-ons, seed repository,
/// tenant-default, S3 escrow and the summary.
async fn bootstrap_init_after_prepare(
    args: &BootstrapInitArgs,
    target: &dyn BootstrapTarget,
    cfg: &BootstrapConfig,
    ingress: Stage0Ingress,
    stage0: Option<&state::Stage0Context>,
) -> Result<BootstrapInitOutcome, CliError> {
    let connection = target.provision_cluster(cfg).await?;

    #[cfg(debug_assertions)]
    if cfg.target.as_str() == "kind" {
        maybe_wire_local_zot_mirror(&cfg.cluster_name).await;
    }

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
    let mut artifacts =
        synthesize_seed_control_plane(cfg, repo_url_override.as_deref(), ingress).await?;

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

    if cfg.with_forge
        || args
            .git_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
    {
        artifacts.flux_git_repository.spec.secret_ref = Some(LocalObjectReference {
            name: "git-credentials".to_string(),
        });
    }

    let mut notes: Vec<String> = Vec::new();
    let kube_target = KubeTarget::new(
        connection.context.as_deref(),
        connection.kubeconfig_path.as_deref(),
    );
    if !args.skip_core_install {
        let stage0 = stage0.expect("Stage 0 context is prepared when core install runs");
        notes.extend(
            addons::install_stage0_core_addons(
                &connection,
                &artifacts,
                stage0,
                cfg.with_forge,
                args.git_token.as_deref(),
            )
            .await?,
        );
    }

    // `--with-forge` creates `keel-system/forge-token` while provisioning Forgejo (below). With an
    // external `--git-url`, the in-cluster keel-server gets the `--git-token` the seed push uses;
    // the token goes to the cluster only, never to Git.
    let keel_server_token = keel_server_token_source(args);
    if keel_server_token == KeelServerTokenSource::GitToken
        && let Some(token) = args
            .git_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    {
        forge::apply_forge_token_secret(kube_target, token).await?;
    }

    let mut _forgejo_pf: Option<TempPortForward> = None;
    let mut push_url_override: Option<String> = None;
    let mut push_creds: Option<GitCredentials> = None;
    // (repo URL, API base, token) for the post-push Forgejo API access check.
    let mut forgejo_access_check: Option<(String, String, zeroize::Zeroizing<String>)> = None;

    if cfg.with_forge && !args.skip_core_install {
        tracing::info!("Provisioning `keel/infra-gitops` repository and API token in Forgejo");
        let pf = start_forgejo_port_forward(
            connection.context.as_deref(),
            connection.kubeconfig_path.as_deref(),
        )
        .await?;
        let forgejo_oidc_secret = artifacts
            .rauthy_config
            .as_ref()
            .and_then(|r| r.forgejo_oidc_secret.as_deref());
        let token = forge::provision_in_cluster_forgejo(
            kube_target,
            pf.local_port,
            forgejo_oidc_secret,
            &artifacts.ingress.discovery_url(),
            &mut notes,
        )
        .await?;
        push_url_override = Some(format!(
            "http://127.0.0.1:{}/keel/infra-gitops.git",
            pf.local_port
        ));
        forgejo_access_check = Some((
            format!("http://127.0.0.1:{}/keel/infra-gitops.git", pf.local_port),
            format!("http://127.0.0.1:{}", pf.local_port),
            token.clone(),
        ));
        push_creds = Some(GitCredentials::HttpsBasic {
            username: forge::FORGEJO_ADMIN_USERNAME.to_string(),
            password_or_token: token.as_str().to_string(),
        });
        _forgejo_pf = Some(pf);
    } else if args.git_url.is_some()
        && let Some(token) = args
            .git_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    {
        if let Err(err) = forge::ensure_external_repo_exists(
            artifacts.keel_config.forge.provider,
            &artifacts.keel_config.forge.repo_url,
            artifacts.keel_config.forge.api_url.as_deref(),
            token,
        )
        .await
        {
            let msg = format!(
                "could not verify/create remote repository {}: {err}. The seed commit will still be attempted; create the repository manually if the push fails",
                artifacts.keel_config.forge.repo_url
            );
            tracing::warn!("{msg}");
            notes.push(msg);
        }
        push_creds = Some(GitCredentials::HttpsBasic {
            username: "git".to_string(),
            password_or_token: token.to_string(),
        });
    }

    let seed_repo = if let Some(repo_dir) = &args.repo_dir {
        Some(
            initialize_seed_git_repo(
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

    // Verify the `keel serve` token against the real ForgeProvider now that the repository
    // has content (the port-forward is still alive via `_forgejo_pf`).
    if let Some((repo_url, api_base, token)) = forgejo_access_check.take()
        && seed_repo.is_some()
        && !forge::verify_forge_access(ForgeKind::Forgejo, &repo_url, Some(&api_base), &token).await
    {
        notes.push(
            "Forgejo API access check failed after the seed push; `keel serve` may be unable to open PRs until the token in keel-system/forge-token is fixed".to_string(),
        );
    }

    // tenant-default: its own repository, repository-scoped read credential and mandatory SOPS
    // key, provisioned like an API-created tenant (the platform repository's read token is never
    // shared with it). Failures are reported, never papered over with a shared credential.
    let mut tenant_default_failure = None;
    if !args.skip_core_install {
        let provisioned = async {
            let access = if let Some(pf) = _forgejo_pf.as_ref() {
                let password = forge::resolve_forgejo_admin_password(kube_target).await;
                Some(
                    tenant_repo::in_cluster_forgejo_forge(
                        kube_target,
                        &artifacts.keel_config.forge,
                        pf.local_port,
                        forge::FORGEJO_ADMIN_USERNAME,
                        password.as_str(),
                    )
                    .await?,
                )
            } else if args.git_url.is_some()
                && artifacts.keel_config.forge.provider != ForgeKind::Local
            {
                let token = args
                    .git_token
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| {
                        CliError::Validation(
                            "tenant-default's repository needs forge API access: pass --git-token"
                                .to_string(),
                        )
                    })?;
                tenant_repo::external_forge(kube_target, &artifacts.keel_config.forge, token)
                    .await?
            } else {
                None
            };
            match access {
                Some(access) => {
                    tenant_repo::provision_default_tenant_repo(kube_target, &access).await
                }
                // Local Git provider (D8): tenant-default lives in the platform repository.
                None => tenant_repo::ensure_default_tenant_age_key(kube_target)
                    .await
                    .map(|created| {
                        format!(
                            "tenant-default lives in the platform repository (Local Git provider, development only, no isolation); SOPS age key {}",
                            if created { "created" } else { "kept" }
                        )
                    }),
            }
        }
        .await;
        match provisioned {
            Ok(note) => notes.push(note),
            Err(err) => {
                let msg = format!(
                    "ERROR: tenant-default was NOT fully provisioned ({err}); its Flux source stays unavailable until this succeeds. Re-run `keel bootstrap init` to retry"
                );
                tracing::error!("{msg}");
                notes.push(msg);
                // Escrow and the one-time credentials below still run; the command then fails.
                tenant_default_failure = Some(err.to_string());
            }
        }
    }

    // Escrow Stage 0 state to the in-cluster versioned S3 bucket (break-glass, NFR-REL-01).
    if !args.skip_core_install
        && let Some(stage0) = stage0
    {
        match state::sync_stage0_state_to_s3(kube_target, stage0).await {
            Ok(report) => {
                notes.push(format!(
                    "Stage 0 state escrowed to {} ({} stacks; bucket versioning: {}{})",
                    report.backend_url.split('?').next().unwrap_or_default(),
                    report.stacks.len(),
                    if report.versioning_enabled {
                        "enabled"
                    } else {
                        "unavailable"
                    },
                    if report.snapshots.is_empty() {
                        String::new()
                    } else {
                        format!("; snapshots: {}", report.snapshots.len())
                    }
                ));
            }
            Err(err) => {
                let msg = format!(
                    "Stage 0 state was NOT escrowed to S3 ({err}); the local copy remains in {}. Re-run `keel bootstrap init` to retry",
                    stage0.backend_dir.display()
                );
                tracing::warn!("{msg}");
                notes.push(msg);
            }
        }
        if let Some(notice) = stage0.escrow_notice(true) {
            notes.push(notice);
        }
    }

    if seed_repo.is_some() {
        notes.extend(keel_server_addon_notes(
            &artifacts,
            matches!(
                keel_server_token,
                KeelServerTokenSource::Forgejo | KeelServerTokenSource::GitToken
            ),
        ));
    } else if artifacts.with_keel_server {
        notes.push(
            "keel-server add-on was NOT seeded (no --repo-dir); Flux will not deploy it"
                .to_string(),
        );
    }

    let mut output = format_bootstrap_init_summary(&connection, &artifacts, seed_repo.as_ref());
    for note in &notes {
        output.push_str(&format!("\nNote: {note}\n"));
    }

    if let Some(reason) = tenant_default_failure {
        // Non-zero exit, but the summary (one-time admin password, escrow notes) is still shown.
        return Err(CliError::Incomplete {
            output,
            message: format!(
                "keel bootstrap init did not complete: tenant-default was not provisioned ({reason}); fix the cause and re-run `keel bootstrap init`"
            ),
        });
    }

    Ok(BootstrapInitOutcome {
        #[cfg(test)]
        artifacts,
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
    let flux_source = &artifacts.flux_git_repository.spec;
    if addons::flux_can_reach_git_url(&flux_source.url) {
        out.push_str(&format!(
            "Flux Polling:    interval={} (outbound pull-polling active)\n",
            flux_source.interval
        ));
    } else {
        out.push_str(
            "Flux Polling:    inactive (GitOps repository is a local path in-cluster Flux cannot reach; merges are never deployed)\n",
        );
    }
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
        out.push_str(&format_seed_repo_summary(repo));
    }
    let ingress = &artifacts.ingress;
    if let Some(ref rauthy) = artifacts.rauthy_config {
        out.push_str(&format!(
            "OIDC Provider:   Rauthy ({}/{})\n",
            KEEL_SYSTEM_NAMESPACE, RAUTHY_CONFIG_SECRET_NAME
        ));
        out.push_str(&format!("OIDC Issuer:     {}\n", ingress.issuer()));
        out.push_str(&format!("IdP Admin UI:    {}/admin\n", ingress.issuer()));
        out.push_str(&format!("Admin Username:  {}\n", artifacts.admin_username));
        if artifacts.admin_password_generated {
            out.push_str(&format!("Admin Password:  {}\n", artifacts.admin_password));
        } else {
            out.push_str("Admin Password:  [PROVIDED VIA CLI]\n");
        }
        if rauthy.forgejo_oidc_secret.is_some() {
            out.push_str(&format!(
                "Forgejo SSO URL: {} (OAuth: rauthy)\n",
                ingress.forgejo_origin()
            ));
            out.push_str(&format!(
                "Local DNS:       ensure `{}` and `{}` resolve to the ingress (e.g. /etc/hosts 127.0.0.1)\n",
                ingress.idp_host, ingress.forgejo_host
            ));
        } else {
            out.push_str(&format!(
                "Local DNS:       ensure `{}` resolves to the ingress (e.g. /etc/hosts 127.0.0.1)\n",
                ingress.idp_host
            ));
        }
    } else if let Some(ref oidc) = artifacts.keel_config.oidc {
        out.push_str(&format!("OIDC Issuer:     {}\n", oidc.issuer_url));
    }
    out.push_str("Portal URL:      http://127.0.0.1:8080\n");
    if artifacts.with_keel_server {
        out.push_str(&format!(
            "In-Cluster API:  {} (Flux add-on flux-system/keel-server, {KEEL_SERVER_IMAGE}:{KEEL_SERVER_IMAGE_TAG})\n",
            ingress.keel_origin()
        ));
    }
    out
}

/// `Seed Git Repo` / `Git Origin` summary lines: the working copy and its real `origin`, with
/// how to reach it from this workstation.
fn format_seed_repo_summary(repo: &SeedRepoInitResult) -> String {
    let mut out = format!(
        "Seed Git Repo:   {} (commit: {})\n",
        repo.repo_dir.display(),
        repo.initial_commit_sha
    );
    let origin = repo.origin_url.as_str();
    let how = if !origin.contains("://") && !origin.contains('@') {
        "local bare repository; Local Git provider, development only".to_string()
    } else if origin.contains(".svc.cluster.local") {
        let local = parse_remote_repo_url(origin)
            .map(|slug| format!("http://127.0.0.1:3000/{}.git", slug.project_path))
            .unwrap_or_else(|_| "http://127.0.0.1:3000/...".to_string());
        format!(
            "in-cluster Forgejo, not reachable from this workstation directly; `keel serve` \
             port-forwards it and rewrites origin to {local} (another port if 3000 is taken); \
             that remote only works while `keel serve` runs"
        )
    } else if repo.pushed {
        "remote forge".to_string()
    } else {
        "remote forge; the initial push FAILED: push `main` from the working copy once the \
         repository and credentials are ready"
            .to_string()
    };
    out.push_str(&format!("Git Origin:      {origin} ({how})\n"));
    out
}

#[cfg(test)]
mod seed_repo_summary_tests {
    use super::*;

    fn repo(origin: &str, pushed: bool) -> SeedRepoInitResult {
        SeedRepoInitResult {
            repo_dir: PathBuf::from("/work/keel-gitops"),
            bare_repo_dir: PathBuf::from("/work/keel-gitops.git"),
            origin_url: origin.to_string(),
            pushed,
            initial_commit_sha: "abc123".to_string(),
            written_files: Vec::new(),
        }
    }

    #[test]
    fn in_cluster_forgejo_origin_is_shown_with_the_keel_serve_route() {
        let out = format_seed_repo_summary(&repo(
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
            true,
        ));
        assert!(out.contains("Seed Git Repo:   /work/keel-gitops (commit: abc123)"));
        assert!(out.contains(
            "Git Origin:      http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git"
        ));
        assert!(out.contains("`keel serve`"));
        assert!(out.contains("http://127.0.0.1:3000/keel/infra-gitops.git"));
        // FU-2: the leftover local bare clone source is not the origin.
        assert!(!out.contains("/work/keel-gitops.git"));
    }

    #[test]
    fn local_provider_origin_is_the_bare_repository() {
        let out = format_seed_repo_summary(&repo("/work/keel-gitops.git", true));
        assert!(out.contains("Git Origin:      /work/keel-gitops.git (local bare repository"));
    }

    #[test]
    fn external_forge_origin_reports_a_failed_push() {
        let ok = format_seed_repo_summary(&repo("https://github.com/acme/gitops.git", true));
        assert!(ok.contains("Git Origin:      https://github.com/acme/gitops.git (remote forge)"));
        let failed = format_seed_repo_summary(&repo("https://github.com/acme/gitops.git", false));
        assert!(failed.contains("initial push FAILED"));
    }
}

#[cfg(test)]
mod escrow_notice_settle_tests {
    use super::*;

    /// FU-1: only a run whose summary carried the notice acknowledges it; any other failure
    /// returns it for stderr and keeps it pending for the next run.
    #[test]
    fn failed_runs_keep_the_notice_and_return_it_for_stderr() {
        if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_some() {
            eprintln!("SKIPPED: PULUMI_CONFIG_PASSPHRASE set");
            return;
        }
        let root = std::env::temp_dir().join(format!("keel-cli-settle-{}", uuid::Uuid::new_v4()));
        let ctx = state::Stage0Context::prepare_in(&root, "settle").unwrap();
        let failed: Result<BootstrapInitOutcome, CliError> =
            Err(CliError::Execution("docker is not running".to_string()));
        let notice = settle_escrow_notice(Some(&ctx), &failed).expect("notice on failure");
        assert!(notice.contains(&ctx.passphrase_file().display().to_string()));
        assert!(!notice.contains(ctx.passphrase.expose()));

        let retry = state::Stage0Context::prepare_in(&root, "settle").unwrap();
        assert!(
            retry.escrow_notice_pending,
            "re-run must still owe the notice"
        );
        let incomplete: Result<BootstrapInitOutcome, CliError> = Err(CliError::Incomplete {
            output: "summary".to_string(),
            message: "tenant-default".to_string(),
        });
        assert!(settle_escrow_notice(Some(&retry), &incomplete).is_none());

        let after = state::Stage0Context::prepare_in(&root, "settle").unwrap();
        assert!(!after.escrow_notice_pending);
        assert!(settle_escrow_notice(Some(&after), &failed).is_none());
        assert!(settle_escrow_notice(None, &failed).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    pub(super) struct FailingTarget;

    #[async_trait::async_trait]
    impl BootstrapTarget for FailingTarget {
        fn kind(&self) -> keel_core::BootstrapTargetKind {
            keel_core::BootstrapTargetKind::Kind
        }

        async fn provision_cluster(
            &self,
            _cfg: &BootstrapConfig,
        ) -> Result<keel_core::ClusterConnection, keel_core::CoreError> {
            Err(keel_core::CoreError::Validation(
                "docker is not running".to_string(),
            ))
        }

        async fn teardown(&self, _cfg: &BootstrapConfig) -> Result<(), keel_core::CoreError> {
            Ok(())
        }
    }

    /// Wiring: a full (`skip_core_install: false`) run whose provisioning fails keeps the
    /// marker and returns the provisioning error (not `Incomplete`), so the next run repeats
    /// the notice.
    #[tokio::test]
    async fn failed_provisioning_keeps_the_escrow_marker() {
        if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_some() {
            eprintln!("SKIPPED: PULUMI_CONFIG_PASSPHRASE set");
            return;
        }
        let root = std::env::temp_dir().join(format!("keel-cli-wire-{}", uuid::Uuid::new_v4()));
        let args = BootstrapInitArgs {
            target: keel_core::BootstrapTargetKind::Kind,
            cluster_name: "wire-test".to_string(),
            kubeconfig: None,
            context: None,
            proxmox_url: None,
            region: None,
            distro: None,
            with_forge: false,
            with_keel_server: false,
            oidc_issuer: None,
            oidc_provider: crate::cli::OidcProviderArg::Rauthy,
            oidc_client_id: None,
            oidc_public_url: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: false,
            repo_dir: None,
            skip_core_install: false,
            git_url: None,
            forge_type: None,
            forge_api_url: None,
            git_token: None,
            pulumi_program_dir: None,
        };
        let err = execute_bootstrap_init_with_target_in(&root, &args, &FailingTarget)
            .await
            .unwrap_err();
        assert!(!matches!(err, CliError::Incomplete { .. }));
        assert!(err.to_string().contains("docker is not running"), "{err}");
        let dir = keel_core::stage0_cluster_dir_in(&root, "wire-test");
        assert!(dir.join(keel_core::STAGE0_PASSPHRASE_FILE).is_file());
        assert!(
            dir.join(keel_core::STAGE0_ESCROW_NOTICE_PENDING_FILE)
                .is_file()
        );
        let retry = state::Stage0Context::prepare_in(&root, "wire-test").unwrap();
        assert!(retry.escrow_notice_pending);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod git_url_credentials_tests {
    use super::*;

    fn args_with(url: Option<&str>) -> BootstrapInitArgs {
        BootstrapInitArgs {
            target: keel_core::BootstrapTargetKind::Kind,
            cluster_name: "c".to_string(),
            kubeconfig: None,
            context: None,
            proxmox_url: None,
            region: None,
            distro: None,
            with_forge: false,
            with_keel_server: false,
            oidc_issuer: None,
            oidc_provider: crate::cli::OidcProviderArg::Rauthy,
            oidc_client_id: None,
            oidc_public_url: None,
            admin_username: None,
            admin_password: None,
            local_dev_docker_socket_exception: false,
            repo_dir: None,
            skip_core_install: true,
            git_url: url.map(str::to_string),
            forge_type: None,
            forge_api_url: None,
            git_token: None,
            pulumi_program_dir: None,
        }
    }

    #[test]
    fn credential_bearing_git_urls_are_rejected_in_every_mode() {
        for bad in [
            "https://user:ghp_secret@github.com/acme/gitops.git",
            "https://ghp_secret@github.com/acme/gitops.git",
            "http://oauth2:tok@gitlab.example.com/acme/gitops.git",
            "ssh://git:hunter2@github.com/acme/gitops.git",
        ] {
            let err = validate_git_url_has_no_credentials(&args_with(Some(bad)))
                .unwrap_err()
                .to_string();
            assert!(err.contains("must not embed credentials"), "{bad}: {err}");
            assert!(!err.contains("secret") && !err.contains("hunter2"), "{err}");
        }
        for ok in [
            None,
            Some("https://github.com/acme/gitops.git"),
            Some("ssh://git@github.com/acme/gitops.git"),
            Some("git@github.com:acme/gitops.git"),
            Some("/srv/git/gitops.git"),
            Some("./keel-gitops.git"),
        ] {
            assert!(
                validate_git_url_has_no_credentials(&args_with(ok)).is_ok(),
                "{ok:?}"
            );
        }
    }

    #[tokio::test]
    async fn bootstrap_init_refuses_a_credential_bearing_git_url_before_provisioning() {
        let args = args_with(Some("https://user:ghp_secret@github.com/acme/gitops.git"));
        let root = std::env::temp_dir().join(format!("keel-cli-cred-{}", uuid::Uuid::new_v4()));
        let err = execute_bootstrap_init_with_target_in(
            &root,
            &args,
            &super::escrow_notice_settle_tests::FailingTarget,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Validation(_)), "{err}");
        assert!(!root.exists());
    }
}

#[cfg(test)]
mod built_in_catalog_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every seeded catalog entry parses, every blueprint/app ships the files it points at, and
    /// every bundle item references an entry that bootstrap seeds alongside it.
    #[test]
    fn built_in_catalog_entries_parse_and_bundles_resolve() {
        let paths: BTreeSet<&str> = BUILT_IN_CATALOG_FILES.iter().map(|(p, _)| *p).collect();
        assert_eq!(paths.len(), BUILT_IN_CATALOG_FILES.len());
        let (mut apps, mut infra, mut bundles) = (BTreeSet::new(), BTreeSet::new(), Vec::new());
        for (path, contents) in BUILT_IN_CATALOG_FILES {
            let Some(rest) = path.strip_suffix("/catalog.yaml") else {
                continue;
            };
            let (kind, id) = rest
                .strip_prefix("catalog/")
                .and_then(|r| r.split_once('/'))
                .unwrap_or_else(|| panic!("unexpected catalog path {path}"));
            let parsed_id = match kind {
                "apps" => {
                    let app = keel_core::parse_catalog_application(contents)
                        .unwrap_or_else(|e| panic!("{path}: {e}"));
                    apps.insert(id);
                    app.metadata.id
                }
                "infra" => {
                    let bp = keel_core::parse_catalog_blueprint(contents)
                        .unwrap_or_else(|e| panic!("{path}: {e}"));
                    let program = format!("catalog/infra/{id}/Pulumi.yaml");
                    assert!(paths.contains(program.as_str()), "{program} missing");
                    infra.insert(id);
                    bp.metadata.id
                }
                "bundles" => {
                    let bundle = keel_core::parse_catalog_bundle(contents)
                        .unwrap_or_else(|e| panic!("{path}: {e}"));
                    let bundle_id = bundle.metadata.id.clone();
                    bundles.push(bundle);
                    bundle_id
                }
                other => panic!("unexpected catalog kind {other} in {path}"),
            };
            assert_eq!(
                parsed_id, id,
                "{path}: metadata.id must match its directory"
            );
        }
        assert!(!apps.is_empty() && !infra.is_empty() && !bundles.is_empty());
        for bundle in &bundles {
            for item in &bundle.spec.items {
                let seeded = match item.kind {
                    keel_core::BundleItemKind::App => &apps,
                    keel_core::BundleItemKind::Infra => &infra,
                };
                assert!(
                    seeded.contains(item.catalog_ref.as_str()),
                    "bundle {} references unseeded {:?} {}",
                    bundle.metadata.id,
                    item.kind,
                    item.catalog_ref
                );
            }
        }
    }
}

#[cfg(test)]
mod security_hardening_tests {
    use super::*;

    use serde_json::Value;
    use state::{CONTROL_PLANE_FORGEJO_PULUMI_YAML, CONTROL_PLANE_PULUMI_YAML};

    /// Forgejo hardening settings (`SDD.md` §6.2) as `(section, KEY=VALUE)`.
    const FORGEJO_HARDENING_SETTINGS: &[(&str, &str)] = &[
        ("repository", "DISABLE_FORKS=true"),
        ("service", "ALLOW_ONLY_EXTERNAL_REGISTRATION=true"),
        ("service", "DEFAULT_ALLOW_CREATE_ORGANIZATION=false"),
    ];

    #[test]
    fn forgejo_program_has_no_hardcoded_admin_password() {
        assert!(!CONTROL_PLANE_FORGEJO_PULUMI_YAML.contains("keel-admin-dev"));
        let doc: Value = noyalib::from_str(CONTROL_PLANE_FORGEJO_PULUMI_YAML).unwrap();
        let admin = &doc["resources"]["forgejo"]["properties"]["values"]["gitea"]["admin"];
        // The chart reads credentials from the Keel-managed Secret; no password in values.
        assert_eq!(admin["existingSecret"], forge::FORGEJO_ADMIN_SECRET_NAME);
        assert!(admin.get("password").is_none());
        // Must not collide with the chart-owned `<release>-admin` Secret (Helm ownership check).
        assert_ne!(forge::FORGEJO_ADMIN_SECRET_NAME, "forgejo-admin");
    }

    #[test]
    fn control_plane_program_enables_flux_multitenancy_and_protects_core() {
        let doc: Value = noyalib::from_str(CONTROL_PLANE_PULUMI_YAML).unwrap();
        let res = &doc["resources"];
        let flux = &res["flux2"]["properties"];
        assert!(flux["version"].as_str().is_some_and(|v| !v.is_empty()));
        assert_eq!(flux["values"]["multitenancy"]["enabled"], true);
        assert_eq!(
            flux["values"]["multitenancy"]["defaultServiceAccount"],
            "default"
        );
        for name in [
            "fluxSystemNamespace",
            "keelSystemNamespace",
            "sopsAgeSecret",
            "flux2",
            "kyverno",
            "pulumiKubernetesOperator",
            "s3StateBackend",
        ] {
            assert_eq!(
                res[name]["options"]["protect"], true,
                "{name} must be protected"
            );
        }
    }

    #[test]
    fn seed_policies_no_longer_clone_admin_credentials_into_tenants() {
        let bundle = KyvernoPolicyGenerator::new(
            keel_kube::pulumi_provider_allowlist(&keel_kube::default_allowed_pulumi_providers()),
            false,
        )
        .generate()
        .unwrap()
        .to_multi_doc_yaml()
        .unwrap();
        // The only GeneratingPolicy copies the public CA bundle ConfigMap; no Secret is cloned.
        assert!(!bundle.contains("sync-tenant-secrets"));
        assert!(bundle.contains("sync-keel-ca-bundle"));
        assert!(!bundle.contains("'secrets'"));
        assert!(!bundle.contains("keel-s3-credentials"));
        assert!(bundle.contains("restrict-tenant-reconciler"));
    }

    #[test]
    fn flux_helmreleases_adopt_stage0_releases_in_place() {
        let program: Value = noyalib::from_str(CONTROL_PLANE_PULUMI_YAML).unwrap();
        for (resource, file) in [
            ("flux2", SEED_CORE_ADDON_FLUX2_YAML),
            ("kyverno", SEED_CORE_ADDON_KYVERNO_YAML),
            ("pulumiKubernetesOperator", SEED_CORE_ADDON_PKO_YAML),
            ("s3StateBackend", SEED_CORE_ADDON_S3_SEAWEEDFS_YAML),
            ("envoyGateway", SEED_CORE_ADDON_ENVOY_GATEWAY_YAML),
            ("prometheus", SEED_CORE_ADDON_PROMETHEUS_YAML),
            ("flagger", SEED_CORE_ADDON_FLAGGER_YAML),
            ("certManager", SEED_CORE_ADDON_CERT_MANAGER_YAML),
        ] {
            let p = &program["resources"][resource]["properties"];
            let hr = file
                .split("\n---\n")
                .map(|d| noyalib::from_str::<Value>(d).unwrap())
                .find(|d| d["kind"] == "HelmRelease")
                .unwrap_or_else(|| panic!("{resource}: no HelmRelease"));
            assert_eq!(
                hr["spec"]["releaseName"], p["name"],
                "{resource} release name"
            );
            // Platform HelmReleases live in `flux-system` (impersonating `helm-controller`) and
            // install into the Stage 0 release namespace.
            assert_eq!(hr["metadata"]["namespace"], "flux-system", "{resource}");
            assert_eq!(
                hr["spec"]["targetNamespace"], p["namespace"],
                "{resource} target ns"
            );
            assert_eq!(
                hr["spec"]["storageNamespace"], p["namespace"],
                "{resource} storage ns"
            );
            assert_eq!(
                hr["spec"]["serviceAccountName"], "helm-controller",
                "{resource}"
            );
            assert_eq!(
                hr["spec"]["chart"]["spec"]["sourceRef"]["namespace"], "flux-system",
                "{resource} HelmRepository must be in the HelmRelease namespace"
            );
            let pinned = p["version"]
                .as_str()
                .unwrap_or_else(|| panic!("{resource} unpinned"));
            assert_eq!(
                hr["spec"]["chart"]["spec"]["version"].as_str().unwrap(),
                pinned,
                "{resource} version drift between Stage 0 and Flux"
            );
            // Identical values, so the first Flux reconcile never reconfigures the release.
            if resource != "kyverno" {
                assert_eq!(
                    hr["spec"]["values"], p["values"],
                    "{resource} values drift between Stage 0 and Flux"
                );
            }
        }
        // Tenant admins reach Stacks/Programs only through the PKO chart's aggregated
        // `edit`/`admin` roles; pinned in both releases so a chart default change cannot drop them.
        let pko_hr = SEED_CORE_ADDON_PKO_YAML
            .split("\n---\n")
            .map(|d| noyalib::from_str::<Value>(d).unwrap())
            .find(|d| d["kind"] == "HelmRelease")
            .unwrap();
        for values in [
            &pko_hr["spec"]["values"],
            &program["resources"]["pulumiKubernetesOperator"]["properties"]["values"],
        ] {
            assert_eq!(
                values["rbac"]["createClusterAggregationRoles"],
                Value::Bool(true)
            );
        }
        // Kyverno's Stage 0 values are templated by `enableKyvernoPolicyExceptions`.
        let stage0_kyverno = |enabled: bool| -> Value {
            serde_json::from_str(
                &program["resources"]["kyverno"]["properties"]["values"]
                    .to_string()
                    .replace(
                        "\"${enableKyvernoPolicyExceptions}\"",
                        if enabled { "true" } else { "false" },
                    ),
            )
            .unwrap()
        };
        for enabled in [true, false] {
            let hr: Value = noyalib::from_str(&render_seed_kyverno_helmrelease(enabled)).unwrap();
            assert_eq!(hr["spec"]["values"], stage0_kyverno(enabled), "kyverno");
        }
        assert_eq!(
            program["resources"]["kyverno"]["properties"]["namespace"],
            "keel-system"
        );
        assert_eq!(
            program["resources"]["kyverno"]["properties"]["version"],
            "3.10.0"
        );

        let off: Value = noyalib::from_str(&render_seed_kyverno_helmrelease(false)).unwrap();
        assert_eq!(
            off["spec"]["values"]["features"]["policyExceptions"]["enabled"],
            false
        );
        let on: Value = noyalib::from_str(&render_seed_kyverno_helmrelease(true)).unwrap();
        assert_eq!(
            on["spec"]["values"]["features"]["policyExceptions"]["enabled"],
            true
        );

        // Deprecated `kyverno.io` policy CRDs are not installed (Keel uses only the CEL
        // `policies.kyverno.io` kinds), identically in Stage 0 and the Flux handover release.
        let stage0_crds = &program["resources"]["kyverno"]["properties"]["values"]["crds"];
        assert_eq!(stage0_crds, &on["spec"]["values"]["crds"]);
        for kind in [
            "clusterpolicies",
            "policies",
            "policyexceptions",
            "cleanuppolicies",
            "clustercleanuppolicies",
        ] {
            assert_eq!(stage0_crds["groups"]["kyverno"][kind], false, "{kind}");
        }
    }

    #[test]
    fn core_catalog_versions_match_seed_core_addons() {
        fn catalog_version(raw: &str) -> String {
            let doc: Value = noyalib::from_str(raw).unwrap();
            doc["spec"]["upstream"]["version"]
                .as_str()
                .expect("catalog upstream version")
                .to_string()
        }
        let seed_version = |file: &str| -> String {
            file.split("\n---\n")
                .map(|d| noyalib::from_str::<Value>(d).unwrap())
                .find(|d| d["kind"] == "HelmRelease")
                .and_then(|hr| {
                    hr["spec"]["chart"]["spec"]["version"]
                        .as_str()
                        .map(String::from)
                })
                .expect("seed HelmRelease version")
        };
        for (id, catalog, seed) in [
            ("flux2", CORE_CATALOG_FLUX2_YAML, SEED_CORE_ADDON_FLUX2_YAML),
            (
                "kyverno",
                include_str!("../../../../deploy/catalog/core/kyverno/catalog.yaml"),
                SEED_CORE_ADDON_KYVERNO_YAML,
            ),
            (
                "pulumi-kubernetes-operator",
                include_str!(
                    "../../../../deploy/catalog/core/pulumi-kubernetes-operator/catalog.yaml"
                ),
                SEED_CORE_ADDON_PKO_YAML,
            ),
            (
                "s3-state-backend",
                include_str!("../../../../deploy/catalog/core/s3-state-backend/catalog.yaml"),
                SEED_CORE_ADDON_S3_SEAWEEDFS_YAML,
            ),
            (
                "envoy-gateway",
                include_str!("../../../../deploy/catalog/core/envoy-gateway/catalog.yaml"),
                SEED_CORE_ADDON_ENVOY_GATEWAY_YAML,
            ),
            (
                "prometheus",
                include_str!("../../../../deploy/catalog/core/prometheus/catalog.yaml"),
                SEED_CORE_ADDON_PROMETHEUS_YAML,
            ),
            (
                "flagger",
                include_str!("../../../../deploy/catalog/core/flagger/catalog.yaml"),
                SEED_CORE_ADDON_FLAGGER_YAML,
            ),
            (
                "cert-manager",
                include_str!("../../../../deploy/catalog/core/cert-manager/catalog.yaml"),
                SEED_CORE_ADDON_CERT_MANAGER_YAML,
            ),
        ] {
            assert_eq!(
                catalog_version(catalog),
                seed_version(seed),
                "{id}: deploy/catalog/core version drifts from deploy/seed/core-addons"
            );
        }
        // Optional Forgejo (`--with-forge`): catalog == seed HelmRelease == Stage 0 Pulumi release.
        let forgejo = catalog_version(CORE_CATALOG_FORGEJO_YAML);
        assert_eq!(
            forgejo,
            seed_version(SEED_CORE_ADDON_FORGEJO_YAML),
            "forgejo"
        );
        let program: Value = noyalib::from_str(CONTROL_PLANE_FORGEJO_PULUMI_YAML).unwrap();
        assert_eq!(
            program["resources"]["forgejo"]["properties"]["version"],
            forgejo.as_str(),
            "forgejo: Stage 0 Pulumi version drift"
        );
        // Rauthy is an image-based Kustomize add-on: the catalog version is the image tag pinned
        // by the Flux Kustomization `spec.images[].newTag`, the plain StatefulSet manifest and the
        // Stage 0 Pulumi StatefulSet (which Flux adopts in place).
        let catalog: Value = noyalib::from_str(CORE_CATALOG_RAUTHY_YAML).unwrap();
        let rauthy = catalog_version(CORE_CATALOG_RAUTHY_YAML);
        let image = catalog["spec"]["upstream"]["image"].as_str().unwrap();
        let ks: Value = noyalib::from_str(SEED_CORE_ADDON_RAUTHY_YAML).unwrap();
        let images = ks["spec"]["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["name"], image);
        assert_eq!(images[0]["newTag"], rauthy.as_str(), "Flux newTag drift");
        assert!(images[0].get("digest").is_none());
        let pinned = format!("{image}:{rauthy}");
        let sts: Value = noyalib::from_str(SEED_ADDON_RAUTHY_STATEFULSET_YAML).unwrap();
        assert_eq!(
            sts["spec"]["template"]["spec"]["containers"][0]["image"],
            pinned.as_str(),
            "addons/rauthy StatefulSet image drift"
        );
        let program: Value = noyalib::from_str(state::CONTROL_PLANE_RAUTHY_PULUMI_YAML).unwrap();
        assert_eq!(
            program["resources"]["rauthyStatefulSet"]["properties"]["spec"]["template"]["spec"]["containers"]
                [0]["image"],
            pinned.as_str(),
            "Stage 0 Pulumi Rauthy image drift"
        );
    }

    /// The in-cluster keel-server image is pinned to the release tag of this build everywhere
    /// it appears (Flux `spec.images`, catalog, Deployment, manual bundle), never `latest`.
    #[test]
    fn keel_server_rbac_limits_exec_and_control_plane_writes() {
        let docs: Vec<Value> = KEEL_SERVER_RBAC_YAML
            .split("\n---\n")
            .map(|d| noyalib::from_str(d).unwrap())
            .collect();
        let role = |ns: &str| {
            docs.iter()
                .find(|d| d["kind"] == "Role" && d["metadata"]["namespace"] == ns)
                .unwrap_or_else(|| panic!("Role in {ns}"))["rules"]
                .as_array()
                .unwrap()
                .clone()
        };
        fn has<'a>(rules: &'a [Value], group: &str, resource: &str) -> Option<&'a Value> {
            rules.iter().find(|r| {
                r["apiGroups"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|g| g == group)
                    && r["resources"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|x| x == resource)
            })
        }
        let keel_system = role(KEEL_SYSTEM_NAMESPACE);
        let exec = has(&keel_system, "", "pods/exec").expect("pods/exec rule");
        assert_eq!(
            exec["resourceNames"],
            serde_json::json!([keel_kube::SEAWEEDFS_FILER_POD])
        );
        // Core HelmReleases live in flux-system; no write access to keel-system HelmReleases.
        assert!(has(&keel_system, "helm.toolkit.fluxcd.io", "helmreleases").is_none());
        let flux = role("flux-system");
        for (group, resource) in [
            ("kustomize.toolkit.fluxcd.io", "kustomizations"),
            ("helm.toolkit.fluxcd.io", "helmreleases"),
            ("source.toolkit.fluxcd.io", "gitrepositories"),
        ] {
            let rule = has(&flux, group, resource).expect(resource);
            assert!(
                rule["verbs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|v| ["get", "list", "watch", "patch"].contains(&v.as_str().unwrap())),
                "{resource}: {rule}"
            );
        }
    }

    #[test]
    fn keel_server_image_is_pinned_to_the_crate_release_tag() {
        let tag = env!("CARGO_PKG_VERSION");
        assert_eq!(KEEL_SERVER_IMAGE_TAG, tag);
        keel_core::validate_management_addon_version(tag).unwrap();
        let pinned = format!("{KEEL_SERVER_IMAGE}:{tag}");

        let ks: Value = noyalib::from_str(SEED_CORE_ADDON_KEEL_SERVER_YAML).unwrap();
        let images = ks["spec"]["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["name"], KEEL_SERVER_IMAGE);
        assert_eq!(images[0]["newTag"], tag, "Flux newTag drift");
        assert!(images[0].get("digest").is_none());

        let catalog: Value = noyalib::from_str(CORE_CATALOG_KEEL_SERVER_YAML).unwrap();
        assert_eq!(catalog["spec"]["upstream"]["image"], KEEL_SERVER_IMAGE);
        assert_eq!(catalog["spec"]["upstream"]["version"], tag, "catalog drift");

        let deployment = yaml_docs(KEEL_SERVER_DEPLOYMENT_YAML)
            .into_iter()
            .find(|d| d["kind"] == "Deployment")
            .unwrap();
        assert_eq!(
            deployment["spec"]["template"]["spec"]["containers"][0]["image"],
            pinned.as_str(),
            "deploy/bootstrap/keel-server Deployment image drift"
        );
        let bundle: Value = noyalib::from_str(include_str!(
            "../../../../deploy/bootstrap/keel-server/kustomization.yaml"
        ))
        .unwrap();
        assert_eq!(bundle["images"][0]["name"], KEEL_SERVER_IMAGE);
        assert_eq!(bundle["images"][0]["newTag"], tag, "manual bundle drift");
        for raw in [
            KEEL_SERVER_DEPLOYMENT_YAML,
            SEED_CORE_ADDON_KEEL_SERVER_YAML,
            CORE_CATALOG_KEEL_SERVER_YAML,
        ] {
            assert!(!raw.contains(":latest") && !raw.contains("newTag: latest"));
        }

        // The tag format is the one the release workflow publishes (`{{version}}`, no `v`), and
        // semantic-release bumps every pinned file together with the crate version.
        let release = include_str!("../../../../.github/workflows/release.yml");
        assert!(release.contains("image=ghcr.io/${OWNER,,}/keel"));
        assert!(release.contains("type=semver,pattern={{version}}"));
        let releaserc: Value =
            serde_json::from_str(include_str!("../../../../.releaserc.json")).unwrap();
        let plugins = releaserc["plugins"].as_array().unwrap();
        let prepare = plugins
            .iter()
            .find(|p| p[0] == "@semantic-release/exec")
            .and_then(|p| p[1]["prepareCmd"].as_str())
            .unwrap();
        let assets = plugins
            .iter()
            .find(|p| p[0] == "@semantic-release/git")
            .map(|p| p[1]["assets"].clone())
            .unwrap();
        for pinned_file in [
            "deploy/bootstrap/keel-server/kustomization.yaml",
            "deploy/bootstrap/keel-server/deployment.yaml",
            "deploy/seed/core-addons/keel-server.yaml",
            "deploy/catalog/core/keel-server/catalog.yaml",
            "deploy/catalog/apps/keel-server/catalog.yaml",
            "deploy/catalog/apps/keel-server/manifests.yaml",
        ] {
            assert!(prepare.contains(pinned_file), "{pinned_file} not bumped");
            assert!(
                assets.as_array().unwrap().iter().any(|a| a == pinned_file),
                "{pinned_file} not committed by semantic-release"
            );
        }
    }

    #[test]
    fn release_is_gated_on_live_e2e_and_ci_installs_use_frozen_lockfiles() {
        let ci = include_str!("../../../../.github/workflows/ci.yml");
        let e2e = include_str!("../../../../.github/workflows/e2e.yml");
        let release = include_str!("../../../../.github/workflows/release.yml");

        // release.yml calls e2e.yml with `run_live: true` and the release job waits for it.
        let release_doc: Value = noyalib::from_str(release).unwrap();
        let jobs = release_doc["jobs"].as_object().unwrap();
        let (gate_name, gate) = jobs
            .iter()
            .find(|(_, j)| j["uses"] == "./.github/workflows/e2e.yml")
            .expect("release.yml calls e2e.yml");
        assert_eq!(gate["with"]["run_live"], true);
        let needs = jobs["release"]["needs"].as_array().unwrap();
        assert!(needs.iter().any(|n| n == gate_name.as_str()));

        // In a called workflow `github.event_name` is the caller's event, so the live jobs
        // must gate on the explicit input, never on `event_name == 'workflow_call'`.
        assert!(!e2e.contains("event_name == 'workflow_call'"));
        let e2e_doc: Value = noyalib::from_str(e2e).unwrap();
        assert_eq!(
            e2e_doc["jobs"]["e2e-live-kind"]["if"],
            "inputs.run_live == true"
        );

        for (name, raw) in [("ci.yml", ci), ("e2e.yml", e2e), ("release.yml", release)] {
            for line in raw.lines().filter(|l| {
                l.contains("pnpm") && l.contains(" install") && !l.contains("playwright install")
            }) {
                assert!(
                    line.contains("--frozen-lockfile"),
                    "{name}: unfrozen install `{}`",
                    line.trim()
                );
            }
        }
    }

    #[tokio::test]
    async fn seed_files_include_keel_server_only_with_flag() {
        for with_keel_server in [false, true] {
            let cfg = BootstrapConfig {
                with_keel_server,
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            };
            let a = artifacts_for(cfg, "https://idp.corp.example:9443").await;
            assert_eq!(a.with_keel_server, with_keel_server);
            let files = seed_management_files(&a).unwrap();
            let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;
            let listed = kustomize_resources(file(&files, &format!("{core}/kustomization.yaml")));
            assert_eq!(listed, files_in(&files, core));
            assert_eq!(listed.contains("keel-server.yaml"), with_keel_server);
            let catalogs = seed_core_catalog_files(&a);
            let catalog = catalogs
                .iter()
                .find(|(p, _)| p == "catalog/core/keel-server/catalog.yaml");
            assert_eq!(catalog.is_some(), with_keel_server);
            if !with_keel_server {
                assert!(files.iter().all(|(p, _)| !p.contains("keel-server")));
                continue;
            }

            // One Flux Kustomization named after the catalog id, reconciling the add-on dir.
            let (_, catalog) = catalog.unwrap();
            let app = keel_core::parse_catalog_application(catalog).unwrap();
            assert_eq!(app.metadata.id, "keel-server");
            let catalog_doc: Value = noyalib::from_str(catalog).unwrap();
            assert_eq!(
                catalog_doc["metadata"]["category"],
                keel_core::MANAGEMENT_CORE_CATEGORY
            );
            assert_eq!(
                catalog_doc["spec"]["serviceUrl"],
                "https://keel.corp.example:9443"
            );
            let ks: Value =
                noyalib::from_str(file(&files, &format!("{core}/keel-server.yaml"))).unwrap();
            assert_eq!(ks["kind"], "Kustomization");
            assert_eq!(ks["metadata"]["name"], app.metadata.id.as_str());
            assert_eq!(ks["metadata"]["namespace"], "flux-system");
            assert_eq!(ks["metadata"]["labels"]["keel.io/protect"], "true");
            assert_eq!(
                ks["spec"]["path"],
                format!("./{KEEL_SERVER_ADDON_DIR}").as_str()
            );
            assert_eq!(ks["spec"]["sourceRef"]["name"], "flux-system");
            // A targetNamespace would rewrite the keel-previews / flux-system Roles into
            // keel-system: the bundle names every namespace explicitly instead.
            assert!(ks["spec"].get("targetNamespace").is_none());
            let mut doc = ks.clone();
            keel_core::validate_management_addon_document("keel-server", &doc).unwrap();
            keel_core::apply_management_addon_upgrade(
                "keel-server",
                &mut doc,
                Some("9.9.9"),
                &json!({}),
            )
            .unwrap();
            assert_eq!(doc["spec"]["images"][0]["newTag"], "9.9.9");

            // Add-on directory: exactly the resources its kustomization lists.
            let addon_kustomization: Value = noyalib::from_str(file(
                &files,
                &format!("{KEEL_SERVER_ADDON_DIR}/kustomization.yaml"),
            ))
            .unwrap();
            let resources = kustomize_resources(file(
                &files,
                &format!("{KEEL_SERVER_ADDON_DIR}/kustomization.yaml"),
            ));
            assert_eq!(resources, files_in(&files, KEEL_SERVER_ADDON_DIR));
            assert_eq!(
                resources,
                ["deployment.yaml", "httproute.yaml", "rbac.yaml"]
                    .map(String::from)
                    .into()
            );
            // The Flux Kustomization is the single version source.
            assert!(addon_kustomization.get("images").is_none());
            let generator = &addon_kustomization["configMapGenerator"][0];
            assert_eq!(generator["name"], "keel-server-config");
            assert_eq!(generator["namespace"], "keel-system");
            assert_eq!(
                generator["literals"],
                json!([
                    "KEEL_GIT_URL=https://git.example.org/keel/gitops.git",
                    "KEEL_GIT_USERNAME=git"
                ])
            );

            let route = yaml_docs(file(
                &files,
                &format!("{KEEL_SERVER_ADDON_DIR}/httproute.yaml"),
            ));
            assert_eq!(route[0]["spec"]["hostnames"], json!(["keel.corp.example"]));
            assert_eq!(
                route[0]["spec"]["rules"][0]["backendRefs"][0]["name"],
                "keel-server"
            );

            let cluster_scoped = ["ClusterRole", "ClusterRoleBinding"];
            for (path, contents) in files
                .iter()
                .filter(|(p, _)| p.starts_with(&format!("{KEEL_SERVER_ADDON_DIR}/")))
            {
                assert!(
                    !contents.contains("${"),
                    "{path} has unrendered placeholders"
                );
                assert!(!contents.contains(":latest"), "{path}");
                if path.ends_with("/kustomization.yaml") {
                    continue;
                }
                for doc in yaml_docs(contents) {
                    let kind = doc["kind"].as_str().unwrap();
                    // Secrets never go to Git (`NFR-SEC-05`).
                    assert_ne!(kind, "Secret", "{path}");
                    if cluster_scoped.contains(&kind) {
                        continue;
                    }
                    let ns = doc["metadata"]["namespace"].as_str();
                    assert!(ns.is_some(), "{path} {kind} has no explicit namespace");
                    // Tenant bindings are owned by `keel-tenants` (`tenants/<id>/rbac.yaml`).
                    assert!(!ns.unwrap().starts_with("tenant-"), "{path} {kind}");
                }
            }
        }

        // `--with-forge`: the in-cluster Forgejo remote and its admin user.
        let cfg = BootstrapConfig {
            with_forge: true,
            with_keel_server: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = synthesize_seed_control_plane(&cfg, None, Stage0Ingress::default())
            .await
            .unwrap();
        let files = seed_management_files(&a).unwrap();
        let addon: Value = noyalib::from_str(file(
            &files,
            &format!("{KEEL_SERVER_ADDON_DIR}/kustomization.yaml"),
        ))
        .unwrap();
        assert_eq!(
            addon["configMapGenerator"][0]["literals"],
            json!([
                "KEEL_GIT_URL=http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
                "KEEL_GIT_USERNAME=keel"
            ])
        );

        // A host-path remote can never be cloned by the pod.
        let cfg = BootstrapConfig {
            with_keel_server: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = synthesize_seed_control_plane(
            &cfg,
            Some("/tmp/keel-gitops.git"),
            Stage0Ingress::default(),
        )
        .await
        .unwrap();
        assert!(seed_management_files(&a).is_err());
    }

    #[test]
    fn external_git_url_requires_a_forge_token_up_front() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["keel", "bootstrap", "init", "--target", "kind"];
            argv.extend_from_slice(extra);
            match <crate::cli::Cli as clap::Parser>::try_parse_from(argv)
                .unwrap()
                .command
            {
                crate::cli::Commands::Bootstrap(crate::cli::BootstrapArgs {
                    command: crate::cli::BootstrapCommands::Init(args),
                }) => args,
                other => panic!("unexpected command: {other:?}"),
            }
        };
        // A blank token counts as missing (also overrides an ambient KEEL_GIT_TOKEN).
        let err = validate_tenant_default_forge_args(&parse(&[
            "--git-url",
            "https://github.com/acme/infra.git",
            "--git-token",
            " ",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("--git-token"), "{err}");
        validate_tenant_default_forge_args(&parse(&[
            "--git-url",
            "https://github.com/acme/infra.git",
            "--git-token",
            "ghp_x",
        ]))
        .unwrap();
        // Local paths (Local Git provider, D8), --skip-core-install and no --git-url need none.
        validate_tenant_default_forge_args(&parse(&[
            "--git-url",
            "/tmp/keel-gitops.git",
            "--git-token",
            " ",
        ]))
        .unwrap();
        validate_tenant_default_forge_args(&parse(&[
            "--git-url",
            "https://github.com/acme/infra.git",
            "--git-token",
            " ",
            "--skip-core-install",
        ]))
        .unwrap();
        validate_tenant_default_forge_args(&parse(&[])).unwrap();
    }

    #[test]
    fn keel_server_git_remote_must_be_cluster_reachable_https() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["keel", "bootstrap", "init", "--target", "kind"];
            argv.extend_from_slice(extra);
            match <crate::cli::Cli as clap::Parser>::try_parse_from(argv)
                .unwrap()
                .command
            {
                crate::cli::Commands::Bootstrap(crate::cli::BootstrapArgs {
                    command: crate::cli::BootstrapCommands::Init(args),
                }) => args,
                other => panic!("unexpected command: {other:?}"),
            }
        };
        // Without the flag nothing is required.
        validate_keel_server_args(&parse(&["--repo-dir", "/tmp/gitops"])).unwrap();
        // A local repository is not reachable from pods.
        let err =
            validate_keel_server_args(&parse(&["--with-keel-server", "--repo-dir", "/tmp/x"]))
                .unwrap_err()
                .to_string();
        assert!(err.contains("--with-forge"), "{err}");
        validate_keel_server_args(&parse(&[
            "--with-keel-server",
            "--with-forge",
            "--repo-dir",
            "/tmp/x",
        ]))
        .unwrap();
        validate_keel_server_args(&parse(&[
            "--with-keel-server",
            "--git-url",
            "https://github.com/acme/infra-gitops.git",
            "--repo-dir",
            "/tmp/x",
        ]))
        .unwrap();
        // Without --repo-dir nothing reaches Git, so Flux never deploys the add-on.
        for extra in [
            &["--with-forge"][..],
            &["--git-url", "https://github.com/acme/infra-gitops.git"][..],
        ] {
            let mut argv = vec!["--with-keel-server"];
            argv.extend_from_slice(extra);
            let err = validate_keel_server_args(&parse(&argv))
                .unwrap_err()
                .to_string();
            assert!(err.contains("--repo-dir"), "{err}");
        }
        // Forgejo credentials must never be paired with an external remote.
        let err = validate_keel_server_args(&parse(&[
            "--with-keel-server",
            "--with-forge",
            "--git-url",
            "https://github.com/acme/infra-gitops.git",
            "--repo-dir",
            "/tmp/x",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("--with-forge and --git-url"), "{err}");
        // Messages carry the case index only: some cases embed sample credentials.
        for (i, bad) in [
            "git@github.com:acme/infra-gitops.git",
            "ssh://git@github.com/acme/infra-gitops.git",
            "file:///srv/git/infra-gitops.git",
            "https://user:token@github.com/acme/infra-gitops.git",
            "https://x-access-token@github.com/acme/infra-gitops.git",
            "/srv/git/infra-gitops.git",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                validate_keel_server_args(&parse(&[
                    "--with-keel-server",
                    "--repo-dir",
                    "/tmp/x",
                    "--git-url",
                    bad
                ]))
                .is_err(),
                "git URL case #{i} accepted"
            );
            assert!(
                validate_keel_server_git_url(bad).is_err(),
                "git URL case #{i} accepted"
            );
        }
    }

    #[test]
    fn keel_server_token_source_covers_every_git_mode() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["keel", "bootstrap", "init", "--target", "kind"];
            argv.extend_from_slice(extra);
            match <crate::cli::Cli as clap::Parser>::try_parse_from(argv)
                .unwrap()
                .command
            {
                crate::cli::Commands::Bootstrap(crate::cli::BootstrapArgs {
                    command: crate::cli::BootstrapCommands::Init(args),
                }) => args,
                other => panic!("unexpected command: {other:?}"),
            }
        };
        let url = "https://github.com/acme/infra-gitops.git";
        let cases: &[(&[&str], KeelServerTokenSource)] = &[
            (&["--with-forge"], KeelServerTokenSource::NotRequired),
            (
                &["--with-keel-server", "--with-forge"],
                KeelServerTokenSource::Forgejo,
            ),
            (
                &["--with-keel-server", "--git-url", url, "--git-token", "t0k"],
                KeelServerTokenSource::GitToken,
            ),
            (
                &["--with-keel-server", "--git-url", url, "--git-token", "  "],
                KeelServerTokenSource::Missing,
            ),
            (
                &["--with-keel-server", "--git-url", url],
                KeelServerTokenSource::Missing,
            ),
            (
                &[
                    "--with-keel-server",
                    "--git-url",
                    url,
                    "--git-token",
                    "t0k",
                    "--skip-core-install",
                ],
                KeelServerTokenSource::Missing,
            ),
            (
                &["--with-keel-server", "--with-forge", "--skip-core-install"],
                KeelServerTokenSource::Missing,
            ),
            // Rejected by validation; never pair the Forgejo token with an external remote.
            (
                &["--with-keel-server", "--with-forge", "--git-url", url],
                KeelServerTokenSource::Missing,
            ),
        ];
        // Messages carry the case index only: some argv vectors include a sample token.
        for (i, (argv, expected)) in cases.iter().enumerate() {
            let mut args = parse(argv);
            if !argv.contains(&"--git-token") {
                // Ignore an ambient `KEEL_GIT_TOKEN`.
                args.git_token = None;
            }
            assert!(
                keel_server_token_source(&args) == *expected,
                "token source case #{i}"
            );
        }
    }

    /// The summary only claims Flux pull-polling when in-cluster Flux can reach the repository; a
    /// local-path repository says polling is inactive instead of contradicting the later note.
    #[tokio::test]
    async fn summary_flux_polling_line_matches_repository_reachability() {
        let cfg = BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig);
        let conn = ClusterConnection {
            target: keel_core::BootstrapTargetKind::Kubeconfig,
            cluster_name: "keel-mgmt".to_string(),
            kubeconfig_yaml: String::new(),
            kubeconfig_path: None,
            context: None,
            server_endpoint: None,
        };
        for (url, active) in [
            ("https://git.example.org/keel/gitops.git", true),
            ("/tmp/work/keel-gitops.git", false),
        ] {
            let a = synthesize_seed_control_plane(&cfg, Some(url), Stage0Ingress::default())
                .await
                .unwrap();
            let summary = format_bootstrap_init_summary(&conn, &a, None);
            assert_eq!(
                summary.contains("Flux Polling:    interval=1m (outbound pull-polling active)"),
                active,
                "{url}: {summary}"
            );
            assert_eq!(
                summary.contains("Flux Polling:    inactive (GitOps repository is a local path"),
                !active,
                "{url}: {summary}"
            );
        }
    }

    /// The bootstrap-written `tenants/default/git-repository.yaml` is the platform-written source
    /// keel-server resolves for tenant-default PKO Stacks (`gitSource`), and the baseline's
    /// `allow-pko-workspace` policy lets the workspace pods clone it from the forge.
    #[tokio::test]
    async fn default_tenant_stub_is_the_tenant_stack_git_source() {
        let url = "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git";
        let cfg = BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig);
        let a = synthesize_seed_control_plane(&cfg, Some(url), Stage0Ingress::default())
            .await
            .unwrap();
        let mut repo = a.flux_git_repository.clone();
        repo.metadata.name = "keel-gitops".to_string();
        repo.metadata.namespace = "tenant-default".to_string();
        repo.spec.secret_ref = Some(LocalObjectReference {
            name: "git-credentials".to_string(),
        });
        let stub = render_default_tenant_git_repository_yaml(&repo).unwrap();
        let source = keel_kube::tenant_git_source_from_manifest("default", Some(&stub)).unwrap();
        assert_eq!(source.url, url);
        assert_eq!(source.branch_or_default(), "main");
        assert_eq!(source.secret_name.as_deref(), Some("git-credentials"));
        // PKO clones the Forgejo HTTPS ingress (Flux keeps the http Service URL).
        assert_eq!(
            source
                .pko_project_repo(Some(&Stage0Ingress::default().forgejo_origin()))
                .unwrap(),
            "https://forgejo.keel.local:8888/keel/infra-gitops.git"
        );
        let stub_docs: Vec<Value> = keel_core::split_yaml_documents(&stub)
            .into_iter()
            .map(|d| noyalib::from_str::<Value>(&d).unwrap())
            .collect();
        assert!(
            stub_docs.iter().all(|d| d["kind"] != "NetworkPolicy"),
            "the NetworkPolicy lives in the baseline (no duplicate kustomize id)"
        );

        let baseline = tenant_baseline_yaml("default").unwrap();
        let policy = keel_core::split_yaml_documents(&baseline)
            .into_iter()
            .map(|d| noyalib::from_str::<Value>(&d).unwrap())
            .find(|d| d["metadata"]["name"] == "allow-pko-workspace")
            .expect("allow-pko-workspace policy");
        assert_eq!(policy["kind"], "NetworkPolicy");
        assert_eq!(policy["metadata"]["namespace"], "tenant-default");
        assert_eq!(
            policy["metadata"]["labels"]["keel.io/platform-owned"],
            "true"
        );
        assert_eq!(
            policy["spec"]["podSelector"]["matchLabels"]["auto.pulumi.com/component"],
            "workspace"
        );
        // Allowlisted egress: the Forgejo HTTPS ingress (keel-ingress Envoy proxy pods) and
        // external forges, never an allow-all rule.
        let egress = policy["spec"]["egress"].as_array().unwrap();
        assert!(
            egress
                .iter()
                .any(|rule| rule["to"][0]["podSelector"]["matchLabels"]
                    [keel_kube::ENVOY_OWNING_GATEWAY_LABEL]
                    == "keel-ingress"),
            "{baseline}"
        );
        assert!(
            egress
                .iter()
                .all(|rule| rule.as_object().is_some_and(|o| !o.is_empty())),
            "{baseline}"
        );
    }

    #[tokio::test]
    async fn keel_server_git_url_is_serialized_not_interpolated() {
        let url = "https://git.example.org/keel/gitops.git#main: x";
        let cfg = BootstrapConfig {
            with_keel_server: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = synthesize_seed_control_plane(&cfg, Some(url), Stage0Ingress::default())
            .await
            .unwrap();
        let rendered = render_keel_server_addon_kustomization(&a).unwrap();
        let doc: Value = noyalib::from_str(&rendered).unwrap();
        assert_eq!(
            doc["configMapGenerator"][0]["literals"][0],
            format!("KEEL_GIT_URL={url}").as_str()
        );
        assert_eq!(doc.as_object().unwrap().len(), 4, "{rendered}");
    }

    #[tokio::test]
    async fn keel_server_notes_list_out_of_git_prerequisites() {
        let off = synthesize_seed_control_plane(
            &BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig),
            None,
            Stage0Ingress::default(),
        )
        .await
        .unwrap();
        assert!(keel_server_addon_notes(&off, false).is_empty());

        let cfg = BootstrapConfig {
            with_keel_server: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = synthesize_seed_control_plane(&cfg, None, Stage0Ingress::default())
            .await
            .unwrap();
        let notes = keel_server_addon_notes(&a, true);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("https://keel.keel.local:8888"));
        assert!(notes[0].contains(&format!("{KEEL_SERVER_IMAGE}:{KEEL_SERVER_IMAGE_TAG}")));
        let notes = keel_server_addon_notes(&a, false);
        assert_eq!(notes.len(), 2);
        assert!(notes[1].contains("keel-system/forge-token"));
        assert!(notes[1].contains("--from-file=token="));
        assert!(notes.iter().all(|n| !n.contains(&a.admin_password)));

        // External IdP: the in-cluster origin must be registered as a redirect URI.
        let cfg = BootstrapConfig {
            with_keel_server: true,
            oidc_issuer: Some("https://login.example.org".to_string()),
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = synthesize_seed_control_plane(&cfg, None, Stage0Ingress::default())
            .await
            .unwrap();
        let notes = keel_server_addon_notes(&a, true);
        assert_eq!(notes.len(), 2);
        assert!(notes[1].contains("https://keel.keel.local:8888/api/v1/auth/callback"));
        assert!(notes[1].contains("https://login.example.org"));
    }

    #[test]
    fn forge_token_secret_stays_in_keel_system() {
        let doc: Value =
            noyalib::from_str(&forge::render_forge_token_secret_yaml("s3cr3t").unwrap()).unwrap();
        assert_eq!(doc["kind"], "Secret");
        assert_eq!(doc["metadata"]["namespace"], KEEL_SYSTEM_NAMESPACE);
        assert_eq!(doc["metadata"]["name"], FORGE_TOKEN_SECRET_NAME);
        assert_eq!(doc["stringData"]["token"], "s3cr3t");
        // The in-cluster Deployment reads exactly this Secret/key.
        let deployment = yaml_docs(KEEL_SERVER_DEPLOYMENT_YAML)
            .into_iter()
            .find(|d| d["kind"] == "Deployment")
            .unwrap();
        let env = deployment["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        let token = env.iter().find(|e| e["name"] == "KEEL_GIT_TOKEN").unwrap();
        assert_eq!(
            token["valueFrom"]["secretKeyRef"],
            json!({"name": FORGE_TOKEN_SECRET_NAME, "key": "token"})
        );
    }

    fn yaml_docs(yaml: &str) -> Vec<Value> {
        yaml.split("\n---\n")
            .filter(|d| {
                d.lines()
                    .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            })
            .map(|d| noyalib::from_str::<Value>(d).unwrap())
            .collect()
    }

    fn kustomize_resources(yaml: &str) -> std::collections::BTreeSet<String> {
        let doc: Value = noyalib::from_str(yaml).unwrap();
        doc["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect()
    }

    /// File names written directly under `dir` (excluding its `kustomization.yaml`).
    fn files_in(files: &[(String, String)], dir: &str) -> std::collections::BTreeSet<String> {
        files
            .iter()
            .filter_map(|(p, _)| p.strip_prefix(&format!("{dir}/")))
            .filter(|n| !n.contains('/') && *n != "kustomization.yaml")
            .map(str::to_string)
            .collect()
    }

    fn file<'a>(files: &'a [(String, String)], path: &str) -> &'a str {
        files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.as_str())
            .unwrap_or_else(|| panic!("{path} not written"))
    }

    async fn artifacts_for(cfg: BootstrapConfig, public_url: &str) -> SeedControlPlaneArtifacts {
        let ingress = Stage0Ingress::from_public_url(public_url).unwrap();
        synthesize_seed_control_plane(
            &cfg,
            Some("https://git.example.org/keel/gitops.git"),
            ingress,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn seed_management_files_reconcile_with_rauthy() {
        let cfg = BootstrapConfig {
            with_forge: true,
            with_keel_server: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = artifacts_for(cfg, "https://idp.corp.example:9443").await;
        assert!(a.rauthy_config.is_some());
        assert!(a.with_forge);
        let files = seed_management_files(&a).unwrap();
        let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;

        // core-addons/kustomization.yaml lists exactly the core add-on files written to Git.
        let listed = kustomize_resources(file(&files, &format!("{core}/kustomization.yaml")));
        assert_eq!(listed, files_in(&files, core));
        assert!(listed.contains("kyverno-policies.yaml"));
        assert!(listed.contains("rauthy.yaml"));
        assert!(listed.contains("forgejo.yaml"));
        assert!(listed.contains("keel-server.yaml"));
        // ... and the static seed kustomization (every optional add-on enabled) is that very list.
        assert_eq!(
            kustomize_resources(SEED_CORE_ADDON_KUSTOMIZATION_YAML),
            listed
        );

        // flux-system/kustomization.yaml lists exactly the flux-system files, which are exactly
        // the manifests Stage 0 applies in-cluster.
        let flux = "clusters/management/flux-system";
        assert_eq!(
            kustomize_resources(file(&files, &format!("{flux}/kustomization.yaml"))),
            files_in(&files, flux)
        );
        let applied = addons::stage0_flux_sync_manifests(&a).unwrap();
        assert_eq!(
            applied,
            [
                file(&files, &format!("{flux}/tenant-reconciler-rbac.yaml")),
                file(&files, &format!("{flux}/git-repository.yaml")),
                file(&files, &format!("{flux}/gotk-sync.yaml")),
            ]
        );
        let repo = yaml_docs(file(&files, &format!("{flux}/git-repository.yaml")));
        assert_eq!(repo[0]["spec"]["interval"], "1m");

        // Rauthy plain manifests live in their own Flux Kustomization path.
        let rauthy_listed = kustomize_resources(file(
            &files,
            &format!("{RAUTHY_ADDON_DIR}/kustomization.yaml"),
        ));
        assert_eq!(rauthy_listed, files_in(&files, RAUTHY_ADDON_DIR));
        let ks = yaml_docs(file(&files, &format!("{core}/rauthy.yaml")));
        assert_eq!(ks.len(), 1);
        assert_eq!(
            ks[0]["spec"]["path"].as_str().unwrap(),
            format!("./{RAUTHY_ADDON_DIR}")
        );
        let kinds: Vec<String> = rauthy_listed
            .iter()
            .flat_map(|f| yaml_docs(file(&files, &format!("{RAUTHY_ADDON_DIR}/{f}"))))
            .map(|d| d["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["HTTPRoute", "Service", "Service", "StatefulSet"]);
        let route = yaml_docs(file(&files, &format!("{RAUTHY_ADDON_DIR}/httproute.yaml")));
        assert_eq!(route[0]["spec"]["hostnames"][0], "idp.corp.example");
        assert_eq!(route[0]["spec"]["parentRefs"].as_array().unwrap().len(), 2);

        // cert-manager hosts follow `--oidc-public-url`, identical to the Stage 0 local CA.
        let cm = yaml_docs(file(&files, &format!("{core}/cert-manager.yaml")));
        let local_ca = yaml_docs(
            &render_manifest(
                include_str!("../../../../deploy/bootstrap/manifests/local-ca.yaml"),
                &a.ingress.template_vars(),
            )
            .unwrap(),
        );
        assert_eq!(&cm[1..], &local_ca[..]);
        assert_eq!(cm[4]["spec"]["dnsNames"][1], "idp.corp.example");

        // Tenant routes may never claim the configured platform hosts (Kyverno, fail closed).
        let policies = yaml_docs(file(&files, &format!("{core}/kyverno-policies.yaml")));
        let hostnames_policy = policies
            .iter()
            .find(|p| p["metadata"]["name"] == "restrict-tenant-route-hostnames")
            .expect("restrict-tenant-route-hostnames policy");
        let denied = hostnames_policy["spec"]["variables"][0]["expression"]
            .as_str()
            .unwrap();
        for host in [
            "'idp.corp.example'",
            "'keel.corp.example'",
            "'forgejo.corp.example'",
            "'*.corp.example'",
            "'idp.keel.local'",
        ] {
            assert!(denied.contains(host), "{host} missing from {denied}");
        }

        // No Secret (e.g. `rauthy-config`) and no unrendered placeholder ever reaches Git.
        for (path, contents) in &files {
            assert!(
                !contents.contains("${"),
                "{path} has an unrendered placeholder"
            );
            for doc in yaml_docs(contents) {
                assert_ne!(doc["kind"], "Secret", "{path} contains a Secret");
            }
        }

        let catalogs = seed_core_catalog_files(&a);
        let rauthy_catalog = catalogs
            .iter()
            .find(|(p, _)| p == "catalog/core/rauthy/catalog.yaml")
            .unwrap();
        assert!(
            rauthy_catalog
                .1
                .contains("serviceUrl: https://idp.corp.example:9443/auth/v1/admin")
        );
        keel_core::parse_catalog_application(&rauthy_catalog.1).unwrap();
    }

    #[tokio::test]
    async fn seed_management_files_omit_rauthy_when_disabled() {
        for cfg in [
            BootstrapConfig {
                oidc_issuer: Some("https://login.example.org".to_string()),
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            },
            BootstrapConfig {
                with_oidc: false,
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            },
        ] {
            let a = artifacts_for(cfg, DEFAULT_OIDC_PUBLIC_URL).await;
            assert!(a.rauthy_config.is_none());
            let files = seed_management_files(&a).unwrap();
            let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;
            let listed = kustomize_resources(file(&files, &format!("{core}/kustomization.yaml")));
            assert_eq!(listed, files_in(&files, core));
            assert!(!listed.contains("rauthy.yaml"));
            assert!(listed.contains("kyverno-policies.yaml"));
            assert!(
                files.iter().all(|(p, _)| !p.contains("rauthy")),
                "Rauthy files written while the built-in IdP is disabled"
            );
            assert!(
                seed_core_catalog_files(&a)
                    .iter()
                    .all(|(p, _)| !p.contains("rauthy"))
            );
            // The CA / wildcard Certificate and the GatewayClass / Gateway stay in Git without
            // Rauthy, and equal what Stage 0 applies unconditionally (local-ca.yaml,
            // ingress-gateway.yaml), so the first Flux reconcile changes nothing.
            let vars = a.ingress.template_vars();
            let cm = yaml_docs(file(&files, &format!("{core}/cert-manager.yaml")));
            let local_ca = yaml_docs(
                &render_manifest(
                    include_str!("../../../../deploy/bootstrap/manifests/local-ca.yaml"),
                    &vars,
                )
                .unwrap(),
            );
            assert_eq!(&cm[1..], &local_ca[..]);
            let eg = yaml_docs(file(&files, &format!("{core}/envoy-gateway.yaml")));
            let gateway = yaml_docs(
                &render_manifest(
                    include_str!("../../../../deploy/bootstrap/manifests/ingress-gateway.yaml"),
                    &vars,
                )
                .unwrap(),
            );
            assert_eq!(&eg[1..], &gateway[..]);
        }
    }

    #[tokio::test]
    async fn core_addon_flux_objects_impersonate_privileged_reconcilers() {
        // Flux runs with `--default-service-account=default`: a core add-on Flux object without
        // `serviceAccountName` would impersonate a permissionless SA and never become Ready.
        for (with_forge, with_keel_server) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let cfg = BootstrapConfig {
                with_forge,
                with_keel_server,
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            };
            let a = artifacts_for(cfg, DEFAULT_OIDC_PUBLIC_URL).await;
            let files = seed_management_files(&a).unwrap();
            let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;
            let mut seen = 0;
            for (path, contents) in files
                .iter()
                .filter(|(p, _)| p.starts_with(&format!("{core}/")))
            {
                for doc in yaml_docs(contents) {
                    let kind = doc["kind"].as_str().unwrap_or_default();
                    let flux_group = doc["apiVersion"]
                        .as_str()
                        .is_some_and(|v| v.contains("toolkit.fluxcd.io"));
                    if !flux_group {
                        continue;
                    }
                    // Flux sources and objects live in `flux-system` (no cross-namespace refs).
                    assert_eq!(doc["metadata"]["namespace"], "flux-system", "{path} {kind}");
                    let expected = match kind {
                        "HelmRelease" => "helm-controller",
                        "Kustomization" => "kustomize-controller",
                        _ => continue,
                    };
                    seen += 1;
                    assert_eq!(doc["spec"]["serviceAccountName"], expected, "{path} {kind}");
                }
            }
            // 8 HelmReleases (incl. Flux itself) + the Rauthy Kustomization (+ Forgejo)
            // (+ the keel-server Kustomization).
            assert_eq!(
                seen,
                9 + usize::from(with_forge) + usize::from(with_keel_server)
            );
        }
    }

    #[test]
    fn forgejo_helmrelease_adopts_stage0_release_in_place() {
        let ingress = Stage0Ingress::from_public_url("https://idp.corp.example:9443").unwrap();
        let root_url = forgejo_root_url(&ingress);
        assert_eq!(root_url, "https://forgejo.corp.example:9443/");
        let rendered = render_seed_forgejo_helmrelease(&ingress).unwrap();
        let docs = yaml_docs(&rendered);
        let repo = docs.iter().find(|d| d["kind"] == "HelmRepository").unwrap();
        let hr = docs.iter().find(|d| d["kind"] == "HelmRelease").unwrap();
        assert_eq!(docs.len(), 3);

        let program: Value = noyalib::from_str(CONTROL_PLANE_FORGEJO_PULUMI_YAML).unwrap();
        let release = &program["resources"]["forgejo"];
        let p = &release["properties"];
        assert_eq!(hr["metadata"]["name"], "forgejo");
        assert_eq!(hr["metadata"]["namespace"], "flux-system");
        assert_eq!(hr["spec"]["releaseName"], p["name"]);
        assert_eq!(hr["spec"]["targetNamespace"], p["namespace"]);
        assert_eq!(hr["spec"]["storageNamespace"], p["namespace"]);
        assert_eq!(hr["spec"]["serviceAccountName"], "helm-controller");
        assert_eq!(hr["spec"]["chart"]["spec"]["version"], p["version"]);
        // Same OCI chart as Stage 0, from a `flux-system` source.
        let source = &hr["spec"]["chart"]["spec"]["sourceRef"];
        assert_eq!(source["name"], repo["metadata"]["name"]);
        assert_eq!(source["namespace"], "flux-system");
        assert_eq!(repo["metadata"]["namespace"], "flux-system");
        assert_eq!(repo["spec"]["type"], "oci");
        assert_eq!(
            p["chart"],
            format!(
                "{}/{}",
                repo["spec"]["url"].as_str().unwrap(),
                hr["spec"]["chart"]["spec"]["chart"].as_str().unwrap()
            )
            .as_str()
        );
        // Identical values once `forgejoRootUrl` is the rendered ROOT_URL Stage 0 receives.
        let stage0_values: Value = serde_json::from_str(
            &p["values"]
                .to_string()
                .replace("${forgejoRootUrl}", &root_url),
        )
        .unwrap();
        assert_eq!(hr["spec"]["values"], stage0_values);
        assert_eq!(
            hr["spec"]["values"]["gitea"]["config"]["server"]["ROOT_URL"],
            root_url.as_str()
        );
        assert_eq!(
            hr["spec"]["values"]["gitea"]["admin"]["existingSecret"],
            forge::FORGEJO_ADMIN_SECRET_NAME
        );
        // Tenant users read their own repository; forks must not outlive their access. No local
        // sign-up (nobody registers Keel's bot name first) and no organisation creation by
        // non-admins (nobody squats `keel-tenants`).
        for values in [&hr["spec"]["values"], &stage0_values] {
            let config = &values["gitea"]["config"];
            assert_eq!(config["repository"]["DISABLE_FORKS"], true);
            assert_eq!(config["service"]["ALLOW_ONLY_EXTERNAL_REGISTRATION"], true);
            assert_eq!(
                config["service"]["DEFAULT_ALLOW_CREATE_ORGANIZATION"],
                false
            );
        }
        // Every hardening setting is in the rendered values.
        for (section, setting) in FORGEJO_HARDENING_SETTINGS {
            let (key, value) = setting.split_once('=').unwrap();
            assert_eq!(
                hr["spec"]["values"]["gitea"]["config"][*section][key].to_string(),
                value
            );
        }
        assert!(
            hr["spec"]["values"]["gitea"]["admin"]
                .get("password")
                .is_none()
        );
        // The Pulumi default only serves stacks created before `forgejoRootUrl` existed.
        assert_eq!(
            program["config"]["forgejoRootUrl"]["default"],
            forgejo_root_url(&Stage0Ingress::default()).as_str()
        );

        // Hosts the GitOps repository: protected, never pruned, never uninstalled by remediation,
        // and Stage 0 never reverts Flux-applied upgrades.
        assert_eq!(hr["metadata"]["labels"]["keel.io/protect"], "true");
        assert_eq!(
            hr["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/prune"],
            "disabled"
        );
        assert!(hr["spec"]["install"].get("remediation").is_none());
        assert_eq!(hr["spec"]["upgrade"]["remediation"]["strategy"], "rollback");
        assert_eq!(release["options"]["protect"], true);
        let ignored: Vec<&str> = release["options"]["ignoreChanges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for input in p.as_object().unwrap().keys() {
            assert!(
                input == "name" || input == "namespace" || ignored.contains(&input.as_str()),
                "forgejo input `{input}` is not ignored after the Flux handoff"
            );
        }

        // Upgrades through the management API cannot touch the admin Secret or OAuth2 settings.
        let mutable = keel_core::management_addon_mutable_values("forgejo").unwrap();
        assert!(mutable.iter().all(|p| !p.contains("gitea")));
    }

    #[tokio::test]
    async fn seed_files_include_forgejo_only_with_forge() {
        for with_forge in [false, true] {
            let cfg = BootstrapConfig {
                with_forge,
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            };
            let a = artifacts_for(cfg, "https://idp.corp.example:9443").await;
            assert_eq!(a.with_forge, with_forge);
            // PKO clones the in-cluster Forgejo through its HTTPS ingress origin (same host and
            // port as `--oidc-public-url`); Flux keeps the http Service URL.
            assert_eq!(
                a.keel_config.forge.public_url.as_deref(),
                with_forge.then_some("https://forgejo.corp.example:9443")
            );
            if with_forge {
                assert_eq!(
                    keel_pko::pko_project_repo_url(
                        "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
                        a.keel_config.forge.public_url.as_deref()
                    )
                    .unwrap(),
                    "https://forgejo.corp.example:9443/keel/infra-gitops.git"
                );
            }
            let files = seed_management_files(&a).unwrap();
            let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;
            let listed = kustomize_resources(file(&files, &format!("{core}/kustomization.yaml")));
            assert_eq!(listed, files_in(&files, core));
            assert_eq!(listed.contains("forgejo.yaml"), with_forge);
            let catalogs = seed_core_catalog_files(&a);
            let catalog = catalogs
                .iter()
                .find(|(p, _)| p == "catalog/core/forgejo/catalog.yaml");
            assert_eq!(catalog.is_some(), with_forge);

            // Exactly one more Flux object (the `forgejo` HelmRelease) with `--with-forge`.
            let helm_releases = files
                .iter()
                .filter(|(p, _)| p.starts_with(&format!("{core}/")))
                .flat_map(|(_, c)| yaml_docs(c))
                .filter(|d| d["kind"] == "HelmRelease")
                .count();
            assert_eq!(helm_releases, if with_forge { 9 } else { 8 });
            if !with_forge {
                assert!(files.iter().all(|(p, _)| !p.contains("forgejo")));
                continue;
            }

            let forgejo = file(&files, &format!("{core}/forgejo.yaml"));
            assert!(!forgejo.contains("${"));
            assert!(forgejo.contains("ROOT_URL: \"https://forgejo.corp.example:9443/\""));
            let (_, catalog) = catalog.unwrap();
            let app = keel_core::parse_catalog_application(catalog).unwrap();
            assert_eq!(app.metadata.id, "forgejo");
            let doc: Value = noyalib::from_str(catalog).unwrap();
            assert_eq!(
                doc["spec"]["serviceUrl"],
                "https://forgejo.corp.example:9443"
            );
            assert_eq!(
                doc["metadata"]["category"],
                keel_core::MANAGEMENT_CORE_CATEGORY
            );
            // The catalog form never offers the locked security values.
            let props = doc["spec"]["schema"]["properties"].as_object().unwrap();
            assert!(!props.contains_key("gitea"));
            // Toggling persistence replaces the pod and its volume: locked in the API and form.
            assert!(
                doc["spec"]["schema"]["properties"]["persistence"]["properties"]
                    .get("enabled")
                    .is_none()
            );
            assert!(
                doc["metadata"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("ephemeral")
            );
        }
    }

    #[test]
    fn forgejo_stack_config_reaches_the_stage0_program() {
        // Every key Stage 0 passes must be declared by the program, or Pulumi falls back to the
        // default `ROOT_URL` and Flux's in-place adoption replaces the Forgejo pod.
        let program: Value = noyalib::from_str(CONTROL_PLANE_FORGEJO_PULUMI_YAML).unwrap();
        let ingress = Stage0Ingress::from_public_url("https://idp.corp.example:9443").unwrap();
        let config = forgejo_stack_config(&ingress);
        assert!(config.iter().any(|(k, _)| *k == "forgejoRootUrl"));
        for (key, _) in &config {
            assert!(
                program["config"].get(*key).is_some(),
                "`{key}` is not a config key of control-plane-forgejo"
            );
            assert!(
                program.to_string().contains(&format!("${{{key}}}")),
                "`{key}` is never referenced by control-plane-forgejo"
            );
        }
        let root_url = &config
            .iter()
            .find(|(k, _)| *k == "forgejoRootUrl")
            .unwrap()
            .1;
        assert_ne!(root_url, &forgejo_root_url(&Stage0Ingress::default()));
        let rendered = render_seed_forgejo_helmrelease(&ingress).unwrap();
        let hr = yaml_docs(&rendered)
            .into_iter()
            .find(|d| d["kind"] == "HelmRelease")
            .unwrap();
        assert_eq!(
            hr["spec"]["values"]["gitea"]["config"]["server"]["ROOT_URL"],
            root_url.as_str()
        );
    }

    #[tokio::test]
    async fn forgejo_without_rauthy_keeps_its_route_in_git() {
        for cfg in [
            BootstrapConfig {
                with_forge: true,
                oidc_issuer: Some("https://login.example.org".to_string()),
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            },
            BootstrapConfig {
                with_forge: true,
                with_oidc: false,
                ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
            },
        ] {
            let a = artifacts_for(cfg, "https://idp.corp.example:9443").await;
            assert!(a.rauthy_config.is_none());
            let files = seed_management_files(&a).unwrap();
            let core = keel_core::MANAGEMENT_CORE_ADDONS_DIR;
            let listed = kustomize_resources(file(&files, &format!("{core}/kustomization.yaml")));
            assert_eq!(listed, files_in(&files, core));
            assert!(listed.contains("forgejo.yaml"));
            assert!(!listed.contains("rauthy.yaml"));

            // The catalog `serviceUrl` resolves through the Git-owned `forgejo-ingress` route.
            let docs = yaml_docs(file(&files, &format!("{core}/forgejo.yaml")));
            let route = docs.iter().find(|d| d["kind"] == "HTTPRoute").unwrap();
            assert_eq!(route["metadata"]["name"], "forgejo-ingress");
            assert_eq!(route["spec"]["hostnames"][0], "forgejo.corp.example");
            let listeners: Vec<&str> = route["spec"]["parentRefs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["sectionName"].as_str().unwrap())
                .collect();
            assert_eq!(listeners, ["https-local", "https"]);
            let catalogs = seed_core_catalog_files(&a);
            let (_, catalog) = catalogs
                .iter()
                .find(|(p, _)| p == "catalog/core/forgejo/catalog.yaml")
                .unwrap();
            let doc: Value = noyalib::from_str(catalog).unwrap();
            assert_eq!(
                doc["spec"]["serviceUrl"],
                "https://forgejo.corp.example:9443"
            );
        }
    }

    #[test]
    fn flux2_helmrelease_self_manages_with_remediation_and_locked_multitenancy() {
        let hr: Value = noyalib::from_str(SEED_CORE_ADDON_FLUX2_YAML).unwrap();
        assert_eq!(hr["kind"], "HelmRelease");
        assert_eq!(hr["metadata"]["name"], "flux2");
        assert_eq!(hr["metadata"]["labels"]["keel.io/protect"], "true");
        // Dropping the document from Git must never uninstall Flux.
        assert_eq!(
            hr["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/prune"],
            "disabled"
        );
        assert_eq!(hr["spec"]["targetNamespace"], "flux-system");
        assert_eq!(hr["spec"]["storageNamespace"], "flux-system");
        let upgrade = &hr["spec"]["upgrade"];
        assert_eq!(upgrade["remediation"]["strategy"], "rollback");
        assert_eq!(upgrade["remediation"]["remediateLastFailure"], true);
        assert!(upgrade["remediation"]["retries"].as_u64().unwrap() >= 1);
        // helm-controller upgrades itself: waiting on its own rollout would be interrupted by
        // the pod replacement and rolled back, so every action applies without waiting.
        for action in ["install", "upgrade", "rollback"] {
            assert_eq!(hr["spec"][action]["disableWait"], true, "{action}");
        }
        // Install remediation is `helm uninstall`, which would delete Flux itself.
        let install = &hr["spec"]["install"];
        assert!(
            install.get("remediation").is_none()
                || install["remediation"]["retries"].as_u64().unwrap_or(0) == 0,
            "flux2 install remediation must never uninstall Flux"
        );
        let values = &hr["spec"]["values"];
        assert_eq!(values["multitenancy"]["enabled"], true);
        assert_eq!(values["multitenancy"]["defaultServiceAccount"], "default");
        assert_eq!(
            values["crds"]["annotations"]["helm.sh/resource-policy"],
            "keep"
        );
        // The OCI source is the one Stage 0 installs from.
        let source = &hr["spec"]["chart"]["spec"]["sourceRef"];
        let repo = yaml_docs(SEED_CORE_ADDON_HELM_REPOSITORIES_YAML)
            .into_iter()
            .find(|d| d["metadata"]["name"] == source["name"])
            .expect("flux2 HelmRepository");
        assert_eq!(repo["metadata"]["namespace"], "flux-system");
        assert_eq!(repo["spec"]["type"], "oci");
        let program: Value = noyalib::from_str(CONTROL_PLANE_PULUMI_YAML).unwrap();
        assert_eq!(
            program["resources"]["flux2"]["properties"]["chart"],
            format!(
                "{}/{}",
                repo["spec"]["url"].as_str().unwrap(),
                hr["spec"]["chart"]["spec"]["chart"].as_str().unwrap()
            )
            .as_str()
        );

        // The catalog form only offers values the management API may change: multitenancy (and
        // every other security setting) is never exposed.
        let catalog: Value = noyalib::from_str(CORE_CATALOG_FLUX2_YAML).unwrap();
        assert_eq!(
            catalog["metadata"]["category"],
            keel_core::MANAGEMENT_CORE_CATEGORY
        );
        let props = catalog["spec"]["schema"]["properties"].as_object().unwrap();
        assert!(!props.is_empty());
        assert!(!props.contains_key("multitenancy"));
        let defaults: serde_json::Map<String, Value> = props
            .iter()
            .map(|(k, v)| (k.clone(), v["default"].clone()))
            .collect();
        assert!(
            keel_core::disallowed_value_changes(
                "flux2",
                &serde_json::json!({}),
                &Value::Object(defaults)
            )
            .is_empty(),
            "catalog schema exposes a locked flux2 value"
        );
        keel_core::parse_catalog_application(CORE_CATALOG_FLUX2_YAML).unwrap();
    }

    #[tokio::test]
    async fn seed_repo_writes_the_dedicated_tenant_default_stub_with_a_forge() {
        let tmp = std::env::temp_dir().join(format!("keel-seed-tenant-{}", uuid::Uuid::new_v4()));
        let work = tmp.join("work");
        let cfg = BootstrapConfig {
            with_forge: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let a = synthesize_seed_control_plane(&cfg, None, Stage0Ingress::default())
            .await
            .unwrap();
        let res = initialize_seed_git_repo(&work, &a, None, None).await;
        let res = res.unwrap_or_else(|e| {
            let _ = std::fs::remove_dir_all(&tmp);
            panic!("seed repo init failed: {e}")
        });
        let read = |p: &str| std::fs::read_to_string(res.repo_dir.join(p)).unwrap();
        let git_repo = read("tenants/default/git-repository.yaml");
        assert!(
            git_repo.contains("/keel-tenants/infra-gitops-tenant-default.git"),
            "{git_repo}"
        );
        assert!(git_repo.contains("keel-gitops-credentials"));
        assert!(!git_repo.contains("name: git-credentials"));
        assert!(read("tenants/default/sync.yaml").contains("keel-tenant-root"));
        let k = read("tenants/kustomization.yaml");
        for entry in [
            "default/rbac.yaml",
            "default/git-repository.yaml",
            "default/sync.yaml",
        ] {
            assert!(k.contains(entry), "{k}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn seed_repo_writes_management_files_and_starter_dex_app() {
        let tmp = std::env::temp_dir().join(format!("keel-seed-files-{}", uuid::Uuid::new_v4()));
        let work = tmp.join("work");
        let bare = default_bare_repo_for_workdir(&work);
        let cfg = BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig);
        let a = synthesize_seed_control_plane(
            &cfg,
            Some(bare.to_str().unwrap()),
            Stage0Ingress::default(),
        )
        .await
        .unwrap();
        let res = initialize_seed_git_repo(&work, &a, None, None)
            .await
            .unwrap_or_else(|e| {
                let _ = std::fs::remove_dir_all(&tmp);
                panic!("seed repo init failed: {e}")
            });
        for (path, contents) in seed_management_files(&a).unwrap() {
            assert!(res.written_files.contains(&path), "{path} not committed");
            assert_eq!(
                std::fs::read_to_string(res.repo_dir.join(&path)).unwrap(),
                contents
            );
        }
        // Self-service starter app stays available even though Keel itself uses Rauthy.
        assert!(
            res.written_files
                .iter()
                .any(|p| p == "catalog/apps/dex/catalog.yaml")
        );
        keel_core::parse_catalog_application(STARTER_DEX_APP_YAML).unwrap();
        // Self-service tenant Rauthy (catalog + manifest template) is seeded alongside.
        for path in [
            "catalog/apps/rauthy/catalog.yaml",
            "catalog/apps/rauthy/manifests.yaml",
        ] {
            assert!(
                res.written_files.iter().any(|p| p == path),
                "{path} not seeded"
            );
        }
        assert_eq!(
            std::fs::read_to_string(res.repo_dir.join("catalog/apps/rauthy/manifests.yaml"))
                .unwrap(),
            STARTER_RAUTHY_APP_MANIFESTS_YAML
        );
        // Self-service Forgejo, SeaweedFS and the workload-cluster twins (operators, keel-server) are
        // seeded with or without `--with-forge` (independent of the management-plane add-ons).
        for (path, contents) in [
            (
                "catalog/apps/forgejo/catalog.yaml",
                STARTER_FORGEJO_APP_YAML,
            ),
            (
                "catalog/apps/seaweedfs/catalog.yaml",
                STARTER_SEAWEEDFS_APP_YAML,
            ),
            (
                "catalog/apps/kyverno/catalog.yaml",
                STARTER_KYVERNO_APP_YAML,
            ),
            (
                "catalog/apps/pulumi-kubernetes-operator/catalog.yaml",
                STARTER_PKO_APP_YAML,
            ),
            ("catalog/apps/flux2/catalog.yaml", STARTER_FLUX2_APP_YAML),
            (
                "catalog/apps/keel-server/catalog.yaml",
                STARTER_KEEL_SERVER_APP_YAML,
            ),
            (
                "catalog/apps/keel-server/manifests.yaml",
                STARTER_KEEL_SERVER_APP_MANIFESTS_YAML,
            ),
        ] {
            assert!(
                res.written_files.iter().any(|p| p == path),
                "{path} not seeded"
            );
            assert_eq!(
                std::fs::read_to_string(res.repo_dir.join(path)).unwrap(),
                contents
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn core_addon_flux_objects_match_core_catalog_ids() {
        // Exactly one Flux object per base add-on, named after its catalog/core id.
        let mut flux_names: Vec<String> = [
            SEED_CORE_ADDON_S3_SEAWEEDFS_YAML,
            SEED_CORE_ADDON_PKO_YAML,
            SEED_CORE_ADDON_KYVERNO_YAML,
            SEED_CORE_ADDON_ENVOY_GATEWAY_YAML,
            SEED_CORE_ADDON_PROMETHEUS_YAML,
            SEED_CORE_ADDON_FLAGGER_YAML,
            SEED_CORE_ADDON_CERT_MANAGER_YAML,
            SEED_CORE_ADDON_RAUTHY_YAML,
            SEED_CORE_ADDON_HELM_REPOSITORIES_YAML,
        ]
        .iter()
        .flat_map(|f| yaml_docs(f))
        .filter_map(|d| keel_core::management_addon_ref(&d))
        .map(|r| r.name)
        .collect();
        flux_names.sort();
        let mut catalog_ids: Vec<String> = [
            CORE_CATALOG_S3_STATE_BACKEND_YAML,
            CORE_CATALOG_KYVERNO_YAML,
            CORE_CATALOG_CERT_MANAGER_YAML,
            CORE_CATALOG_RAUTHY_YAML,
            CORE_CATALOG_ENVOY_GATEWAY_YAML,
            CORE_CATALOG_PROMETHEUS_YAML,
            CORE_CATALOG_FLAGGER_YAML,
            CORE_CATALOG_PKO_YAML,
        ]
        .iter()
        .map(|c| keel_core::parse_catalog_application(c).unwrap().metadata.id)
        .collect();
        catalog_ids.sort();
        assert_eq!(flux_names, catalog_ids);
        // Core add-on HelmRepositories only serve base add-ons, not self-service apps.
        let repos: Vec<Value> = yaml_docs(SEED_CORE_ADDON_HELM_REPOSITORIES_YAML);
        assert!(repos.iter().all(|r| r["metadata"]["name"] != "dex"));
    }

    #[test]
    fn root_flux_kustomizations_gate_tenants_on_core_addons() {
        let docs = yaml_docs(SEED_FLUX_GOTK_SYNC_YAML);
        let by_name = |n: &str| {
            docs.iter()
                .find(|d| d["metadata"]["name"] == n)
                .unwrap_or_else(|| panic!("missing Kustomization {n}"))
        };
        assert_eq!(docs.len(), 3);
        for d in &docs {
            assert_eq!(d["apiVersion"], "kustomize.toolkit.fluxcd.io/v1");
            assert_eq!(d["metadata"]["namespace"], "flux-system");
            assert_eq!(d["spec"]["sourceRef"]["name"], "flux-system");
            assert_eq!(d["spec"]["decryption"]["secretRef"]["name"], "sops-age");
        }
        for d in &docs {
            // `flux-system` (prune: true) applies this file; it must never prune a root
            // Kustomization (and with it the whole inventory, e.g. every tenant namespace).
            assert_eq!(
                d["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/prune"], "disabled",
                "{}",
                d["metadata"]["name"]
            );
        }
        let fs = by_name("flux-system");
        assert_eq!(fs["spec"]["path"], "./clusters/management/flux-system");
        assert_eq!(fs["spec"]["serviceAccountName"], "kustomize-controller");
        let core = by_name("core-addons");
        assert_eq!(
            core["spec"]["path"].as_str().unwrap(),
            format!("./{}", keel_core::MANAGEMENT_CORE_ADDONS_DIR)
        );
        assert_eq!(core["spec"]["serviceAccountName"], "kustomize-controller");
        assert_eq!(core["spec"]["prune"], false);
        // Health is per add-on Flux object; a broken optional add-on must not block tenants.
        assert_eq!(core["spec"]["wait"], false);
        assert!(core["spec"].get("healthChecks").is_none());
        let tenants = by_name("keel-tenants");
        assert_eq!(tenants["spec"]["path"], "./tenants");
        assert_eq!(
            tenants["spec"]["serviceAccountName"],
            keel_kube::TENANT_RECONCILER_SERVICE_ACCOUNT
        );
        assert_eq!(tenants["spec"]["dependsOn"][0]["name"], "core-addons");
        assert_eq!(tenants["spec"]["interval"], "1m");

        let rauthy: Value = noyalib::from_str(SEED_CORE_ADDON_RAUTHY_YAML).unwrap();
        assert_eq!(rauthy["metadata"]["namespace"], "flux-system");
        assert_eq!(rauthy["metadata"]["labels"]["keel.io/protect"], "true");
        assert_eq!(rauthy["spec"]["serviceAccountName"], "kustomize-controller");
        assert_eq!(rauthy["spec"]["targetNamespace"], "keel-system");
        assert_eq!(rauthy["spec"]["prune"], false);
        assert_eq!(rauthy["spec"]["sourceRef"]["kind"], "GitRepository");
        assert_eq!(rauthy["spec"]["sourceRef"]["name"], "flux-system");
        assert_eq!(rauthy["spec"]["healthChecks"][0]["kind"], "StatefulSet");
        assert_eq!(rauthy["spec"]["healthChecks"][0]["name"], "rauthy");
    }

    #[test]
    fn stage0_pulumi_does_not_revert_flux_managed_addons() {
        let program: Value = noyalib::from_str(CONTROL_PLANE_PULUMI_YAML).unwrap();
        for name in [
            "flux2",
            "kyverno",
            "pulumiKubernetesOperator",
            "s3StateBackend",
            "envoyGateway",
            "prometheus",
            "flagger",
            "certManager",
        ] {
            let ignored = &program["resources"][name]["options"]["ignoreChanges"];
            assert_eq!(
                ignored,
                &serde_json::json!([
                    "chart",
                    "version",
                    "values",
                    "repositoryOpts",
                    "timeout",
                    "waitForJobs",
                    "createNamespace"
                ]),
                "{name}"
            );
            // Every chart-affecting input set on the release is ignored after the handoff.
            let ignored: Vec<&str> = ignored
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            for input in program["resources"][name]["properties"]
                .as_object()
                .unwrap()
                .keys()
            {
                assert!(
                    ["name", "namespace"].contains(&input.as_str())
                        || ignored.contains(&input.as_str()),
                    "{name}: input `{input}` would make a Pulumi re-run upgrade the release"
                );
            }
        }

        let rauthy: Value = noyalib::from_str(state::CONTROL_PLANE_RAUTHY_PULUMI_YAML).unwrap();
        let res = &rauthy["resources"];
        for name in [
            "rauthyConfigSecret",
            "rauthyService",
            "rauthyHeadlessService",
            "rauthyStatefulSet",
        ] {
            assert_eq!(
                res[name]["options"]["protect"], true,
                "{name} must be protected"
            );
        }
        assert!(
            res["rauthyConfigSecret"]["options"]
                .get("ignoreChanges")
                .is_none()
        );

        // The Stage 0 objects equal the Git manifests Flux adopts (modulo Pulumi config refs).
        let unref = |v: &Value| -> Value {
            serde_json::from_str(
                &v.to_string()
                    .replace("${rauthySecretNamespace}", "keel-system")
                    .replace("${rauthySecretName}", RAUTHY_CONFIG_SECRET_NAME),
            )
            .unwrap()
        };
        let services = yaml_docs(SEED_ADDON_RAUTHY_SERVICES_YAML);
        let sts: Value = noyalib::from_str(SEED_ADDON_RAUTHY_STATEFULSET_YAML).unwrap();
        for (name, manifest) in [
            ("rauthyService", &services[0]),
            ("rauthyHeadlessService", &services[1]),
            ("rauthyStatefulSet", &sts),
        ] {
            assert_eq!(
                res[name]["options"]["ignoreChanges"],
                serde_json::json!(["spec", "metadata.labels", "metadata.annotations"]),
                "{name}"
            );
            let props = unref(&res[name]["properties"]);
            assert_eq!(props["spec"], manifest["spec"], "{name} spec drift");
            assert_eq!(props["metadata"]["name"], manifest["metadata"]["name"]);
            assert_eq!(
                props["metadata"]["namespace"],
                manifest["metadata"]["namespace"]
            );
            assert_eq!(props["metadata"]["labels"], manifest["metadata"]["labels"]);
        }
    }

    #[test]
    fn oidc_public_url_derives_issuer_and_hosts() {
        let d = Stage0Ingress::default();
        assert_eq!(d.issuer(), "https://idp.keel.local:8888/auth/v1");
        assert_eq!(d.forgejo_origin(), "https://forgejo.keel.local:8888");
        let c = Stage0Ingress::from_public_url("https://IdP.Example.com/").unwrap();
        assert_eq!(c.issuer(), "https://idp.example.com/auth/v1");
        assert_eq!(c.forgejo_host, "forgejo.example.com");
        assert_eq!(c.keel_origin(), "https://keel.example.com");
        for bad in [
            "http://idp.example.com",
            "https://idp.example.com/path",
            "https://localhost",
            "https://idp.example.com?x=1",
            "not a url",
        ] {
            assert!(Stage0Ingress::from_public_url(bad).is_err(), "{bad}");
        }
        assert!(render_manifest("a: ${X}", &[("X", "ok; rm -rf")]).is_err());
        assert!(render_manifest("a: ${Y}", &[("X", "ok")]).is_err());
    }

    #[tokio::test]
    async fn rauthy_config_uses_public_url() {
        let cfg = BootstrapConfig {
            with_forge: true,
            ..BootstrapConfig::new(keel_core::BootstrapTargetKind::Kubeconfig)
        };
        let ingress = Stage0Ingress::from_public_url("https://idp.corp.example:9443").unwrap();
        let a = synthesize_seed_control_plane(&cfg, None, ingress)
            .await
            .unwrap();
        let r = a.rauthy_config.unwrap();
        assert!(r.config_toml.contains("pub_url = 'idp.corp.example:9443'"));
        assert!(
            r.config_toml
                .contains("rp_origin = 'https://idp.corp.example:9443'")
        );
        assert!(
            r.clients_json
                .contains("https://forgejo.corp.example:9443/user/oauth2/rauthy/callback")
        );
        assert_eq!(
            a.keel_config.oidc.unwrap().issuer_url,
            "https://idp.corp.example:9443/auth/v1"
        );
    }
}

/// Self-service tenant Rauthy catalog app (`deploy/catalog/apps/rauthy`).
#[cfg(test)]
mod tenant_rauthy_app_tests {
    use super::*;

    use serde_json::{Value, json};

    const SECRET_FIELDS: [&str; 4] = [
        "encKey",
        "hiqliteSecretRaft",
        "hiqliteSecretApi",
        "adminPassword",
    ];
    const SAMPLE_ENC_KEY: &str = "q2Vz7mU0ZcV1x7pYb7Yk2r9mD0aH3sW5tL8nQ4eR6fA=";
    const SAMPLE_RAFT: &str = "raftSecretSampleValue-0123456789";
    const SAMPLE_API: &str = "apiSecretSampleValue-9876543210";
    const SAMPLE_ADMIN_PASSWORD: &str = "Sample-Admin#Passw0rd!";

    fn sample_values() -> Value {
        json!({
            "hostname": "auth.acme.example",
            "publicPortSuffix": "",
            "adminEmail": "ops@acme.example",
            "logLevel": "warn",
            "storageSize": "2Gi",
            "encKeyId": "acme1",
            "encKey": SAMPLE_ENC_KEY,
            "hiqliteSecretRaft": SAMPLE_RAFT,
            "hiqliteSecretApi": SAMPLE_API,
            "adminPassword": SAMPLE_ADMIN_PASSWORD,
        })
    }

    fn app() -> keel_core::CatalogApplication {
        keel_core::parse_catalog_application(STARTER_RAUTHY_APP_YAML).unwrap()
    }

    /// Renders the template exactly like `POST /api/v1/tenants/{tenant}/proposals` (Flux engine): only the
    /// non-secret partition is substituted at proposal time.
    fn render(values: &Value) -> (String, Value, Vec<Value>) {
        let app = app();
        keel_core::validate_schema_values(&app.spec.schema, values).unwrap();
        let partitioned = keel_core::partition_values_by_schema(&app.spec.schema, values).unwrap();
        let rendered = keel_core::render_manifest_template(
            STARTER_RAUTHY_APP_MANIFESTS_YAML,
            &partitioned.normal_values,
        )
        .unwrap();
        let docs = keel_core::verify_rendered_manifest_structure(
            STARTER_RAUTHY_APP_MANIFESTS_YAML,
            &rendered,
        )
        .unwrap();
        (rendered, partitioned.secret_values, docs)
    }

    fn doc<'a>(docs: &'a [Value], kind: &str) -> &'a Value {
        docs.iter()
            .find(|d| d["kind"] == kind)
            .unwrap_or_else(|| panic!("{kind} missing"))
    }

    fn container_env(docs: &[Value]) -> Vec<Value> {
        doc(docs, "StatefulSet")["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn env_value(env: &[Value], name: &str) -> String {
        env.iter().find(|e| e["name"] == name).unwrap()["value"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn catalog_entry_is_self_service_manifest_app_pinned_to_core_version() {
        let app = app();
        assert_eq!(app.metadata.id, "rauthy");
        assert_eq!(app.metadata.name, "Rauthy OIDC Provider");
        assert_eq!(app.metadata.category, "security-identity");
        assert_eq!(app.metadata.runtime, keel_core::AppRuntime::Manifest);
        assert_eq!(
            app.spec.default_engine,
            Some(keel_core::ManifestEngine::Flux)
        );
        assert_eq!(
            app.spec.upstream.repo_url,
            "https://github.com/sebadob/rauthy"
        );
        assert!(app.spec.upstream.image.is_none());
        assert!(!app.spec.progressive_delivery.flagger_supported);
        // Hiqlite raft membership is config-driven: replicas are never exposed.
        let props = app.spec.schema["properties"].as_object().unwrap();
        assert!(props.keys().all(|k| !k.to_lowercase().contains("replica")));

        // Bump the tenant app together with Keel's own Rauthy (catalog/core) and its image tag.
        let core = keel_core::parse_catalog_application(CORE_CATALOG_RAUTHY_YAML).unwrap();
        let core_version = core.spec.upstream.version.clone().unwrap();
        assert_eq!(
            app.spec.upstream.version.as_deref(),
            Some(core_version.as_str()),
            "catalog/apps/rauthy version drifts from catalog/core/rauthy"
        );
        let image = format!("{}:{core_version}", core.spec.upstream.image.unwrap());
        let (_, _, docs) = render(&sample_values());
        assert_eq!(
            doc(&docs, "StatefulSet")["spec"]["template"]["spec"]["containers"][0]["image"],
            image.as_str()
        );
    }

    #[test]
    fn secret_fields_are_password_write_only_and_never_defaulted() {
        let app = app();
        let props = &app.spec.schema["properties"];
        let required: Vec<&str> = app.spec.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for field in SECRET_FIELDS {
            assert_eq!(props[field]["format"], "password", "{field}");
            assert_eq!(props[field]["writeOnly"], true, "{field}");
            assert!(props[field].get("default").is_none(), "{field} defaulted");
            assert!(required.contains(&field), "{field} not required");
        }
        let partitioned =
            keel_core::partition_values_by_schema(&app.spec.schema, &sample_values()).unwrap();
        assert_eq!(
            partitioned.secret_values.as_object().unwrap().len(),
            SECRET_FIELDS.len()
        );
        for field in SECRET_FIELDS {
            assert!(partitioned.normal_values.get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn schema_rejects_values_that_would_break_flux_substitution() {
        let schema = app().spec.schema;
        // A macro (not a closure) so a failure points at the offending `reject!` line: the
        // message names only the field, never the (possibly secret) sample value.
        macro_rules! reject {
            ($field:expr, $value:expr) => {{
                let field: &str = $field;
                let mut v = sample_values();
                v[field] = json!($value);
                assert!(
                    keel_core::validate_schema_values(&schema, &v).is_err(),
                    "{field}: invalid value accepted"
                );
            }};
        }
        reject!("adminPassword", "has\"quote-1234567");
        reject!("adminPassword", "has\\backslash-1234567");
        reject!("adminPassword", "has space-1234567890");
        reject!("adminPassword", "short");
        // Flux postBuild also substitutes the decrypted `<instance>-secrets` Secret.
        reject!("adminPassword", "has${dollar}-1234567");
        reject!("adminPassword", "has$dollar-12345678");
        reject!("encKey", "not-base64");
        reject!("hiqliteSecretRaft", "tooshort");
        reject!("hiqliteSecretApi", "contains\"quote-123456");
        reject!("encKeyId", "bad/id");
        reject!("storageSize", "1Ti");
        reject!("publicPortSuffix", "8888");
        let mut missing = sample_values();
        missing.as_object_mut().unwrap().remove("encKey");
        assert!(keel_core::validate_schema_values(&schema, &missing).is_err());
    }

    #[test]
    fn rendered_manifests_never_contain_plaintext_secrets() {
        let (rendered, secrets, docs) = render(&sample_values());
        // Messages name only the field: secret sample values never reach test output.
        let samples = [
            SAMPLE_ENC_KEY,
            SAMPLE_RAFT,
            SAMPLE_API,
            SAMPLE_ADMIN_PASSWORD,
        ];
        for (i, field) in SECRET_FIELDS.into_iter().enumerate() {
            let value = samples[i];
            assert!(secrets[field] == value, "{field} sample mismatch");
            assert!(!rendered.contains(value), "{field} leaked into manifests");
            // Left for Flux `postBuild.substituteFrom` (`<instance>-secrets`).
            assert!(rendered.contains(&format!("${{{field}}}")), "{field}");
        }
        let secret = doc(&docs, "Secret");
        assert_eq!(secret["metadata"]["name"], "rauthy-env");
        assert_eq!(secret["stringData"]["ENC_KEYS"], "acme1/${encKey}");
        assert_eq!(
            secret["stringData"]["BOOTSTRAP_ADMIN_PASSWORD_PLAIN"],
            "${adminPassword}"
        );
        // No secret value is ever inlined into the pod spec.
        let env = container_env(&docs);
        for name in [
            "ENC_KEYS",
            "HQL_SECRET_RAFT",
            "HQL_SECRET_API",
            "BOOTSTRAP_ADMIN_PASSWORD_PLAIN",
        ] {
            let e = env.iter().find(|e| e["name"] == name).unwrap();
            assert!(e.get("value").is_none(), "{name}");
            assert_eq!(e["valueFrom"]["secretKeyRef"]["name"], "rauthy-env");
        }
    }

    #[test]
    fn rendered_manifests_apply_form_values() {
        let (_, _, docs) = render(&sample_values());
        let env = container_env(&docs);
        assert_eq!(env_value(&env, "PUB_URL"), "auth.acme.example");
        assert_eq!(env_value(&env, "RP_ID"), "auth.acme.example");
        assert_eq!(env_value(&env, "RP_ORIGIN"), "https://auth.acme.example");
        assert_eq!(env_value(&env, "LOG_LEVEL"), "warn");
        assert_eq!(env_value(&env, "ENC_KEY_ACTIVE"), "acme1");
        assert_eq!(env_value(&env, "BOOTSTRAP_ADMIN_EMAIL"), "ops@acme.example");
        assert_eq!(
            doc(&docs, "StatefulSet")["spec"]["volumeClaimTemplates"][0]["spec"]["resources"]["requests"]
                ["storage"],
            "2Gi"
        );
        assert_eq!(
            doc(&docs, "HTTPRoute")["spec"]["hostnames"][0],
            "auth.acme.example"
        );

        // Local kind default: the port suffix flows into the issuer and the WebAuthn origin.
        let mut local = sample_values();
        local["publicPortSuffix"] = json!(":8888");
        let (_, _, docs) = render(&local);
        let env = container_env(&docs);
        assert_eq!(env_value(&env, "PUB_URL"), "auth.acme.example:8888");
        assert_eq!(
            env_value(&env, "RP_ORIGIN"),
            "https://auth.acme.example:8888"
        );
    }

    #[test]
    fn rendered_manifests_are_tenant_namespaced_and_restricted_pss() {
        let (_, _, docs) = render(&sample_values());
        let expected = [
            "ConfigMap",
            "Secret",
            "Service",
            "Service",
            "StatefulSet",
            "NetworkPolicy",
            "HTTPRoute",
        ];
        let kinds: Vec<&str> = docs.iter().filter_map(|d| d["kind"].as_str()).collect();
        assert!(
            docs.len() == expected.len(),
            "unexpected rendered document count"
        );
        assert!(kinds == expected, "rendered kinds differ from {expected:?}");
        // Messages name only the expected kind: rendered docs derive from secret samples.
        for (i, kind) in expected.into_iter().enumerate() {
            let d = &docs[i];
            // Flux `targetNamespace` pins every (namespaced) object into `tenant-<id>`.
            assert!(
                d["metadata"].get("namespace").is_none(),
                "{kind} namespaced"
            );
            // Must not collide with the Keel-generated `<instance>-values` ConfigMap and
            // `<instance>-secrets` Secret of the default instance name.
            let name = d["metadata"]["name"].as_str().unwrap();
            assert!(
                name != "rauthy-values" && name != "rauthy-secrets",
                "{kind} name collides with a Keel-generated object"
            );
        }

        let sts = doc(&docs, "StatefulSet");
        assert_eq!(sts["spec"]["replicas"], 1);
        let pod = &sts["spec"]["template"]["spec"];
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
        assert_eq!(
            pod["securityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );
        assert_eq!(pod["automountServiceAccountToken"], false);
        for key in ["hostNetwork", "hostPID", "hostIPC", "initContainers"] {
            assert!(pod.get(key).is_none(), "{key}");
        }
        for v in pod["volumes"].as_array().unwrap() {
            assert!(v.get("hostPath").is_none());
        }
        for c in pod["containers"].as_array().unwrap() {
            let sc = &c["securityContext"];
            assert_eq!(sc["allowPrivilegeEscalation"], false);
            assert_eq!(sc["runAsNonRoot"], true);
            assert_eq!(sc["capabilities"]["drop"], json!(["ALL"]));
            assert_eq!(sc["seccompProfile"]["type"], "RuntimeDefault");
            assert!(sc.get("privileged").is_none());
            assert!(c["resources"]["limits"]["cpu"].is_string());
            assert!(c["resources"]["limits"]["memory"].is_string());
            for p in c["ports"].as_array().unwrap() {
                assert!(p.get("hostPort").is_none());
            }
            // Probes mirror the management Rauthy seed.
            assert_eq!(c["startupProbe"]["httpGet"]["path"], "/auth/v1/health");
            assert_eq!(c["startupProbe"]["httpGet"]["port"], 8080);
            assert_eq!(c["readinessProbe"]["httpGet"]["path"], "/ready");
            assert_eq!(c["readinessProbe"]["httpGet"]["port"], 8200);
        }
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["metadata"]["name"],
            "rauthy-data"
        );

        // Exposed only through the shared `keel-ingress` Gateway HTTPS listeners.
        let route = doc(&docs, "HTTPRoute");
        for parent in route["spec"]["parentRefs"].as_array().unwrap() {
            assert_eq!(parent["name"], "keel-ingress");
            assert_eq!(parent["namespace"], "keel-system");
        }
        assert_eq!(
            route["spec"]["rules"][0]["backendRefs"][0]["name"],
            "rauthy"
        );
        let np = doc(&docs, "NetworkPolicy");
        assert_eq!(
            np["spec"]["ingress"][0]["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "keel-system"
        );
        assert_eq!(np["spec"]["ingress"][0]["ports"][0]["port"], 8080);
    }

    fn config_toml(docs: &[Value]) -> String {
        doc(docs, "ConfigMap")["data"]["config.toml"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn admin_mfa_is_opt_in_like_the_stage0_rauthy() {
        let app = app();
        assert_eq!(
            app.spec.schema["properties"]["adminForceMfa"]["default"],
            false
        );
        // Omitted (API without the field) and the form default both render `false`.
        let mut omitted = sample_values();
        omitted.as_object_mut().unwrap().remove("adminForceMfa");
        let (_, _, docs) = render(&omitted);
        assert!(config_toml(&docs).contains("[mfa]\nadmin_force_mfa = false\n"));
        let mut forced = sample_values();
        forced["adminForceMfa"] = json!(true);
        let (_, _, docs) = render(&forced);
        assert!(config_toml(&docs).contains("[mfa]\nadmin_force_mfa = true\n"));
        let mut bad = sample_values();
        bad["adminForceMfa"] = json!("false; x = 1");
        assert!(keel_core::validate_schema_values(&app.spec.schema, &bad).is_err());
    }

    #[test]
    fn trusted_proxies_cover_the_stage0_rauthy_list() {
        let (_, _, docs) = render(&sample_values());
        let toml = config_toml(&docs);
        // Same CIDRs as the Stage 0 Rauthy `config.toml` (kubelet probes arrive over loopback /
        // node bridges), plus CGNAT ranges some providers use for pod/node networks.
        for cidr in [
            "'10.0.0.0/8'",
            "'172.16.0.0/12'",
            "'192.168.0.0/16'",
            "'127.0.0.0/8'",
            "'100.64.0.0/10'",
        ] {
            assert!(toml.contains(cidr), "{cidr} missing from trusted_proxies");
        }
        assert!(toml.contains("proxy_mode = true"));
    }

    #[test]
    fn hostname_is_required_without_a_shared_default() {
        let app = app();
        assert!(
            app.spec.schema["properties"]["hostname"]
                .get("default")
                .is_none(),
            "a shared default hostname collides across tenants"
        );
        let mut missing = sample_values();
        missing.as_object_mut().unwrap().remove("hostname");
        assert!(keel_core::validate_schema_values(&app.spec.schema, &missing).is_err());
        // Platform hosts are rejected before anything is committed.
        let reserved = keel_core::platform_reserved_hostnames(std::iter::empty());
        for host in ["idp.keel.local", "keel.keel.local", "forgejo.keel.local"] {
            let mut v = sample_values();
            v["hostname"] = json!(host);
            let (_, _, docs) = render(&v);
            let hosts = keel_core::gateway_route_hostnames(&docs);
            assert!(keel_core::validate_route_hostnames(&hosts, &reserved).is_err());
        }
    }
}

/// Self-service tenant Helm apps mirroring management-plane base add-ons
/// (`deploy/catalog/apps/{forgejo,seaweedfs}`).
#[cfg(test)]
mod tenant_helm_app_tests {
    use super::*;

    use serde_json::Value;

    fn yaml(raw: &str) -> Value {
        noyalib::from_str(raw).unwrap()
    }

    /// Bump the tenant app together with its catalog/core twin (and the seed HelmRelease /
    /// Stage 0 Pulumi release, enforced by `core_catalog_versions_match_seed_core_addons`).
    #[test]
    fn tenant_helm_apps_pin_their_core_chart_versions() {
        for (id, app_raw, core_raw) in [
            (
                "forgejo",
                STARTER_FORGEJO_APP_YAML,
                CORE_CATALOG_FORGEJO_YAML,
            ),
            (
                "seaweedfs",
                STARTER_SEAWEEDFS_APP_YAML,
                CORE_CATALOG_S3_STATE_BACKEND_YAML,
            ),
        ] {
            let app = keel_core::parse_catalog_application(app_raw).unwrap();
            let core = keel_core::parse_catalog_application(core_raw).unwrap();
            assert_eq!(app.metadata.id, id);
            assert_eq!(app.metadata.runtime, keel_core::AppRuntime::Helm);
            assert_ne!(app.metadata.category, keel_core::MANAGEMENT_CORE_CATEGORY);
            assert_eq!(
                app.spec.upstream.version, core.spec.upstream.version,
                "catalog/apps/{id} version drifts from catalog/core/{}",
                core.metadata.id
            );
            // Same chart: the core entry may spell the OCI chart as a full reference.
            let app_chart = app.spec.upstream.chart.clone().unwrap();
            let core_chart = core.spec.upstream.chart.clone().unwrap();
            assert!(
                core_chart == app_chart || core_chart.ends_with(&format!("/{app_chart}")),
                "{id}: chart {app_chart} differs from core {core_chart}"
            );
            assert!(
                core.spec
                    .upstream
                    .repo_url
                    .starts_with(app.spec.upstream.repo_url.trim_end_matches('/')),
                "{id}: repoURL differs from core"
            );
            // Namespaced: installable into the tenant namespace on the management cluster.
            assert!(!app.spec.placement.requires_remote_cluster(), "{id}");
            assert!(
                app.spec.network_policy.is_some(),
                "{id}: default-deny tenant"
            );
            assert!(!app.spec.progressive_delivery.flagger_supported);
        }
    }

    /// The tenant variants reuse the seed core-addon values that matter and lock the
    /// `restricted` Pod Security settings no form value can drop.
    #[test]
    fn tenant_helm_apps_reuse_core_values_with_restricted_pss() {
        let forgejo = yaml(STARTER_FORGEJO_APP_YAML);
        let locked = &forgejo["spec"]["lockedValues"];
        let seed_forgejo = SEED_CORE_ADDON_FORGEJO_YAML
            .split("\n---\n")
            .map(yaml)
            .find(|d| d["kind"] == "HelmRelease")
            .unwrap();
        let core_values = &seed_forgejo["spec"]["values"];
        assert_eq!(
            locked["gitea"]["config"]["database"],
            core_values["gitea"]["config"]["database"]
        );
        assert_eq!(
            locked["gitea"]["config"]["oauth2_client"],
            core_values["gitea"]["config"]["oauth2_client"]
        );
        assert_eq!(locked["image"]["rootless"], true);
        let csc = &locked["containerSecurityContext"];
        assert_eq!(csc["allowPrivilegeEscalation"], false);
        assert_eq!(csc["runAsNonRoot"], true);
        assert_eq!(csc["capabilities"]["drop"][0], "ALL");
        assert_eq!(csc["seccompProfile"]["type"], "RuntimeDefault");
        // The admin password is a secret field; persistence defaults on.
        let schema = &forgejo["spec"]["schema"]["properties"];
        assert_eq!(
            schema["gitea"]["properties"]["admin"]["properties"]["password"]["format"],
            "password"
        );
        assert_eq!(schema["gitea"]["properties"]["oauth"]["writeOnly"], true);
        assert_eq!(
            schema["persistence"]["properties"]["enabled"]["default"],
            true
        );

        let seaweedfs = yaml(STARTER_SEAWEEDFS_APP_YAML);
        let locked = &seaweedfs["spec"]["lockedValues"];
        let seed_s3 = yaml(SEED_CORE_ADDON_S3_SEAWEEDFS_YAML);
        let core_values = &seed_s3["spec"]["values"];
        assert_eq!(locked["global"]["seaweedfs"]["createClusterRole"], false);
        assert_eq!(locked["s3"]["enabled"], core_values["s3"]["enabled"]);
        assert_eq!(locked["s3"]["enableAuth"], core_values["s3"]["enableAuth"]);
        let schema = &seaweedfs["spec"]["schema"]["properties"];
        for component in ["master", "filer", "volume", "s3"] {
            assert_eq!(
                locked[component]["logs"], core_values[component]["logs"],
                "{component} logs"
            );
            let csc = &locked[component]["containerSecurityContext"];
            assert_eq!(csc["enabled"], true, "{component}");
            assert_eq!(csc["allowPrivilegeEscalation"], false, "{component}");
            assert_eq!(
                csc["seccompProfile"]["type"], "RuntimeDefault",
                "{component}"
            );
            assert_eq!(
                locked[component]["podSecurityContext"]["runAsNonRoot"], true,
                "{component}"
            );
        }
        for (component, key) in [("master", "data"), ("filer", "data"), ("volume", "idx")] {
            assert_eq!(
                locked[component][key]["type"], core_values[component][key]["type"],
                "{component}.{key}"
            );
            assert_eq!(
                schema[component]["properties"][key]["properties"]["size"]["default"],
                core_values[component][key]["size"],
                "{component}.{key} size"
            );
        }
        let data_dir = &schema["volume"]["properties"]["dataDirs"]["default"][0];
        assert_eq!(data_dir, &core_values["volume"]["dataDirs"][0]);
        let admin = &schema["s3"]["properties"]["credentials"]["properties"]["admin"]["properties"];
        assert_eq!(admin["accessKey"]["format"], "password");
        assert_eq!(admin["secretKey"]["format"], "password");
    }
}

/// Self-service workload-cluster twins of the management operators
/// (`deploy/catalog/apps/{kyverno,pulumi-kubernetes-operator,flux2}`).
#[cfg(test)]
mod workload_operator_app_tests {
    use super::*;

    use serde_json::Value;

    fn yaml(raw: &str) -> Value {
        noyalib::from_str(raw).unwrap()
    }

    /// The values an untouched form submits (`default` of every leaf).
    fn schema_defaults(schema: &Value) -> Value {
        let mut out = serde_json::Map::new();
        if let Some(props) = schema["properties"].as_object() {
            for (key, node) in props {
                if node["type"] == "object" {
                    let nested = schema_defaults(node);
                    if nested.as_object().is_some_and(|o| !o.is_empty()) {
                        out.insert(key.clone(), nested);
                    }
                } else if let Some(default) = node.get("default") {
                    out.insert(key.clone(), default.clone());
                }
            }
        }
        Value::Object(out)
    }

    fn seed_helm_release(raw: &str) -> Value {
        raw.split("\n---\n")
            .map(yaml)
            .find(|d| d["kind"] == "HelmRelease")
            .expect("seed HelmRelease")
    }

    /// `(app id, catalog/apps, catalog/core, seed core-addon, Stage 0 Pulumi resource)`.
    fn operator_twins() -> [(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ); 3] {
        [
            (
                "kyverno",
                STARTER_KYVERNO_APP_YAML,
                CORE_CATALOG_KYVERNO_YAML,
                SEED_CORE_ADDON_KYVERNO_YAML,
                "kyverno",
            ),
            (
                "pulumi-kubernetes-operator",
                STARTER_PKO_APP_YAML,
                CORE_CATALOG_PKO_YAML,
                SEED_CORE_ADDON_PKO_YAML,
                "pulumiKubernetesOperator",
            ),
            (
                "flux2",
                STARTER_FLUX2_APP_YAML,
                CORE_CATALOG_FLUX2_YAML,
                SEED_CORE_ADDON_FLUX2_YAML,
                "flux2",
            ),
        ]
    }

    /// Every Helm management add-on with a self-service twin:
    /// `(app id, catalog/apps, catalog/core, seed core-addon, Stage 0 Pulumi program)`.
    fn helm_chart_twins() -> [(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ); 9] {
        [
            (
                "kyverno",
                STARTER_KYVERNO_APP_YAML,
                CORE_CATALOG_KYVERNO_YAML,
                SEED_CORE_ADDON_KYVERNO_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "pulumi-kubernetes-operator",
                STARTER_PKO_APP_YAML,
                CORE_CATALOG_PKO_YAML,
                SEED_CORE_ADDON_PKO_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "flux2",
                STARTER_FLUX2_APP_YAML,
                CORE_CATALOG_FLUX2_YAML,
                SEED_CORE_ADDON_FLUX2_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "envoy-gateway",
                include_str!("../../../../deploy/catalog/apps/envoy-gateway/catalog.yaml"),
                CORE_CATALOG_ENVOY_GATEWAY_YAML,
                SEED_CORE_ADDON_ENVOY_GATEWAY_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "flagger",
                include_str!("../../../../deploy/catalog/apps/flagger/catalog.yaml"),
                CORE_CATALOG_FLAGGER_YAML,
                SEED_CORE_ADDON_FLAGGER_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "prometheus",
                include_str!("../../../../deploy/catalog/apps/prometheus/catalog.yaml"),
                CORE_CATALOG_PROMETHEUS_YAML,
                SEED_CORE_ADDON_PROMETHEUS_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "cert-manager",
                STARTER_CERT_MANAGER_APP_YAML,
                CORE_CATALOG_CERT_MANAGER_YAML,
                SEED_CORE_ADDON_CERT_MANAGER_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
            (
                "forgejo",
                STARTER_FORGEJO_APP_YAML,
                CORE_CATALOG_FORGEJO_YAML,
                SEED_CORE_ADDON_FORGEJO_YAML,
                state::CONTROL_PLANE_FORGEJO_PULUMI_YAML,
            ),
            (
                "seaweedfs",
                STARTER_SEAWEEDFS_APP_YAML,
                CORE_CATALOG_S3_STATE_BACKEND_YAML,
                SEED_CORE_ADDON_S3_SEAWEEDFS_YAML,
                state::CONTROL_PLANE_PULUMI_YAML,
            ),
        ]
    }

    /// Number of `version: "<version>"` lines directly preceded by the Renovate annotation of
    /// `dep_name` (the `renovate.json` custom regex manager bumps exactly those lines). Helm
    /// repositories need `datasource=helm` plus their `registryUrl`; OCI charts `datasource=docker`.
    fn renovate_annotated_versions(
        raw: &str,
        dep_name: &str,
        registry_url: Option<&str>,
        version: &str,
    ) -> usize {
        let lines: Vec<&str> = raw.lines().map(str::trim).collect();
        lines
            .windows(2)
            .filter(|pair| {
                let fields: Vec<&str> = pair[0].split_whitespace().collect();
                let has = |f: &str| fields.contains(&f);
                let registry_ok = match registry_url {
                    Some(url) => {
                        has("datasource=helm")
                            && fields.iter().any(|f| {
                                f.strip_prefix("registryUrl=")
                                    .is_some_and(|u| u.trim_end_matches('/') == url)
                            })
                    }
                    None => {
                        has("datasource=docker")
                            && !fields.iter().any(|f| f.starts_with("registryUrl="))
                    }
                };
                pair[0].starts_with("# renovate: datasource=")
                    && has(&format!("depName={dep_name}"))
                    && registry_ok
                    && pair[1] == format!("version: \"{version}\"")
            })
            .count()
    }

    /// Renovate bumps all four pins of each management Helm chart in one PR (`renovate.json`):
    /// every pin carries the annotation, so a bump can never update only some of them.
    #[test]
    fn management_helm_chart_pins_are_renovate_annotated() {
        let renovate: Value =
            serde_json::from_str(include_str!("../../../../renovate.json")).unwrap();
        for (id, app_raw, core_raw, seed_raw, program_raw) in helm_chart_twins() {
            let app = keel_core::parse_catalog_application(app_raw).unwrap();
            assert_eq!(app.metadata.id, id);
            assert_eq!(app.metadata.runtime, keel_core::AppRuntime::Helm, "{id}");
            let version = app.spec.upstream.version.clone().unwrap();
            let repo = app.spec.upstream.repo_url.trim_end_matches('/');
            let chart = app.spec.upstream.chart.clone().unwrap();
            let (dep_name, registry_url) = match repo.strip_prefix("oci://") {
                Some(registry) => (format!("{registry}/{chart}"), None),
                None => (chart, Some(repo)),
            };
            for (file, raw) in [
                ("catalog/apps", app_raw),
                ("catalog/core", core_raw),
                ("seed core-addon", seed_raw),
                ("Stage 0 Pulumi program", program_raw),
            ] {
                assert_eq!(
                    renovate_annotated_versions(raw, &dep_name, registry_url, &version),
                    1,
                    "{id}: {file} version pin {version} lacks the Renovate annotation"
                );
            }
            // One grouped, manually reviewed PR per chart.
            let rule = renovate["packageRules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["matchDepNames"] == serde_json::json!([dep_name]))
                .unwrap_or_else(|| panic!("{id}: no renovate packageRule"));
            assert_eq!(rule["automerge"], false, "{id}");
            assert_eq!(rule["groupName"], format!("{id}-chart"), "{id}");
            assert_eq!(rule["matchManagers"], serde_json::json!(["custom.regex"]));
        }
    }

    /// Twins of charts shipping their CRDs in `crds/` replace them on upgrade like the seed
    /// HelmRelease, so bumping the shared version also updates existing installs' CRDs.
    #[test]
    fn helm_twins_reuse_the_seed_crd_policy() {
        for (id, app_raw, _, seed_raw, _) in helm_chart_twins() {
            let app = yaml(app_raw);
            let seed = seed_helm_release(seed_raw);
            let policy = &app["spec"]["crdPolicy"];
            if [
                "envoy-gateway",
                "flagger",
                "kyverno",
                "pulumi-kubernetes-operator",
            ]
            .contains(&id)
            {
                assert_eq!(policy, "CreateReplace", "{id}");
            }
            if !policy.is_null() {
                assert_eq!(policy, &seed["spec"]["install"]["crds"], "{id}: install");
                assert_eq!(policy, &seed["spec"]["upgrade"]["crds"], "{id}: upgrade");
            }
        }
    }

    /// Every management add-on (`deploy/catalog/core/<id>`) has a self-service twin in
    /// `deploy/catalog/apps/` pinned to the same upstream version (chart version or image tag).
    /// The mapping is explicit: a new core entry fails here until its twin is listed.
    #[test]
    fn every_core_catalog_app_pins_the_same_version_as_its_self_service_twin() {
        const TWINS: [(&str, &str); 11] = [
            ("cert-manager", "cert-manager"),
            ("envoy-gateway", "envoy-gateway"),
            ("flagger", "flagger"),
            ("flux2", "flux2"),
            ("forgejo", "forgejo"),
            ("keel-server", "keel-server"),
            ("kyverno", "kyverno"),
            ("prometheus", "prometheus"),
            ("pulumi-kubernetes-operator", "pulumi-kubernetes-operator"),
            ("rauthy", "rauthy"),
            ("s3-state-backend", "seaweedfs"),
        ];
        let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/catalog");
        let mut core_ids: Vec<String> = std::fs::read_dir(catalog.join("core"))
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.path().join("catalog.yaml").is_file())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        core_ids.sort();
        let mapped: Vec<&str> = TWINS.iter().map(|(core, _)| *core).collect();
        assert_eq!(
            core_ids, mapped,
            "every deploy/catalog/core/<id> needs an explicit self-service twin mapping"
        );
        let read = |dir: &str, id: &str| -> Value {
            let path = catalog.join(dir).join(id).join("catalog.yaml");
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            noyalib::from_str(&raw).unwrap()
        };
        for (core_id, app_id) in TWINS {
            let core = read("core", core_id);
            let app = read("apps", app_id);
            assert_eq!(core["metadata"]["id"], core_id);
            assert_eq!(app["metadata"]["id"], app_id);
            // Helm add-ons have Helm twins (same chart); image-based Kustomize add-ons (Rauthy,
            // keel-server) have manifest twins pinning the same image tag.
            if core["metadata"]["runtime"] == "helm" {
                assert_eq!(app["metadata"]["runtime"], "helm", "{app_id}: runtime");
            }
            let version = core["spec"]["upstream"]["version"]
                .as_str()
                .unwrap_or_else(|| panic!("catalog/core/{core_id} has no upstream version"));
            assert_eq!(
                app["spec"]["upstream"]["version"], version,
                "catalog/apps/{app_id} version drifts from catalog/core/{core_id}"
            );
        }
    }

    /// A version bump changes `catalog/core/<id>`, `catalog/apps/<id>`, the seed HelmRelease and
    /// the Stage 0 Pulumi release together; any of them drifting fails here.
    #[test]
    fn workload_operator_apps_pin_their_core_chart_versions() {
        let program: Value = noyalib::from_str(state::CONTROL_PLANE_PULUMI_YAML).unwrap();
        for (id, app_raw, core_raw, seed_raw, pulumi_resource) in operator_twins() {
            let app = keel_core::parse_catalog_application(app_raw).unwrap();
            let core = keel_core::parse_catalog_application(core_raw).unwrap();
            assert_eq!(app.metadata.id, id);
            assert_eq!(core.metadata.id, id);
            assert_eq!(app.metadata.runtime, keel_core::AppRuntime::Helm);
            assert_ne!(app.metadata.category, keel_core::MANAGEMENT_CORE_CATEGORY);
            let version = app.spec.upstream.version.clone().unwrap();
            assert_eq!(
                Some(&version),
                core.spec.upstream.version.as_ref(),
                "catalog/apps/{id} version drifts from catalog/core/{id}"
            );
            let seed = seed_helm_release(seed_raw);
            assert_eq!(
                seed["spec"]["chart"]["spec"]["version"], version,
                "catalog/apps/{id} version drifts from deploy/seed/core-addons"
            );
            let release = &program["resources"][pulumi_resource]["properties"];
            assert_eq!(
                release["version"], version,
                "catalog/apps/{id} version drifts from the Stage 0 Pulumi release"
            );
            // Same chart: the core entry and Stage 0 spell OCI charts as a full reference.
            let app_chart = app.spec.upstream.chart.clone().unwrap();
            let app_repo = app.spec.upstream.repo_url.trim_end_matches('/').to_string();
            let core_chart = core.spec.upstream.chart.clone().unwrap();
            assert!(
                core_chart == app_chart || core_chart == format!("{app_repo}/{app_chart}"),
                "{id}: chart {app_repo} {app_chart} differs from core {core_chart}"
            );
            assert!(
                core.spec.upstream.repo_url.starts_with(&app_repo),
                "{id}: repoURL differs from core"
            );
            let stage0_chart = release["chart"].as_str().unwrap();
            assert!(
                stage0_chart == app_chart || stage0_chart == format!("{app_repo}/{app_chart}"),
                "{id}: chart differs from Stage 0 {stage0_chart}"
            );
            // The tenant HelmRepository is `type: oci` exactly for OCI charts.
            assert_eq!(
                app_repo.starts_with("oci://"),
                stage0_chart.starts_with("oci://"),
                "{id}"
            );
            // Cluster-wide CRDs, RBAC and webhooks: remote workload clusters only, in a
            // dedicated namespace (never the tenant's shared `tenant-<id>`).
            assert!(app.spec.placement.requires_remote_cluster(), "{id}");
            assert_eq!(
                app.spec.placement.namespace.as_deref(),
                Some(match id {
                    "kyverno" => "kyverno",
                    "pulumi-kubernetes-operator" => "pulumi-kubernetes-operator",
                    _ => "flux-system",
                }),
                "{id}"
            );
            assert!(!app.spec.progressive_delivery.flagger_supported, "{id}");
            // No management-cluster-only features (routes/NetworkPolicy target `tenant-<id>`).
            assert!(app.spec.gateway_routes.is_empty(), "{id}");
            assert!(app.spec.network_policy.is_none(), "{id}");
            // The form is the whole interface (locked values imply strict values).
            assert!(app.strict_values(), "{id}");
            // No secret fields: nothing to SOPS-encrypt for an operator install.
            assert!(!app_raw.contains("format: password"), "{id}");
            // The form defaults validate against the schema and stay inside it.
            let defaults = schema_defaults(&app.spec.schema);
            keel_core::validate_schema_values(&app.spec.schema, &defaults)
                .unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(
                keel_core::undeclared_value_paths(&app.spec.schema, &defaults).is_empty(),
                "{id}"
            );
        }
    }

    /// The twins reuse the seed core-addon values that are not management specific.
    #[test]
    fn workload_operator_apps_reuse_seed_values() {
        // Kyverno: only the CEL `policies.kyverno.io` CRDs; no `keel-system` exception namespace.
        let kyverno = yaml(STARTER_KYVERNO_APP_YAML);
        let seed = seed_helm_release(SEED_CORE_ADDON_KYVERNO_YAML);
        assert_eq!(
            kyverno["spec"]["lockedValues"]["crds"],
            seed["spec"]["values"]["crds"]
        );
        assert_eq!(
            kyverno["spec"]["crdPolicy"],
            seed["spec"]["install"]["crds"]
        );
        assert_eq!(
            kyverno["spec"]["crdPolicy"],
            seed["spec"]["upgrade"]["crds"]
        );
        let schema = &kyverno["spec"]["schema"]["properties"];
        assert_eq!(
            schema["admissionController"]["properties"]["replicas"]["default"],
            seed["spec"]["values"]["admissionController"]["replicas"]
        );
        let exceptions = &schema["features"]["properties"]["policyExceptions"]["properties"];
        assert_eq!(exceptions["enabled"]["default"], false);
        // The exception namespace is locked to the dedicated install namespace (never a form
        // field: an empty value would make Kyverno read exceptions from every namespace).
        assert!(exceptions.get("namespace").is_none());
        assert_eq!(
            kyverno["spec"]["lockedValues"]["features"]["policyExceptions"]["namespace"],
            kyverno["spec"]["placement"]["namespace"]
        );
        assert_ne!(kyverno["spec"]["placement"]["namespace"], "keel-system");
        assert!(!STARTER_KYVERNO_APP_YAML.contains("keel:policy-exceptions"));

        // PKO: the seed's hardened container security context, CRDs replaced on upgrade.
        let pko = yaml(STARTER_PKO_APP_YAML);
        let seed = seed_helm_release(SEED_CORE_ADDON_PKO_YAML);
        let locked = &pko["spec"]["lockedValues"];
        assert_eq!(
            locked["securityContext"],
            seed["spec"]["values"]["securityContext"]
        );
        assert_eq!(locked["podSecurityContext"]["runAsNonRoot"], true);
        assert_eq!(
            locked["podSecurityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );
        assert_eq!(pko["spec"]["crdPolicy"], "CreateReplace");
        assert_eq!(pko["spec"]["crdPolicy"], seed["spec"]["install"]["crds"]);
        assert_eq!(pko["spec"]["crdPolicy"], seed["spec"]["upgrade"]["crds"]);
        assert_eq!(
            pko["spec"]["schema"]["properties"]["replicaCount"]["default"],
            seed["spec"]["values"]["replicaCount"]
        );

        // Flux: the multi-tenancy lockdown (on by default, optional) and CRDs kept on uninstall.
        let flux = yaml(STARTER_FLUX2_APP_YAML);
        let seed = seed_helm_release(SEED_CORE_ADDON_FLUX2_YAML);
        let locked = &flux["spec"]["lockedValues"];
        let seed_values = &seed["spec"]["values"];
        assert_eq!(locked["crds"], seed_values["crds"]);
        for key in ["defaultServiceAccount", "privileged"] {
            assert_eq!(
                locked["multitenancy"][key], seed_values["multitenancy"][key],
                "multitenancy.{key}"
            );
        }
        let schema = &flux["spec"]["schema"]["properties"];
        assert_eq!(
            schema["multitenancy"]["properties"]["enabled"]["default"],
            seed_values["multitenancy"]["enabled"]
        );
        assert!(locked["multitenancy"].get("enabled").is_none());
        // An edit (no schema defaults) must submit the lockdown switch; a first install that
        // omits it gets the default (on) from the server-side schema defaults.
        let flux_app = keel_core::parse_catalog_application(STARTER_FLUX2_APP_YAML).unwrap();
        for missing in [
            serde_json::json!({}),
            serde_json::json!({ "multitenancy": {} }),
        ] {
            assert!(
                keel_core::validate_schema_values(&flux_app.spec.schema, &missing).is_err(),
                "{missing}"
            );
            let installed = keel_core::apply_schema_defaults(&flux_app.spec.schema, &missing);
            assert_eq!(installed["multitenancy"]["enabled"], true, "{missing}");
            keel_core::validate_schema_values(&flux_app.spec.schema, &installed).unwrap();
        }
        keel_core::validate_schema_values(
            &flux_app.spec.schema,
            &serde_json::json!({ "multitenancy": { "enabled": false } }),
        )
        .unwrap();
        assert_eq!(
            schema["logLevel"]["enum"],
            yaml(CORE_CATALOG_FLUX2_YAML)["spec"]["schema"]["properties"]["logLevel"]["enum"]
        );
    }
}

/// Self-service workload-cluster keel-server catalog app (`deploy/catalog/apps/keel-server`).
#[cfg(test)]
mod tenant_keel_server_app_tests {
    use super::*;

    use serde_json::{Value, json};

    const SECRET_FIELDS: [&str; 2] = ["oidcClientSecret", "gitToken"];
    const SAMPLE_CLIENT_SECRET: &str = "oidc-Client-Secret-0123456789";
    const SAMPLE_GIT_TOKEN: &str = "glpat-SampleGitToken0123456789";

    fn yaml_docs(yaml: &str) -> Vec<Value> {
        keel_core::split_yaml_documents(yaml)
            .iter()
            .filter_map(|d| noyalib::from_str::<Value>(d).ok())
            .filter(|d| d.is_object())
            .collect()
    }

    fn sample_values() -> Value {
        json!({
            "hostname": "keel.team-a.example.com",
            "publicPortSuffix": ":8443",
            "gatewayName": "edge",
            "gatewayNamespace": "gateway-system",
            "oidcIssuerUrl": "https://auth.team-a.example.com/auth/v1",
            "oidcClientId": "team-a-keel",
            "oidcClientSecret": SAMPLE_CLIENT_SECRET,
            "adminGroup": "team-a-platform",
            "gitUrl": "https://git.example.com/team-a/infra-gitops.git",
            "gitUsername": "keel-bot",
            "gitToken": SAMPLE_GIT_TOKEN,
        })
    }

    fn app() -> keel_core::CatalogApplication {
        keel_core::parse_catalog_application(STARTER_KEEL_SERVER_APP_YAML).unwrap()
    }

    /// Renders the template like `POST /api/v1/tenants/{tenant}/proposals` (Flux engine): only the non-secret
    /// partition is substituted at proposal time.
    fn render(values: &Value) -> (String, Value, Vec<Value>) {
        let app = app();
        keel_core::validate_schema_values(&app.spec.schema, values).unwrap();
        let partitioned = keel_core::partition_values_by_schema(&app.spec.schema, values).unwrap();
        let rendered = keel_core::render_manifest_template(
            STARTER_KEEL_SERVER_APP_MANIFESTS_YAML,
            &partitioned.normal_values,
        )
        .unwrap();
        let docs = keel_core::verify_rendered_manifest_structure(
            STARTER_KEEL_SERVER_APP_MANIFESTS_YAML,
            &rendered,
        )
        .unwrap();
        (rendered, partitioned.secret_values, docs)
    }

    fn find<'a>(docs: &'a [Value], kind: &str, name: &str, ns: Option<&str>) -> &'a Value {
        docs.iter()
            .find(|d| {
                d["kind"] == kind
                    && d["metadata"]["name"] == name
                    && d["metadata"]["namespace"].as_str() == ns
            })
            .unwrap_or_else(|| panic!("{kind}/{name} ({ns:?}) missing"))
    }

    fn container(docs: &[Value]) -> Value {
        find(docs, "Deployment", "keel-server", Some("keel-system"))["spec"]["template"]["spec"]
            ["containers"][0]
            .clone()
    }

    fn env_entry(container: &Value, name: &str) -> Value {
        container["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap_or_else(|| panic!("env {name} missing"))
            .clone()
    }

    #[test]
    fn catalog_entry_is_workload_cluster_manifest_app_pinned_to_release_tag() {
        let app = app();
        assert_eq!(app.metadata.id, "keel-server");
        assert_ne!(app.metadata.category, keel_core::MANAGEMENT_CORE_CATEGORY);
        assert_eq!(app.metadata.runtime, keel_core::AppRuntime::Manifest);
        assert_eq!(
            app.spec.default_engine,
            Some(keel_core::ManifestEngine::Flux)
        );
        assert!(app.spec.placement.requires_remote_cluster());
        assert_eq!(app.spec.placement.namespace.as_deref(), Some("keel-system"));
        assert_eq!(
            app.multi_namespace_placement(),
            vec!["keel-system", "keel-previews", "flux-system"]
        );
        assert!(!app.spec.progressive_delivery.flagger_supported);

        // One release tag everywhere: crate version == catalog/core == catalog/apps == image.
        let tag = KEEL_SERVER_IMAGE_TAG;
        let core = keel_core::parse_catalog_application(CORE_CATALOG_KEEL_SERVER_YAML).unwrap();
        assert_eq!(core.spec.upstream.version.as_deref(), Some(tag));
        assert_eq!(
            app.spec.upstream.version.as_deref(),
            Some(tag),
            "catalog/apps/keel-server version drifts from the crate / catalog/core/keel-server"
        );
        assert_eq!(core.spec.upstream.image.as_deref(), Some(KEEL_SERVER_IMAGE));
        let (rendered, _, docs) = render(&sample_values());
        assert_eq!(
            container(&docs)["image"],
            format!("{KEEL_SERVER_IMAGE}:{tag}").as_str()
        );
        assert!(!rendered.contains(":latest"));
        // The release `sed` only rewrites the pinned tag and `upstream.version`: no other quoted,
        // indented `version:` / `newTag:` line may exist in the bumped files.
        let bumped = |raw: &str| {
            raw.lines()
                .filter(|l| {
                    let t = l.trim_start();
                    l.starts_with(' ')
                        && (t.starts_with("version: \"") || t.starts_with("newTag: \""))
                })
                .count()
        };
        assert_eq!(bumped(STARTER_KEEL_SERVER_APP_YAML), 1);
        assert_eq!(bumped(STARTER_KEEL_SERVER_APP_MANIFESTS_YAML), 0);
        assert_eq!(
            STARTER_KEEL_SERVER_APP_MANIFESTS_YAML
                .matches("ghcr.io/rdylanwalker/keel:")
                .count(),
            1
        );
    }

    #[test]
    fn manifests_reuse_the_known_good_install_config() {
        let (_, _, docs) = render(&sample_values());
        // Least-privilege RBAC identical to `deploy/bootstrap/keel-server/rbac.yaml`, plus the
        // preview workspace identity of the Stage 0 manifests.
        let mut expected: Vec<Value> = yaml_docs(KEEL_SERVER_RBAC_YAML);
        expected.extend(yaml_docs(include_str!(
            "../../../../deploy/bootstrap/manifests/preview-workspace-rbac.yaml"
        )));
        for want in &expected {
            let got = find(
                &docs,
                want["kind"].as_str().unwrap(),
                want["metadata"]["name"].as_str().unwrap(),
                want["metadata"]["namespace"].as_str(),
            );
            let mut want = want.clone();
            if want["kind"] == "ClusterRole" && want["metadata"]["name"] == "keel-server-read" {
                // The one deliberate difference: Kyverno writes are narrowed to the provider
                // allowlist policy (no PolicyException / GeneratingPolicy writes).
                let rules: Vec<Value> = want["rules"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|rule| {
                        if rule["apiGroups"] == json!(["policies.kyverno.io"]) {
                            let mut read = rule.clone();
                            read["verbs"] = json!(["get", "list", "watch"]);
                            vec![read, workload_kyverno_write_rule()]
                        } else {
                            vec![rule.clone()]
                        }
                    })
                    .collect();
                want["rules"] = Value::Array(rules);
            }
            assert_eq!(got, &want, "RBAC drift: {}", want["metadata"]["name"]);
        }
        let rbac_kinds = [
            "ServiceAccount",
            "ClusterRole",
            "ClusterRoleBinding",
            "Role",
            "RoleBinding",
        ];
        assert_eq!(
            docs.iter()
                .filter(|d| rbac_kinds.contains(&d["kind"].as_str().unwrap()))
                .count(),
            expected.len(),
            "unexpected extra RBAC objects"
        );

        // Same pod spec as the shared Deployment; only the environment differs.
        let bundle = yaml_docs(KEEL_SERVER_DEPLOYMENT_YAML);
        let known = bundle.iter().find(|d| d["kind"] == "Deployment").unwrap();
        let ours = find(&docs, "Deployment", "keel-server", Some("keel-system"));
        let strip = |d: &Value| {
            let mut pod = d["spec"]["template"]["spec"].clone();
            let c = &mut pod["containers"][0];
            c.as_object_mut().unwrap().remove("env");
            c.as_object_mut().unwrap().remove("envFrom");
            pod
        };
        assert_eq!(strip(ours), strip(known));
        assert_eq!(ours["spec"]["selector"], known["spec"]["selector"]);
        let svc = bundle.iter().find(|d| d["kind"] == "Service").unwrap();
        assert_eq!(
            find(&docs, "Service", "keel-server", Some("keel-system")),
            svc
        );

        // Route shape of the management add-on, attached to the configured Gateway.
        let route = find(
            &docs,
            "HTTPRoute",
            "keel-server-ingress",
            Some("keel-system"),
        );
        let seed: Value = noyalib::from_str(SEED_ADDON_KEEL_SERVER_HTTPROUTE_YAML).unwrap();
        assert_eq!(route["spec"]["rules"], seed["spec"]["rules"]);
        assert_eq!(
            route["spec"]["parentRefs"],
            json!([{ "name": "edge", "namespace": "gateway-system" }])
        );
        assert_eq!(
            route["spec"]["hostnames"],
            json!(["keel.team-a.example.com"])
        );

        // Every namespaced object names one of the placement namespaces (no targetNamespace).
        let rendered =
            keel_core::render_manifest_template(STARTER_KEEL_SERVER_APP_MANIFESTS_YAML, &json!({}))
                .unwrap();
        app().ensure_manifest_namespaces(&rendered).unwrap();
        assert!(
            docs.iter().all(|d| d["kind"] != "Namespace"),
            "Keel synthesizes the namespaces"
        );
    }

    /// The only Kyverno write of the workload variant of `keel-server-read`.
    fn workload_kyverno_write_rule() -> Value {
        json!({
            "apiGroups": ["policies.kyverno.io"],
            "resources": ["validatingpolicies"],
            "resourceNames": [keel_kube::POLICY_RESTRICT_PULUMI_PROVIDERS],
            "verbs": ["create", "update", "patch"],
        })
    }

    #[test]
    fn kyverno_writes_are_limited_to_the_provider_allowlist_policy() {
        let (_, _, docs) = render(&sample_values());
        let role = find(&docs, "ClusterRole", "keel-server-read", None);
        let writes = [
            "create",
            "update",
            "patch",
            "delete",
            "deletecollection",
            "*",
        ];
        let kyverno_writes: Vec<&Value> = role["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| {
                r["apiGroups"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|g| g == "policies.kyverno.io" || g == "*")
                    && r["verbs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| writes.contains(&v.as_str().unwrap()))
            })
            .collect();
        assert_eq!(kyverno_writes, vec![&workload_kyverno_write_rule()]);
        // No other Role/ClusterRole of the app touches Kyverno objects.
        for d in docs
            .iter()
            .filter(|d| d["kind"] == "Role" || d["kind"] == "ClusterRole")
            .filter(|d| d["metadata"]["name"] != "keel-server-read")
        {
            assert!(
                !serde_json::to_string(d).unwrap().contains("kyverno"),
                "{}",
                d["metadata"]["name"]
            );
        }
    }

    /// Keel's Kyverno `protect-control-plane` rule confining keel-server's reconcile requests.
    fn protect_control_plane_keel_server_rule() -> String {
        let policy = keel_kube::build_protect_control_plane_policy();
        policy["spec"]["validations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["expression"].as_str())
            .find(|e| e.contains("'system:serviceaccount:keel-system:keel-server'"))
            .expect("keel-server rule of protect-control-plane")
            .to_string()
    }

    #[test]
    fn flux_system_patch_grant_is_confined_by_a_native_admission_policy() {
        let (_, _, docs) = render(&sample_values());
        let policy = find(
            &docs,
            "ValidatingAdmissionPolicy",
            "keel-server-reconcile-only",
            None,
        );
        assert_eq!(policy["apiVersion"], "admissionregistration.k8s.io/v1");
        let spec = &policy["spec"];
        assert_eq!(spec["failurePolicy"], "Fail");
        // Same CEL as the management cluster's Kyverno rule (kept in sync by this test).
        let validations = spec["validations"].as_array().unwrap();
        assert_eq!(validations.len(), 1);
        assert_eq!(
            validations[0]["expression"].as_str().unwrap(),
            protect_control_plane_keel_server_rule()
        );
        assert_eq!(
            spec["matchConditions"],
            json!([{
                "name": "keel-server-service-account",
                "expression":
                    "request.userInfo.username == 'system:serviceaccount:keel-system:keel-server'",
            }])
        );
        assert_eq!(
            spec["matchConstraints"]["namespaceSelector"],
            json!({ "matchExpressions": [{
                "key": "kubernetes.io/metadata.name",
                "operator": "In",
                "values": ["flux-system", "keel-system"],
            }]})
        );
        let rules = spec["matchConstraints"]["resourceRules"]
            .as_array()
            .unwrap();
        assert!(
            rules
                .iter()
                .all(|r| r["operations"] == json!(["UPDATE"]) && r["apiVersions"] == json!(["*"]))
        );
        let covers = |group: &str, resource: &str| {
            rules.iter().any(|r| {
                r["apiGroups"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|g| g == group)
                    && r["resources"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|x| x == resource || x == "*")
            })
        };
        // Every object the server may patch in flux-system is matched by the policy.
        let flux_role = find(&docs, "Role", "keel-server", Some("flux-system"));
        for rule in flux_role["rules"].as_array().unwrap() {
            if !rule["verbs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "patch" || v == "update")
            {
                continue;
            }
            for group in rule["apiGroups"].as_array().unwrap() {
                for resource in rule["resources"].as_array().unwrap() {
                    let (g, r) = (group.as_str().unwrap(), resource.as_str().unwrap());
                    assert!(covers(g, r), "{g}/{r} patchable but not policed");
                }
            }
        }

        let binding = find(
            &docs,
            "ValidatingAdmissionPolicyBinding",
            "keel-server-reconcile-only",
            None,
        );
        assert_eq!(binding["spec"]["policyName"], "keel-server-reconcile-only");
        assert_eq!(binding["spec"]["validationActions"], json!(["Deny"]));
        assert!(binding["spec"].get("paramRef").is_none());
        assert!(binding["spec"].get("matchResources").is_none());
    }

    #[test]
    fn rendered_manifests_apply_form_values_and_defaults() {
        let (_, _, docs) = render(&sample_values());
        let c = container(&docs);
        let value = |name: &str| env_entry(&c, name)["value"].as_str().unwrap().to_string();
        assert_eq!(
            value("KEEL_GIT_URL"),
            "https://git.example.com/team-a/infra-gitops.git"
        );
        assert_eq!(value("KEEL_GIT_USERNAME"), "keel-bot");
        assert_eq!(
            value(keel_core::OIDC_ISSUER_URL_ENV),
            "https://auth.team-a.example.com/auth/v1"
        );
        assert_eq!(value(keel_core::OIDC_CLIENT_ID_ENV), "team-a-keel");
        assert_eq!(
            value(keel_core::OIDC_REDIRECT_URL_ENV),
            "https://keel.team-a.example.com:8443/api/v1/auth/callback"
        );
        assert_eq!(value("KEEL_ADMIN_GROUPS"), "team-a-platform");
        assert_eq!(value("HOME"), "/var/lib/keel");

        // The env the server reads resolves to a complete OIDC config.
        let env: Vec<(String, String)> = c["env"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                Some((
                    e["name"].as_str()?.to_string(),
                    e["value"].as_str()?.to_string(),
                ))
            })
            .collect();
        let oidc = keel_core::OidcConfig::with_env_overrides(None, |name| {
            env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        })
        .unwrap()
        .unwrap();
        assert!(oidc.enabled);
        assert_eq!(oidc.client_id, "team-a-keel");

        // Optional fields fall back to the schema defaults (443, keel-ingress, keel-admins, keel).
        let mut minimal = sample_values();
        for field in [
            "publicPortSuffix",
            "gatewayName",
            "gatewayNamespace",
            "adminGroup",
            "gitUsername",
        ] {
            minimal.as_object_mut().unwrap().remove(field);
        }
        let (rendered, _, docs) = render(&minimal);
        assert!(!rendered.contains(":="), "unrendered default left");
        let c = container(&docs);
        assert_eq!(
            env_entry(&c, keel_core::OIDC_REDIRECT_URL_ENV)["value"],
            "https://keel.team-a.example.com/api/v1/auth/callback"
        );
        assert_eq!(env_entry(&c, "KEEL_ADMIN_GROUPS")["value"], "keel-admins");
        assert_eq!(env_entry(&c, "KEEL_GIT_USERNAME")["value"], "keel");
        let route = find(
            &docs,
            "HTTPRoute",
            "keel-server-ingress",
            Some("keel-system"),
        );
        assert_eq!(
            route["spec"]["parentRefs"],
            json!([{ "name": "keel-ingress", "namespace": "keel-system" }])
        );
        let app = app();
        let props = &app.spec.schema["properties"];
        assert_eq!(props["publicPortSuffix"]["default"], "");
        assert_eq!(props["gatewayName"]["default"], "keel-ingress");
        assert_eq!(props["gatewayNamespace"]["default"], "keel-system");
        assert_eq!(props["adminGroup"]["default"], "keel-admins");
        assert_eq!(props["gitUsername"]["default"], "keel");
    }

    #[test]
    fn secrets_only_reach_the_pod_through_the_flux_substituted_secret() {
        let app = app();
        let props = &app.spec.schema["properties"];
        let required: Vec<&str> = app.spec.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for field in SECRET_FIELDS {
            assert_eq!(props[field]["format"], "password", "{field}");
            assert_eq!(props[field]["writeOnly"], true, "{field}");
            assert!(props[field].get("default").is_none(), "{field} defaulted");
            assert!(required.contains(&field), "{field} not required");
        }

        let (rendered, secrets, docs) = render(&sample_values());
        // Messages name only the field: secret sample values never reach test output.
        let samples = [SAMPLE_CLIENT_SECRET, SAMPLE_GIT_TOKEN];
        for (i, field) in SECRET_FIELDS.into_iter().enumerate() {
            let value = samples[i];
            assert!(secrets[field] == value, "{field} sample mismatch");
            assert!(!rendered.contains(value), "{field} leaked into manifests");
            // `${field}` stays for Flux `postBuild.substituteFrom` and never has a default
            // (Keel would otherwise render the default at proposal time).
            assert!(rendered.contains(&format!("${{{field}}}")), "{field}");
            assert!(!rendered.contains(&format!("${{{field}:=")), "{field}");
        }
        let secret = find(
            &docs,
            "Secret",
            "keel-server-credentials",
            Some("keel-system"),
        );
        assert_eq!(secret["stringData"]["git-token"], "${gitToken}");
        assert_eq!(
            secret["stringData"]["oidc-client-secret"],
            "${oidcClientSecret}"
        );
        let c = container(&docs);
        for (name, key) in [
            ("KEEL_GIT_TOKEN", "git-token"),
            (keel_core::OIDC_CLIENT_SECRET_ENV, "oidc-client-secret"),
        ] {
            let e = env_entry(&c, name);
            assert!(e.get("value").is_none(), "{name} inlined");
            assert_eq!(
                e["valueFrom"]["secretKeyRef"]["name"],
                "keel-server-credentials"
            );
            assert_eq!(e["valueFrom"]["secretKeyRef"]["key"], key);
        }
    }

    #[test]
    fn schema_rejects_values_that_would_break_substitution_or_leak_credentials() {
        let schema = app().spec.schema;
        // A macro (not a closure) so a failure points at the offending `reject!` line: the
        // message names only the field, never the (possibly secret) sample value.
        macro_rules! reject {
            ($field:expr, $value:expr) => {{
                let field: &str = $field;
                let mut v = sample_values();
                v[field] = json!($value);
                assert!(
                    keel_core::validate_schema_values(&schema, &v).is_err(),
                    "{field}: invalid value accepted"
                );
            }};
        }
        // No credentials in URLs (the token is injected by keel, never via the remote URL).
        reject!("gitUrl", "https://user:pw@git.example.com/a/b.git");
        reject!("gitUrl", "ssh://git@git.example.com/a/b.git");
        reject!("gitUrl", "https://git.example.com/a/${b}.git");
        // The Git token is sent on every clone/fetch/push: never over cleartext HTTP.
        reject!("gitUrl", "http://git.example.com/team-a/infra-gitops.git");
        reject!("oidcIssuerUrl", "http://auth.team-a.example.com");
        reject!("oidcIssuerUrl", "https://auth.example.com/\"x");
        for field in SECRET_FIELDS {
            reject!(field, "short");
            reject!(field, "has space-1234567890");
            reject!(field, "has\"quote-1234567");
            reject!(field, "has\\backslash-1234567");
            reject!(field, "has${dollar}-1234567");
        }
        reject!("adminGroup", "a,b");
        reject!("adminGroup", "with space");
        reject!("gatewayName", "Edge_GW");
        reject!("gatewayNamespace", "-bad");
        reject!("publicPortSuffix", "8443");
        reject!("oidcClientId", "has space");
        reject!("gitUsername", "a/b");
        for field in [
            "hostname",
            "oidcIssuerUrl",
            "oidcClientId",
            "oidcClientSecret",
            "gitUrl",
            "gitToken",
        ] {
            let mut missing = sample_values();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                keel_core::validate_schema_values(&schema, &missing).is_err(),
                "{field} not required"
            );
        }
    }
}
