//! Stage 0 seed control-plane synthesis and cluster bootstrap handlers (`keel bootstrap init`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use keel_core::{
    AuthPhase, BootstrapConfig, BootstrapTarget, ClusterConnection, DEFAULT_POLL_INTERVAL,
    FLUX_SYSTEM_NAMESPACE, ForgeConfig, ForgeKind, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE,
    KeelConfig, OidcConfig, ProviderRegistry, SopsConfig, SopsManager, bootstrap_target_factory,
    generate_admin_password,
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
use reqwest::header::AUTHORIZATION;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

use crate::cli::BootstrapInitArgs;

use super::{CliError, absolutize_path};

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
}

impl std::fmt::Debug for SeedControlPlaneArtifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeedControlPlaneArtifacts")
            .field("keel_config", &self.keel_config)
            .field("admin_username", &self.admin_username)
            .field("admin_password", &"[REDACTED]")
            .field("admin_password_generated", &self.admin_password_generated)
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

    let external_oidc_issuer = cfg
        .oidc_issuer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let use_in_cluster_rauthy = external_oidc_issuer.is_none() && cfg.with_oidc;
    let oidc_issuer = external_oidc_issuer.clone().or_else(|| {
        use_in_cluster_rauthy.then(|| "https://idp.keel.local:8888/auth/v1".to_string())
    });
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
        KyvernoPolicyGenerator::new(enabled_provider_ids, cfg.local_dev_docker_socket_exception);
    let kyverno_bundle = kyverno_gen.generate()?;

    // Protected anti-lockout `management-cluster-core` PKO v2 Stack (`protect: true`)
    let use_in_cluster_s3 = keel_config.s3.is_none();
    let mgmt_backend = if let Some(s3) = keel_config.s3.as_ref() {
        format!("s3://{}", s3.bucket)
    } else {
        IN_CLUSTER_S3_BACKEND_URL.to_string()
    };
    let mut mgmt_params =
        ManagementCoreStackParams::new(mgmt_backend, FLUX_SYSTEM_NAMESPACE, "clusters/management");
    if use_in_cluster_s3 {
        attach_s3_credentials_env_refs(&mut mgmt_params.env_refs, S3_CREDENTIALS_SECRET_NAME);
    }
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

        let config_toml = format!(
            "[access]\ncookie_mode = 'secure'\n\n\
             [bootstrap]\nbootstrap_dir = '/app/bootstrap'\n\n\
             [cluster]\nnode_id = 1\nnodes = ['1 127.0.0.1:8100 127.0.0.1:8200']\nsecret_raft = '{secret_raft}'\nsecret_api = '{secret_api}'\n\n\
             [database]\nhiqlite = true\n\n\
             [encryption]\nkeys = ['{enc_key_id}/{enc_key_b64}']\nkey_active = '{enc_key_id}'\n\n\
             [logging]\nlevel = 'info'\n\n\
             [mfa]\nadmin_force_mfa = false\n\n\
             [server]\nlisten_address = '0.0.0.0'\nport_http = 8080\nport_https = 8443\nscheme = 'http_https'\npub_url = 'idp.keel.local:8888'\nproxy_mode = true\ntrusted_proxies = ['10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '127.0.0.0/8']\n\n\
             [tls]\ngenerate_self_signed = true\n\n\
             [webauthn]\nrp_id = 'idp.keel.local'\nrp_origin = 'https://idp.keel.local:8888'\nrp_name = 'Keel IAM'\n"
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
                "http://keel.keel.local:8888/api/v1/auth/callback"
            ],
            "allowed_origins": [
                "http://127.0.0.1:8080",
                "http://localhost:8080",
                "http://keel.keel.local:8888"
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
                    "https://forgejo.keel.local:8888/user/oauth2/rauthy/callback"
                ],
                "allowed_origins": [
                    "https://forgejo.keel.local:8888"
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

const STARTER_KIND_CLUSTER_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/kind-cluster/catalog.yaml");
const STARTER_KIND_CLUSTER_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/kind-cluster/Pulumi.yaml");
const STARTER_AWS_EKS_CLUSTER_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/aws-eks-cluster/catalog.yaml");
const STARTER_AWS_EKS_CLUSTER_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/aws-eks-cluster/Pulumi.yaml");
const STARTER_GCP_GKE_CLUSTER_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/gcp-gke-cluster/catalog.yaml");
const STARTER_GCP_GKE_CLUSTER_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/gcp-gke-cluster/Pulumi.yaml");
const STARTER_AZURE_AKS_CLUSTER_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/azure-aks-cluster/catalog.yaml");
const STARTER_AZURE_AKS_CLUSTER_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/azure-aks-cluster/Pulumi.yaml");
const STARTER_AWS_VM_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/aws-vm/catalog.yaml");
const STARTER_AWS_VM_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/aws-vm/Pulumi.yaml");
const STARTER_AZURE_VM_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/azure-vm/catalog.yaml");
const STARTER_AZURE_VM_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/azure-vm/Pulumi.yaml");
const STARTER_GCP_VM_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/gcp-vm/catalog.yaml");
const STARTER_GCP_VM_PULUMI_YAML: &str =
    include_str!("../../../../deploy/catalog/infra/gcp-vm/Pulumi.yaml");
const STARTER_PODINFO_CATALOG_YAML: &str =
    include_str!("../../../../deploy/catalog/apps/podinfo/catalog.yaml");

const SEED_CORE_ADDON_KUSTOMIZATION_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/kustomization.yaml");
const SEED_CORE_ADDON_HELM_REPOSITORIES_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/helm-repositories.yaml");
const SEED_CORE_ADDON_S3_SEAWEEDFS_YAML: &str =
    include_str!("../../../../deploy/seed/core-addons/s3-seaweedfs.yaml");
const SEED_CORE_ADDON_PKO_YAML: &str = include_str!("../../../../deploy/seed/core-addons/pko.yaml");
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

const CORE_CATALOG_S3_STATE_BACKEND_YAML: &str =
    include_str!("../../../../deploy/catalog/core/s3-state-backend/catalog.yaml");
const CORE_CATALOG_KYVERNO_YAML: &str =
    include_str!("../../../../deploy/catalog/core/kyverno/catalog.yaml");
const CORE_CATALOG_CERT_MANAGER_YAML: &str =
    include_str!("../../../../deploy/catalog/core/cert-manager/catalog.yaml");
const CORE_CATALOG_RAUTHY_YAML: &str =
    include_str!("../../../../deploy/catalog/core/rauthy/catalog.yaml");
const CORE_CATALOG_DEX_APP_YAML: &str =
    include_str!("../../../../deploy/catalog/apps/dex/catalog.yaml");
const STARTER_CERT_MANAGER_APP_YAML: &str =
    include_str!("../../../../deploy/catalog/apps/cert-manager/catalog.yaml");
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

pub(crate) const FORGE_TOKEN_SECRET_NAME: &str = "forge-token";

/// RAII guard for a background `kubectl port-forward` child process.
pub(crate) struct TempPortForward {
    pub(crate) local_port: u16,
    child: tokio::process::Child,
}

impl Drop for TempPortForward {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub(crate) async fn start_forgejo_port_forward(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<TempPortForward, CliError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:3000")
        .or_else(|_| std::net::TcpListener::bind("127.0.0.1:0"))?;
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

pub(crate) async fn start_gateway_port_forward(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<TempPortForward, CliError> {
    let mut svc_cmd = tokio::process::Command::new("kubectl");
    if let Some(kc) = kubeconfig {
        svc_cmd.arg("--kubeconfig").arg(kc);
    }
    if let Some(ctx) = context {
        svc_cmd.arg("--context").arg(ctx);
    }
    let svc_out = svc_cmd
        .args([
            "-n",
            KEEL_SYSTEM_NAMESPACE,
            "get",
            "svc",
            "-l",
            "gateway.envoyproxy.io/owning-gateway-name=keel-ingress",
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])
        .output()
        .await
        .map_err(|e| CliError::Validation(format!("failed to query Envoy Gateway service: {e}")))?;
    let svc_name = String::from_utf8_lossy(&svc_out.stdout).trim().to_string();
    if svc_name.is_empty() {
        return Err(CliError::Validation(
            "Envoy Gateway service for keel-ingress not found in keel-system".to_string(),
        ));
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:8888")
        .or_else(|_| std::net::TcpListener::bind("127.0.0.1:0"))?;
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
        &format!("svc/{svc_name}"),
        &format!("{local_port}:8888"),
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| CliError::Validation(format!("failed to spawn kubectl port-forward: {e}")))?;
    let pf = TempPortForward { local_port, child };

    for _ in 0..40 {
        if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .is_ok()
        {
            return Ok(pf);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(CliError::Validation(format!(
        "timed out waiting for Envoy Gateway / Rauthy port-forward on 127.0.0.1:{local_port}"
    )))
}

/// Applies the `git-credentials` Secret to `flux-system` and ensures `tenant-default`
/// exists so Kyverno's `sync-tenant-secrets` `GeneratingPolicy` automatically clones and
/// synchronizes `git-credentials` into `tenant-default` and all future `tenant-*` namespaces.
async fn apply_git_credentials_secret(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
    username: &str,
    password: &str,
) -> Result<(), CliError> {
    let default_tenant_rbac =
        TenantManifestBundle::synthesize(&TenantIsolationConfig::new("default"))?
            .to_multi_doc_yaml()?;
    run_kubectl_apply(context, kubeconfig, &default_tenant_rbac).await?;

    let creds_doc = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": "git-credentials",
            "namespace": FLUX_SYSTEM_NAMESPACE,
        },
        "type": "Opaque",
        "stringData": {
            "username": username,
            "password": password,
        },
    });
    let creds_yaml = serde_yml::to_string(&creds_doc)?;
    run_kubectl_apply(context, kubeconfig, &creds_yaml).await?;
    Ok(())
}

async fn provision_in_cluster_forgejo(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
    local_port: u16,
    forgejo_oidc_secret: Option<&str>,
) -> Result<String, CliError> {
    let base = format!("http://127.0.0.1:{local_port}");
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| CliError::Validation(e.to_string()))?;

    let forgejo_user = "keel";
    let forgejo_pass = std::env::var("KEEL_FORGEJO_ADMIN_PASSWORD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "keel-admin-dev".to_string());

    // 1. Ensure `keel/infra-gitops` repository exists (201 Created or 409 Already Exists)
    let repo_resp = client
        .post(format!("{base}/api/v1/user/repos"))
        .basic_auth(forgejo_user, Some(forgejo_pass.as_str()))
        .json(&serde_json::json!({
            "name": "infra-gitops",
            "private": true,
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
        .basic_auth(forgejo_user, Some(forgejo_pass.as_str()))
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
        .get("token")
        .or_else(|| tok_json.get("sha1"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CliError::Validation("Forgejo access token response missing token field".to_string())
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

    apply_git_credentials_secret(context, kubeconfig, forgejo_user, &token).await?;

    // 4. Register Rauthy OIDC SSO provider in Forgejo when enabled
    if let Some(oidc_secret) = forgejo_oidc_secret {
        let mut list_cmd = tokio::process::Command::new("kubectl");
        if let Some(kc) = kubeconfig {
            list_cmd.arg("--kubeconfig").arg(kc);
        }
        if let Some(ctx) = context {
            list_cmd.arg("--context").arg(ctx);
        }
        let list_out = list_cmd
            .args([
                "-n",
                KEEL_SYSTEM_NAMESPACE,
                "exec",
                "deploy/forgejo",
                "--",
                "forgejo",
                "admin",
                "auth",
                "list",
            ])
            .output()
            .await;

        let already_configured = list_out.as_ref().is_ok_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("rauthy")
        });

        if !already_configured {
            let mut rauthy_wait = tokio::process::Command::new("kubectl");
            if let Some(kc) = kubeconfig {
                rauthy_wait.arg("--kubeconfig").arg(kc);
            }
            if let Some(ctx) = context {
                rauthy_wait.arg("--context").arg(ctx);
            }
            let _ = rauthy_wait
                .args([
                    "-n",
                    KEEL_SYSTEM_NAMESPACE,
                    "rollout",
                    "status",
                    "statefulset/rauthy",
                    "--timeout=120s",
                ])
                .output()
                .await;

            let mut registered = false;
            let mut last_err = String::new();
            for attempt in 1..=15 {
                let mut add_cmd = tokio::process::Command::new("kubectl");
                if let Some(kc) = kubeconfig {
                    add_cmd.arg("--kubeconfig").arg(kc);
                }
                if let Some(ctx) = context {
                    add_cmd.arg("--context").arg(ctx);
                }
                match add_cmd
                    .args([
                        "-n",
                        KEEL_SYSTEM_NAMESPACE,
                        "exec",
                        "deploy/forgejo",
                        "--",
                        "env",
                        "SSL_CERT_DIR=/etc/ssl/certs:/etc/ssl/keel-ca",
                        "forgejo",
                        "admin",
                        "auth",
                        "add-oauth",
                        "--name",
                        "rauthy",
                        "--provider",
                        "openidConnect",
                        "--key",
                        "forgejo",
                        "--secret",
                        oidc_secret,
                        "--auto-discover-url",
                        "https://idp.keel.local:8888/auth/v1/.well-known/openid-configuration",
                        "--scopes",
                        "openid email profile groups",
                    ])
                    .output()
                    .await
                {
                    Ok(out) if out.status.success() => {
                        tracing::info!(attempt, "Registered Rauthy OIDC SSO provider in Forgejo");
                        registered = true;
                        break;
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        if stderr.contains("already exists") {
                            tracing::info!(
                                "Rauthy OIDC SSO provider already registered in Forgejo"
                            );
                            registered = true;
                            break;
                        }
                        last_err = stderr;
                        tracing::debug!(
                            attempt,
                            error = %last_err,
                            "Waiting for Rauthy OIDC discovery endpoint before registering in Forgejo"
                        );
                    }
                    Err(err) => {
                        last_err = err.to_string();
                        tracing::debug!(
                            attempt,
                            error = %last_err,
                            "Retrying forgejo admin auth add-oauth"
                        );
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            if !registered {
                tracing::warn!(
                    error = %last_err,
                    "Failed to register Rauthy OIDC SSO provider in Forgejo after retries (non-fatal)"
                );
            }
        }

        // Warm up Forgejo's in-memory OIDC provider so the first browser login redirect includes PKCE `code_challenge`.
        if let Ok(no_redirect_client) = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
        {
            let warmup_url = format!("{base}/user/oauth2/rauthy");
            for attempt in 1..=3 {
                match no_redirect_client.get(&warmup_url).send().await {
                    Ok(resp) => {
                        let has_pkce = resp
                            .headers()
                            .get(reqwest::header::LOCATION)
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|loc| loc.contains("code_challenge="));
                        tracing::debug!(
                            attempt,
                            status = %resp.status(),
                            has_pkce,
                            "Warmed up Forgejo Rauthy OIDC redirect"
                        );
                        if has_pkce {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(
                            attempt,
                            error = %err,
                            "Forgejo Rauthy OIDC warm-up request failed (non-fatal)"
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

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

pub(crate) fn render_default_tenant_git_repository_yaml(
    repo: &FluxGitRepository,
) -> Result<String, CliError> {
    let mut repo_yaml = repo.to_yaml()?;
    if !repo_yaml.ends_with('\n') {
        repo_yaml.push('\n');
    }
    Ok(format!(
        "{repo_yaml}---\n\
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
         \x20 - {{}}\n"
    ))
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
    let default_tenant_git_repo_yaml =
        render_default_tenant_git_repository_yaml(&default_tenant_git_repo)?;

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
            "clusters/management/core-addons/kustomization.yaml",
            SEED_CORE_ADDON_KUSTOMIZATION_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/helm-repositories.yaml",
            SEED_CORE_ADDON_HELM_REPOSITORIES_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/s3-seaweedfs.yaml",
            SEED_CORE_ADDON_S3_SEAWEEDFS_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/pko.yaml",
            SEED_CORE_ADDON_PKO_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/kyverno.yaml",
            SEED_CORE_ADDON_KYVERNO_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/envoy-gateway.yaml",
            SEED_CORE_ADDON_ENVOY_GATEWAY_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/prometheus.yaml",
            SEED_CORE_ADDON_PROMETHEUS_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/flagger.yaml",
            SEED_CORE_ADDON_FLAGGER_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/cert-manager.yaml",
            SEED_CORE_ADDON_CERT_MANAGER_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/rauthy.yaml",
            SEED_CORE_ADDON_RAUTHY_YAML.to_string(),
        ),
        (
            "clusters/management/core-addons/kyverno-policies.yaml",
            artifacts.kyverno_bundle.to_multi_doc_yaml()?,
        ),
        (
            "catalog/core/s3-state-backend/catalog.yaml",
            CORE_CATALOG_S3_STATE_BACKEND_YAML.to_string(),
        ),
        (
            "catalog/core/kyverno/catalog.yaml",
            CORE_CATALOG_KYVERNO_YAML.to_string(),
        ),
        (
            "catalog/core/cert-manager/catalog.yaml",
            CORE_CATALOG_CERT_MANAGER_YAML.to_string(),
        ),
        (
            "catalog/core/rauthy/catalog.yaml",
            CORE_CATALOG_RAUTHY_YAML.to_string(),
        ),
        (
            "catalog/core/envoy-gateway/catalog.yaml",
            CORE_CATALOG_ENVOY_GATEWAY_YAML.to_string(),
        ),
        (
            "catalog/core/prometheus/catalog.yaml",
            CORE_CATALOG_PROMETHEUS_YAML.to_string(),
        ),
        (
            "catalog/core/flagger/catalog.yaml",
            CORE_CATALOG_FLAGGER_YAML.to_string(),
        ),
        (
            "catalog/core/pulumi-kubernetes-operator/catalog.yaml",
            CORE_CATALOG_PKO_YAML.to_string(),
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
            "catalog/infra/aws-eks-cluster/catalog.yaml",
            STARTER_AWS_EKS_CLUSTER_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/aws-eks-cluster/Pulumi.yaml",
            STARTER_AWS_EKS_CLUSTER_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/gcp-gke-cluster/catalog.yaml",
            STARTER_GCP_GKE_CLUSTER_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/gcp-gke-cluster/Pulumi.yaml",
            STARTER_GCP_GKE_CLUSTER_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/azure-aks-cluster/catalog.yaml",
            STARTER_AZURE_AKS_CLUSTER_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/azure-aks-cluster/Pulumi.yaml",
            STARTER_AZURE_AKS_CLUSTER_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/aws-vm/catalog.yaml",
            STARTER_AWS_VM_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/aws-vm/Pulumi.yaml",
            STARTER_AWS_VM_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/azure-vm/catalog.yaml",
            STARTER_AZURE_VM_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/azure-vm/Pulumi.yaml",
            STARTER_AZURE_VM_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/gcp-vm/catalog.yaml",
            STARTER_GCP_VM_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/infra/gcp-vm/Pulumi.yaml",
            STARTER_GCP_VM_PULUMI_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/apps/podinfo/catalog.yaml",
            STARTER_PODINFO_CATALOG_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/apps/dex/catalog.yaml",
            CORE_CATALOG_DEX_APP_YAML.to_string(),
        ));
        files_to_write.push((
            "catalog/apps/cert-manager/catalog.yaml",
            STARTER_CERT_MANAGER_APP_YAML.to_string(),
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

const CONTROL_PLANE_PULUMI_YAML: &str =
    include_str!("../../../../deploy/bootstrap/control-plane/Pulumi.yaml");
const CONTROL_PLANE_FORGEJO_PULUMI_YAML: &str =
    include_str!("../../../../deploy/bootstrap/control-plane-forgejo/Pulumi.yaml");
const CONTROL_PLANE_RAUTHY_PULUMI_YAML: &str =
    include_str!("../../../../deploy/bootstrap/control-plane-rauthy/Pulumi.yaml");

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

async fn materialize_control_plane_program_dir(
    program_name: &str,
    embedded_yaml: &str,
    state_dir: &Path,
) -> Result<PathBuf, CliError> {
    let rel = PathBuf::from("deploy").join("bootstrap").join(program_name);
    if rel.join("Pulumi.yaml").is_file() {
        return Ok(rel);
    }
    let parent_rel = PathBuf::from("..").join(&rel);
    if parent_rel.join("Pulumi.yaml").is_file() {
        return Ok(parent_rel);
    }

    let embedded_dir = state_dir.join(format!("program-{program_name}"));
    tokio::fs::create_dir_all(&embedded_dir).await?;
    tokio::fs::write(embedded_dir.join("Pulumi.yaml"), embedded_yaml).await?;
    Ok(embedded_dir)
}

async fn run_pulumi_cmd(program_dir: &Path, args: &[&str]) -> Result<(), CliError> {
    let mut cmd = tokio::process::Command::new("pulumi");
    cmd.args(args)
        .current_dir(program_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PULUMI_SKIP_UPDATE_CHECK", "true");

    if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_none() {
        cmd.env("PULUMI_CONFIG_PASSPHRASE", "");
    }

    let output = cmd.output().await.map_err(|e| {
        CliError::Validation(format!(
            "failed to invoke `pulumi` (is it installed and in PATH?): {e}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        let subcmd = args.first().copied().unwrap_or_default();
        return Err(CliError::Validation(format!(
            "`pulumi {subcmd}` failed: {detail}"
        )));
    }
    Ok(())
}

async fn run_pulumi_control_plane_stack(
    program_name: &str,
    embedded_yaml: &str,
    cluster_name: &str,
    config_pairs: &[(&str, &str, bool)],
) -> Result<(), CliError> {
    let stage0_root = std::env::temp_dir().join("keel-stage0-pulumi");
    let state_dir = stage0_root
        .join(format!("control-plane-{cluster_name}"))
        .join(program_name);
    tokio::fs::create_dir_all(&state_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        let _ = std::fs::set_permissions(&stage0_root, perms.clone());
        if let Some(cluster_dir) = state_dir.parent() {
            let _ = std::fs::set_permissions(cluster_dir, perms.clone());
        }
        std::fs::set_permissions(&state_dir, perms)?;
    }
    let program_dir =
        materialize_control_plane_program_dir(program_name, embedded_yaml, &state_dir).await?;

    let backend_url = format!("file://{}", state_dir.display());
    let stack_name = format!("stage0-{program_name}-{cluster_name}");

    run_pulumi_cmd(&program_dir, &["login", &backend_url]).await?;
    run_pulumi_cmd(&program_dir, &["stack", "select", &stack_name, "--create"]).await?;

    if !config_pairs.is_empty() {
        let mut owned_args: Vec<String> = Vec::with_capacity(2 + config_pairs.len() * 2);
        owned_args.push("config".to_string());
        owned_args.push("set-all".to_string());
        for &(key, value, is_secret) in config_pairs {
            owned_args.push(if is_secret {
                "--secret".to_string()
            } else {
                "--plaintext".to_string()
            });
            owned_args.push(format!("{key}={value}"));
        }
        let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        run_pulumi_cmd(&program_dir, &arg_refs).await?;
    }

    run_pulumi_cmd(&program_dir, &["up", "--yes", "--non-interactive"]).await?;
    Ok(())
}

/// Infers `(ForgeKind, Option<api_url>)` from a remote `--git-url` and optional explicit overrides.
pub(crate) fn infer_forge_config(
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

async fn read_secret_key(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
    namespace: &str,
    secret_name: &str,
    key: &str,
) -> Option<String> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(kc) = kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    let tmpl = format!("{{{{.data.{key} | base64decode}}}}");
    let out = cmd
        .args([
            "-n",
            namespace,
            "get",
            "secret",
            secret_name,
            "-o",
            &format!("go-template={tmpl}"),
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

async fn sync_in_cluster_s3_credentials_from_seaweedfs(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<(), CliError> {
    const SEAWEEDFS_S3_SECRET: &str = "s3-state-backend-seaweedfs-s3-secret";
    let access_key = read_secret_key(
        context,
        kubeconfig,
        KEEL_SYSTEM_NAMESPACE,
        SEAWEEDFS_S3_SECRET,
        "admin_access_key_id",
    )
    .await
    .ok_or_else(|| {
        CliError::Validation(format!(
            "missing `admin_access_key_id` in `{KEEL_SYSTEM_NAMESPACE}/{SEAWEEDFS_S3_SECRET}`"
        ))
    })?;
    let secret_key = read_secret_key(
        context,
        kubeconfig,
        KEEL_SYSTEM_NAMESPACE,
        SEAWEEDFS_S3_SECRET,
        "admin_secret_access_key",
    )
    .await
    .ok_or_else(|| {
        CliError::Validation(format!(
            "missing `admin_secret_access_key` in `{KEEL_SYSTEM_NAMESPACE}/{SEAWEEDFS_S3_SECRET}`"
        ))
    })?;

    let default_tenant_rbac =
        TenantManifestBundle::synthesize(&TenantIsolationConfig::new("default"))?
            .to_multi_doc_yaml()?;
    run_kubectl_apply(context, kubeconfig, &default_tenant_rbac).await?;

    for ns in [
        KEEL_SYSTEM_NAMESPACE,
        FLUX_SYSTEM_NAMESPACE,
        KEEL_PREVIEWS_NAMESPACE,
    ] {
        let secret_doc = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": S3_CREDENTIALS_SECRET_NAME,
                "namespace": ns,
                "labels": {
                    "app.kubernetes.io/managed-by": "keel",
                    "keel.io/secret-kind": "s3-credentials",
                },
            },
            "type": "Opaque",
            "stringData": {
                "accessKey": access_key,
                "secretKey": secret_key,
            },
        });
        let secret_yaml = serde_yml::to_string(&secret_doc)?;
        run_kubectl_apply(context, kubeconfig, &secret_yaml).await?;
    }

    Ok(())
}

async fn ensure_seaweedfs_pulumi_bucket(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<(), CliError> {
    use tokio::io::AsyncWriteExt;

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
        "exec",
        "-i",
        "sts/s3-state-backend-seaweedfs-filer",
        "--",
        "weed",
        "shell",
        "-master=s3-state-backend-seaweedfs-master:9333",
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(b"s3.bucket.create -name pulumi-state\n")
            .await?;
        drop(stdin);
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stderr.contains("already exists") || stdout.contains("already exists") {
            return Ok(());
        }
        return Err(CliError::Validation(format!(
            "failed to create `pulumi-state` bucket in SeaweedFS: {stderr}"
        )));
    }
    Ok(())
}

/// Best-effort CoreDNS rewrite so in-cluster pods (such as Forgejo) resolve `idp.keel.local`
/// and `forgejo.keel.local` to the Envoy Gateway proxy service in `keel-system`.
async fn configure_coredns_local_rewrites(context: Option<&str>, kubeconfig: Option<&Path>) {
    let mut svc_name = String::new();
    for _ in 0..15 {
        let mut cmd = tokio::process::Command::new("kubectl");
        if let Some(kc) = kubeconfig {
            cmd.arg("--kubeconfig").arg(kc);
        }
        if let Some(ctx) = context {
            cmd.arg("--context").arg(ctx);
        }
        if let Ok(out) = cmd
            .args([
                "-n",
                KEEL_SYSTEM_NAMESPACE,
                "get",
                "svc",
                "-l",
                "gateway.envoyproxy.io/owning-gateway-name=keel-ingress",
                "-o",
                "jsonpath={.items[0].metadata.name}",
            ])
            .output()
            .await
        {
            let candidate = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !candidate.is_empty() {
                svc_name = candidate;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    if svc_name.is_empty() {
        tracing::warn!(
            "Envoy Gateway service for keel-ingress not ready yet; skipping CoreDNS local rewrites"
        );
        return;
    }

    let mut get_cm = tokio::process::Command::new("kubectl");
    if let Some(kc) = kubeconfig {
        get_cm.arg("--kubeconfig").arg(kc);
    }
    if let Some(ctx) = context {
        get_cm.arg("--context").arg(ctx);
    }
    let Ok(cm_out) = get_cm
        .args([
            "-n",
            "kube-system",
            "get",
            "configmap",
            "coredns",
            "-o",
            "jsonpath={.data.Corefile}",
        ])
        .output()
        .await
    else {
        return;
    };
    if !cm_out.status.success() {
        return;
    }
    let corefile = String::from_utf8_lossy(&cm_out.stdout).to_string();
    if corefile.contains("idp.keel.local") {
        return;
    }
    let target_fqdn = format!("{svc_name}.{KEEL_SYSTEM_NAMESPACE}.svc.cluster.local");
    let rewrite_block = format!(
        "    rewrite name exact idp.keel.local {target_fqdn}\n\
         \x20   rewrite name exact forgejo.keel.local {target_fqdn}\n\
         \x20   kubernetes cluster.local"
    );
    let updated_corefile = corefile.replacen("    kubernetes cluster.local", &rewrite_block, 1);
    if updated_corefile == corefile {
        return;
    }

    let cm_doc = serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "coredns",
            "namespace": "kube-system",
        },
        "data": {
            "Corefile": updated_corefile,
        },
    });
    if let Ok(cm_yaml) = serde_yml::to_string(&cm_doc)
        && run_kubectl_apply(context, kubeconfig, &cm_yaml)
            .await
            .is_ok()
    {
        let mut restart = tokio::process::Command::new("kubectl");
        if let Some(kc) = kubeconfig {
            restart.arg("--kubeconfig").arg(kc);
        }
        if let Some(ctx) = context {
            restart.arg("--context").arg(ctx);
        }
        let _ = restart
            .args([
                "-n",
                "kube-system",
                "rollout",
                "restart",
                "deployment/coredns",
            ])
            .output()
            .await;
        let mut status = tokio::process::Command::new("kubectl");
        if let Some(kc) = kubeconfig {
            status.arg("--kubeconfig").arg(kc);
        }
        if let Some(ctx) = context {
            status.arg("--context").arg(ctx);
        }
        let _ = status
            .args([
                "-n",
                "kube-system",
                "rollout",
                "status",
                "deployment/coredns",
                "--timeout=60s",
            ])
            .output()
            .await;
        tracing::info!(
            envoy_svc = %svc_name,
            "Configured CoreDNS rewrites for idp.keel.local and forgejo.keel.local"
        );
    }
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

    let sops_age_key =
        match read_secret_key(ctx, kc, FLUX_SYSTEM_NAMESPACE, "sops-age", "age.agekey").await {
            Some(existing) => existing,
            None => artifacts.sops_age_keyfile.clone().unwrap_or_default(),
        };
    let enable_policy_exceptions = if artifacts.kyverno_bundle.policy_exception.is_some() {
        "true"
    } else {
        "false"
    };
    let kube_context = ctx.unwrap_or("");

    tracing::info!(
        context = ?ctx,
        "Deploying Stage 0 seed control-plane addons via Pulumi (`control-plane`)"
    );
    run_pulumi_control_plane_stack(
        "control-plane",
        CONTROL_PLANE_PULUMI_YAML,
        &connection.cluster_name,
        &[
            ("kubeconfig", &connection.kubeconfig_yaml, true),
            ("kubeContext", kube_context, false),
            ("sopsAgeKey", &sops_age_key, true),
            (
                "enableKyvernoPolicyExceptions",
                enable_policy_exceptions,
                false,
            ),
        ],
    )
    .await?;

    let needs_seaweedfs_sync = artifacts
        .management_stack
        .spec
        .backend
        .as_deref()
        .is_some_and(|b| b.contains("s3-state-backend-seaweedfs-s3.keel-system.svc.cluster.local"));

    let metric_templates_yaml = format!(
        "apiVersion: flagger.app/v1beta1\n\
         kind: MetricTemplate\n\
         metadata:\n\
         \x20 name: envoy-gateway-request-success-rate\n\
         \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
         \x20 labels:\n\
         \x20   app.kubernetes.io/managed-by: keel\n\
         spec:\n\
         \x20 provider:\n\
         \x20   type: prometheus\n\
         \x20   address: http://prometheus.{KEEL_SYSTEM_NAMESPACE}:80\n\
         \x20 query: |\n\
         \x20   sum(rate(envoy_cluster_upstream_rq{{envoy_cluster_name=~\"httproute/{{{{ namespace }}}}/{{{{ target }}}}/.*\",envoy_response_code!~\"5.*\"}}[{{{{ interval }}}}]))\n\
         \x20   /\n\
         \x20   sum(rate(envoy_cluster_upstream_rq{{envoy_cluster_name=~\"httproute/{{{{ namespace }}}}/{{{{ target }}}}/.*\"}}[{{{{ interval }}}}]))\n\
         \x20   * 100\n\
         ---\n\
         apiVersion: flagger.app/v1beta1\n\
         kind: MetricTemplate\n\
         metadata:\n\
         \x20 name: envoy-gateway-request-duration\n\
         \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
         \x20 labels:\n\
         \x20   app.kubernetes.io/managed-by: keel\n\
         spec:\n\
         \x20 provider:\n\
         \x20   type: prometheus\n\
         \x20   address: http://prometheus.{KEEL_SYSTEM_NAMESPACE}:80\n\
         \x20 query: |\n\
         \x20   histogram_quantile(0.99,\n\
         \x20     sum(rate(envoy_cluster_upstream_rq_time_bucket{{envoy_cluster_name=~\"httproute/{{{{ namespace }}}}/{{{{ target }}}}/.*\"}}[{{{{ interval }}}}])) by (le)\n\
         \x20   )\n"
    );
    let kyverno_yaml = artifacts.kyverno_bundle.to_multi_doc_yaml()?;
    let docker_proxy_yaml = artifacts.kyverno_bundle.policy_exception.as_ref().map(|_| {
        format!(
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
             \x20           - -t\n\
             \x20           - \"600\"\n\
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
             \x20     targetPort: 2375\n\
             ---\n\
             apiVersion: networking.k8s.io/v1\n\
             kind: NetworkPolicy\n\
             metadata:\n\
             \x20 name: allow-keel-docker-proxy\n\
             \x20 namespace: {FLUX_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             \x20   keel.io/local-dev-only: \"true\"\n\
             spec:\n\
             \x20 podSelector:\n\
             \x20   matchLabels:\n\
             \x20     app: keel-docker-proxy\n\
             \x20 policyTypes:\n\
             \x20   - Ingress\n\
             \x20 ingress:\n\
             \x20   - from:\n\
             \x20       - namespaceSelector: {{}}\n\
             \x20         podSelector:\n\
             \x20           matchLabels:\n\
             \x20             auto.pulumi.com/component: workspace\n\
             \x20     ports:\n\
             \x20       - port: 2375\n\
             \x20         protocol: TCP\n"
        )
    });

    let infra_setup_fut = async {
        if needs_seaweedfs_sync {
            sync_in_cluster_s3_credentials_from_seaweedfs(ctx, kc).await?;
            ensure_seaweedfs_pulumi_bucket(ctx, kc).await?;
        }
        run_kubectl_apply(ctx, kc, &metric_templates_yaml).await?;
        tracing::info!("Applying Kyverno policies & management core Stack");
        let mut last_err = None;
        for attempt in 1..=6 {
            match run_kubectl_apply(ctx, kc, &kyverno_yaml).await {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(err) => {
                    let msg = err.to_string();
                    let transient_webhook = msg.contains("failed calling webhook")
                        || msg.contains("context deadline exceeded")
                        || msg.contains("connection refused");
                    if transient_webhook && attempt < 6 {
                        tracing::warn!(
                            attempt,
                            "Kyverno webhook not ready yet; retrying policy apply in 5s: {msg}"
                        );
                        last_err = Some(err);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        if let Some(ref proxy_yaml) = docker_proxy_yaml {
            tracing::info!(
                "Deploying local-dev Docker socket proxy in flux-system for nested kind"
            );
            run_kubectl_apply(ctx, kc, proxy_yaml).await?;
        }
        Ok::<(), CliError>(())
    };

    let rauthy_fut = async {
        let Some(ref rauthy_cfg) = artifacts.rauthy_config else {
            return Ok::<(), CliError>(());
        };
        tracing::info!("Applying cert-manager self-signed CA & keel-wildcard-tls Certificate");
        let ca_yaml = format!(
            "apiVersion: cert-manager.io/v1\n\
             kind: ClusterIssuer\n\
             metadata:\n\
             \x20 name: keel-selfsigned\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 selfSigned: {{}}\n\
             ---\n\
             apiVersion: cert-manager.io/v1\n\
             kind: Certificate\n\
             metadata:\n\
             \x20 name: keel-ca\n\
             \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 isCA: true\n\
             \x20 commonName: keel-local-ca\n\
             \x20 secretName: keel-ca-secret\n\
             \x20 privateKey:\n\
             \x20   algorithm: ECDSA\n\
             \x20   size: 256\n\
             \x20 issuerRef:\n\
             \x20   name: keel-selfsigned\n\
             \x20   kind: ClusterIssuer\n\
             \x20   group: cert-manager.io\n\
             ---\n\
             apiVersion: cert-manager.io/v1\n\
             kind: ClusterIssuer\n\
             metadata:\n\
             \x20 name: keel-ca-issuer\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 ca:\n\
             \x20   secretName: keel-ca-secret\n\
             ---\n\
             apiVersion: cert-manager.io/v1\n\
             kind: Certificate\n\
             metadata:\n\
             \x20 name: keel-wildcard-tls\n\
             \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 secretName: keel-wildcard-tls\n\
             \x20 dnsNames:\n\
             \x20   - \"*.keel.local\"\n\
             \x20   - idp.keel.local\n\
             \x20   - forgejo.keel.local\n\
             \x20   - keel.keel.local\n\
             \x20 issuerRef:\n\
             \x20   name: keel-ca-issuer\n\
             \x20   kind: ClusterIssuer\n\
             \x20   group: cert-manager.io\n"
        );
        run_kubectl_apply(ctx, kc, &ca_yaml).await?;

        for _ in 0..30 {
            if read_secret_key(
                ctx,
                kc,
                KEEL_SYSTEM_NAMESPACE,
                "keel-wildcard-tls",
                "tls.crt",
            )
            .await
            .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        tracing::info!("Applying Envoy GatewayClass & keel-ingress Gateway (HTTPS port 8888)");
        let gateway_yaml = format!(
            "apiVersion: gateway.networking.k8s.io/v1\n\
             kind: GatewayClass\n\
             metadata:\n\
             \x20 name: keel-envoy\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 controllerName: gateway.envoyproxy.io/gatewayclass-controller\n\
             ---\n\
             apiVersion: gateway.networking.k8s.io/v1\n\
             kind: Gateway\n\
             metadata:\n\
             \x20 name: keel-ingress\n\
             \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 gatewayClassName: keel-envoy\n\
             \x20 listeners:\n\
             \x20   - name: http\n\
             \x20     protocol: HTTP\n\
             \x20     port: 80\n\
             \x20     allowedRoutes:\n\
             \x20       namespaces:\n\
             \x20         from: All\n\
             \x20   - name: https-local\n\
             \x20     protocol: HTTPS\n\
             \x20     port: 8888\n\
             \x20     tls:\n\
             \x20       mode: Terminate\n\
             \x20       certificateRefs:\n\
             \x20         - kind: Secret\n\
             \x20           name: keel-wildcard-tls\n\
             \x20     allowedRoutes:\n\
             \x20       namespaces:\n\
             \x20         from: All\n\
             ---\n\
             apiVersion: gateway.networking.k8s.io/v1\n\
             kind: HTTPRoute\n\
             metadata:\n\
             \x20 name: rauthy-ingress\n\
             \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 parentRefs:\n\
             \x20   - name: keel-ingress\n\
             \x20     namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20     sectionName: https-local\n\
             \x20 hostnames:\n\
             \x20   - idp.keel.local\n\
             \x20 rules:\n\
             \x20   - matches:\n\
             \x20       - path:\n\
             \x20           type: PathPrefix\n\
             \x20           value: /\n\
             \x20     backendRefs:\n\
             \x20       - name: rauthy\n\
             \x20         port: 8080\n"
        );
        run_kubectl_apply(ctx, kc, &gateway_yaml).await?;

        let groups_b64 = STANDARD.encode(rauthy_cfg.groups_json.as_bytes());
        let users_b64 = STANDARD.encode(rauthy_cfg.users_json.as_bytes());
        let clients_b64 = STANDARD.encode(rauthy_cfg.clients_json.as_bytes());

        tracing::info!(
            "Installing Stage 0 Rauthy OIDC provider via Pulumi (`control-plane-rauthy`)"
        );
        run_pulumi_control_plane_stack(
            "control-plane-rauthy",
            CONTROL_PLANE_RAUTHY_PULUMI_YAML,
            &connection.cluster_name,
            &[
                ("kubeconfig", &connection.kubeconfig_yaml, true),
                ("kubeContext", kube_context, false),
                ("rauthyConfigToml", &rauthy_cfg.config_toml, true),
                ("rauthyGroupsB64", &groups_b64, false),
                ("rauthyUsersB64", &users_b64, true),
                ("rauthyClientsB64", &clients_b64, false),
                ("rauthySecretName", &rauthy_cfg.secret_name, false),
                ("rauthySecretNamespace", &rauthy_cfg.secret_namespace, false),
            ],
        )
        .await
    };

    let forgejo_fut = async {
        if !with_forge {
            return Ok::<(), CliError>(());
        }
        tracing::info!(
            "Installing in-cluster Forgejo Git forge via Pulumi (`control-plane-forgejo`)"
        );
        run_pulumi_control_plane_stack(
            "control-plane-forgejo",
            CONTROL_PLANE_FORGEJO_PULUMI_YAML,
            &connection.cluster_name,
            &[
                ("kubeconfig", &connection.kubeconfig_yaml, true),
                ("kubeContext", kube_context, false),
            ],
        )
        .await
    };

    tokio::try_join!(infra_setup_fut, rauthy_fut, forgejo_fut)?;

    if with_forge && artifacts.rauthy_config.is_some() {
        let forgejo_route_yaml = format!(
            "apiVersion: gateway.networking.k8s.io/v1\n\
             kind: HTTPRoute\n\
             metadata:\n\
             \x20 name: forgejo-ingress\n\
             \x20 namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20 labels:\n\
             \x20   app.kubernetes.io/managed-by: keel\n\
             spec:\n\
             \x20 parentRefs:\n\
             \x20   - name: keel-ingress\n\
             \x20     namespace: {KEEL_SYSTEM_NAMESPACE}\n\
             \x20     sectionName: https-local\n\
             \x20 hostnames:\n\
             \x20   - forgejo.keel.local\n\
             \x20 rules:\n\
             \x20   - matches:\n\
             \x20       - path:\n\
             \x20           type: PathPrefix\n\
             \x20           value: /\n\
             \x20     backendRefs:\n\
             \x20       - name: forgejo-http\n\
             \x20         port: 3000\n"
        );
        run_kubectl_apply(ctx, kc, &forgejo_route_yaml).await?;
        configure_coredns_local_rewrites(ctx, kc).await;
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
            apply_git_credentials_secret(ctx, kc, "git", token).await?;
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
             \x20   name: {FLUX_SYSTEM_NAMESPACE}\n\
             \x20 decryption:\n\
             \x20   provider: sops\n\
             \x20   secretRef:\n\
             \x20     name: sops-age\n"
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
        let forgejo_oidc_secret = artifacts
            .rauthy_config
            .as_ref()
            .and_then(|r| r.forgejo_oidc_secret.as_deref());
        let token = provision_in_cluster_forgejo(
            connection.context.as_deref(),
            connection.kubeconfig_path.as_deref(),
            pf.local_port,
            forgejo_oidc_secret,
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
    if let Some(ref rauthy) = artifacts.rauthy_config {
        out.push_str(&format!(
            "OIDC Provider:   Rauthy ({}/{})\n",
            KEEL_SYSTEM_NAMESPACE, RAUTHY_CONFIG_SECRET_NAME
        ));
        out.push_str("OIDC Issuer:     https://idp.keel.local:8888/auth/v1\n");
        out.push_str("IdP Admin UI:    https://idp.keel.local:8888/auth/v1/admin\n");
        out.push_str(&format!("Admin Username:  {}\n", artifacts.admin_username));
        if artifacts.admin_password_generated {
            out.push_str(&format!("Admin Password:  {}\n", artifacts.admin_password));
        } else {
            out.push_str("Admin Password:  [PROVIDED VIA CLI]\n");
        }
        if rauthy.forgejo_oidc_secret.is_some() {
            out.push_str("Forgejo SSO URL: https://forgejo.keel.local:8888 (OAuth: rauthy)\n");
            out.push_str(
                "Local /etc/hosts: ensure `127.0.0.1 idp.keel.local forgejo.keel.local` is configured\n",
            );
        } else {
            out.push_str("Local /etc/hosts: ensure `127.0.0.1 idp.keel.local` is configured\n");
        }
    } else if let Some(ref oidc) = artifacts.keel_config.oidc {
        out.push_str(&format!("OIDC Issuer:     {}\n", oidc.issuer_url));
    }
    out.push_str("Portal URL:      http://127.0.0.1:8080\n");
    out
}
