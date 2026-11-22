//! Stateless control-plane HTTP/SSE server bootstrap (`keel serve`, `SDD.md` §6.3, §7.2).

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{
    AuthPhase, FLUX_SYSTEM_NAMESPACE, ForgeKind, KEEL_SYSTEM_NAMESPACE, KeelConfig,
    OIDC_CLIENT_SECRET_ENV, OidcConfig, ProviderRegistry, TENANT_REPO_READER_SECRET_NAME,
    TENANT_REPOS_ORG,
};
use keel_git::{
    CommitAuthor, ForgeProvider, ForgejoGiteaProvider, GitCredentials, GitHubProvider,
    GitLabProvider, GitRunner, LocalGitProvider, RemoteRepoSlug, parse_remote_repo_url,
};
use keel_kube::{
    InMemorySecretStore, K8sSecretStore, KubeClusterStatusReader, S3_CREDENTIALS_SECRET_NAME,
    SecretStateStore, TelemetryMux, TenantIsolationConfig, TenantManifestBundle, WatchedCrdKind,
};
use keel_pko::{
    IN_CLUSTER_S3_BACKEND_URL, ManagementCoreStackParams, attach_s3_credentials_env_refs,
    synthesize_management_core_stack,
};
use keel_server::{
    AppState, KubeClusterStateProvider, RepoClusterStateProvider, TenantRepoForge,
    TenantRepoForgeKind, TenantRepoManifests, TenantRepoNaming, TenantRepos, build_router,
};
use zeroize::Zeroizing;

use crate::cli::ServeArgs;

use super::bootstrap::{
    FORGE_TOKEN_SECRET_NAME, TempPortForward, default_bare_repo_for_workdir,
    start_forgejo_port_forward, start_gateway_port_forward,
};
use super::{CliError, absolutize_path, is_relative_local_remote};

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
            let api_base = forgejo_api_base(effective_api, &slug);
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

/// Forgejo/Gitea API base (no `/api/v1` suffix) from `forge.apiUrl` (or an override), else the
/// repository origin.
fn forgejo_api_base(effective_api: Option<&str>, slug: &RemoteRepoSlug) -> String {
    effective_api
        .map(|u| {
            let trimmed = u.trim_end_matches('/');
            trimmed
                .strip_suffix("/api/v1")
                .unwrap_or(trimmed)
                .to_string()
        })
        .unwrap_or_else(|| slug.origin_url.clone())
}

/// Inputs of [`build_tenant_repos`] (platform configuration and cluster-only Secrets).
pub(crate) struct TenantRepoInputs<'a> {
    pub(crate) cfg: Option<&'a KeelConfig>,
    /// The platform forge provider (`build_forge_provider`).
    pub(crate) admin: Arc<dyn ForgeProvider>,
    /// `true` when `admin` talks to the configured remote forge (a token was found).
    pub(crate) remote_forge: bool,
    pub(crate) api_url_override: Option<&'a str>,
    /// Forgejo tenant-reader bot (`keel-system/keel-forgejo-tenant-reader` username/password).
    pub(crate) reader_bot: Option<(String, Zeroizing<String>)>,
    /// Public Keel CA (`keel-system/keel-wildcard-tls` `ca.crt`).
    pub(crate) keel_ca: Option<String>,
}

/// Per-tenant repository registry (`SDD.md` §3.2): dedicated repositories on every remote forge
/// (Forgejo/Gitea: organisation `keel-tenants`, HTTPS bot tokens; GitLab: the platform group,
/// deploy tokens; GitHub: the platform owner, SSH deploy keys), the monorepo layout only for the
/// development-only Local Git provider (D8) — `forge.provider: local`, or no `.keel/config.yaml`
/// at all (no forge configured). A remote forge Keel cannot use (no forge token) never falls
/// back to the monorepo: tenant provisioning fails closed instead.
pub(crate) fn build_tenant_repos(
    inputs: TenantRepoInputs<'_>,
    cache_root: std::path::PathBuf,
) -> Result<TenantRepos, CliError> {
    let Some(cfg) = inputs.cfg.filter(|c| c.forge.provider != ForgeKind::Local) else {
        return Ok(TenantRepos::monorepo(cache_root));
    };
    if !inputs.remote_forge {
        let reason = format!(
            "`.keel/config.yaml` configures a {:?} forge but no forge token was found (--git-token, KEEL_GIT_TOKEN or {KEEL_SYSTEM_NAMESPACE}/{FORGE_TOKEN_SECRET_NAME}); configure one and restart keel-server",
            cfg.forge.provider
        );
        tracing::warn!("Tenant repositories are unavailable: {reason}");
        return Ok(TenantRepos::unavailable(reason, cache_root));
    }
    let Some(naming) = TenantRepoNaming::from_forge_config(&cfg.forge)
        .map_err(|e| CliError::Validation(format!("tenant repositories: {e}")))?
    else {
        return Ok(TenantRepos::monorepo(cache_root));
    };
    let token_minter = match (naming.kind, inputs.reader_bot) {
        (TenantRepoForgeKind::Forgejo, Some((user, password))) => {
            let slug = parse_remote_repo_url(&cfg.forge.repo_url).map_err(|e| {
                CliError::Validation(format!(
                    "invalid forge.repo_url '{}': {e}",
                    cfg.forge.repo_url
                ))
            })?;
            let effective_api = inputs.api_url_override.or(cfg.forge.api_url.as_deref());
            Some(Arc::new(
                ForgejoGiteaProvider::with_basic_auth(
                    forgejo_api_base(effective_api, &slug),
                    TENANT_REPOS_ORG,
                    &slug.repo,
                    &user,
                    password.as_str(),
                )
                .map_err(|e| CliError::Validation(e.to_string()))?,
            ) as Arc<dyn ForgeProvider>)
        }
        (TenantRepoForgeKind::Forgejo, None) => {
            tracing::warn!(
                "keel-system/{TENANT_REPO_READER_SECRET_NAME} is missing: tenant repository credentials cannot be minted until `keel bootstrap init` provisions the tenant-reader bot"
            );
            None
        }
        _ => None,
    };
    let forge = TenantRepoForge {
        // The Keel CA only fronts the in-cluster Forgejo (`forge.publicUrl`).
        ca_crt: match naming.kind {
            TenantRepoForgeKind::Forgejo => cfg.forge.public_url.as_ref().and(inputs.keel_ca),
            _ => None,
        },
        naming,
        admin: inputs.admin,
        token_minter,
    };
    tracing::info!(
        kind = ?forge.kind,
        owner = %forge.owner,
        "Tenants use dedicated repositories (<monorepo>-tenant-<id>)"
    );
    Ok(TenantRepos::dedicated(forge, cache_root))
}

/// Default HTTPS username paired with `KEEL_GIT_TOKEN` for `--git-url` (the Stage 0 Forgejo admin).
const DEFAULT_REMOTE_GIT_USERNAME: &str = "keel";

/// Builds the [`GitRunner`] used to clone/fetch `--git-url`. HTTP(S) remotes with a token get
/// ephemeral Basic credentials; everything else (local paths, SSH, anonymous) runs without.
fn remote_bootstrap_runner(url: &str, token: Option<&str>, username: Option<&str>) -> GitRunner {
    let is_http = {
        let lower = url.trim().to_ascii_lowercase();
        lower.starts_with("http://") || lower.starts_with("https://")
    };
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(tok) if is_http => GitRunner::new().with_credentials(GitCredentials::HttpsBasic {
            username: username
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .unwrap_or(DEFAULT_REMOTE_GIT_USERNAME)
                .to_string(),
            password_or_token: tok.to_string(),
        }),
        _ => GitRunner::new(),
    }
}

/// Builds a configured stateless [`AppState`] from [`ServeArgs`] (`SDD.md` §6.3, §7.2), plus any
/// in-cluster port-forwards that must stay alive for as long as the state is in use.
pub(crate) async fn build_serve_state(
    args: &ServeArgs,
    enable_in_cluster_port_forward: bool,
) -> Result<(AppState, Vec<TempPortForward>), CliError> {
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

    let remote_git_url = args
        .git_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let cli_token = args
        .git_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);

    let (git_runner, bare_str) = if let Some(ref url) = remote_git_url {
        // OPEN-30: stateless/in-cluster mode. The workdir is (re)hydrated from the remote GitOps
        // repository; no local bare repo is created. The token is injected ephemerally by
        // `GitRunner` (never argv, `.git/config`, or logs).
        let runner =
            remote_bootstrap_runner(url, cli_token.as_deref(), args.git_username.as_deref());
        let outcome = keel_git::ensure_workdir_from_remote(&runner, url, &repo_workdir).await?;
        tracing::info!(
            ?outcome,
            repo_dir = %repo_workdir.display(),
            "Bootstrapped keel serve workdir from remote GitOps repository"
        );
        (runner, url.clone())
    } else {
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
        (git_runner, bare_str)
    };

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
            let mut mgmt_params = ManagementCoreStackParams::new(
                IN_CLUSTER_S3_BACKEND_URL,
                FLUX_SYSTEM_NAMESPACE,
                "clusters/management",
            );
            attach_s3_credentials_env_refs(&mut mgmt_params.env_refs, S3_CREDENTIALS_SECRET_NAME);
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
        .unwrap_or(AuthPhase::OidcActive);
    let repo_oidc_cfg = parsed_keel_cfg
        .as_ref()
        .and_then(|c| c.oidc.as_ref())
        .filter(|o| o.enabled)
        .cloned();
    // `KEEL_OIDC_*` env overrides (e.g. the catalog-installed keel-server, whose OIDC client is
    // configured in its Deployment) win over the repository `oidc` block.
    let oidc_cfg = OidcConfig::with_env_overrides(repo_oidc_cfg, |name| std::env::var(name).ok())?;

    let telemetry = TelemetryMux::default();
    let kube_reader = KubeClusterStatusReader::try_default().await.ok();
    if let Some(ref reader) = kube_reader {
        tracing::info!("Spawning background kube-rs CRD watchers for SSE telemetry");
        for kind in WatchedCrdKind::ALL {
            telemetry.spawn_crd_watcher(reader.client().clone(), kind, None);
        }
    }

    let env_token = std::env::var("KEEL_GIT_TOKEN").ok();
    let mut resolved_token: Option<String> = cli_token.clone().or_else(|| {
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

    let mut port_forwards: Vec<TempPortForward> = Vec::new();
    let mut api_url_override: Option<String> = None;
    // Inside a Pod the in-cluster Service DNS resolves directly; port-forwards are a laptop aid.
    let running_in_pod = std::env::var_os("KUBERNETES_SERVICE_HOST").is_some();
    if enable_in_cluster_port_forward
        && !running_in_pod
        && remote_git_url.is_none()
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
        port_forwards.push(pf);
    }

    if enable_in_cluster_port_forward
        && !running_in_pod
        && kube_reader.is_some()
        && let Some(ref oidc) = oidc_cfg
        && oidc.issuer_url.contains("idp.keel.local")
    {
        match start_gateway_port_forward(None, None).await {
            Ok(pf) => {
                tracing::info!(
                    local_port = pf.local_port,
                    "Started automatic port-forward to Envoy Gateway (keel-ingress:8888) for Rauthy OIDC"
                );
                port_forwards.push(pf);
            }
            Err(err) => {
                tracing::warn!(error = %err, "Failed to start automatic port-forward to Envoy Gateway (keel-ingress:8888)");
            }
        }
    }

    // `LocalGitProvider` fallback operates on the bare repo; in `--git-url` mode that is the remote
    // itself when it is a local path (otherwise the forge must be a remote provider).
    let local_forge_repo = match remote_git_url.as_deref() {
        Some(url) if !url.contains("://") && !url.contains('@') => std::path::PathBuf::from(url),
        _ => bare_repo.clone(),
    };
    let forge = build_forge_provider(
        parsed_keel_cfg.as_ref(),
        resolved_token.as_deref(),
        api_url_override.as_deref(),
        &local_forge_repo,
    )?;
    let remote_forge = parsed_keel_cfg
        .as_ref()
        .is_some_and(|c| c.forge.provider != ForgeKind::Local)
        && resolved_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
    let (reader_bot, keel_ca) = match kube_reader.as_ref() {
        Some(reader) if remote_forge => {
            let user = reader
                .read_secret_key(
                    KEEL_SYSTEM_NAMESPACE,
                    TENANT_REPO_READER_SECRET_NAME,
                    "username",
                )
                .await;
            let password = reader
                .read_secret_key(
                    KEEL_SYSTEM_NAMESPACE,
                    TENANT_REPO_READER_SECRET_NAME,
                    "password",
                )
                .await;
            let ca = reader
                .read_secret_key(KEEL_SYSTEM_NAMESPACE, "keel-wildcard-tls", "ca.crt")
                .await;
            (user.zip(password.map(Zeroizing::new)), ca)
        }
        _ => (None, None),
    };
    let tenant_repos = build_tenant_repos(
        TenantRepoInputs {
            cfg: parsed_keel_cfg.as_ref(),
            admin: forge.clone(),
            remote_forge,
            api_url_override: api_url_override.as_deref(),
            reader_bot,
            keel_ca,
        },
        TenantRepos::default_cache_root(&repo_workdir),
    )?;

    let git_runner =
        if remote_git_url.is_some() && !matches!(git_runner.credentials(), GitCredentials::None) {
            // `--git-url` mode already carries the explicit clone credentials.
            git_runner
        } else if let Some(ref tok) = resolved_token
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

    let (secret_store, cluster_secret_store): (Arc<dyn SecretStateStore>, bool) =
        if let Some(ref reader) = kube_reader {
            let k8s_store = K8sSecretStore::new(reader.client().clone());
            if k8s_store.get_or_create_session_key().await.is_ok() {
                (Arc::new(k8s_store) as Arc<dyn SecretStateStore>, true)
            } else {
                (Arc::new(InMemorySecretStore::new()), false)
            }
        } else {
            (Arc::new(InMemorySecretStore::new()), false)
        };

    let live_mgmt_backend = if let Some(ref reader) = kube_reader {
        reader
            .read_stack_backend(FLUX_SYSTEM_NAMESPACE, "management-cluster-core")
            .await
    } else {
        None
    };
    // Tenants with their own repositories: workloads are listed from each tenant repository's
    // committed `main` (read-only mirrors), never from the platform repository alone.
    let repo_state_provider = if tenant_repos.is_dedicated() {
        RepoClusterStateProvider::new(&repo_workdir).with_tenant_source(Arc::new(
            TenantRepoManifests::new(
                tenant_repos.clone(),
                Clone::clone(&git_runner),
                &repo_workdir,
            ),
        ))
    } else {
        RepoClusterStateProvider::new(&repo_workdir)
    };
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
    .with_oidc_config(oidc_cfg)
    .with_oidc_client_secret(std::env::var(OIDC_CLIENT_SECRET_ENV).ok())
    // Forge HTTPS origin tenant/preview PKO Stacks clone the in-cluster Forgejo through
    // (`forge.publicUrl`, platform configuration from the committed `.keel/config.yaml`).
    .with_forge_public_url(
        parsed_keel_cfg
            .as_ref()
            .and_then(|c| c.forge.public_url.clone()),
    )
    // Tenant Stack proposals require the tenant state passphrase Secret whenever Secrets live in
    // the cluster; cluster-less local dev (in-memory store, nothing applied) skips the gate.
    .with_require_pulumi_passphrase(cluster_secret_store)
    .with_tenant_repos(tenant_repos)
    .with_state_provider(Arc::new(repo_state_provider.clone()));
    if let Some(backend) = mgmt_stack_backend {
        state.backend_url = backend;
    }
    // Scoped state credentials can only be provisioned/verified against a live cluster; a
    // cluster-less server (in-memory secrets, nothing applied) must not enforce them — otherwise
    // the in-cluster default backend URL alone would reject every Pulumi proposal with 409.
    if cluster_secret_store
        && (state
            .backend_url
            .contains("s3-state-backend-seaweedfs-s3.keel-system.svc.cluster.local")
            || state
                .secret_store
                .has_s3_credentials()
                .await
                .unwrap_or(false))
    {
        state = state.with_s3_credentials_secret(Some(S3_CREDENTIALS_SECRET_NAME.to_string()));
    }
    // Tenant stacks use per-tenant scoped SeaweedFS identities (`tenants/<id>/*`), provisioned
    // through kube-rs exec into the in-cluster SeaweedFS filer.
    if let Some(ref reader) = kube_reader
        && state.s3_credentials_secret.is_some()
        && state
            .backend_url
            .contains("s3-state-backend-seaweedfs-s3.keel-system.svc.cluster.local")
    {
        let provisioner = keel_kube::SeaweedFsTenantS3Provisioner::new(
            reader.client().clone(),
            state.backend_url.clone(),
        );
        state = state.with_tenant_s3_provisioner(Some(Arc::new(provisioner)));
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
        if let Some(ca_pem) = reader
            .read_secret_key(KEEL_SYSTEM_NAMESPACE, "keel-wildcard-tls", "ca.crt")
            .await
        {
            state = state.with_oidc_ca_pem(Some(ca_pem.into_bytes()));
        }
        // Real ephemeral PKO previews: preview Stacks are applied into `keel-previews` and their
        // state persisted in-cluster (labels/annotations + report ConfigMap).
        state = state.with_preview_cluster(Some(Arc::new(keel_kube::KubePreviewCluster::new(
            reader.client().clone(),
        ))));
        state = state.with_state_provider(Arc::new(KubeClusterStateProvider::new(
            repo_state_provider,
            Arc::new(reader),
        )));
        if let Ok(raw_reg) =
            tokio::fs::read_to_string(state.repo_workdir.join("providers/registry.yaml")).await
            && let Ok(registry) = ProviderRegistry::from_yaml(&raw_reg)
        {
            let enabled_ids: Vec<String> = registry
                .providers
                .into_iter()
                .filter(|p| p.enabled)
                .map(|p| p.id)
                .collect();
            if let Ok(policy_val) =
                keel_kube::build_restrict_pulumi_providers_policy_for_registry(&enabled_ids)
            {
                let _ = state
                    .state_provider
                    .apply_cluster_policy(
                        keel_kube::kyverno::POLICY_RESTRICT_PULUMI_PROVIDERS,
                        &policy_val,
                    )
                    .await;
            }
        }
    }

    Ok((state, port_forwards))
}

/// Binds `keel-server` on `args.bind` and runs until `shutdown` resolves.
pub async fn execute_serve_until<F>(args: &ServeArgs, shutdown: F) -> Result<SocketAddr, CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (state, _forge_pf) = build_serve_state(args, true).await?;
    // Restart-/replica-safe preview GC + result publication driven by cluster state and forge
    // PR states (OPEN-10). No-op without a live cluster.
    let preview_reconciler = keel_server::preview_lifecycle::spawn_preview_reconciler(
        state.clone(),
        keel_server::preview_lifecycle::PREVIEW_RECONCILE_INTERVAL,
    );
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    let local_addr = listener.local_addr()?;

    println!("Keel Control Plane listening on http://{local_addr}");
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
    if let Some(handle) = preview_reconciler {
        handle.abort();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: ForgeKind, repo_url: &str, public_url: Option<&str>) -> KeelConfig {
        let mut cfg = KeelConfig::default();
        cfg.forge.provider = provider;
        cfg.forge.repo_url = repo_url.to_string();
        cfg.forge.public_url = public_url.map(str::to_string);
        cfg
    }

    fn inputs<'a>(cfg: Option<&'a KeelConfig>, remote_forge: bool) -> TenantRepoInputs<'a> {
        TenantRepoInputs {
            cfg,
            admin: Arc::new(LocalGitProvider::new(std::env::temp_dir())),
            remote_forge,
            api_url_override: None,
            reader_bot: Some((
                "keel-tenant-reader".to_string(),
                Zeroizing::new("pw".to_string()),
            )),
            keel_ca: Some("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into()),
        }
    }

    #[test]
    fn only_the_local_provider_keeps_tenants_in_the_monorepo() {
        let root = std::path::PathBuf::from("/tmp/cache");
        assert!(
            !build_tenant_repos(inputs(None, false), root.clone())
                .unwrap()
                .is_dedicated()
        );
        let local = cfg(ForgeKind::Local, "", None);
        assert!(
            !build_tenant_repos(inputs(Some(&local), true), root.clone())
                .unwrap()
                .is_dedicated()
        );
        // A remote forge without a token fails closed instead of falling back to the monorepo.
        let gh = cfg(ForgeKind::GitHub, "https://github.com/acme/infra.git", None);
        let repos = build_tenant_repos(inputs(Some(&gh), false), root).unwrap();
        assert!(repos.is_dedicated());
        assert!(repos.forge().is_none());
        let err = repos.require_forge().unwrap_err().to_string();
        assert!(err.contains("no forge token"), "{err}");
    }

    #[test]
    fn remote_forges_get_dedicated_repositories_with_their_transport() {
        let root = std::path::PathBuf::from("/tmp/cache");
        let forgejo = cfg(
            ForgeKind::Forgejo,
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
            Some("https://forgejo.keel.local:8888"),
        );
        let repos = build_tenant_repos(inputs(Some(&forgejo), true), root.clone()).unwrap();
        let f = repos.forge().unwrap();
        assert_eq!(f.kind, TenantRepoForgeKind::Forgejo);
        assert_eq!(f.owner, TENANT_REPOS_ORG);
        assert!(f.token_minter.is_some());
        assert!(f.ca_crt.is_some());
        assert_eq!(
            f.clone_url("acme").unwrap(),
            "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-acme.git"
        );

        // External Forgejo: no Keel CA, origin from the repository URL.
        let external = cfg(
            ForgeKind::Forgejo,
            "https://git.example.com/ops/infra.git",
            None,
        );
        let repos = build_tenant_repos(inputs(Some(&external), true), root.clone()).unwrap();
        let f = repos.forge().unwrap();
        assert!(f.ca_crt.is_none());
        assert_eq!(
            f.clone_url("acme").unwrap(),
            "https://git.example.com/keel-tenants/infra-tenant-acme.git"
        );

        let gitlab = cfg(
            ForgeKind::GitLab,
            "https://gitlab.com/acme/platform/infra.git",
            None,
        );
        let repos = build_tenant_repos(inputs(Some(&gitlab), true), root.clone()).unwrap();
        let f = repos.forge().unwrap();
        assert_eq!(f.owner, "acme/platform");
        assert_eq!(
            f.clone_url("web").unwrap(),
            "https://gitlab.com/acme/platform/infra-tenant-web.git"
        );

        let github = cfg(ForgeKind::GitHub, "https://github.com/acme/infra.git", None);
        let repos = build_tenant_repos(inputs(Some(&github), true), root).unwrap();
        let f = repos.forge().unwrap();
        assert_eq!(f.kind, TenantRepoForgeKind::GitHub);
        assert_eq!(
            f.clone_url("web").unwrap(),
            "ssh://git@github.com/acme/infra-tenant-web.git"
        );
    }
}
