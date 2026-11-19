//! REST and Server-Sent Events (SSE) API handlers (`SDD.md` §7.3).

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::{Stream, StreamExt};
use keel_core::{
    AppRuntime, BundleItemKind, CATALOG_API_VERSION, CATALOG_APPLICATION_KIND, CATALOG_BUNDLE_KIND,
    CATALOG_INFRA_BLUEPRINT_KIND, CatalogAppMetadata, CatalogAppSpec, CatalogApplication,
    CatalogBlueprintMetadata, CatalogBlueprintSpec, CatalogBundle, CatalogBundleItem,
    CatalogBundleMetadata, CatalogBundleSpec, CatalogInfraBlueprint, CatalogManifest,
    KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE, ManifestEngine, ProgressiveDeliverySpec,
    ProviderRegistry, SopsManager, StackSshInfoResponse, UpstreamSource,
    extract_manifest_variables, parse_catalog, parse_catalog_application, parse_catalog_blueprint,
    parse_catalog_bundle, partition_values_by_schema, render_helm_values_yaml,
    render_manifest_template, tenant_namespace, validate_dns_label, validate_k8s_manifests_yaml,
    validate_schema_values,
};
use keel_git::{
    CommitAuthor, CreatePrRequest, PullRequestComment, PullRequestInfo, PullRequestState,
    parse_proposal_branch_tenant,
};
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, FlaggerCanarySpecInput, FluxDependencyRef,
    FluxHealthCheckRef, FluxKustomizationSpecInput, FluxSubstituteReference,
    GITHUB_TOKEN_SECRET_NAME, HelmChartSourceKind, HelmChartSourceRef, HelmReleaseSpecInput,
    HelmValuesSecretRef, RemoteKubeConfigRef, StackDiagnostics, TenantAgeKey,
    TenantIsolationConfig, TenantManifestBundle, UserGitCredentials, WatchedCrdKind,
    generate_flux_kustomization_manifest, synthesize_canary_gateway_network_policy_yaml,
    synthesize_flagger_canary_yaml, synthesize_flux_kustomization_yaml,
    synthesize_helm_release_yaml, user_git_creds_secret_name,
};
use keel_pko::{
    ChangeOp, EphemeralPreviewParams, GcPlan, PkoStack, PreviewReport, PreviewResourceCandidate,
    PreviewSummary, ResourceDiff, TenantStackParams, attach_github_token_env_ref,
    attach_s3_credentials_env_refs, build_preview_branch, format_markdown_report, plan_preview_gc,
    synthesize_ephemeral_preview, synthesize_k8s_manifest_pulumi_yaml, synthesize_tenant_stack,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::{AppState, CachedPreview, ReleaseSummary, StackSummary};

static LAST_BRANCH_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn attach_infra_secret_refs(
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
    secrets_ref: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
    secret_name: &str,
    secret_keys: &[String],
) {
    for secret_key in secret_keys {
        let ref_val = keel_pko::PkoEnvRef::from_secret(secret_name, secret_key);
        match secret_key.as_str() {
            // AWS provider credentials must go into Pulumi `secretsRef` (`aws:accessKey` /
            // `aws:secretKey`) so they do not overwrite `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
            // used by the SeaweedFS S3 state backend.
            "awsAccessKeyId" => {
                secrets_ref.insert("aws:accessKey".to_string(), ref_val);
            }
            "awsSecretAccessKey" => {
                secrets_ref.insert("aws:secretKey".to_string(), ref_val);
            }
            // Azure & GCP providers read standard uppercase environment variables.
            "armClientId" => {
                env_refs.insert("ARM_CLIENT_ID".to_string(), ref_val);
            }
            "armClientSecret" => {
                env_refs.insert("ARM_CLIENT_SECRET".to_string(), ref_val);
            }
            "armTenantId" => {
                env_refs.insert("ARM_TENANT_ID".to_string(), ref_val);
            }
            "armSubscriptionId" => {
                env_refs.insert("ARM_SUBSCRIPTION_ID".to_string(), ref_val);
            }
            "googleCredentials" => {
                env_refs.insert("GOOGLE_CREDENTIALS".to_string(), ref_val);
            }
            other => {
                env_refs.insert(other.to_string(), ref_val);
            }
        }
    }
}

async fn resolve_sops_recipients(repo_workdir: &std::path::Path, tenant_pub: &str) -> String {
    let sops_cfg = repo_workdir.join(".sops.yaml");
    if let Ok(raw) = tokio::fs::read_to_string(&sops_cfg).await {
        for line in raw.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("age:") {
                let cluster_pub = rest.trim();
                if cluster_pub.starts_with("age1") && cluster_pub != tenant_pub.trim() {
                    return format!("{},{cluster_pub}", tenant_pub.trim());
                }
            }
        }
    }
    tenant_pub.trim().to_string()
}

fn next_unique_branch_timestamp() -> u64 {
    let now = Utc::now().timestamp().max(1) as u64;
    let mut prev = LAST_BRANCH_TS.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST_BRANCH_TS.compare_exchange_weak(
            prev,
            next,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(actual) => prev = actual,
        }
    }
}

/// `GET /healthz`
pub async fn healthz() -> &'static str {
    "OK"
}

/// `GET /metrics`
pub async fn metrics(State(state): State<AppState>) -> Response {
    let phase = *state.auth_phase.read().await;
    let phase_label = match phase {
        keel_core::AuthPhase::OidcActive => "oidc-active",
        keel_core::AuthPhase::BreakGlass => "break-glass",
    };
    let cached_previews = state.preview_cache.read().await.len();
    let body = format!(
        "# HELP keel_server_up Whether keel-server is running.\n\
         # TYPE keel_server_up gauge\n\
         keel_server_up 1\n\
         # HELP keel_auth_phase Active authentication phase.\n\
         # TYPE keel_auth_phase gauge\n\
         keel_auth_phase{{phase=\"{phase_label}\"}} 1\n\
         # HELP keel_ephemeral_previews_cached Number of cached PR previews.\n\
         # TYPE keel_ephemeral_previews_cached gauge\n\
         keel_ephemeral_previews_cached {cached_previews}\n"
    );
    (
        StatusCode::OK,
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// Application catalog DTO enriched with optional `manifestsYaml` and `defaultEngine` fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogItemDto {
    #[serde(flatten)]
    pub application: CatalogApplication,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifests_yaml: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_engine: Option<String>,
}

impl CatalogItemDto {
    pub fn from_application(
        application: CatalogApplication,
        manifests_yaml: Option<String>,
    ) -> Self {
        let default_engine = application
            .spec
            .default_engine
            .map(|e| e.as_str().to_string())
            .or_else(|| {
                if application.metadata.runtime == AppRuntime::Manifest {
                    Some(ManifestEngine::Flux.as_str().to_string())
                } else {
                    None
                }
            });
        Self {
            application,
            manifests_yaml,
            default_engine,
        }
    }
}

/// Response body for `GET /api/v1/catalog`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogResponse {
    pub applications: Vec<CatalogItemDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core_addons: Vec<CatalogApplication>,
    pub blueprints: Vec<CatalogInfraBlueprint>,
    #[serde(default)]
    pub bundles: Vec<CatalogBundle>,
    pub enabled_providers: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub pulumi_yaml: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub manifests_yaml: HashMap<String, String>,
}

/// `GET /api/v1/catalog`
///
/// Scans `catalog/apps/*/catalog.yaml` and `catalog/infra/*/catalog.yaml` in `repo_workdir`,
/// filtering infrastructure blueprints by enabled providers in `providers/registry.yaml`.
/// Joins `rel` onto `base`, validating that `rel` is a relative path containing no `ParentDir` (`..`)
/// components and that the resulting path remains within `base`.
fn safe_relative_path(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel_trim = rel.trim();
    if rel_trim.is_empty() {
        return None;
    }
    let rel_path = Path::new(rel_trim);
    if rel_path.is_absolute() {
        return None;
    }
    for component in rel_path.components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => return None,
            _ => {}
        }
    }
    let joined = base.join(rel_path);
    if joined.starts_with(base) {
        Some(joined)
    } else {
        None
    }
}

async fn load_catalog_items_from_dir(
    base_dir: &Path,
) -> Result<(Vec<CatalogItemDto>, HashMap<String, String>), ServerError> {
    let mut applications = Vec::new();
    let mut manifests_map = HashMap::new();
    for path in find_catalog_files(base_dir).await {
        let yaml = tokio::fs::read_to_string(&path).await?;
        match parse_catalog_application(&yaml) {
            Ok(app) => {
                let mut loaded_manifests = None;
                if let Some(dir) = path.parent() {
                    let default_manifest_path = dir.join("manifests.yaml");
                    if let Ok(content) = tokio::fs::read_to_string(&default_manifest_path).await {
                        loaded_manifests = Some(content);
                    } else if app.metadata.runtime == AppRuntime::Manifest
                        && let Some(rel_path) = app.spec.upstream.path.as_deref()
                        && let Some(resolved) = safe_relative_path(dir, rel_path)
                        && let Ok(content) = tokio::fs::read_to_string(&resolved).await
                    {
                        loaded_manifests = Some(content);
                    }
                }
                if let Some(ref content) = loaded_manifests {
                    manifests_map.insert(app.metadata.id.clone(), content.clone());
                }
                applications.push(CatalogItemDto::from_application(app, loaded_manifests));
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "Skipping invalid CatalogApplication");
            }
        }
    }
    applications.sort_by(|a, b| a.application.metadata.id.cmp(&b.application.metadata.id));
    Ok((applications, manifests_map))
}

pub async fn get_catalog(
    _auth: AuthSession,
    State(state): State<AppState>,
) -> Result<Json<CatalogResponse>, ServerError> {
    let registry_path = state.repo_workdir.join("providers/registry.yaml");
    let registry = if tokio::fs::try_exists(&registry_path).await.unwrap_or(false) {
        let yaml = tokio::fs::read_to_string(&registry_path).await?;
        ProviderRegistry::from_yaml(&yaml)?
    } else {
        ProviderRegistry::default_day1()
    };

    let enabled_providers: Vec<String> = registry
        .enabled_providers()
        .into_iter()
        .map(|p| p.id.clone())
        .collect();

    let (applications, manifests_yaml) =
        load_catalog_items_from_dir(&state.repo_workdir.join("catalog/apps")).await?;

    let mut core_addons = Vec::new();
    for path in find_catalog_files(&state.repo_workdir.join("catalog/core")).await {
        let yaml = tokio::fs::read_to_string(&path).await?;
        match parse_catalog_application(&yaml) {
            Ok(app) => core_addons.push(app),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "Skipping invalid core CatalogApplication");
            }
        }
    }
    core_addons.sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));

    let mut blueprints = Vec::new();
    let mut pulumi_yaml = HashMap::new();
    for path in find_catalog_files(&state.repo_workdir.join("catalog/infra")).await {
        let yaml = tokio::fs::read_to_string(&path).await?;
        match parse_catalog_blueprint(&yaml) {
            Ok(bp) => {
                let is_kubernetes_builtin = bp.metadata.provider == "kubernetes"
                    && !registry.providers.iter().any(|p| p.id == "kubernetes");
                if registry.is_enabled(&bp.metadata.provider) || is_kubernetes_builtin {
                    if let Some(dir) = path.parent() {
                        let entrypoint = if bp.spec.entrypoint.trim().is_empty() {
                            "Pulumi.yaml"
                        } else {
                            bp.spec.entrypoint.as_str()
                        };
                        if let Some(pulumi_path) = safe_relative_path(dir, entrypoint)
                            && let Ok(content) = tokio::fs::read_to_string(&pulumi_path).await
                        {
                            pulumi_yaml.insert(bp.metadata.id.clone(), content);
                        }
                    }
                    blueprints.push(bp);
                }
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "Skipping invalid CatalogInfraBlueprint");
            }
        }
    }
    blueprints.sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));

    let mut bundles = Vec::new();
    for path in find_catalog_files(&state.repo_workdir.join("catalog/bundles")).await {
        let yaml = tokio::fs::read_to_string(&path).await?;
        match parse_catalog_bundle(&yaml) {
            Ok(bundle) => bundles.push(bundle),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "Skipping invalid CatalogBundle");
            }
        }
    }
    bundles.sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));

    Ok(Json(CatalogResponse {
        applications,
        core_addons,
        blueprints,
        bundles,
        enabled_providers,
        pulumi_yaml,
        manifests_yaml,
    }))
}

/// Summary of an infrastructure provider returned by `GET /api/v1/providers` and `PUT /api/v1/providers/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatusEntry {
    pub id: String,
    pub enabled: bool,
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Response body for `GET /api/v1/providers`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderStatusEntry>,
}

/// Request body for `PUT /api/v1/providers/{id}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleProviderRequest {
    pub enabled: bool,
}

/// Response body for `PUT /api/v1/providers/{id}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleProviderResponse {
    pub provider: ProviderStatusEntry,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

async fn load_provider_registry(repo_workdir: &Path) -> Result<ProviderRegistry, ServerError> {
    let registry_path = repo_workdir.join("providers/registry.yaml");
    if tokio::fs::try_exists(&registry_path).await.unwrap_or(false) {
        let yaml = tokio::fs::read_to_string(&registry_path).await?;
        Ok(ProviderRegistry::from_yaml(&yaml)?)
    } else {
        Ok(ProviderRegistry::default_day1())
    }
}

/// `GET /api/v1/providers`
///
/// Reads `providers/registry.yaml` (falling back to `ProviderRegistry::default_day1()`).
pub async fn list_providers(
    _auth: AuthSession,
    State(state): State<AppState>,
) -> Result<Json<ProvidersResponse>, ServerError> {
    let registry = load_provider_registry(&state.repo_workdir).await?;
    let providers = registry
        .providers
        .into_iter()
        .map(|p| ProviderStatusEntry {
            id: p.id,
            enabled: p.enabled,
            package: p.plugin,
            description: p.description,
        })
        .collect();
    Ok(Json(ProvidersResponse { providers }))
}

/// `PUT /api/v1/providers/{id}`
///
/// Validates DNS-1123 `id`, toggles `enabled` in `providers/registry.yaml` (`404` if missing),
/// commits on `keel/providers/toggle-<id>-<ts>`, pushes, and opens a PR via `ForgeProvider`.
pub async fn toggle_provider(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<ToggleProviderRequest>,
) -> Result<Json<ToggleProviderResponse>, ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;

    let mut registry = load_provider_registry(&state.repo_workdir).await?;
    let existing = registry
        .providers
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ServerError::NotFound(format!("Provider '{id}' not found in registry")))?;
    if existing.enabled == req.enabled {
        return Err(ServerError::Conflict(format!(
            "Provider '{id}' is already {} on main",
            if req.enabled { "enabled" } else { "disabled" }
        )));
    }
    registry.set_enabled(&id, req.enabled);
    let updated_yaml = registry.to_yaml()?;

    let updated_entry = registry
        .providers
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ServerError::NotFound(format!("Provider '{id}' not found in registry")))?;
    let provider = ProviderStatusEntry {
        id: updated_entry.id,
        enabled: updated_entry.enabled,
        package: updated_entry.plugin,
        description: updated_entry.description,
    };

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch("providers", &format!("toggle-{id}"), timestamp)?;
    let rel_file = "providers/registry.yaml";
    let action_verb = if req.enabled { "enable" } else { "disable" };

    let commit_sha = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        let providers_dir = state.repo_workdir.join("providers");
        tokio::fs::create_dir_all(&providers_dir).await?;
        tokio::fs::write(providers_dir.join("registry.yaml"), &updated_yaml).await?;

        state
            .git_runner
            .add(&state.repo_workdir, [rel_file])
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("feat(providers): {action_verb} provider {id}");
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        sha
    };

    let pr_req = CreatePrRequest::new(
        format!("feat(providers): {action_verb} provider {id}"),
        format!(
            "Automated provider registry update for `{id}` (`enabled={}`)\n\n- Commit: `{commit_sha}`\n- Path: `{rel_file}`",
            req.enabled
        ),
        &branch,
    )
    .with_tenant("providers")
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    Ok(Json(ToggleProviderResponse {
        provider,
        proposal,
        branch,
    }))
}

async fn find_catalog_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(base_dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let candidate = entry.path().join("catalog.yaml");
        if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            out.push(candidate);
        }
    }
    out.sort();
    out
}

fn default_catalog_item_kind() -> String {
    "app".to_string()
}

/// Request body for `POST /api/v1/catalog`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterCatalogAppRequest {
    #[serde(default = "default_catalog_item_kind")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub category: String,
    #[serde(default)]
    pub runtime: Option<AppRuntime>,
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub chart: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub flagger_supported: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub pulumi_runtime: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub protect_by_default: bool,
    #[serde(default)]
    pub terraform_provider: Option<String>,
    #[serde(default)]
    pub pulumi_yaml: Option<String>,
    #[serde(default)]
    pub manifests_yaml: Option<String>,
    #[serde(default)]
    pub default_engine: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub items: Option<Vec<CatalogBundleItem>>,
    #[serde(default)]
    pub schema: Option<Value>,
}

/// Type alias for catalog item creation requests.
pub type CreateCatalogItemRequest = RegisterCatalogAppRequest;

/// Response body for `POST /api/v1/catalog`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterCatalogAppResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<CatalogItemDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blueprint: Option<CatalogInfraBlueprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<CatalogBundle>,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

/// `POST /api/v1/catalog`
///
/// Registers a new `CatalogApplication` (`kind: "app"`, default) at `catalog/apps/<id>/catalog.yaml`
/// or a `CatalogInfraBlueprint` (`kind: "infra"`) at `catalog/infra/<id>/catalog.yaml` + `Pulumi.yaml`
/// on proposal branch `keel/catalog/add-<id>-<ts>`, commits, pushes, and opens a PR via `ForgeProvider`.
pub async fn register_catalog_app(
    auth: AuthSession,
    State(state): State<AppState>,
    Json(req): Json<RegisterCatalogAppRequest>,
) -> Result<(StatusCode, Json<RegisterCatalogAppResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&req.id, "id")?;

    match req.kind.as_str() {
        "app" => {
            let runtime = req.runtime.ok_or_else(|| {
                ServerError::BadRequest("runtime is required when kind is 'app'".to_string())
            })?;

            let (repo_url, path, validated_manifests_yaml, parsed_default_engine) = if runtime
                == AppRuntime::Manifest
            {
                let raw_manifests = req
                    .manifests_yaml
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ServerError::BadRequest(
                            "manifestsYaml is required when runtime is 'manifest'".to_string(),
                        )
                    })?;
                validate_k8s_manifests_yaml(raw_manifests)?;
                let normalized_manifests = if raw_manifests.ends_with('\n') {
                    raw_manifests.to_string()
                } else {
                    format!("{raw_manifests}\n")
                };
                let repo_url = req
                    .repo_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("https://kubernetes.io/docs/concepts/overview/working-with-objects/")
                    .to_string();
                let path = Some(
                    req.path
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("./manifests.yaml")
                        .to_string(),
                );
                let eng = match req
                    .default_engine
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(raw_eng) => raw_eng.parse::<ManifestEngine>()?,
                    None => ManifestEngine::Flux,
                };
                (repo_url, path, Some(normalized_manifests), Some(eng))
            } else {
                let repo_url = req
                    .repo_url
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        ServerError::BadRequest(
                            "repoUrl is required when kind is 'app'".to_string(),
                        )
                    })?;
                let eng = match req
                    .default_engine
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(raw_eng) => Some(raw_eng.parse::<ManifestEngine>()?),
                    None => None,
                };
                (repo_url, req.path, None, eng)
            };

            let default_app_schema = || {
                json!({
                    "type": "object",
                    "properties": {
                        "replicaCount": {
                            "type": "integer",
                            "default": 1
                        }
                    }
                })
            };

            let schema = match req.schema {
                Some(s) => s,
                None if runtime == keel_core::AppRuntime::Helm => {
                    let chart_name = req.chart.as_deref().unwrap_or(&req.id);
                    match fetch_upstream_helm_schema(&repo_url, chart_name, req.version.as_deref())
                        .await
                    {
                        Ok((fetched, _source)) => fetched,
                        Err(err) => {
                            tracing::warn!(
                                chart = %chart_name,
                                repo_url = %repo_url,
                                error = %err,
                                "Unable to fetch upstream Helm schema during catalog registration; using default fallback"
                            );
                            default_app_schema()
                        }
                    }
                }
                None if runtime == AppRuntime::Manifest => {
                    let vars = validated_manifests_yaml
                        .as_deref()
                        .map(extract_manifest_variables)
                        .unwrap_or_default();
                    if vars.is_empty() {
                        json!({
                            "type": "object",
                            "properties": {}
                        })
                    } else {
                        let mut props = serde_json::Map::new();
                        for var in vars {
                            props.insert(
                                var.clone(),
                                json!({
                                    "type": "string",
                                    "title": humanize_schema_key(&var)
                                }),
                            );
                        }
                        json!({
                            "type": "object",
                            "properties": props
                        })
                    }
                }
                None => default_app_schema(),
            };

            let application = CatalogApplication {
                api_version: CATALOG_API_VERSION.to_string(),
                kind: CATALOG_APPLICATION_KIND.to_string(),
                metadata: CatalogAppMetadata {
                    id: req.id.clone(),
                    name: req.name,
                    description: req.description.filter(|s| !s.trim().is_empty()),
                    category: req.category,
                    runtime,
                },
                spec: CatalogAppSpec {
                    upstream: UpstreamSource {
                        repo_url,
                        chart: req.chart,
                        version: req.version,
                        path,
                        git_ref: None,
                    },
                    progressive_delivery: ProgressiveDeliverySpec {
                        flagger_supported: req.flagger_supported,
                        default_canary: false,
                    },
                    companion_infrastructure: Vec::new(),
                    schema,
                    ui_schema: None,
                    service_url: None,
                    default_engine: parsed_default_engine,
                },
            };
            application.validate()?;

            let catalog_yaml = serde_yml::to_string(&application)
                .map_err(|e| ServerError::Internal(e.to_string()))?;

            let rel_dir = format!("catalog/apps/{}", req.id);
            let rel_file = format!("{rel_dir}/catalog.yaml");
            let rel_manifests = format!("{rel_dir}/manifests.yaml");
            if tokio::fs::try_exists(state.repo_workdir.join(&rel_file))
                .await
                .unwrap_or(false)
            {
                return Err(ServerError::Conflict(format!(
                    "Catalog application '{}' already exists",
                    req.id
                )));
            }

            let timestamp = Utc::now().timestamp().max(1) as u64;
            let branch = build_preview_branch("catalog", &format!("add-{}", req.id), timestamp)?;

            let commit_sha = {
                let _guard = state.git_lock.lock().await;
                state
                    .git_runner
                    .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
                    .await?;
                state
                    .git_runner
                    .checkout_new_branch(&state.repo_workdir, &branch)
                    .await?;

                let target_dir = state.repo_workdir.join(&rel_dir);
                tokio::fs::create_dir_all(&target_dir).await?;
                tokio::fs::write(target_dir.join("catalog.yaml"), &catalog_yaml).await?;
                let mut staged_files = vec![rel_file.clone()];
                if let Some(ref manifests_content) = validated_manifests_yaml {
                    tokio::fs::write(target_dir.join("manifests.yaml"), manifests_content).await?;
                    staged_files.push(rel_manifests.clone());
                }

                state
                    .git_runner
                    .add(&state.repo_workdir, staged_files.iter().map(String::as_str))
                    .await?;

                let author = CommitAuthor::new(
                    auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
                    auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
                );
                let commit_msg = format!("feat(catalog): add application {}", req.id);
                let sha = state
                    .git_runner
                    .commit(&state.repo_workdir, &commit_msg, &author)
                    .await?;
                state
                    .git_runner
                    .push(&state.repo_workdir, "origin", &branch)
                    .await?;

                let _ = state
                    .git_runner
                    .checkout_branch(&state.repo_workdir, "main")
                    .await;
                sha
            };

            let pr_req = CreatePrRequest::new(
                format!("feat(catalog): register application {}", req.id),
                format!(
                    "Automated catalog registration for `{}` (`{}`)\n\n- Commit: `{}`\n- Path: `{}`",
                    req.id, application.metadata.name, commit_sha, rel_file
                ),
                &branch,
            )
            .with_tenant("catalog")
            .with_author(format!(
                "{} <{}>",
                auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
                auth.claims.email.as_deref().unwrap_or("keel@keel.local")
            ));
            let proposal = state.forge.create_pull_request(pr_req).await?;

            Ok((
                StatusCode::CREATED,
                Json(RegisterCatalogAppResponse {
                    application: Some(CatalogItemDto::from_application(
                        application,
                        validated_manifests_yaml,
                    )),
                    blueprint: None,
                    bundle: None,
                    proposal,
                    branch,
                }),
            ))
        }
        "infra" => {
            let provider = req.provider.ok_or_else(|| {
                ServerError::BadRequest("provider is required when kind is 'infra'".to_string())
            })?;
            validate_dns_label(&provider, "provider")?;

            let schema = req.schema.unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "title": "Resource Name",
                            "default": "workload-1"
                        }
                    }
                })
            });

            let pulumi_runtime = req
                .pulumi_runtime
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "yaml".to_string());
            let entrypoint = req
                .entrypoint
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Pulumi.yaml".to_string());

            let blueprint = CatalogInfraBlueprint {
                api_version: CATALOG_API_VERSION.to_string(),
                kind: CATALOG_INFRA_BLUEPRINT_KIND.to_string(),
                metadata: CatalogBlueprintMetadata {
                    id: req.id.clone(),
                    name: req.name,
                    description: req.description.filter(|s| !s.trim().is_empty()),
                    category: req.category,
                    provider: provider.clone(),
                },
                spec: CatalogBlueprintSpec {
                    pulumi_runtime: pulumi_runtime.clone(),
                    entrypoint,
                    protect_by_default: req.protect_by_default,
                    outputs: Vec::new(),
                    schema,
                    ui_schema: None,
                },
            };
            blueprint.validate()?;

            let catalog_yaml = serde_yml::to_string(&blueprint)
                .map_err(|e| ServerError::Internal(e.to_string()))?;

            let pulumi_yaml = if let Some(custom_yaml) =
                req.pulumi_yaml.filter(|s| !s.trim().is_empty())
            {
                serde_yml::from_str::<Value>(&custom_yaml).map_err(|e| {
                    ServerError::BadRequest(format!("Invalid Pulumi.yaml YAML syntax: {e}"))
                })?;
                if custom_yaml.ends_with('\n') {
                    custom_yaml
                } else {
                    format!("{custom_yaml}\n")
                }
            } else {
                let mut pulumi_doc = serde_json::Map::new();
                pulumi_doc.insert("name".to_string(), json!(req.id));
                pulumi_doc.insert("runtime".to_string(), json!(pulumi_runtime));
                pulumi_doc.insert(
                    "description".to_string(),
                    json!(format!("{} ({})", blueprint.metadata.name, provider)),
                );
                if let Some(tf_provider) = req.terraform_provider.filter(|s| !s.trim().is_empty()) {
                    pulumi_doc.insert(
                        "packages".to_string(),
                        json!({
                            provider.clone(): {
                                "source": "terraform-provider",
                                "parameters": [tf_provider]
                            }
                        }),
                    );
                }
                pulumi_doc.insert(
                    "config".to_string(),
                    json!({
                        "name": {
                            "type": "string",
                            "default": "workload-1"
                        }
                    }),
                );
                pulumi_doc.insert("resources".to_string(), json!({}));
                pulumi_doc.insert("outputs".to_string(), json!({}));
                serde_yml::to_string(&Value::Object(pulumi_doc))
                    .map_err(|e| ServerError::Internal(e.to_string()))?
            };

            let rel_dir = format!("catalog/infra/{}", req.id);
            let rel_catalog = format!("{rel_dir}/catalog.yaml");
            let rel_pulumi = format!("{rel_dir}/Pulumi.yaml");
            if tokio::fs::try_exists(state.repo_workdir.join(&rel_catalog))
                .await
                .unwrap_or(false)
            {
                return Err(ServerError::Conflict(format!(
                    "Catalog blueprint '{}' already exists",
                    req.id
                )));
            }

            let timestamp = Utc::now().timestamp().max(1) as u64;
            let branch = build_preview_branch("catalog", &format!("add-{}", req.id), timestamp)?;

            let commit_sha = {
                let _guard = state.git_lock.lock().await;
                state
                    .git_runner
                    .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
                    .await?;
                state
                    .git_runner
                    .checkout_new_branch(&state.repo_workdir, &branch)
                    .await?;

                let target_dir = state.repo_workdir.join(&rel_dir);
                tokio::fs::create_dir_all(&target_dir).await?;
                tokio::fs::write(target_dir.join("catalog.yaml"), &catalog_yaml).await?;
                tokio::fs::write(target_dir.join("Pulumi.yaml"), &pulumi_yaml).await?;

                state
                    .git_runner
                    .add(&state.repo_workdir, [&rel_catalog, &rel_pulumi])
                    .await?;

                let author = CommitAuthor::new(
                    auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
                    auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
                );
                let commit_msg = format!("feat(catalog): add blueprint {}", req.id);
                let sha = state
                    .git_runner
                    .commit(&state.repo_workdir, &commit_msg, &author)
                    .await?;
                state
                    .git_runner
                    .push(&state.repo_workdir, "origin", &branch)
                    .await?;

                let _ = state
                    .git_runner
                    .checkout_branch(&state.repo_workdir, "main")
                    .await;
                sha
            };

            let pr_req = CreatePrRequest::new(
                format!("feat(catalog): register blueprint {}", req.id),
                format!(
                    "Automated catalog blueprint registration for `{}` (`{}`)\n\n- Commit: `{}`\n- Catalog: `{}`\n- Entrypoint: `{}`",
                    req.id, blueprint.metadata.name, commit_sha, rel_catalog, rel_pulumi
                ),
                &branch,
            )
            .with_tenant("catalog")
            .with_author(format!(
                "{} <{}>",
                auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
                auth.claims.email.as_deref().unwrap_or("keel@keel.local")
            ));
            let proposal = state.forge.create_pull_request(pr_req).await?;

            Ok((
                StatusCode::CREATED,
                Json(RegisterCatalogAppResponse {
                    application: None,
                    blueprint: Some(blueprint),
                    bundle: None,
                    proposal,
                    branch,
                }),
            ))
        }
        "bundle" => {
            let items = req.items.ok_or_else(|| {
                ServerError::BadRequest("items is required when kind is 'bundle'".to_string())
            })?;
            if items.is_empty() {
                return Err(ServerError::BadRequest(
                    "bundle items must contain at least one item".to_string(),
                ));
            }

            let schema = if let Some(custom_schema) = req.schema {
                validate_bundle_items_canary(&state.repo_workdir, &items).await?;
                custom_schema
            } else {
                compose_bundle_schema(&state.repo_workdir, &items).await?
            };

            let bundle = CatalogBundle {
                api_version: CATALOG_API_VERSION.to_string(),
                kind: CATALOG_BUNDLE_KIND.to_string(),
                metadata: CatalogBundleMetadata {
                    id: req.id.clone(),
                    name: req.name.clone(),
                    description: req.description.clone(),
                    category: req.category.clone(),
                },
                spec: CatalogBundleSpec {
                    items,
                    schema,
                    ui_schema: None,
                },
            };
            bundle.validate()?;

            let catalog_yaml =
                serde_yml::to_string(&bundle).map_err(|e| ServerError::Internal(e.to_string()))?;

            let rel_dir = format!("catalog/bundles/{}", req.id);
            let rel_catalog = format!("{rel_dir}/catalog.yaml");
            if tokio::fs::try_exists(state.repo_workdir.join(&rel_catalog))
                .await
                .unwrap_or(false)
            {
                return Err(ServerError::Conflict(format!(
                    "Catalog bundle '{}' already exists",
                    req.id
                )));
            }

            let timestamp = Utc::now().timestamp().max(1) as u64;
            let branch = build_preview_branch("catalog", &format!("add-{}", req.id), timestamp)?;

            let commit_sha = {
                let _guard = state.git_lock.lock().await;
                state
                    .git_runner
                    .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
                    .await?;
                state
                    .git_runner
                    .checkout_new_branch(&state.repo_workdir, &branch)
                    .await?;

                let target_dir = state.repo_workdir.join(&rel_dir);
                tokio::fs::create_dir_all(&target_dir).await?;
                tokio::fs::write(target_dir.join("catalog.yaml"), &catalog_yaml).await?;

                state
                    .git_runner
                    .add(&state.repo_workdir, [&rel_catalog])
                    .await?;

                let author = CommitAuthor::new(
                    auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
                    auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
                );
                let commit_msg = format!("feat(catalog): add bundle {}", req.id);
                let sha = state
                    .git_runner
                    .commit(&state.repo_workdir, &commit_msg, &author)
                    .await?;
                state
                    .git_runner
                    .push(&state.repo_workdir, "origin", &branch)
                    .await?;

                let _ = state
                    .git_runner
                    .checkout_branch(&state.repo_workdir, "main")
                    .await;
                sha
            };

            let pr_req = CreatePrRequest::new(
                format!("feat(catalog): register bundle {}", req.id),
                format!(
                    "Automated catalog bundle registration for `{}` (`{}`)\n\n- Commit: `{}`\n- Catalog: `{}`",
                    req.id, bundle.metadata.name, commit_sha, rel_catalog
                ),
                &branch,
            )
            .with_tenant("catalog")
            .with_author(format!(
                "{} <{}>",
                auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
                auth.claims.email.as_deref().unwrap_or("keel@keel.local")
            ));
            let proposal = state.forge.create_pull_request(pr_req).await?;

            Ok((
                StatusCode::CREATED,
                Json(RegisterCatalogAppResponse {
                    application: None,
                    blueprint: None,
                    bundle: Some(bundle),
                    proposal,
                    branch,
                }),
            ))
        }
        other => Err(ServerError::BadRequest(format!(
            "Unsupported catalog kind '{other}'; expected 'app', 'infra', or 'bundle'"
        ))),
    }
}

async fn validate_bundle_items_canary(
    repo_workdir: &std::path::Path,
    items: &[CatalogBundleItem],
) -> Result<(), ServerError> {
    for item in items {
        if !item.canary {
            continue;
        }
        if item.kind != BundleItemKind::App {
            return Err(ServerError::BadRequest(format!(
                "Bundle item '{}' sets canary: true, which is only valid for kind: app",
                item.id
            )));
        }
        let ref_manifest =
            load_catalog_manifest(repo_workdir, &item.catalog_ref, Some("app")).await?;
        if let CatalogManifest::Application(app) = ref_manifest
            && !app.spec.progressive_delivery.flagger_supported
        {
            return Err(ServerError::BadRequest(format!(
                "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
                app.metadata.id
            )));
        }
    }
    Ok(())
}

async fn compose_bundle_schema(
    repo_workdir: &std::path::Path,
    items: &[CatalogBundleItem],
) -> Result<Value, ServerError> {
    let mut props = serde_json::Map::new();
    let mut required = Vec::with_capacity(items.len());
    for item in items {
        validate_dns_label(&item.id, "items[].id")?;
        validate_dns_label(&item.catalog_ref, "items[].catalogRef")?;
        let kind_hint = match item.kind {
            BundleItemKind::Infra => "infra",
            BundleItemKind::App => "app",
        };
        let ref_manifest =
            load_catalog_manifest(repo_workdir, &item.catalog_ref, Some(kind_hint)).await?;
        let (ref_title, ref_schema) = match ref_manifest {
            CatalogManifest::Application(a) => {
                if item.canary && !a.spec.progressive_delivery.flagger_supported {
                    return Err(ServerError::BadRequest(format!(
                        "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
                        a.metadata.id
                    )));
                }
                (a.metadata.name, a.spec.schema)
            }
            CatalogManifest::InfraBlueprint(b) => {
                if item.canary {
                    return Err(ServerError::BadRequest(format!(
                        "Bundle item '{}' sets canary: true, which is only valid for kind: app",
                        item.id
                    )));
                }
                (b.metadata.name, b.spec.schema)
            }
            CatalogManifest::Bundle(_) => {
                return Err(ServerError::BadRequest(format!(
                    "Nested bundle '{}' is not supported inside bundle items",
                    item.catalog_ref
                )));
            }
        };
        let mut item_schema = ref_schema.as_object().cloned().unwrap_or_default();
        item_schema
            .entry("type".to_string())
            .or_insert_with(|| json!("object"));
        item_schema.insert(
            "title".to_string(),
            json!(format!("{} ({})", ref_title, item.id)),
        );
        props.insert(item.id.clone(), Value::Object(item_schema));
        required.push(json!(item.id));
    }
    Ok(json!({
        "type": "object",
        "required": required,
        "properties": props,
    }))
}

/// Request body for `PUT /api/v1/catalog/{kind}/{id}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCatalogItemRequest {
    pub name: String,
    pub category: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub runtime: Option<AppRuntime>,
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub chart: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub flagger_supported: Option<bool>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub pulumi_runtime: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub protect_by_default: Option<bool>,
    #[serde(default)]
    pub pulumi_yaml: Option<String>,
    #[serde(default)]
    pub manifests_yaml: Option<String>,
    #[serde(default)]
    pub default_engine: Option<String>,
    #[serde(default)]
    pub items: Option<Vec<CatalogBundleItem>>,
    #[serde(default)]
    pub schema: Option<Value>,
}

fn merge_optional_text(incoming: Option<String>, existing: Option<String>) -> Option<String> {
    match incoming {
        Some(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        None => existing,
    }
}

/// `PUT /api/v1/catalog/{kind}/{id}`
///
/// Updates an existing `CatalogApplication` (`kind: "app"`), `CatalogInfraBlueprint`
/// (`kind: "infra"` | `"infrastructure"`), or `CatalogBundle` (`kind: "bundle"`) on proposal
/// branch `keel/catalog/update-<id>-<ts>`, commits, pushes, and opens a PR via `ForgeProvider`.
pub async fn update_catalog_item(
    auth: AuthSession,
    State(state): State<AppState>,
    axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
    Json(req): Json<UpdateCatalogItemRequest>,
) -> Result<(StatusCode, Json<RegisterCatalogAppResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;
    let normalized_kind = match kind.as_str() {
        "app" => "app",
        "infra" | "infrastructure" => "infra",
        "bundle" => "bundle",
        other => {
            return Err(ServerError::BadRequest(format!(
                "Unsupported catalog kind '{other}'; expected 'app', 'infra', or 'bundle'"
            )));
        }
    };

    let rel_file = match normalized_kind {
        "app" => format!("catalog/apps/{id}/catalog.yaml"),
        "infra" => format!("catalog/infra/{id}/catalog.yaml"),
        "bundle" => format!("catalog/bundles/{id}/catalog.yaml"),
        _ => unreachable!(),
    };

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch("catalog", &format!("update-{id}"), timestamp)?;

    let (commit_sha, updated_app, updated_bp, updated_bundle, display_name) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

        let abs_file = state.repo_workdir.join(&rel_file);
        if !tokio::fs::try_exists(&abs_file).await.unwrap_or(false) {
            return Err(ServerError::NotFound(format!(
                "Catalog {normalized_kind} '{id}' not found"
            )));
        }
        let existing_yaml = tokio::fs::read_to_string(&abs_file).await?;

        let validated_manifests_yaml = if normalized_kind == "app"
            && let Some(raw_manifests) = req.manifests_yaml.filter(|s| !s.trim().is_empty())
        {
            validate_k8s_manifests_yaml(&raw_manifests)?;
            Some(if raw_manifests.ends_with('\n') {
                raw_manifests
            } else {
                format!("{raw_manifests}\n")
            })
        } else {
            None
        };

        let (catalog_yaml, app_out, bp_out, bundle_out, title_name) = match normalized_kind {
            "app" => {
                let mut app = parse_catalog_application(&existing_yaml)?;
                app.metadata.name = req.name.clone();
                app.metadata.category = req.category.clone();
                app.metadata.description =
                    merge_optional_text(req.description, app.metadata.description);
                if let Some(runtime) = req.runtime {
                    app.metadata.runtime = runtime;
                }
                if let Some(repo_url) = req.repo_url {
                    let trimmed = repo_url.trim();
                    if trimmed.is_empty() {
                        if app.metadata.runtime == AppRuntime::Manifest {
                            app.spec.upstream.repo_url =
                                "https://kubernetes.io/docs/concepts/overview/working-with-objects/"
                                    .to_string();
                        } else {
                            return Err(ServerError::BadRequest(
                                "repoUrl cannot be empty".to_string(),
                            ));
                        }
                    } else {
                        app.spec.upstream.repo_url = trimmed.to_string();
                    }
                } else if app.metadata.runtime == AppRuntime::Manifest
                    && app.spec.upstream.repo_url.trim().is_empty()
                {
                    app.spec.upstream.repo_url =
                        "https://kubernetes.io/docs/concepts/overview/working-with-objects/"
                            .to_string();
                }
                app.spec.upstream.chart = merge_optional_text(req.chart, app.spec.upstream.chart);
                app.spec.upstream.version =
                    merge_optional_text(req.version, app.spec.upstream.version);
                app.spec.upstream.path = merge_optional_text(req.path, app.spec.upstream.path);
                if app.metadata.runtime == AppRuntime::Manifest && app.spec.upstream.path.is_none()
                {
                    app.spec.upstream.path = Some("./manifests.yaml".to_string());
                }
                if let Some(raw_eng) = req
                    .default_engine
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    app.spec.default_engine = Some(raw_eng.parse::<ManifestEngine>()?);
                } else if app.metadata.runtime == AppRuntime::Manifest
                    && app.spec.default_engine.is_none()
                {
                    app.spec.default_engine = Some(ManifestEngine::Flux);
                }
                if let Some(flagger) = req.flagger_supported {
                    app.spec.progressive_delivery.flagger_supported = flagger;
                }
                if let Some(schema) = req.schema {
                    app.spec.schema = schema;
                }
                app.validate()?;
                let yaml =
                    serde_yml::to_string(&app).map_err(|e| ServerError::Internal(e.to_string()))?;
                let name = app.metadata.name.clone();
                let current_manifests = if let Some(ref updated_m) = validated_manifests_yaml {
                    Some(updated_m.clone())
                } else {
                    tokio::fs::read_to_string(
                        state
                            .repo_workdir
                            .join(format!("catalog/apps/{id}/manifests.yaml")),
                    )
                    .await
                    .ok()
                };
                (
                    yaml,
                    Some(CatalogItemDto::from_application(app, current_manifests)),
                    None,
                    None,
                    name,
                )
            }
            "infra" => {
                let mut bp = parse_catalog_blueprint(&existing_yaml)?;
                bp.metadata.name = req.name.clone();
                bp.metadata.category = req.category.clone();
                bp.metadata.description =
                    merge_optional_text(req.description, bp.metadata.description);
                if let Some(provider) = req.provider {
                    validate_dns_label(&provider, "provider")?;
                    bp.metadata.provider = provider;
                }
                if let Some(pulumi_runtime) = req.pulumi_runtime {
                    let trimmed = pulumi_runtime.trim();
                    if !trimmed.is_empty() {
                        bp.spec.pulumi_runtime = trimmed.to_string();
                    }
                }
                if let Some(entrypoint) = req.entrypoint {
                    let trimmed = entrypoint.trim();
                    if !trimmed.is_empty() {
                        bp.spec.entrypoint = trimmed.to_string();
                    }
                }
                if let Some(protect) = req.protect_by_default {
                    bp.spec.protect_by_default = protect;
                }
                if let Some(schema) = req.schema {
                    bp.spec.schema = schema;
                }
                bp.validate()?;
                let yaml =
                    serde_yml::to_string(&bp).map_err(|e| ServerError::Internal(e.to_string()))?;
                let name = bp.metadata.name.clone();
                (yaml, None, Some(bp), None, name)
            }
            "bundle" => {
                let mut bundle = parse_catalog_bundle(&existing_yaml)?;
                bundle.metadata.name = req.name.clone();
                bundle.metadata.category = req.category.clone();
                bundle.metadata.description =
                    merge_optional_text(req.description, bundle.metadata.description);
                let items_changed = if let Some(items) = req.items {
                    if items.is_empty() {
                        return Err(ServerError::BadRequest(
                            "bundle items must contain at least one item".to_string(),
                        ));
                    }
                    bundle.spec.items = items;
                    true
                } else {
                    false
                };
                if let Some(schema) = req.schema {
                    validate_bundle_items_canary(&state.repo_workdir, &bundle.spec.items).await?;
                    bundle.spec.schema = schema;
                } else if items_changed {
                    bundle.spec.schema =
                        compose_bundle_schema(&state.repo_workdir, &bundle.spec.items).await?;
                }
                bundle.validate()?;
                let yaml = serde_yml::to_string(&bundle)
                    .map_err(|e| ServerError::Internal(e.to_string()))?;
                let name = bundle.metadata.name.clone();
                (yaml, None, None, Some(bundle), name)
            }
            _ => unreachable!(),
        };

        let validated_pulumi_yaml = if normalized_kind == "infra"
            && let Some(raw_pulumi) = req.pulumi_yaml.filter(|s| !s.trim().is_empty())
        {
            serde_yml::from_str::<Value>(&raw_pulumi).map_err(|e| {
                ServerError::BadRequest(format!("Invalid Pulumi.yaml YAML syntax: {e}"))
            })?;
            Some(if raw_pulumi.ends_with('\n') {
                raw_pulumi
            } else {
                format!("{raw_pulumi}\n")
            })
        } else {
            None
        };

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;
        tokio::fs::write(&abs_file, &catalog_yaml).await?;
        let mut staged = vec![rel_file.clone()];
        if let Some(ref pulumi_content) = validated_pulumi_yaml {
            let rel_pulumi = format!("catalog/infra/{id}/Pulumi.yaml");
            let abs_pulumi = state.repo_workdir.join(&rel_pulumi);
            tokio::fs::write(&abs_pulumi, pulumi_content).await?;
            staged.push(rel_pulumi);
        }
        if let Some(ref manifests_content) = validated_manifests_yaml {
            let rel_manifests = format!("catalog/apps/{id}/manifests.yaml");
            let abs_manifests = state.repo_workdir.join(&rel_manifests);
            tokio::fs::write(&abs_manifests, manifests_content).await?;
            staged.push(rel_manifests);
        }
        state
            .git_runner
            .add(&state.repo_workdir, staged.iter().map(String::as_str))
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("feat(catalog): update {normalized_kind} {id}");
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;
        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;

        (sha, app_out, bp_out, bundle_out, title_name)
    };

    let pr_req = CreatePrRequest::new(
        format!("feat(catalog): update {normalized_kind} {id}"),
        format!(
            "Automated catalog update for `{id}` (`{display_name}`)\n\n- Commit: `{commit_sha}`\n- Path: `{rel_file}`"
        ),
        &branch,
    )
    .with_tenant("catalog")
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    Ok((
        StatusCode::OK,
        Json(RegisterCatalogAppResponse {
            application: updated_app,
            blueprint: updated_bp,
            bundle: updated_bundle,
            proposal,
            branch,
        }),
    ))
}

/// Request body for `POST /api/v1/catalog/fetch-schema`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchCatalogSchemaRequest {
    pub repo_url: String,
    pub chart: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Response body for `POST /api/v1/catalog/fetch-schema`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchCatalogSchemaResponse {
    pub schema: Value,
    pub source: String,
}

fn humanize_schema_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    let mut prev_lower = false;
    for ch in key.chars() {
        if ch == '_' || ch == '-' || ch == '.' {
            out.push(' ');
            prev_lower = false;
        } else if ch.is_uppercase() && prev_lower {
            out.push(' ');
            out.push(ch);
            prev_lower = false;
        } else {
            if out.is_empty() || out.ends_with(' ') {
                for up in ch.to_uppercase() {
                    out.push(up);
                }
            } else {
                out.push(ch);
            }
            prev_lower = ch.is_lowercase();
        }
    }
    out
}

fn yaml_value_to_json_schema(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            if map.is_empty() {
                return json!({
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                });
            }
            let mut props = serde_json::Map::new();
            for (k, v) in map {
                let mut child = if (k == "securityContext" || k == "podSecurityContext")
                    && v.as_object().map(|m| m.is_empty()).unwrap_or(false)
                {
                    if k == "podSecurityContext" {
                        json!({
                            "type": "object",
                            "properties": {
                                "runAsNonRoot": { "type": "boolean", "title": "Run As Non Root", "default": true },
                                "runAsUser": { "type": "integer", "title": "Run As User", "default": 1000 },
                                "seccompProfile": {
                                    "type": "object",
                                    "title": "Seccomp Profile",
                                    "properties": {
                                        "type": { "type": "string", "title": "Type", "default": "RuntimeDefault" }
                                    }
                                }
                            }
                        })
                    } else {
                        json!({
                            "type": "object",
                            "properties": {
                                "allowPrivilegeEscalation": { "type": "boolean", "title": "Allow Privilege Escalation", "default": false },
                                "runAsNonRoot": { "type": "boolean", "title": "Run As Non Root", "default": true },
                                "runAsUser": { "type": "integer", "title": "Run As User", "default": 1000 },
                                "readOnlyRootFilesystem": { "type": "boolean", "title": "Read Only Root Filesystem", "default": false },
                                "capabilities": {
                                    "type": "object",
                                    "title": "Capabilities",
                                    "properties": {
                                        "drop": {
                                            "type": "array",
                                            "title": "Drop",
                                            "items": { "type": "string" },
                                            "default": ["ALL"]
                                        }
                                    }
                                },
                                "seccompProfile": {
                                    "type": "object",
                                    "title": "Seccomp Profile",
                                    "properties": {
                                        "type": { "type": "string", "title": "Type", "default": "RuntimeDefault" }
                                    }
                                }
                            }
                        })
                    }
                } else {
                    yaml_value_to_json_schema(v)
                };
                if let Some(obj) = child.as_object_mut() {
                    obj.entry("title".to_string())
                        .or_insert_with(|| Value::String(humanize_schema_key(k)));
                }
                props.insert(k.clone(), child);
            }
            json!({
                "type": "object",
                "properties": props
            })
        }
        Value::Array(items) => {
            let item_schema = items
                .first()
                .map(yaml_value_to_json_schema)
                .unwrap_or_else(|| json!({ "type": "string" }));
            json!({
                "type": "array",
                "items": item_schema,
                "default": items
            })
        }
        Value::Bool(b) => json!({ "type": "boolean", "default": b }),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                json!({ "type": "integer", "default": n })
            } else {
                json!({ "type": "number", "default": n })
            }
        }
        Value::String(s) => json!({ "type": "string", "default": s }),
        Value::Null => json!({ "type": "string" }),
    }
}

fn is_disallowed_ssrf_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

async fn validate_upstream_helm_repo_url(repo_url: &str) -> Result<url::Url, ServerError> {
    let parsed = url::Url::parse(repo_url).map_err(|_| {
        ServerError::BadRequest(
            "repoUrl must be a valid http://, https://, or oci:// URL".to_string(),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https" | "oci") {
        return Err(ServerError::BadRequest(
            "repoUrl must start with http://, https://, or oci://".to_string(),
        ));
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(ServerError::BadRequest(
            "repoUrl must not contain embedded credentials".to_string(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| ServerError::BadRequest("repoUrl must include a valid host".to_string()))?;
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".cluster.local")
    {
        return Err(ServerError::BadRequest(
            "repoUrl host is not permitted (private/cluster-internal hosts are blocked)"
                .to_string(),
        ));
    }
    if let Ok(ip) = host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::IpAddr>()
        && is_disallowed_ssrf_ip(ip)
    {
        return Err(ServerError::BadRequest(
            "repoUrl IP address is not permitted (private/loopback/link-local ranges are blocked)"
                .to_string(),
        ));
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    if let Ok(addrs) = tokio::net::lookup_host((host, port)).await {
        for addr in addrs {
            if is_disallowed_ssrf_ip(addr.ip()) {
                return Err(ServerError::BadRequest(
                    "repoUrl resolves to a disallowed private/loopback/link-local IP".to_string(),
                ));
            }
        }
    }
    Ok(parsed)
}

async fn fetch_upstream_helm_schema(
    repo_url: &str,
    chart: &str,
    version: Option<&str>,
) -> Result<(Value, String), ServerError> {
    const MAX_SCHEMA_BYTES: u64 = 2 * 1024 * 1024;
    let repo_url = repo_url.trim();
    let chart = chart.trim();
    let _validated_url = validate_upstream_helm_repo_url(repo_url).await?;
    if chart.is_empty()
        || chart.starts_with('-')
        || chart.contains("..")
        || chart.contains('/')
        || chart.contains('\\')
        || chart.contains('\0')
        || chart.chars().any(char::is_whitespace)
    {
        return Err(ServerError::BadRequest(
            "invalid Helm chart name".to_string(),
        ));
    }
    if let Some(v) = version {
        let v = v.trim();
        if v.starts_with('-') || v.contains('\0') || v.chars().any(char::is_whitespace) {
            return Err(ServerError::BadRequest(
                "invalid Helm chart version".to_string(),
            ));
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!("keel-helm-schema-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp_path = tmp_dir.to_string_lossy().to_string();

    let mut args = vec!["pull".to_string()];
    if repo_url.starts_with("oci://") {
        args.push(format!("{}/{}", repo_url.trim_end_matches('/'), chart));
    } else {
        args.push(chart.to_string());
        args.push("--repo".to_string());
        args.push(repo_url.to_string());
    }
    if let Some(v) = version.map(str::trim).filter(|v| !v.is_empty()) {
        args.push("--version".to_string());
        args.push(v.to_string());
    }
    args.push("--untar".to_string());
    args.push("--untardir".to_string());
    args.push(tmp_path);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("helm").args(&args).output(),
    )
    .await
    .map_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        ServerError::Internal("helm pull timed out after 30s".to_string())
    })?
    .map_err(|e| ServerError::Internal(format!("failed to invoke helm CLI: {e}")))?;

    if !output.status.success() {
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Internal(format!(
            "helm pull failed: {}",
            stderr.trim()
        )));
    }

    let chart_dir = tmp_dir.join(chart);
    let schema_json_path = chart_dir.join("values.schema.json");
    if tokio::fs::try_exists(&schema_json_path)
        .await
        .unwrap_or(false)
    {
        let meta = tokio::fs::metadata(&schema_json_path).await?;
        if meta.len() > MAX_SCHEMA_BYTES {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            return Err(ServerError::BadRequest(
                "upstream values.schema.json exceeds 2 MiB size limit".to_string(),
            ));
        }
        let raw = tokio::fs::read_to_string(&schema_json_path).await?;
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
            ServerError::Internal(format!("invalid upstream values.schema.json: {e}"))
        })?;
        return Ok((parsed, "values.schema.json".to_string()));
    }

    let values_yaml_path = chart_dir.join("values.yaml");
    if tokio::fs::try_exists(&values_yaml_path)
        .await
        .unwrap_or(false)
    {
        let meta = tokio::fs::metadata(&values_yaml_path).await?;
        if meta.len() > MAX_SCHEMA_BYTES {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            return Err(ServerError::BadRequest(
                "upstream values.yaml exceeds 2 MiB size limit".to_string(),
            ));
        }
        let raw = tokio::fs::read_to_string(&values_yaml_path).await?;
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        let parsed: Value = serde_yml::from_str(&raw)
            .map_err(|e| ServerError::Internal(format!("invalid upstream values.yaml: {e}")))?;
        return Ok((
            yaml_value_to_json_schema(&parsed),
            "values.yaml".to_string(),
        ));
    }

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    Err(ServerError::NotFound(format!(
        "chart '{chart}' contains neither values.schema.json nor values.yaml"
    )))
}

/// `POST /api/v1/catalog/fetch-schema`
///
/// Pulls the upstream Helm chart and returns its `values.schema.json` (or a JSON Schema inferred
/// from `values.yaml`) so Catalog forms mirror upstream chart configuration options.
pub async fn fetch_catalog_schema(
    _auth: AuthSession,
    Json(req): Json<FetchCatalogSchemaRequest>,
) -> Result<Json<FetchCatalogSchemaResponse>, ServerError> {
    let (schema, source) =
        fetch_upstream_helm_schema(&req.repo_url, &req.chart, req.version.as_deref()).await?;
    Ok(Json(FetchCatalogSchemaResponse { schema, source }))
}

/// Summary of a tenant directory returned by `GET /api/v1/tenants`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantSummary {
    pub id: String,
    pub namespace: String,
}

/// Response body for `GET /api/v1/tenants`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantsResponse {
    pub tenants: Vec<TenantSummary>,
}

/// `GET /api/v1/tenants`
///
/// Scans `tenants/*` directories in `state.repo_workdir`, returning deterministic sorted order.
pub async fn list_tenants(
    auth: AuthSession,
    State(state): State<AppState>,
) -> Result<Json<TenantsResponse>, ServerError> {
    let mut tenants = Vec::new();
    let tenants_dir = state.repo_workdir.join("tenants");
    if let Ok(mut entries) = tokio::fs::read_dir(&tenants_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !auth.is_admin() && auth.require_tenant_access(&id).is_err() {
                continue;
            }
            let namespace = tenant_namespace(&id).unwrap_or_else(|_| format!("tenant-{id}"));
            tenants.push(TenantSummary { id, namespace });
        }
    }
    tenants.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(TenantsResponse { tenants }))
}

/// Request body for `POST /api/v1/tenants`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTenantRequest {
    pub id: String,
}

/// Response body for `POST /api/v1/tenants`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTenantResponse {
    pub tenant: TenantSummary,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

/// `POST /api/v1/tenants`
///
/// Validates DNS-1123 `id`, rejects duplicates, synthesizes `tenants/<id>/rbac.yaml`,
/// optionally generates & stores a SOPS `age` keypair, commits on `keel/<id>/create-tenant-<ts>`,
/// pushes, opens a PR via `ForgeProvider`, and publishes a `TelemetryMux` event.
pub async fn create_tenant(
    auth: AuthSession,
    State(state): State<AppState>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<(StatusCode, Json<CreateTenantResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&req.id, "id")?;
    let namespace = tenant_namespace(&req.id)?;

    let rel_dir = format!("tenants/{}", req.id);
    let tenant_dir = state.repo_workdir.join(&rel_dir);
    if tokio::fs::try_exists(&tenant_dir).await.unwrap_or(false) {
        return Err(ServerError::Conflict(format!(
            "Tenant '{}' already exists",
            req.id
        )));
    }

    let rbac_yaml = TenantManifestBundle::synthesize(&TenantIsolationConfig::new(&req.id))?
        .to_multi_doc_yaml()?;

    if let Ok(keypair) = SopsManager::new().generate_age_keypair().await {
        let age_key = TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
        state
            .secret_store
            .store_tenant_age_key(&req.id, &age_key)
            .await?;
    }

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch(&req.id, "create-tenant", timestamp)?;
    let rel_file = format!("{rel_dir}/rbac.yaml");

    let commit_sha = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        let target_dir = state.repo_workdir.join(&rel_dir);
        tokio::fs::create_dir_all(&target_dir).await?;
        tokio::fs::write(target_dir.join("rbac.yaml"), &rbac_yaml).await?;

        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        let existing_kustomization = if tokio::fs::try_exists(&tenants_kustomization_path)
            .await
            .unwrap_or(false)
        {
            tokio::fs::read_to_string(&tenants_kustomization_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut resources: Vec<String> =
            serde_yml::from_str::<serde_json::Value>(&existing_kustomization)
                .ok()
                .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        let rbac_entry = format!("{}/rbac.yaml", req.id);
        if !resources.contains(&rbac_entry) {
            resources.push(rbac_entry);
        }
        tokio::fs::write(
            &tenants_kustomization_path,
            serialize_tenants_kustomization(&resources)?,
        )
        .await?;

        state
            .git_runner
            .add(
                &state.repo_workdir,
                [&rel_file, "tenants/kustomization.yaml"],
            )
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("feat({}): create tenant {}", req.id, req.id);
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        sha
    };

    let pr_req = CreatePrRequest::new(
        format!("feat({}): create tenant {}", req.id, req.id),
        format!(
            "Automated tenant onboarding for `{}` (namespace `{}`)\n\n- Commit: `{}`\n- Path: `{}`",
            req.id, namespace, commit_sha, rel_file
        ),
        &branch,
    )
    .with_tenant(&req.id)
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    state.telemetry.publish(CrdWatchEvent::new(
        WatchedCrdKind::Kustomization,
        &namespace,
        format!("tenant-{}", req.id),
        CrdReconcileStatus::Progressing,
        format!(
            "Proposed tenant '{}' creation in PR #{} ({})",
            req.id, proposal.id, branch
        ),
        Some(commit_sha),
    ));

    Ok((
        StatusCode::CREATED,
        Json(CreateTenantResponse {
            tenant: TenantSummary {
                id: req.id,
                namespace,
            },
            proposal,
            branch,
        }),
    ))
}

/// Response body for `DELETE /api/v1/tenants/{id}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTenantResponse {
    pub id: String,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

/// `DELETE /api/v1/tenants/{id}`
///
/// Rejects deleting `"default"`, validates `id`, ensures `tenants/<id>` exists,
/// removes `tenants/<id>` on proposal branch `keel/<id>/delete-tenant-<ts>`,
/// commits, pushes, and opens a PR via `ForgeProvider`.
pub async fn delete_tenant(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DeleteTenantResponse>, ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;
    if id == "default" {
        return Err(ServerError::BadRequest(
            "Cannot delete the 'default' tenant".to_string(),
        ));
    }
    validate_dns_label(&id, "id")?;

    let rel_dir = format!("tenants/{id}");
    let tenant_dir = state.repo_workdir.join(&rel_dir);
    if !tokio::fs::try_exists(&tenant_dir).await.unwrap_or(false) {
        return Err(ServerError::NotFound(format!("Tenant '{id}' not found")));
    }

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch(&id, "delete-tenant", timestamp)?;

    let commit_sha = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        tokio::fs::remove_dir_all(state.repo_workdir.join(&rel_dir)).await?;
        let mut staged_paths = vec![rel_dir.clone()];

        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        if tokio::fs::try_exists(&tenants_kustomization_path)
            .await
            .unwrap_or(false)
        {
            let existing = tokio::fs::read_to_string(&tenants_kustomization_path)
                .await
                .unwrap_or_default();
            let resources: Vec<String> = serde_yml::from_str::<Value>(&existing)
                .ok()
                .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .filter(|r| !r.starts_with(&format!("{id}/")))
                .collect();
            tokio::fs::write(
                &tenants_kustomization_path,
                serialize_tenants_kustomization(&resources)?,
            )
            .await?;
            staged_paths.push("tenants/kustomization.yaml".to_string());
        }

        state
            .git_runner
            .add(&state.repo_workdir, staged_paths)
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("feat({id}): delete tenant {id}");
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        sha
    };

    let pr_req = CreatePrRequest::new(
        format!("feat({id}): delete tenant {id}"),
        format!(
            "Automated tenant decommission proposal for `{id}`\n\n- Commit: `{commit_sha}`\n- Removed: `{rel_dir}`"
        ),
        &branch,
    )
    .with_tenant(&id)
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    Ok(Json(DeleteTenantResponse {
        id,
        proposal,
        branch,
    }))
}

/// Query parameters for `GET /api/v1/proposals`.
#[derive(Debug, Deserialize)]
pub struct ListProposalsQuery {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Response body for `GET /api/v1/proposals`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalsListResponse {
    pub proposals: Vec<PullRequestInfo>,
}

/// `GET /api/v1/proposals`
pub async fn list_proposals(
    _auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListProposalsQuery>,
) -> Result<Json<ProposalsListResponse>, ServerError> {
    let pr_state = match query.status.as_deref() {
        None | Some("open") => Some(PullRequestState::Open),
        Some("merged") => Some(PullRequestState::Merged),
        Some("closed") => Some(PullRequestState::Closed),
        Some("all") => None,
        Some(other) => {
            return Err(ServerError::BadRequest(format!(
                "Invalid proposal status filter '{other}', expected open|merged|closed|all"
            )));
        }
    };
    let proposals = state
        .forge
        .list_pull_requests(query.tenant.as_deref(), pr_state)
        .await?;
    Ok(Json(ProposalsListResponse { proposals }))
}

/// Request body for `POST /api/v1/proposals`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalRequest {
    pub tenant: String,
    pub catalog_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub values: Value,
    #[serde(default)]
    pub canary: bool,
    #[serde(default)]
    pub target_cluster: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub target_namespace: Option<String>,
    #[serde(default, alias = "action_engine")]
    pub action_engine: Option<String>,
}

/// Response body for `POST /api/v1/proposals`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProposalResponse {
    pub proposal: PullRequestInfo,
    pub branch: String,
    pub preview_stack: PkoStack,
    pub report: PreviewReport,
}

async fn read_pulumi_project_name(pulumi_yaml_path: &std::path::Path, fallback: &str) -> String {
    let Ok(raw) = tokio::fs::read_to_string(pulumi_yaml_path).await else {
        return fallback.to_string();
    };
    let Ok(doc) = serde_yml::from_str::<serde_json::Value>(&raw) else {
        return fallback.to_string();
    };
    doc.get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn populate_stack_config_from_values(
    config: &mut std::collections::BTreeMap<String, String>,
    values: &Value,
) {
    let Some(obj) = values.as_object() else {
        return;
    };
    for (key, val) in obj {
        let rendered = match val {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(_) | Value::Object(_) => val.to_string(),
        };
        config.insert(key.clone(), rendered);
    }
}

fn build_infra_secret_manifest_yaml(
    secret_name: &str,
    namespace: &str,
    secret_values: &Value,
) -> Result<(String, Vec<String>), ServerError> {
    let mut string_data = serde_json::Map::new();
    let mut keys = Vec::new();
    if let Some(obj) = secret_values.as_object() {
        for (key, val) in obj {
            let rendered = match val {
                Value::Null => continue,
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Array(_) | Value::Object(_) => val.to_string(),
            };
            keys.push(key.clone());
            string_data.insert(key.clone(), Value::String(rendered));
        }
    }
    let secret_doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": secret_name,
            "namespace": namespace,
            "annotations": {
                "kustomize.toolkit.fluxcd.io/prune": "disabled"
            },
            "labels": {
                "app.kubernetes.io/managed-by": "keel"
            }
        },
        "type": "Opaque",
        "stringData": string_data,
    });
    let yaml = serde_yml::to_string(&secret_doc)
        .map_err(|e| ServerError::Internal(format!("failed to serialize secret manifest: {e}")))?;
    Ok((yaml, keys))
}

fn build_values_configmap_manifest_yaml(
    configmap_name: &str,
    namespace: &str,
    tenant: &str,
    catalog_id: &str,
    normal_values: &Value,
) -> Result<String, ServerError> {
    let mut data_map = serde_json::Map::new();
    if let Some(obj) = normal_values.as_object() {
        for (key, val) in obj {
            let rendered = match val {
                Value::Null => continue,
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Array(_) | Value::Object(_) => val.to_string(),
            };
            data_map.insert(key.clone(), Value::String(rendered));
        }
    }
    let cm_doc = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": configmap_name,
            "namespace": namespace,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/tenant": tenant,
                "keel.io/catalog-id": catalog_id
            }
        },
        "data": data_map,
    });
    serde_yml::to_string(&cm_doc)
        .map_err(|e| ServerError::Internal(format!("failed to serialize values ConfigMap: {e}")))
}

async fn load_catalog_app_raw_manifests(repo_workdir: &Path, app: &CatalogApplication) -> String {
    let app_dir = repo_workdir.join("catalog/apps").join(&app.metadata.id);
    let default_path = app_dir.join("manifests.yaml");
    if let Ok(content) = tokio::fs::read_to_string(&default_path).await {
        return content;
    }
    if let Some(rel_path) = app.spec.upstream.path.as_deref()
        && let Some(resolved) = safe_relative_path(&app_dir, rel_path)
        && let Ok(content) = tokio::fs::read_to_string(&resolved).await
    {
        return content;
    }
    format!(
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {}\ndata: {{}}\n",
        app.metadata.id
    )
}

/// `POST /api/v1/proposals`
///
/// Validates payload against `catalog.yaml` JSONSchema, splits secrets (`writeOnly`/`format: password`),
/// writes tenant manifests (`tenants/<tenant>/...`), commits on `keel/<tenant>/<action>-<ts>`,
/// pushes, opens PR via `ForgeProvider`, synthesizes ephemeral PKO preview CRs, and publishes an SSE event.
pub async fn create_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    Json(req): Json<CreateProposalRequest>,
) -> Result<(StatusCode, Json<CreateProposalResponse>), ServerError> {
    validate_dns_label(&req.tenant, "tenant")?;
    auth.require_tenant_access(&req.tenant)?;
    validate_dns_label(&req.catalog_id, "catalog_id")?;

    let instance_name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&req.catalog_id)
        .to_string();
    validate_dns_label(&instance_name, "name")?;

    let manifest =
        load_catalog_manifest(&state.repo_workdir, &req.catalog_id, req.kind.as_deref()).await?;

    if let CatalogManifest::Bundle(bundle) = manifest {
        return create_bundle_proposal(auth, state, req, *bundle).await;
    }

    let parsed_action_engine = match req
        .action_engine
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw_eng) => Some(raw_eng.parse::<ManifestEngine>()?),
        None => None,
    };

    let (schema, action_label, is_app, manifest_engine) = match &manifest {
        CatalogManifest::Application(app) => {
            if req.canary && !app.spec.progressive_delivery.flagger_supported {
                return Err(ServerError::BadRequest(format!(
                    "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
                    app.metadata.id
                )));
            }
            let engine = if app.metadata.runtime == AppRuntime::Manifest {
                Some(
                    parsed_action_engine
                        .or(app.spec.default_engine)
                        .unwrap_or(ManifestEngine::Flux),
                )
            } else {
                None
            };
            (
                &app.spec.schema,
                format!("app-{}", app.metadata.id),
                true,
                engine,
            )
        }
        CatalogManifest::InfraBlueprint(bp) => (
            &bp.spec.schema,
            format!("infra-{}", bp.metadata.id),
            false,
            None,
        ),
        CatalogManifest::Bundle(_) => unreachable!("handled above"),
    };

    validate_dns_label(&action_label, "action")?;
    validate_schema_values(schema, &req.values)?;
    let partitioned = partition_values_by_schema(schema, &req.values)?;
    let values_yaml = render_helm_values_yaml(&partitioned.normal_values)?;
    let plaintext_secrets_yaml = match partitioned.secret_values.as_object() {
        Some(obj) if !obj.is_empty() => Some(render_helm_values_yaml(&partitioned.secret_values)?),
        _ => None,
    };

    let tenant_ns = tenant_namespace(&req.tenant)?;
    let infra_secret_name = format!("{instance_name}-secrets");

    // Encrypt extracted secret fields via SOPS + age before touching Git (`SDD.md` §6.3)
    let (encrypted_secrets_yaml, encrypted_infra_k8s_secret_yaml, infra_secret_keys) = if let Some(
        plaintext_secrets,
    ) =
        &plaintext_secrets_yaml
    {
        let age_key = match state.secret_store.get_tenant_age_key(&req.tenant).await? {
            Some(existing) => existing,
            None => {
                let keypair = SopsManager::new()
                        .generate_age_keypair()
                        .await
                        .map_err(|_| {
                            ServerError::BadRequest(format!(
                                "Tenant '{}' has no SOPS age key configured; cannot encrypt secret fields",
                                req.tenant
                            ))
                        })?;
                let generated = TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
                state
                    .secret_store
                    .store_tenant_age_key(&req.tenant, &generated)
                    .await?;
                generated
            }
        };
        let sops = SopsManager::new();
        let sops_recipients =
            resolve_sops_recipients(&state.repo_workdir, &age_key.public_key).await;
        let enc_values = sops
            .encrypt_yaml(&sops_recipients, plaintext_secrets)
            .await?;
        let (enc_k8s_secret, keys) = if !is_app || manifest_engine.is_some() {
            let (plain_k8s_secret, keys) = build_infra_secret_manifest_yaml(
                &infra_secret_name,
                &tenant_ns,
                &partitioned.secret_values,
            )?;
            let enc_k8s = sops
                .encrypt_yaml(&sops_recipients, &plain_k8s_secret)
                .await?;
            (Some(enc_k8s), keys)
        } else {
            (None, Vec::new())
        };
        (Some(enc_values), enc_k8s_secret, keys)
    } else {
        (None, None, Vec::new())
    };

    let timestamp = next_unique_branch_timestamp();
    let branch = build_preview_branch(&req.tenant, &action_label, timestamp)?;

    let subdir = if is_app { "apps" } else { "infra" };
    let resolved_dir =
        resolve_tenant_resource_dir(&state.repo_workdir, &req.tenant, subdir, &instance_name).await;
    let bundle_item = resolved_dir.as_ref().and_then(|r| r.bundle_item.clone());

    let mut rendered_manifest_template_yaml: Option<String> = None;
    let mut synthesized_manifest_pulumi_yaml: Option<String> = None;
    let mut values_configmap_yaml: Option<String> = None;

    let (
        rel_dir,
        manifest_yaml,
        preview_resource_type,
        remote_app_kube_config,
        remote_app_target_ns,
    ) = match &manifest {
        CatalogManifest::Application(app) if manifest_engine.is_some() => {
            let rel_dir = resolved_dir
                .map(|r| r.rel_dir)
                .unwrap_or_else(|| format!("tenants/{}/apps/{instance_name}", req.tenant));
            let raw_manifests = load_catalog_app_raw_manifests(&state.repo_workdir, app).await;
            let mut rendered = render_manifest_template(&raw_manifests, &partitioned.normal_values);
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            rendered_manifest_template_yaml = Some(rendered.clone());

            let remote_kc = req
                .target_cluster
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "local")
                .map(|t| RemoteKubeConfigRef::with_key(t, "kubeconfig"));
            let remote_ns = if let Some(target_ns) = req
                .target_namespace
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                validate_dns_label(target_ns, "target_namespace")?;
                if remote_kc.is_none() && target_ns != tenant_ns {
                    return Err(ServerError::BadRequest(format!(
                        "targetNamespace '{target_ns}' requires a remote targetCluster; management cluster apps are confined to '{tenant_ns}'"
                    )));
                }
                Some(target_ns.to_string())
            } else {
                None
            };

            match manifest_engine.unwrap() {
                ManifestEngine::Flux => {
                    values_configmap_yaml = Some(build_values_configmap_manifest_yaml(
                        &format!("{instance_name}-values"),
                        &tenant_ns,
                        &req.tenant,
                        &req.catalog_id,
                        &partitioned.normal_values,
                    )?);
                    let mut fk_input = FluxKustomizationSpecInput::new(
                        &req.tenant,
                        &instance_name,
                        "keel-gitops",
                        format!("./{rel_dir}"),
                    );
                    fk_input
                        .extra_labels
                        .insert("keel.io/runtime".to_string(), "manifest".to_string());
                    fk_input
                        .extra_labels
                        .insert("keel.io/engine".to_string(), "flux".to_string());
                    fk_input
                        .extra_labels
                        .insert("keel.io/catalog-id".to_string(), req.catalog_id.clone());
                    if let Some((ref bundle_name, ref item_id)) = bundle_item {
                        fk_input
                            .extra_labels
                            .insert("keel.io/bundle".to_string(), bundle_name.clone());
                        fk_input
                            .extra_labels
                            .insert("keel.io/bundle-item".to_string(), item_id.clone());
                    }
                    fk_input.kube_config = remote_kc.clone();
                    fk_input.target_namespace = remote_ns.clone();
                    fk_input.post_build_substitute_from = vec![
                        FluxSubstituteReference {
                            kind: "ConfigMap".to_string(),
                            name: format!("{instance_name}-values"),
                            optional: true,
                        },
                        FluxSubstituteReference {
                            kind: "Secret".to_string(),
                            name: format!("{instance_name}-secrets"),
                            optional: true,
                        },
                    ];
                    let fk_yaml = generate_flux_kustomization_manifest(&fk_input)?;
                    (
                        rel_dir,
                        fk_yaml,
                        "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization",
                        remote_kc,
                        remote_ns,
                    )
                }
                ManifestEngine::Pulumi => {
                    let mut schema_props: Vec<String> = app
                        .spec
                        .schema
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    if let Some(obj) = partitioned.normal_values.as_object() {
                        for k in obj.keys() {
                            if !schema_props.contains(k) {
                                schema_props.push(k.clone());
                            }
                        }
                    }
                    schema_props.sort();

                    let pulumi_yaml = synthesize_k8s_manifest_pulumi_yaml(
                        &instance_name,
                        &rendered,
                        &schema_props,
                        remote_kc.is_some(),
                    )?;
                    synthesized_manifest_pulumi_yaml = Some(pulumi_yaml);

                    let mut stack_params = TenantStackParams::new(
                        &req.tenant,
                        &instance_name,
                        format!(
                            "organization/{instance_name}/{}-{instance_name}",
                            req.tenant
                        ),
                        &state.backend_url,
                        "keel-gitops",
                        &rel_dir,
                    );
                    stack_params
                        .config
                        .insert("keel:provider".to_string(), "kubernetes".to_string());
                    populate_stack_config_from_values(
                        &mut stack_params.config,
                        &partitioned.normal_values,
                    );
                    if let Some(ref kc) = remote_kc {
                        stack_params
                            .config
                            .insert("kubeconfigSecretName".to_string(), kc.secret_name.clone());
                        stack_params
                            .config
                            .insert("kubeconfigSecretNamespace".to_string(), tenant_ns.clone());
                    }
                    attach_infra_secret_refs(
                        &mut stack_params.env_refs,
                        &mut stack_params.secrets_ref,
                        &infra_secret_name,
                        &infra_secret_keys,
                    );
                    if let Some(ref s3_secret) = state.s3_credentials_secret {
                        let _ = state
                            .secret_store
                            .ensure_s3_credentials_in_namespace(&tenant_ns)
                            .await;
                        attach_s3_credentials_env_refs(&mut stack_params.env_refs, s3_secret);
                    }
                    if matches!(
                        state
                            .secret_store
                            .ensure_github_token_in_namespace(&tenant_ns)
                            .await,
                        Ok(true)
                    ) {
                        attach_github_token_env_ref(
                            &mut stack_params.env_refs,
                            GITHUB_TOKEN_SECRET_NAME,
                        );
                    }
                    let mut stack = synthesize_tenant_stack(&stack_params)?;
                    stack
                        .metadata
                        .labels
                        .insert("keel.io/runtime".to_string(), "manifest".to_string());
                    stack
                        .metadata
                        .labels
                        .insert("keel.io/engine".to_string(), "pulumi".to_string());
                    stack
                        .metadata
                        .labels
                        .insert("keel.io/catalog-id".to_string(), req.catalog_id.clone());
                    let stack_yaml = stack.to_yaml()?;
                    (
                        rel_dir,
                        stack_yaml,
                        "kubernetes:yaml/v2:ConfigGroup",
                        remote_kc,
                        remote_ns,
                    )
                }
            }
        }
        CatalogManifest::Application(app) => {
            let rel_dir = resolved_dir
                .map(|r| r.rel_dir)
                .unwrap_or_else(|| format!("tenants/{}/apps/{instance_name}", req.tenant));
            let mut hr_input = HelmReleaseSpecInput::new(
                &req.tenant,
                &instance_name,
                app.spec
                    .upstream
                    .chart
                    .clone()
                    .unwrap_or_else(|| app.metadata.id.clone()),
                HelmChartSourceRef {
                    kind: HelmChartSourceKind::HelmRepository,
                    name: format!("{instance_name}-repo"),
                    namespace: Some(tenant_ns.clone()),
                },
            );
            if let Some((ref bundle_name, ref item_id)) = bundle_item {
                hr_input
                    .extra_labels
                    .insert("keel.io/bundle".to_string(), bundle_name.clone());
                hr_input
                    .extra_labels
                    .insert("keel.io/bundle-item".to_string(), item_id.clone());

                // Preserve bundle-level dependsOn and valuesFrom bindings from existing HelmRelease
                let abs_dir = state.repo_workdir.join(&rel_dir);
                let existing_hr_path =
                    if tokio::fs::try_exists(abs_dir.join("helmrelease-canary.yaml"))
                        .await
                        .unwrap_or(false)
                    {
                        Some(abs_dir.join("helmrelease-canary.yaml"))
                    } else if tokio::fs::try_exists(abs_dir.join("helmrelease.yaml"))
                        .await
                        .unwrap_or(false)
                    {
                        Some(abs_dir.join("helmrelease.yaml"))
                    } else {
                        None
                    };
                if let Some(hr_path) = existing_hr_path
                    && let Ok(raw) = tokio::fs::read_to_string(&hr_path).await
                    && let Ok(doc) = serde_yml::from_str::<Value>(&raw)
                {
                    if let Some(deps) = doc.pointer("/spec/dependsOn").and_then(Value::as_array) {
                        for dep in deps {
                            if let Some(dep_name) = dep.get("name").and_then(Value::as_str) {
                                hr_input.depends_on.push(FluxDependencyRef {
                                    name: dep_name.to_string(),
                                });
                            }
                        }
                    }
                    if let Some(vals_from) =
                        doc.pointer("/spec/valuesFrom").and_then(Value::as_array)
                    {
                        for vf in vals_from {
                            if vf.get("kind").and_then(Value::as_str) == Some("Secret")
                                && let Some(sec_name) = vf.get("name").and_then(Value::as_str)
                            {
                                hr_input.values_from_secrets.push(HelmValuesSecretRef {
                                    name: sec_name.to_string(),
                                    values_key: vf
                                        .get("valuesKey")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    target_path: vf
                                        .get("targetPath")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    optional: vf.get("optional").and_then(Value::as_bool),
                                });
                            }
                        }
                    }
                }
            }
            hr_input.chart_version = req
                .version
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .or_else(|| app.spec.upstream.version.clone());
            if let Some(target) = req
                .target_cluster
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "local")
            {
                hr_input.kube_config = Some(RemoteKubeConfigRef::with_key(target, "kubeconfig"));
            }
            if let Some(target_ns) = req
                .target_namespace
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                validate_dns_label(target_ns, "target_namespace")?;
                if hr_input.kube_config.is_none() && target_ns != tenant_ns {
                    return Err(ServerError::BadRequest(format!(
                        "targetNamespace '{target_ns}' requires a remote targetCluster; management cluster apps are confined to '{tenant_ns}'"
                    )));
                }
                hr_input.target_namespace = Some(target_ns.to_string());
            }
            if let Some(obj) = partitioned.normal_values.as_object()
                && !obj.is_empty()
            {
                hr_input.values = Some(partitioned.normal_values.clone());
            }
            let hr_yaml = synthesize_helm_release_yaml(&hr_input)?;
            (
                rel_dir,
                hr_yaml,
                "helm:helm.toolkit.fluxcd.io/v2:HelmRelease",
                hr_input.kube_config,
                hr_input.target_namespace,
            )
        }
        CatalogManifest::InfraBlueprint(bp) => {
            let rel_dir = resolved_dir
                .map(|r| r.rel_dir)
                .unwrap_or_else(|| format!("tenants/{}/infra/{instance_name}", req.tenant));
            let blueprint_pulumi_path = state
                .repo_workdir
                .join("catalog/infra")
                .join(&req.catalog_id)
                .join("Pulumi.yaml");
            let pulumi_project =
                read_pulumi_project_name(&blueprint_pulumi_path, &bp.metadata.name).await;
            let mut stack_params = TenantStackParams::new(
                &req.tenant,
                &instance_name,
                format!(
                    "organization/{pulumi_project}/{}-{instance_name}",
                    req.tenant
                ),
                &state.backend_url,
                "keel-gitops",
                &rel_dir,
            );
            stack_params
                .config
                .insert("keel:provider".to_string(), bp.metadata.provider.clone());
            populate_stack_config_from_values(&mut stack_params.config, &partitioned.normal_values);
            if bp.metadata.provider == "aws"
                && let Some(region) = stack_params.config.get("region").cloned()
            {
                stack_params
                    .config
                    .entry("aws:region".to_string())
                    .or_insert(region);
            }
            if bp.metadata.provider == "gcp"
                && let Some(project) = stack_params.config.get("project").cloned()
            {
                stack_params
                    .config
                    .entry("gcp:project".to_string())
                    .or_insert(project);
            }
            if bp.metadata.provider == "azure"
                && let Some(location) = stack_params.config.get("location").cloned()
            {
                stack_params
                    .config
                    .entry("azure-native:location".to_string())
                    .or_insert(location);
            }
            attach_infra_secret_refs(
                &mut stack_params.env_refs,
                &mut stack_params.secrets_ref,
                &infra_secret_name,
                &infra_secret_keys,
            );
            let tenant_ns = tenant_namespace(&req.tenant)?;
            if let Some(ref s3_secret) = state.s3_credentials_secret {
                let _ = state
                    .secret_store
                    .ensure_s3_credentials_in_namespace(&tenant_ns)
                    .await;
                attach_s3_credentials_env_refs(&mut stack_params.env_refs, s3_secret);
            }
            if matches!(
                state
                    .secret_store
                    .ensure_github_token_in_namespace(&tenant_ns)
                    .await,
                Ok(true)
            ) {
                attach_github_token_env_ref(&mut stack_params.env_refs, GITHUB_TOKEN_SECRET_NAME);
            }
            if bp.spec.outputs.iter().any(|o| o == "kubeconfig") {
                stack_params.config.insert(
                    "kubeconfigSecretName".to_string(),
                    format!("{instance_name}-kubeconfig"),
                );
                stack_params.config.insert(
                    "kubeconfigSecretNamespace".to_string(),
                    tenant_namespace(&req.tenant)?,
                );
            } else if bundle_item.is_some()
                && let Ok(raw) =
                    tokio::fs::read_to_string(state.repo_workdir.join(&rel_dir).join("stack.yaml"))
                        .await
                && let Ok(doc) = serde_yml::from_str::<Value>(&raw)
            {
                if let Some(secret_name) = doc
                    .pointer("/spec/config/kubeconfigSecretName")
                    .and_then(Value::as_str)
                {
                    stack_params
                        .config
                        .insert("kubeconfigSecretName".to_string(), secret_name.to_string());
                }
                if let Some(secret_ns) = doc
                    .pointer("/spec/config/kubeconfigSecretNamespace")
                    .and_then(Value::as_str)
                {
                    stack_params.config.insert(
                        "kubeconfigSecretNamespace".to_string(),
                        secret_ns.to_string(),
                    );
                }
            }
            if bp.metadata.provider == "kind" {
                stack_params.docker_host = state.docker_proxy_url.clone();
            }
            let stack = synthesize_tenant_stack(&stack_params)?;
            let stack_yaml = stack.to_yaml()?;
            (
                rel_dir,
                stack_yaml,
                "pulumi:pulumi.com/v1:Stack",
                None,
                None,
            )
        }
        CatalogManifest::Bundle(_) => unreachable!("handled above"),
    };

    let (commit_sha, is_update) = {
        let _guard = state.git_lock.lock().await;
        // Ensure we start from main
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        let target_dir = state.repo_workdir.join(&rel_dir);
        let is_update = tokio::fs::try_exists(&target_dir).await.unwrap_or(false);
        tokio::fs::create_dir_all(&target_dir).await?;

        let mut staged_paths = Vec::new();
        let values_path = target_dir.join("values.yaml");
        tokio::fs::write(&values_path, &values_yaml).await?;
        staged_paths.push(format!("{rel_dir}/values.yaml"));

        let manifest_filename = match manifest_engine {
            Some(ManifestEngine::Flux) => "flux-kustomization.yaml",
            Some(ManifestEngine::Pulumi) => "stack.yaml",
            None => {
                if is_app {
                    if req.canary {
                        "helmrelease-canary.yaml"
                    } else {
                        "helmrelease.yaml"
                    }
                } else {
                    "stack.yaml"
                }
            }
        };
        let manifest_path = target_dir.join(manifest_filename);
        tokio::fs::write(&manifest_path, &manifest_yaml).await?;
        staged_paths.push(format!("{rel_dir}/{manifest_filename}"));

        if is_app {
            let stale_files: &[&str] = match manifest_engine {
                Some(ManifestEngine::Flux) => &[
                    "helmrelease.yaml",
                    "helmrelease-canary.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                    "stack.yaml",
                    "Pulumi.yaml",
                    "secret.enc.yaml",
                ],
                Some(ManifestEngine::Pulumi) => &[
                    "helmrelease.yaml",
                    "helmrelease-canary.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                    "flux-kustomization.yaml",
                    "values-configmap.yaml",
                    "secrets.enc.yaml",
                ],
                None if req.canary => &["helmrelease.yaml"],
                None => &[
                    "helmrelease-canary.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                ],
            };
            for stale_file in stale_files {
                let stale_path = target_dir.join(stale_file);
                if tokio::fs::try_exists(&stale_path).await.unwrap_or(false) {
                    tokio::fs::remove_file(&stale_path).await?;
                    staged_paths.push(format!("{rel_dir}/{stale_file}"));
                }
            }
        }

        let mut extra_kustomize_resources: Vec<String> = Vec::new();
        let mut bundle_item_kustomize_resources: Vec<String> = vec![manifest_filename.to_string()];

        if let Some(eng) = manifest_engine {
            if let Some(ref rendered_manifests) = rendered_manifest_template_yaml {
                tokio::fs::write(target_dir.join("manifests.yaml"), rendered_manifests).await?;
                staged_paths.push(format!("{rel_dir}/manifests.yaml"));
            }
            match eng {
                ManifestEngine::Flux => {
                    let mut app_k_resources = vec![
                        "manifests.yaml".to_string(),
                        "values-configmap.yaml".to_string(),
                    ];
                    if let Some(ref cm_yaml) = values_configmap_yaml {
                        tokio::fs::write(target_dir.join("values-configmap.yaml"), cm_yaml).await?;
                        staged_paths.push(format!("{rel_dir}/values-configmap.yaml"));
                    }
                    if let Some(ref enc_k8s_secret) = encrypted_infra_k8s_secret_yaml {
                        tokio::fs::write(target_dir.join("secrets.enc.yaml"), enc_k8s_secret)
                            .await?;
                        staged_paths.push(format!("{rel_dir}/secrets.enc.yaml"));
                        app_k_resources.push("secrets.enc.yaml".to_string());
                    }
                    let app_k_doc = serde_json::json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": app_k_resources,
                    });
                    tokio::fs::write(
                        target_dir.join("kustomization.yaml"),
                        serde_yml::to_string(&app_k_doc).map_err(|e| {
                            ServerError::Internal(format!(
                                "failed to serialize kustomization.yaml: {e}"
                            ))
                        })?,
                    )
                    .await?;
                    staged_paths.push(format!("{rel_dir}/kustomization.yaml"));
                }
                ManifestEngine::Pulumi => {
                    if let Some(ref pulumi_yaml) = synthesized_manifest_pulumi_yaml {
                        tokio::fs::write(target_dir.join("Pulumi.yaml"), pulumi_yaml).await?;
                        staged_paths.push(format!("{rel_dir}/Pulumi.yaml"));
                    }
                    let mut app_k_resources = vec!["stack.yaml".to_string()];
                    if let Some(ref enc_k8s_secret) = encrypted_infra_k8s_secret_yaml {
                        tokio::fs::write(target_dir.join("secret.enc.yaml"), enc_k8s_secret)
                            .await?;
                        staged_paths.push(format!("{rel_dir}/secret.enc.yaml"));
                        app_k_resources.push("secret.enc.yaml".to_string());
                        extra_kustomize_resources.push(format!(
                            "{}/apps/{instance_name}/secret.enc.yaml",
                            req.tenant
                        ));
                    }
                    let app_k_doc = serde_json::json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": app_k_resources,
                    });
                    tokio::fs::write(
                        target_dir.join("kustomization.yaml"),
                        serde_yml::to_string(&app_k_doc).map_err(|e| {
                            ServerError::Internal(format!(
                                "failed to serialize kustomization.yaml: {e}"
                            ))
                        })?,
                    )
                    .await?;
                    staged_paths.push(format!("{rel_dir}/kustomization.yaml"));
                }
            }
        } else if let CatalogManifest::Application(app) = &manifest {
            let repo_url = app.spec.upstream.repo_url.trim();
            if !repo_url.is_empty() {
                let mut repo_spec = serde_json::json!({
                    "interval": "5m",
                    "url": repo_url,
                });
                if repo_url.starts_with("oci://")
                    && let Some(obj) = repo_spec.as_object_mut()
                {
                    obj.insert("type".to_string(), serde_json::json!("oci"));
                }
                let mut repo_labels = serde_json::Map::new();
                repo_labels.insert(
                    "app.kubernetes.io/managed-by".to_string(),
                    serde_json::json!("keel"),
                );
                repo_labels.insert("keel.io/tenant".to_string(), serde_json::json!(req.tenant));
                if let Some((ref bundle_name, ref item_id)) = bundle_item {
                    repo_labels
                        .insert("keel.io/bundle".to_string(), serde_json::json!(bundle_name));
                    repo_labels.insert(
                        "keel.io/bundle-item".to_string(),
                        serde_json::json!(item_id),
                    );
                }
                let helm_repo_doc = serde_json::json!({
                    "apiVersion": "source.toolkit.fluxcd.io/v1",
                    "kind": "HelmRepository",
                    "metadata": {
                        "name": format!("{instance_name}-repo"),
                        "namespace": tenant_ns,
                        "labels": Value::Object(repo_labels),
                    },
                    "spec": repo_spec,
                });
                let helm_repo_yaml = serde_yml::to_string(&helm_repo_doc).map_err(|e| {
                    ServerError::Internal(format!("failed to serialize helm-repository.yaml: {e}"))
                })?;
                let helm_repo_path = target_dir.join("helm-repository.yaml");
                tokio::fs::write(&helm_repo_path, helm_repo_yaml).await?;
                staged_paths.push(format!("{rel_dir}/helm-repository.yaml"));
                bundle_item_kustomize_resources.push("helm-repository.yaml".to_string());
                extra_kustomize_resources.push(format!(
                    "{}/apps/{instance_name}/helm-repository.yaml",
                    req.tenant
                ));
            }
            if req.canary {
                let mut canary_input = FlaggerCanarySpecInput::new(&req.tenant, &instance_name);
                let chart_name = app
                    .spec
                    .upstream
                    .chart
                    .as_deref()
                    .unwrap_or(&app.metadata.id);
                canary_input.service_port = partitioned
                    .normal_values
                    .get("service")
                    .and_then(|s| s.get("port"))
                    .and_then(|p| p.as_u64())
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p > 0)
                    .unwrap_or(if chart_name == "podinfo" { 9898 } else { 80 });
                if let Some(host) = partitioned
                    .normal_values
                    .get("ingressHost")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    canary_input.hosts.push(host.to_string());
                }
                if let Some((ref bundle_name, ref item_id)) = bundle_item {
                    canary_input
                        .extra_labels
                        .insert("keel.io/bundle".to_string(), bundle_name.clone());
                    canary_input
                        .extra_labels
                        .insert("keel.io/bundle-item".to_string(), item_id.clone());
                }
                let canary_yaml = synthesize_flagger_canary_yaml(&canary_input)?;
                let np_yaml = synthesize_canary_gateway_network_policy_yaml(
                    &req.tenant,
                    &instance_name,
                    canary_input.service_port,
                )?;
                if let Some(ref remote_kc) = remote_app_kube_config {
                    let canary_dir = target_dir.join("canary");
                    tokio::fs::create_dir_all(&canary_dir).await?;
                    tokio::fs::write(canary_dir.join("canary.yaml"), canary_yaml).await?;
                    tokio::fs::write(canary_dir.join("networkpolicy-canary.yaml"), np_yaml).await?;
                    let canary_k_doc = serde_json::json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": ["canary.yaml", "networkpolicy-canary.yaml"],
                    });
                    let canary_k_yaml = serde_yml::to_string(&canary_k_doc).map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to serialize canary/kustomization.yaml: {e}"
                        ))
                    })?;
                    tokio::fs::write(canary_dir.join("kustomization.yaml"), canary_k_yaml).await?;
                    staged_paths.push(format!("{rel_dir}/canary/canary.yaml"));
                    staged_paths.push(format!("{rel_dir}/canary/networkpolicy-canary.yaml"));
                    staged_paths.push(format!("{rel_dir}/canary/kustomization.yaml"));

                    let mut fk_input = FluxKustomizationSpecInput::new(
                        &req.tenant,
                        format!("{instance_name}-canary"),
                        "keel-gitops",
                        format!("./{rel_dir}/canary"),
                    );
                    fk_input.kube_config = Some(remote_kc.clone());
                    fk_input.target_namespace = Some(
                        remote_app_target_ns
                            .clone()
                            .unwrap_or_else(|| tenant_ns.clone()),
                    );
                    let fk_yaml = synthesize_flux_kustomization_yaml(&fk_input)?;
                    tokio::fs::write(target_dir.join("canary-kustomization.yaml"), fk_yaml).await?;
                    staged_paths.push(format!("{rel_dir}/canary-kustomization.yaml"));
                    bundle_item_kustomize_resources.push("canary-kustomization.yaml".to_string());
                    extra_kustomize_resources.push(format!(
                        "{}/apps/{instance_name}/canary-kustomization.yaml",
                        req.tenant
                    ));
                } else {
                    let canary_path = target_dir.join("canary.yaml");
                    tokio::fs::write(&canary_path, canary_yaml).await?;
                    staged_paths.push(format!("{rel_dir}/canary.yaml"));
                    bundle_item_kustomize_resources.push("canary.yaml".to_string());
                    extra_kustomize_resources
                        .push(format!("{}/apps/{instance_name}/canary.yaml", req.tenant));

                    let np_path = target_dir.join("networkpolicy-canary.yaml");
                    tokio::fs::write(&np_path, np_yaml).await?;
                    staged_paths.push(format!("{rel_dir}/networkpolicy-canary.yaml"));
                    bundle_item_kustomize_resources.push("networkpolicy-canary.yaml".to_string());
                    extra_kustomize_resources.push(format!(
                        "{}/apps/{instance_name}/networkpolicy-canary.yaml",
                        req.tenant
                    ));
                }
            }
        }

        if !is_app {
            let blueprint_pulumi_path = state
                .repo_workdir
                .join("catalog/infra")
                .join(&req.catalog_id)
                .join("Pulumi.yaml");
            let pulumi_yaml = if tokio::fs::try_exists(&blueprint_pulumi_path)
                .await
                .unwrap_or(false)
            {
                tokio::fs::read_to_string(&blueprint_pulumi_path).await?
            } else {
                format!(
                    "name: {instance_name}\nruntime: yaml\ndescription: Keel provisioned {catalog_id} stack for tenant {tenant}\noutputs:\n  status: provisioned\n",
                    catalog_id = req.catalog_id,
                    tenant = req.tenant
                )
            };
            let target_pulumi_path = target_dir.join("Pulumi.yaml");
            tokio::fs::write(&target_pulumi_path, pulumi_yaml).await?;
            staged_paths.push(format!("{rel_dir}/Pulumi.yaml"));

            if let Some(enc_k8s_secret) = &encrypted_infra_k8s_secret_yaml {
                let secret_path = target_dir.join("secret.enc.yaml");
                tokio::fs::write(&secret_path, enc_k8s_secret).await?;
                staged_paths.push(format!("{rel_dir}/secret.enc.yaml"));
                bundle_item_kustomize_resources.push("secret.enc.yaml".to_string());
                extra_kustomize_resources.push(format!(
                    "{}/infra/{instance_name}/secret.enc.yaml",
                    req.tenant
                ));
            }
        }

        // Keep tenants/kustomization.yaml updated so Flux reconciles only K8s manifests (never values.yaml / Pulumi.yaml)
        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        let existing_kustomization = if tokio::fs::try_exists(&tenants_kustomization_path)
            .await
            .unwrap_or(false)
        {
            tokio::fs::read_to_string(&tenants_kustomization_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut resources: Vec<String> =
            serde_yml::from_str::<serde_json::Value>(&existing_kustomization)
                .ok()
                .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        for candidate in [
            format!("{}/rbac.yaml", req.tenant),
            format!("{}/git-repository.yaml", req.tenant),
        ] {
            if tokio::fs::try_exists(state.repo_workdir.join("tenants").join(&candidate))
                .await
                .unwrap_or(false)
                && !resources.contains(&candidate)
            {
                resources.push(candidate);
            }
        }
        if let Some((ref bundle_name, ref item_id)) = bundle_item {
            let item_k_doc = serde_json::json!({
                "apiVersion": "kustomize.config.k8s.io/v1beta1",
                "kind": "Kustomization",
                "resources": bundle_item_kustomize_resources,
            });
            tokio::fs::write(
                target_dir.join("kustomization.yaml"),
                serde_yml::to_string(&item_k_doc).map_err(|e| {
                    ServerError::Internal(format!("failed to serialize kustomization.yaml: {e}"))
                })?,
            )
            .await?;
            staged_paths.push(format!("{rel_dir}/kustomization.yaml"));

            let fk_rel = format!(
                "{}/bundles/{bundle_name}/{item_id}/flux-kustomization.yaml",
                req.tenant
            );
            if !resources.contains(&fk_rel) {
                resources.push(fk_rel);
            }
        } else {
            let manifest_rel_to_tenants = format!(
                "{}/{}/{instance_name}/{manifest_filename}",
                req.tenant,
                if is_app { "apps" } else { "infra" }
            );
            if is_app {
                if manifest_engine == Some(ManifestEngine::Flux) {
                    let stale_stack = format!("{}/apps/{instance_name}/stack.yaml", req.tenant);
                    let stale_sec = format!("{}/apps/{instance_name}/secret.enc.yaml", req.tenant);
                    resources.retain(|r| r != &stale_stack && r != &stale_sec);
                } else if manifest_engine == Some(ManifestEngine::Pulumi) {
                    let stale_fk = format!(
                        "{}/apps/{instance_name}/flux-kustomization.yaml",
                        req.tenant
                    );
                    resources.retain(|r| r != &stale_fk);
                } else if req.canary {
                    let stale = format!("{}/apps/{instance_name}/helmrelease.yaml", req.tenant);
                    resources.retain(|r| r != &stale);
                } else {
                    let stale_hr = format!(
                        "{}/apps/{instance_name}/helmrelease-canary.yaml",
                        req.tenant
                    );
                    let stale_canary = format!("{}/apps/{instance_name}/canary.yaml", req.tenant);
                    let stale_np = format!(
                        "{}/apps/{instance_name}/networkpolicy-canary.yaml",
                        req.tenant
                    );
                    resources.retain(|r| r != &stale_hr && r != &stale_canary && r != &stale_np);
                }
            }
            for res in extra_kustomize_resources {
                if !resources.contains(&res) {
                    resources.push(res);
                }
            }
            if !resources.contains(&manifest_rel_to_tenants) {
                resources.push(manifest_rel_to_tenants);
            }
        }
        tokio::fs::create_dir_all(state.repo_workdir.join("tenants")).await?;
        tokio::fs::write(
            &tenants_kustomization_path,
            serialize_tenants_kustomization(&resources)?,
        )
        .await?;
        staged_paths.push("tenants/kustomization.yaml".to_string());

        if manifest_engine.is_none()
            && let Some(enc_yaml) = encrypted_secrets_yaml
        {
            let enc_path = target_dir.join("secrets.enc.yaml");
            tokio::fs::write(&enc_path, enc_yaml).await?;
            staged_paths.push(format!("{rel_dir}/secrets.enc.yaml"));
        }

        state
            .git_runner
            .add(&state.repo_workdir, &staged_paths)
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!(
            "feat({}): propose {} {}",
            req.tenant,
            if is_app { "app" } else { "infra" },
            instance_name
        );
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        // Return working directory to main so subsequent proposals start cleanly
        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        (sha, is_update)
    };

    let mode_tag = if is_update { "[Update]" } else { "[New]" };
    let pr_title = req
        .title
        .map(|t| format!("{mode_tag} {t}"))
        .unwrap_or_else(|| {
            format!(
                "{mode_tag} feat({}): {} {}",
                req.tenant,
                if is_app { "deploy" } else { "provision" },
                instance_name
            )
        });
    let pr_body = req.body.unwrap_or_else(|| {
        format!(
            "{mode_tag} Automated proposal for tenant `{}` (`{}`)\n\n- Action: {}\n- Commit: `{}`\n- Path: `{}`",
            req.tenant,
            req.catalog_id,
            if is_update {
                "Update existing instance"
            } else {
                "Create new instance"
            },
            commit_sha,
            rel_dir
        )
    });

    let pr_req = CreatePrRequest::new(pr_title, pr_body, &branch)
        .with_tenant(&req.tenant)
        .with_author(format!(
            "{} <{}>",
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local")
        ));
    let pr = state.forge.create_pull_request(pr_req).await?;

    // Synthesize ephemeral PKO v2 preview CRs in `keel-previews`
    let mut preview_params = EphemeralPreviewParams::for_action(
        &pr.id,
        &req.tenant,
        &action_label,
        timestamp,
        &state.remote_url,
        &rel_dir,
        format!("organization/{instance_name}-pr-{}/{}", pr.id, req.tenant),
        &state.backend_url,
    )?;
    if !is_app || manifest_engine == Some(ManifestEngine::Pulumi) {
        populate_stack_config_from_values(&mut preview_params.config, &partitioned.normal_values);
        if let Some(ref s3_secret) = state.s3_credentials_secret {
            let _ = state
                .secret_store
                .ensure_s3_credentials_in_namespace(KEEL_PREVIEWS_NAMESPACE)
                .await;
            attach_s3_credentials_env_refs(&mut preview_params.env_refs, s3_secret);
        }
        if matches!(
            state
                .secret_store
                .ensure_github_token_in_namespace(KEEL_PREVIEWS_NAMESPACE)
                .await,
            Ok(true)
        ) {
            attach_github_token_env_ref(&mut preview_params.env_refs, GITHUB_TOKEN_SECRET_NAME);
        }
    }
    let preview_stack = synthesize_ephemeral_preview(&preview_params)?;

    let mut preview_resources = vec![ResourceDiff {
        urn: format!(
            "urn:pulumi:pr-{}::{}::{}::{}",
            pr.id, req.tenant, preview_resource_type, instance_name
        ),
        resource_type: preview_resource_type.to_string(),
        name: instance_name.clone(),
        op: ChangeOp::Create,
        changed_fields: vec!["spec".to_string()],
    }];
    if manifest_engine == Some(ManifestEngine::Pulumi)
        && let Some(ref rendered_manifests) = rendered_manifest_template_yaml
        && let Ok(docs) = validate_k8s_manifests_yaml(rendered_manifests)
    {
        for doc in docs {
            let api_v = doc
                .get("apiVersion")
                .and_then(Value::as_str)
                .unwrap_or("v1");
            let kind = doc
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("Resource");
            let name = doc
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or(&instance_name);
            let k8s_type = format!("kubernetes:{api_v}:{kind}");
            preview_resources.push(ResourceDiff {
                urn: format!(
                    "urn:pulumi:pr-{}::{}::{}::{}",
                    pr.id, req.tenant, k8s_type, name
                ),
                resource_type: k8s_type,
                name: name.to_string(),
                op: ChangeOp::Create,
                changed_fields: vec!["metadata".to_string(), "spec".to_string()],
            });
        }
    }

    let report = PreviewReport {
        summary: PreviewSummary {
            create: preview_resources.len(),
            ..PreviewSummary::default()
        },
        resources: preview_resources,
    };

    let markdown = format_markdown_report(&report, &pr.id);
    if let Err(err) = state.forge.post_preview_comment(&pr.id, &markdown).await {
        tracing::warn!(pr_id = %pr.id, error = %err, "Failed to post preview comment to forge");
    }

    let candidates = PreviewResourceCandidate::from_preview_bundle(&preview_stack).to_vec();
    state.preview_cache.write().await.insert(
        pr.id.clone(),
        CachedPreview {
            report: report.clone(),
            preview_stack: preview_stack.clone(),
            candidates,
        },
    );

    // Broadcast real-time SSE telemetry event
    state.telemetry.publish(CrdWatchEvent::new(
        WatchedCrdKind::Stack,
        KEEL_PREVIEWS_NAMESPACE,
        &preview_stack.stack.metadata.name,
        CrdReconcileStatus::Previewing,
        format!("Ephemeral preview created for PR #{} ({})", pr.id, branch),
        Some(commit_sha),
    ));

    Ok((
        StatusCode::CREATED,
        Json(CreateProposalResponse {
            proposal: pr,
            branch,
            preview_stack: preview_stack.stack,
            report,
        }),
    ))
}

async fn create_bundle_proposal(
    auth: AuthSession,
    state: AppState,
    req: CreateProposalRequest,
    bundle: CatalogBundle,
) -> Result<(StatusCode, Json<CreateProposalResponse>), ServerError> {
    let instance_name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&req.catalog_id)
        .to_string();
    validate_dns_label(&instance_name, "name")?;

    let action_label = format!("bundle-{}", bundle.metadata.id);
    validate_dns_label(&action_label, "action")?;
    validate_schema_values(&bundle.spec.schema, &req.values)?;

    let ordered_items = bundle.topological_order()?;
    let tenant_ns = tenant_namespace(&req.tenant)?;
    let timestamp = next_unique_branch_timestamp();
    let branch = build_preview_branch(&req.tenant, &action_label, timestamp)?;
    let bundle_rel_dir = format!("tenants/{}/bundles/{instance_name}", req.tenant);

    // Resolve each referenced catalog manifest and prepare per-item files before touching Git.
    struct PreparedBundleItem {
        id: String,
        kind: BundleItemKind,
        depends_on: Vec<String>,
        item_rel_dir: String,
        resource_name: String,
        values_yaml: String,
        encrypted_secrets_yaml: Option<String>,
        primary_filename: String,
        primary_yaml: String,
        extra_files: Vec<(String, String)>,
        flux_kustomization_yaml: String,
    }

    let mut item_kinds = std::collections::BTreeMap::new();
    let mut target_cluster_sources = std::collections::HashSet::new();
    for item in &ordered_items {
        item_kinds.insert(item.id.as_str(), item.kind);
        if let Some(src) = &item.target_cluster_from {
            target_cluster_sources.insert(src.as_str());
        }
    }

    let mut prepared_items = Vec::with_capacity(ordered_items.len());
    for item in ordered_items {
        let item_values = req
            .values
            .get(&item.id)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let item_resource_name = format!("{instance_name}-{}", item.id);
        validate_dns_label(&item_resource_name, "bundle_item_resource")?;
        let item_rel_dir = format!("{bundle_rel_dir}/{}", item.id);

        let kind_hint = match item.kind {
            BundleItemKind::Infra => "infra",
            BundleItemKind::App => "app",
        };
        let ref_manifest =
            load_catalog_manifest(&state.repo_workdir, &item.catalog_ref, Some(kind_hint)).await?;

        let sub_schema = bundle
            .spec
            .schema
            .get("properties")
            .and_then(|p| p.get(&item.id))
            .unwrap_or(match &ref_manifest {
                CatalogManifest::Application(a) => &a.spec.schema,
                CatalogManifest::InfraBlueprint(b) => &b.spec.schema,
                CatalogManifest::Bundle(b) => &b.spec.schema,
            });

        let partitioned = partition_values_by_schema(sub_schema, &item_values)?;
        let values_yaml = render_helm_values_yaml(&partitioned.normal_values)?;
        let plaintext_secrets_yaml = match partitioned.secret_values.as_object() {
            Some(obj) if !obj.is_empty() => {
                Some(render_helm_values_yaml(&partitioned.secret_values)?)
            }
            _ => None,
        };

        let encrypted_secrets_yaml = if let Some(plaintext_secrets) = &plaintext_secrets_yaml {
            let age_key = match state.secret_store.get_tenant_age_key(&req.tenant).await? {
                Some(existing) => existing,
                None => {
                    let keypair = SopsManager::new()
                        .generate_age_keypair()
                        .await
                        .map_err(|_| {
                            ServerError::BadRequest(format!(
                                "Tenant '{}' has no SOPS age key configured; cannot encrypt secret fields",
                                req.tenant
                            ))
                        })?;
                    let generated =
                        TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
                    state
                        .secret_store
                        .store_tenant_age_key(&req.tenant, &generated)
                        .await?;
                    generated
                }
            };
            let sops_recipients =
                resolve_sops_recipients(&state.repo_workdir, &age_key.public_key).await;
            Some(
                SopsManager::new()
                    .encrypt_yaml(&sops_recipients, plaintext_secrets)
                    .await?,
            )
        } else {
            None
        };

        let mut bundle_labels = std::collections::BTreeMap::new();
        bundle_labels.insert("keel.io/bundle".to_string(), instance_name.clone());
        bundle_labels.insert("keel.io/bundle-item".to_string(), item.id.clone());

        let (primary_filename, primary_yaml, extra_files, health_check) = match ref_manifest {
            CatalogManifest::Application(app) => {
                if item.canary && !app.spec.progressive_delivery.flagger_supported {
                    return Err(ServerError::BadRequest(format!(
                        "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
                        app.metadata.id
                    )));
                }
                let mut hr_input = HelmReleaseSpecInput::new(
                    &req.tenant,
                    &item_resource_name,
                    app.spec
                        .upstream
                        .chart
                        .clone()
                        .unwrap_or_else(|| app.metadata.id.clone()),
                    HelmChartSourceRef {
                        kind: HelmChartSourceKind::HelmRepository,
                        name: format!("{item_resource_name}-repo"),
                        namespace: Some(tenant_ns.clone()),
                    },
                );
                hr_input.chart_version = app.spec.upstream.version.clone();
                hr_input.extra_labels = bundle_labels.clone();

                if let Some(cluster_item) = &item.target_cluster_from {
                    hr_input.kube_config = Some(RemoteKubeConfigRef::with_key(
                        format!("{instance_name}-{cluster_item}-kubeconfig"),
                        "kubeconfig",
                    ));
                } else if let Some(target) = req
                    .target_cluster
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "local")
                {
                    hr_input.kube_config =
                        Some(RemoteKubeConfigRef::with_key(target, "kubeconfig"));
                }

                if let Some(target_ns) = &item.target_namespace {
                    if hr_input.kube_config.is_none() && target_ns != &tenant_ns {
                        return Err(ServerError::BadRequest(format!(
                            "Bundle item '{}' targetNamespace '{target_ns}' requires targetClusterFrom or remote targetCluster",
                            item.id
                        )));
                    }
                    hr_input.target_namespace = Some(target_ns.clone());
                }

                for dep in &item.depends_on {
                    if item_kinds.get(dep.as_str()) == Some(&BundleItemKind::App) {
                        hr_input.depends_on.push(FluxDependencyRef {
                            name: format!("{instance_name}-{dep}"),
                        });
                    } else if !item.bind_values.is_empty() {
                        // Bind outputs Secret from upstream infra item when bindValues are declared
                        hr_input.values_from_secrets.push(HelmValuesSecretRef {
                            name: format!("{instance_name}-{dep}-outputs"),
                            values_key: None,
                            target_path: None,
                            optional: Some(true),
                        });
                    }
                }

                if let Some(obj) = partitioned.normal_values.as_object()
                    && !obj.is_empty()
                {
                    hr_input.values = Some(partitioned.normal_values.clone());
                }

                let hr_yaml = synthesize_helm_release_yaml(&hr_input)?;
                let primary_filename = if item.canary {
                    "helmrelease-canary.yaml".to_string()
                } else {
                    "helmrelease.yaml".to_string()
                };
                let mut extras = Vec::new();
                let mut item_k_res = vec![primary_filename.clone()];
                let repo_url = app.spec.upstream.repo_url.trim();
                if !repo_url.is_empty() {
                    let mut repo_spec = serde_json::json!({
                        "interval": "5m",
                        "url": repo_url,
                    });
                    if repo_url.starts_with("oci://")
                        && let Some(obj) = repo_spec.as_object_mut()
                    {
                        obj.insert("type".to_string(), serde_json::json!("oci"));
                    }
                    let helm_repo_doc = serde_json::json!({
                        "apiVersion": "source.toolkit.fluxcd.io/v1",
                        "kind": "HelmRepository",
                        "metadata": {
                            "name": format!("{item_resource_name}-repo"),
                            "namespace": tenant_ns,
                            "labels": {
                                "app.kubernetes.io/managed-by": "keel",
                                "keel.io/tenant": req.tenant,
                                "keel.io/bundle": instance_name,
                                "keel.io/bundle-item": item.id,
                            }
                        },
                        "spec": repo_spec,
                    });
                    let helm_repo_yaml = serde_yml::to_string(&helm_repo_doc).map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to serialize helm-repository.yaml: {e}"
                        ))
                    })?;
                    extras.push(("helm-repository.yaml".to_string(), helm_repo_yaml));
                    item_k_res.push("helm-repository.yaml".to_string());
                }

                if item.canary {
                    let mut canary_input =
                        FlaggerCanarySpecInput::new(&req.tenant, &item_resource_name);
                    let chart_name = app
                        .spec
                        .upstream
                        .chart
                        .as_deref()
                        .unwrap_or(&app.metadata.id);
                    canary_input.service_port = partitioned
                        .normal_values
                        .get("service")
                        .and_then(|s| s.get("port"))
                        .and_then(|p| p.as_u64())
                        .and_then(|p| u16::try_from(p).ok())
                        .filter(|p| *p > 0)
                        .unwrap_or(if chart_name == "podinfo" { 9898 } else { 80 });
                    if let Some(host) = partitioned
                        .normal_values
                        .get("ingressHost")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        canary_input.hosts.push(host.to_string());
                    }
                    canary_input.extra_labels = bundle_labels.clone();
                    let canary_yaml = synthesize_flagger_canary_yaml(&canary_input)?;
                    extras.push(("canary.yaml".to_string(), canary_yaml));
                    item_k_res.push("canary.yaml".to_string());

                    let np_yaml = synthesize_canary_gateway_network_policy_yaml(
                        &req.tenant,
                        &item_resource_name,
                        canary_input.service_port,
                    )?;
                    extras.push(("networkpolicy-canary.yaml".to_string(), np_yaml));
                    item_k_res.push("networkpolicy-canary.yaml".to_string());
                }

                // Item-level kustomization.yaml so the Flux Kustomization applies only K8s manifests
                let item_k_doc = serde_json::json!({
                    "apiVersion": "kustomize.config.k8s.io/v1beta1",
                    "kind": "Kustomization",
                    "resources": item_k_res,
                });
                extras.push((
                    "kustomization.yaml".to_string(),
                    serde_yml::to_string(&item_k_doc).map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to serialize kustomization.yaml: {e}"
                        ))
                    })?,
                ));

                (
                    primary_filename,
                    hr_yaml,
                    extras,
                    FluxHealthCheckRef {
                        api_version: "helm.toolkit.fluxcd.io/v2".to_string(),
                        kind: "HelmRelease".to_string(),
                        name: item_resource_name.clone(),
                        namespace: Some(tenant_ns.clone()),
                    },
                )
            }
            CatalogManifest::InfraBlueprint(bp) => {
                let blueprint_pulumi_path = state
                    .repo_workdir
                    .join("catalog/infra")
                    .join(&item.catalog_ref)
                    .join("Pulumi.yaml");
                let pulumi_project =
                    read_pulumi_project_name(&blueprint_pulumi_path, &bp.metadata.name).await;
                let mut stack_params = TenantStackParams::new(
                    &req.tenant,
                    &item_resource_name,
                    format!(
                        "organization/{pulumi_project}/{}-{item_resource_name}",
                        req.tenant
                    ),
                    &state.backend_url,
                    "keel-gitops",
                    &item_rel_dir,
                );
                stack_params
                    .config
                    .insert("keel:provider".to_string(), bp.metadata.provider.clone());
                if bp.spec.outputs.iter().any(|o| o == "kubeconfig")
                    || target_cluster_sources.contains(item.id.as_str())
                {
                    stack_params.config.insert(
                        "kubeconfigSecretName".to_string(),
                        format!("{item_resource_name}-kubeconfig"),
                    );
                    stack_params
                        .config
                        .insert("kubeconfigSecretNamespace".to_string(), tenant_ns.clone());
                } else if let Some(cluster_item) = &item.target_cluster_from {
                    stack_params.config.insert(
                        "kubeconfigSecretName".to_string(),
                        format!("{instance_name}-{cluster_item}-kubeconfig"),
                    );
                    stack_params
                        .config
                        .insert("kubeconfigSecretNamespace".to_string(), tenant_ns.clone());
                } else if let Some(target) = req
                    .target_cluster
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "local")
                {
                    stack_params
                        .config
                        .insert("kubeconfigSecretName".to_string(), target.to_string());
                    stack_params
                        .config
                        .insert("kubeconfigSecretNamespace".to_string(), tenant_ns.clone());
                }
                if bp.metadata.provider == "kind" {
                    stack_params.docker_host = state.docker_proxy_url.clone();
                }
                if let Some(ref s3_secret) = state.s3_credentials_secret {
                    let _ = state
                        .secret_store
                        .ensure_s3_credentials_in_namespace(&tenant_ns)
                        .await;
                    attach_s3_credentials_env_refs(&mut stack_params.env_refs, s3_secret);
                }
                if matches!(
                    state
                        .secret_store
                        .ensure_github_token_in_namespace(&tenant_ns)
                        .await,
                    Ok(true)
                ) {
                    attach_github_token_env_ref(
                        &mut stack_params.env_refs,
                        GITHUB_TOKEN_SECRET_NAME,
                    );
                }
                if let Some(obj) = partitioned.normal_values.as_object() {
                    for (k, v) in obj {
                        let str_val = match v {
                            Value::String(s) => s.clone(),
                            Value::Number(n) => n.to_string(),
                            Value::Bool(b) => b.to_string(),
                            _ => continue,
                        };
                        stack_params.config.insert(k.clone(), str_val);
                    }
                }
                let stack = synthesize_tenant_stack(&stack_params)?;
                let stack_yaml = stack.to_yaml()?;

                let blueprint_pulumi_path = state
                    .repo_workdir
                    .join("catalog/infra")
                    .join(&item.catalog_ref)
                    .join("Pulumi.yaml");
                let pulumi_yaml = if tokio::fs::try_exists(&blueprint_pulumi_path)
                    .await
                    .unwrap_or(false)
                {
                    tokio::fs::read_to_string(&blueprint_pulumi_path).await?
                } else {
                    format!(
                        "name: {item_resource_name}\nruntime: yaml\ndescription: Keel provisioned {} stack for tenant {}\noutputs:\n  status: provisioned\n",
                        item.catalog_ref, req.tenant
                    )
                };
                let item_k_doc = serde_json::json!({
                    "apiVersion": "kustomize.config.k8s.io/v1beta1",
                    "kind": "Kustomization",
                    "resources": ["stack.yaml"],
                });
                let extras = vec![
                    ("Pulumi.yaml".to_string(), pulumi_yaml),
                    (
                        "kustomization.yaml".to_string(),
                        serde_yml::to_string(&item_k_doc).map_err(|e| {
                            ServerError::Internal(format!(
                                "failed to serialize kustomization.yaml: {e}"
                            ))
                        })?,
                    ),
                ];

                (
                    "stack.yaml".to_string(),
                    stack_yaml,
                    extras,
                    FluxHealthCheckRef {
                        api_version: "pulumi.com/v1".to_string(),
                        kind: "Stack".to_string(),
                        name: item_resource_name.clone(),
                        namespace: Some(tenant_ns.clone()),
                    },
                )
            }
            CatalogManifest::Bundle(_) => {
                return Err(ServerError::BadRequest(format!(
                    "Nested CatalogBundle '{}' is not supported inside bundle items",
                    item.catalog_ref
                )));
            }
        };

        let mut fk_input = FluxKustomizationSpecInput::new(
            &req.tenant,
            &item_resource_name,
            "keel-gitops",
            format!("./{item_rel_dir}"),
        );
        fk_input.depends_on = item
            .depends_on
            .iter()
            .map(|dep| FluxDependencyRef {
                name: format!("{instance_name}-{dep}"),
            })
            .collect();
        fk_input.health_checks = vec![health_check];
        fk_input.extra_labels = bundle_labels;
        let flux_kustomization_yaml = synthesize_flux_kustomization_yaml(&fk_input)?;

        prepared_items.push(PreparedBundleItem {
            id: item.id.clone(),
            kind: item.kind,
            depends_on: item.depends_on.clone(),
            item_rel_dir,
            resource_name: item_resource_name,
            values_yaml,
            encrypted_secrets_yaml,
            primary_filename,
            primary_yaml,
            extra_files,
            flux_kustomization_yaml,
        });
    }

    let (commit_sha, is_update) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        let target_dir = state.repo_workdir.join(&bundle_rel_dir);
        let is_update = tokio::fs::try_exists(&target_dir).await.unwrap_or(false);
        tokio::fs::create_dir_all(&target_dir).await?;

        let mut staged_paths = Vec::new();
        let mut kustomize_additions = Vec::new();

        for item in &prepared_items {
            let item_dir = state.repo_workdir.join(&item.item_rel_dir);
            tokio::fs::create_dir_all(&item_dir).await?;

            tokio::fs::write(item_dir.join("values.yaml"), &item.values_yaml).await?;
            staged_paths.push(format!("{}/values.yaml", item.item_rel_dir));

            tokio::fs::write(item_dir.join(&item.primary_filename), &item.primary_yaml).await?;
            staged_paths.push(format!("{}/{}", item.item_rel_dir, item.primary_filename));

            if item.kind == BundleItemKind::App {
                let stale_files: &[&str] = if item.primary_filename == "helmrelease-canary.yaml" {
                    &["helmrelease.yaml"]
                } else {
                    &[
                        "helmrelease-canary.yaml",
                        "canary.yaml",
                        "networkpolicy-canary.yaml",
                    ]
                };
                for stale_file in stale_files {
                    let stale_path = item_dir.join(stale_file);
                    if tokio::fs::try_exists(&stale_path).await.unwrap_or(false) {
                        tokio::fs::remove_file(&stale_path).await?;
                        staged_paths.push(format!("{}/{stale_file}", item.item_rel_dir));
                    }
                }
            }

            for (fname, content) in &item.extra_files {
                tokio::fs::write(item_dir.join(fname), content).await?;
                staged_paths.push(format!("{}/{fname}", item.item_rel_dir));
            }

            if let Some(enc_yaml) = &item.encrypted_secrets_yaml {
                tokio::fs::write(item_dir.join("secrets.enc.yaml"), enc_yaml).await?;
                staged_paths.push(format!("{}/secrets.enc.yaml", item.item_rel_dir));
            }

            tokio::fs::write(
                item_dir.join("flux-kustomization.yaml"),
                &item.flux_kustomization_yaml,
            )
            .await?;
            staged_paths.push(format!("{}/flux-kustomization.yaml", item.item_rel_dir));
            kustomize_additions.push(format!(
                "{}/bundles/{instance_name}/{}/flux-kustomization.yaml",
                req.tenant, item.id
            ));
        }

        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        let existing_kustomization = if tokio::fs::try_exists(&tenants_kustomization_path)
            .await
            .unwrap_or(false)
        {
            tokio::fs::read_to_string(&tenants_kustomization_path)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut resources: Vec<String> =
            serde_yml::from_str::<serde_json::Value>(&existing_kustomization)
                .ok()
                .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        for res in kustomize_additions {
            if !resources.contains(&res) {
                resources.push(res);
            }
        }
        tokio::fs::create_dir_all(state.repo_workdir.join("tenants")).await?;
        tokio::fs::write(
            &tenants_kustomization_path,
            serialize_tenants_kustomization(&resources)?,
        )
        .await?;
        staged_paths.push("tenants/kustomization.yaml".to_string());

        state
            .git_runner
            .add(&state.repo_workdir, &staged_paths)
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("feat({}): propose bundle {}", req.tenant, instance_name);
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        (sha, is_update)
    };

    let mode_tag = if is_update { "[Update]" } else { "[New]" };
    let pr_title = req
        .title
        .map(|t| format!("{mode_tag} {t}"))
        .unwrap_or_else(|| {
            format!(
                "{mode_tag} feat({}): deploy bundle {instance_name}",
                req.tenant
            )
        });
    let pr_body = req.body.unwrap_or_else(|| {
        format!(
            "{mode_tag} Automated bundle proposal for tenant `{}` (`{}`)\n\n- Items: {}\n- Commit: `{}`\n- Path: `{}`",
            req.tenant,
            req.catalog_id,
            prepared_items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>()
                .join(" -> "),
            commit_sha,
            bundle_rel_dir
        )
    });

    let pr_req = CreatePrRequest::new(pr_title, pr_body, &branch)
        .with_tenant(&req.tenant)
        .with_author(format!(
            "{} <{}>",
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local")
        ));
    let pr = state.forge.create_pull_request(pr_req).await?;

    let mut preview_params = EphemeralPreviewParams::for_action(
        &pr.id,
        &req.tenant,
        &action_label,
        timestamp,
        &state.remote_url,
        &bundle_rel_dir,
        format!("organization/{instance_name}-pr-{}/{}", pr.id, req.tenant),
        &state.backend_url,
    )?;
    preview_params.docker_host = state.docker_proxy_url.clone();
    if let Some(ref s3_secret) = state.s3_credentials_secret {
        let _ = state
            .secret_store
            .ensure_s3_credentials_in_namespace(KEEL_PREVIEWS_NAMESPACE)
            .await;
        attach_s3_credentials_env_refs(&mut preview_params.env_refs, s3_secret);
    }
    if matches!(
        state
            .secret_store
            .ensure_github_token_in_namespace(KEEL_PREVIEWS_NAMESPACE)
            .await,
        Ok(true)
    ) {
        attach_github_token_env_ref(&mut preview_params.env_refs, GITHUB_TOKEN_SECRET_NAME);
    }
    let preview_stack = synthesize_ephemeral_preview(&preview_params)?;

    let resources: Vec<ResourceDiff> = prepared_items
        .iter()
        .map(|item| {
            let rtype = match item.kind {
                BundleItemKind::Infra => "pulumi:pulumi.com/v1:Stack",
                BundleItemKind::App => "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
            };
            let mut changed_fields = vec!["spec".to_string()];
            if !item.depends_on.is_empty() {
                changed_fields.push(format!(
                    "deferred-preview(dependsOn: {})",
                    item.depends_on.join(", ")
                ));
            }
            ResourceDiff {
                urn: format!(
                    "urn:pulumi:pr-{}::{}::{}::{}",
                    pr.id, req.tenant, rtype, item.resource_name
                ),
                resource_type: rtype.to_string(),
                name: item.resource_name.clone(),
                op: if is_update {
                    ChangeOp::Update
                } else {
                    ChangeOp::Create
                },
                changed_fields,
            }
        })
        .collect();

    let report = PreviewReport {
        summary: PreviewSummary {
            create: if is_update { 0 } else { resources.len() },
            update: if is_update { resources.len() } else { 0 },
            ..PreviewSummary::default()
        },
        resources,
    };

    let markdown = format_markdown_report(&report, &pr.id);
    if let Err(err) = state.forge.post_preview_comment(&pr.id, &markdown).await {
        tracing::warn!(pr_id = %pr.id, error = %err, "Failed to post preview comment to forge");
    }

    let candidates = PreviewResourceCandidate::from_preview_bundle(&preview_stack).to_vec();
    state.preview_cache.write().await.insert(
        pr.id.clone(),
        CachedPreview {
            report: report.clone(),
            preview_stack: preview_stack.clone(),
            candidates,
        },
    );

    state.telemetry.publish(CrdWatchEvent::new(
        WatchedCrdKind::Stack,
        KEEL_PREVIEWS_NAMESPACE,
        &preview_stack.stack.metadata.name,
        CrdReconcileStatus::Previewing,
        format!(
            "Ephemeral bundle preview created for PR #{} ({})",
            pr.id, branch
        ),
        Some(commit_sha),
    ));

    Ok((
        StatusCode::CREATED,
        Json(CreateProposalResponse {
            proposal: pr,
            branch,
            preview_stack: preview_stack.stack,
            report,
        }),
    ))
}

async fn load_catalog_manifest(
    repo_workdir: &Path,
    catalog_id: &str,
    kind_hint: Option<&str>,
) -> Result<CatalogManifest, ServerError> {
    let app_path = repo_workdir
        .join("catalog/apps")
        .join(catalog_id)
        .join("catalog.yaml");
    let infra_path = repo_workdir
        .join("catalog/infra")
        .join(catalog_id)
        .join("catalog.yaml");
    let bundle_path = repo_workdir
        .join("catalog/bundles")
        .join(catalog_id)
        .join("catalog.yaml");

    let target_path = match kind_hint.map(|k| k.to_ascii_lowercase()).as_deref() {
        Some("application") | Some("app") => app_path,
        Some("infra") | Some("blueprint") | Some("infrablueprint") => infra_path,
        Some("bundle") | Some("catalogbundle") => bundle_path,
        _ => {
            if tokio::fs::try_exists(&app_path).await.unwrap_or(false) {
                app_path
            } else if tokio::fs::try_exists(&infra_path).await.unwrap_or(false) {
                infra_path
            } else {
                bundle_path
            }
        }
    };

    if !tokio::fs::try_exists(&target_path).await.unwrap_or(false) {
        return Err(ServerError::NotFound(format!(
            "Catalog item '{catalog_id}' not found"
        )));
    }

    let yaml = tokio::fs::read_to_string(&target_path).await?;
    Ok(parse_catalog(&yaml)?)
}

/// Response body for `GET /api/v1/proposals/{pr_id}/preview`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalPreviewResponse {
    pub pr_id: String,
    pub url: String,
    pub state: PullRequestState,
    pub source_branch: String,
    pub target_branch: String,
    pub report: PreviewReport,
    pub comments: Vec<PullRequestComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_diff: Option<String>,
}

fn synthesize_report_from_git_diff(pr_id: &str, git_diff: &str) -> PreviewReport {
    let mut resources = Vec::new();
    let mut summary = PreviewSummary::default();

    let mut current_path: Option<String> = None;
    let mut current_op = ChangeOp::Update;

    let flush = |path_opt: Option<String>,
                 op: ChangeOp,
                 resources: &mut Vec<ResourceDiff>,
                 summary: &mut PreviewSummary| {
        let Some(path) = path_opt else { return };
        let parts: Vec<&str> = path.split('/').collect();
        let (tenant, resource_type, name) = if parts.first() == Some(&"tenants") && parts.len() >= 4
        {
            let tenant = parts[1];
            let file_name = parts.last().copied().unwrap_or("");
            if parts[2] == "infra" {
                let res_name = if parts.len() >= 5 {
                    parts[3].to_string()
                } else {
                    file_name.trim_end_matches(".yaml").to_string()
                };
                if file_name == "stack.yaml" || parts.len() == 4 {
                    (tenant, "pulumi:pulumi.com/v1:Stack", res_name)
                } else {
                    return;
                }
            } else if parts[2] == "apps" {
                let res_name = if parts.len() >= 5 {
                    parts[3].to_string()
                } else {
                    file_name.trim_end_matches(".yaml").to_string()
                };
                if file_name == "helmrelease.yaml"
                    || file_name == "helmrelease-canary.yaml"
                    || parts.len() == 4
                {
                    (
                        tenant,
                        "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
                        res_name,
                    )
                } else if file_name == "flux-kustomization.yaml" {
                    (
                        tenant,
                        "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization",
                        res_name,
                    )
                } else if file_name == "stack.yaml" {
                    (tenant, "pulumi:pulumi.com/v1:Stack", res_name)
                } else {
                    return;
                }
            } else if parts[2] == "bundles" && parts.len() >= 6 {
                let res_name = format!("{}-{}", parts[3], parts[4]);
                if file_name == "stack.yaml" {
                    (tenant, "pulumi:pulumi.com/v1:Stack", res_name)
                } else if file_name == "helmrelease.yaml" {
                    (
                        tenant,
                        "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
                        res_name,
                    )
                } else {
                    return;
                }
            } else if file_name == "rbac.yaml" {
                (
                    tenant,
                    "k8s:rbac.authorization.k8s.io/v1:RoleBinding",
                    format!("tenant-{tenant}"),
                )
            } else {
                (tenant, "gitops:manifest", path.clone())
            }
        } else if parts.first() == Some(&"catalog") && parts.len() >= 4 {
            let kind = match parts[1] {
                "apps" => "keel.io/v1alpha1:CatalogApplication",
                "infra" => "keel.io/v1alpha1:CatalogInfraBlueprint",
                "bundles" => "keel.io/v1alpha1:CatalogBundle",
                _ => "keel.io/v1alpha1:CatalogItem",
            };
            ("catalog", kind, parts[2].to_string())
        } else {
            ("system", "gitops:manifest", path.clone())
        };

        match op {
            ChangeOp::Create => summary.create += 1,
            ChangeOp::Update => summary.update += 1,
            ChangeOp::Delete => summary.delete += 1,
            ChangeOp::Replace => summary.replace += 1,
            ChangeOp::Same => summary.same += 1,
            ChangeOp::Drift => summary.drift += 1,
        }

        resources.push(ResourceDiff {
            urn: format!("urn:pulumi:pr-{pr_id}::{tenant}::{resource_type}::{name}"),
            resource_type: resource_type.to_string(),
            name,
            op,
            changed_fields: vec!["spec".to_string()],
        });
    };

    for line in git_diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            flush(
                current_path.take(),
                current_op,
                &mut resources,
                &mut summary,
            );
            current_op = ChangeOp::Update;
            if let Some((_, b_path)) = rest.split_once(" b/") {
                current_path = Some(b_path.to_string());
            }
        } else if line.starts_with("new file mode ") {
            current_op = ChangeOp::Create;
        } else if line.starts_with("deleted file mode ") {
            current_op = ChangeOp::Delete;
        }
    }
    flush(
        current_path.take(),
        current_op,
        &mut resources,
        &mut summary,
    );

    PreviewReport { resources, summary }
}

/// `GET /api/v1/proposals/{pr_id}/preview`
pub async fn get_proposal_preview(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<ProposalPreviewResponse>, ServerError> {
    let pr = state.forge.get_pull_request(&pr_id).await?;
    let cached_report = state
        .preview_cache
        .read()
        .await
        .get(&pr_id)
        .map(|c| c.report.clone());

    let git_diff = if tokio::fs::try_exists(state.repo_workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        let _git_guard = state.git_lock.lock().await;
        let _ = state.git_runner.fetch(&state.repo_workdir, "origin").await;
        let base_ref = format!("origin/{}", pr.target_branch);
        let head_ref = format!("origin/{}", pr.source_branch);
        state
            .git_runner
            .diff_refs(&state.repo_workdir, &base_ref, &head_ref)
            .await
            .ok()
            .filter(|d| !d.is_empty())
    } else {
        None
    };

    let report = match cached_report {
        Some(r) => r,
        None => match git_diff.as_deref() {
            Some(diff) => synthesize_report_from_git_diff(&pr_id, diff),
            None => PreviewReport::default(),
        },
    };

    Ok(Json(ProposalPreviewResponse {
        pr_id: pr.id,
        url: pr.url,
        state: pr.state,
        source_branch: pr.source_branch,
        target_branch: pr.target_branch,
        report,
        comments: pr.comments,
        git_diff,
    }))
}

/// Response body for `POST /api/v1/proposals/{pr_id}/merge`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeProposalResponse {
    pub proposal: PullRequestInfo,
    pub gc_plan: GcPlan,
}

fn serialize_tenants_kustomization(resources: &[String]) -> Result<String, ServerError> {
    let kustomization_doc = serde_json::json!({
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": resources,
    });
    let mut yaml = serde_yml::to_string(&kustomization_doc).map_err(|e| {
        ServerError::Internal(format!(
            "failed to serialize tenants/kustomization.yaml: {e}"
        ))
    })?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml)
}

/// Validates that a PR's changed files are strictly scoped to `tenants/<tenant>/` (excluding `rbac.yaml`)
/// and only tenant-scoped additions/removals in `tenants/kustomization.yaml`.
fn validate_tenant_admin_pr_paths(
    tenant: &str,
    changed_files: &[String],
    kustomization_diff: Option<&str>,
) -> Result<(), ServerError> {
    if changed_files.is_empty() {
        return Err(ServerError::Forbidden(
            "Tenant administrators can only manage PRs with verified tenant-scoped file changes"
                .to_string(),
        ));
    }
    let tenant_prefix = format!("tenants/{tenant}/");
    let rbac_path = format!("tenants/{tenant}/rbac.yaml");
    for path in changed_files {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == rbac_path {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot modify '{rbac_path}'"
            )));
        }
        if trimmed.starts_with(&tenant_prefix) {
            continue;
        }
        if trimmed == "tenants/kustomization.yaml" {
            let diff = kustomization_diff.ok_or_else(|| {
                ServerError::Forbidden(
                    "Unable to verify 'tenants/kustomization.yaml' diff for tenant admin merge"
                        .to_string(),
                )
            })?;
            let kustomize_prefix = format!("{tenant}/");
            for line in diff.lines() {
                if line.starts_with("+++")
                    || line.starts_with("---")
                    || line.starts_with("@@")
                    || line.starts_with("diff ")
                    || line.starts_with("index ")
                {
                    continue;
                }
                if let Some(rest) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
                    let item = rest.trim();
                    if matches!(
                        item,
                        "apiVersion: kustomize.config.k8s.io/v1beta1"
                            | "kind: Kustomization"
                            | "resources:"
                            | ""
                    ) {
                        continue;
                    }
                    let Some(entry) = item.strip_prefix('-').map(str::trim) else {
                        return Err(ServerError::Forbidden(
                            "Tenant administrators may only add/remove resource entries in 'tenants/kustomization.yaml'"
                                .to_string(),
                        ));
                    };
                    if !entry.starts_with(&kustomize_prefix) {
                        return Err(ServerError::Forbidden(format!(
                            "Tenant administrators cannot modify '{entry}' in 'tenants/kustomization.yaml'"
                        )));
                    }
                }
            }
            continue;
        }
        return Err(ServerError::Forbidden(format!(
            "Tenant administrators cannot manage PRs modifying '{trimmed}'"
        )));
    }
    Ok(())
}

/// Authorizes `merge_proposal`, `reject_proposal`, and `reopen_proposal` for either cluster admins
/// (`keel-admins`) or tenant-scoped admins (`tenant-<id>-admin`) when the PR is strictly scoped to `<id>`.
async fn require_pr_tenant_admin_access(
    auth: &AuthSession,
    state: &AppState,
    pr_id: &str,
    expected_state: Option<PullRequestState>,
) -> Result<(), ServerError> {
    if auth.is_admin() {
        return Ok(());
    }

    let prs = state.forge.list_pull_requests(None, expected_state).await?;
    let pr = prs
        .into_iter()
        .find(|p| p.id == pr_id)
        .ok_or_else(|| ServerError::NotFound(format!("Pull request '{pr_id}' not found")))?;

    let tenant = parse_proposal_branch_tenant(&pr.source_branch).ok_or_else(|| {
        ServerError::Forbidden(format!(
            "PR #{pr_id} branch '{}' is not scoped to a tenant",
            pr.source_branch
        ))
    })?;
    auth.require_tenant_admin(tenant)?;

    if !tokio::fs::try_exists(state.repo_workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        return Err(ServerError::Forbidden(
            "Git workdir unavailable to verify PR file scope for tenant administrator".to_string(),
        ));
    }

    let (changed_files, kustomization_diff) = {
        let _git_guard = state.git_lock.lock().await;
        let _ = state.git_runner.fetch(&state.repo_workdir, "origin").await;
        let range = format!("origin/main...origin/{}", pr.source_branch);
        let diff_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&state.repo_workdir)
            .args(["diff", "--name-only", &range])
            .output()
            .await
            .map_err(|e| ServerError::Internal(format!("failed to run git diff: {e}")))?;
        if !diff_out.status.success() {
            return Err(ServerError::Forbidden(format!(
                "Unable to verify changed files on PR #{pr_id} for tenant administrator"
            )));
        }
        let files: Vec<String> = String::from_utf8_lossy(&diff_out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();

        let k_diff = if files.iter().any(|f| f == "tenants/kustomization.yaml") {
            let k_out = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&state.repo_workdir)
                .args(["diff", "-U0", &range, "--", "tenants/kustomization.yaml"])
                .output()
                .await
                .map_err(|e| ServerError::Internal(format!("failed to run git diff: {e}")))?;
            if k_out.status.success() {
                Some(String::from_utf8_lossy(&k_out.stdout).into_owned())
            } else {
                None
            }
        } else {
            None
        };
        (files, k_diff)
    };

    validate_tenant_admin_pr_paths(tenant, &changed_files, kustomization_diff.as_deref())
}

/// `POST /api/v1/proposals/{pr_id}/merge`
pub async fn merge_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    require_pr_tenant_admin_access(&auth, &state, &pr_id, Some(PullRequestState::Open)).await?;
    let merged = state.forge.merge_pull_request(&pr_id).await?;

    // Sync local repo_workdir to origin/main so subsequent catalog/tenant/stack/release reads reflect the merge.
    if tokio::fs::try_exists(state.repo_workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        let _git_guard = state.git_lock.lock().await;
        let _ = state.git_runner.fetch(&state.repo_workdir, "origin").await;
        let _ = state
            .git_runner
            .fast_forward_merge(&state.repo_workdir, "main", "origin/main")
            .await;
    }

    if merged.title.starts_with("feat(providers):")
        && let Ok(registry) = load_provider_registry(&state.repo_workdir).await
    {
        let enabled_ids: Vec<String> = registry
            .providers
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.id)
            .collect();
        if let Ok(policy_val) = keel_kube::build_restrict_pulumi_providers_policy(&enabled_ids)
            && let Ok(policy_json) = serde_json::to_string(&policy_val)
        {
            use tokio::io::AsyncWriteExt;
            if let Ok(mut child) = tokio::process::Command::new("kubectl")
                .args(["apply", "-f", "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(policy_json.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                }
                let _ = child.wait().await;
            }
        }
    }

    let cached = state.preview_cache.write().await.remove(&pr_id);
    let gc_plan = if let Some(entry) = cached {
        let plan = plan_preview_gc(&entry.candidates, &[pr_id.as_str()]);
        state.telemetry.publish(CrdWatchEvent::new(
            WatchedCrdKind::Stack,
            KEEL_PREVIEWS_NAMESPACE,
            &entry.preview_stack.stack.metadata.name,
            CrdReconcileStatus::Deleted,
            format!("Garbage-collected ephemeral preview for merged PR #{pr_id}"),
            Some(merged.head_sha.clone()),
        ));
        plan
    } else {
        GcPlan::default()
    };

    Ok(Json(MergeProposalResponse {
        proposal: merged,
        gc_plan,
    }))
}

/// `POST /api/v1/proposals/{pr_id}/reject`
///
/// Closes an open PR without merging and garbage-collects any ephemeral PKO preview resources.
pub async fn reject_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    require_pr_tenant_admin_access(&auth, &state, &pr_id, Some(PullRequestState::Open)).await?;
    let closed = state.forge.close_pull_request(&pr_id).await?;

    let cached = state.preview_cache.write().await.remove(&pr_id);
    let gc_plan = if let Some(entry) = cached {
        let plan = plan_preview_gc(&entry.candidates, &[pr_id.as_str()]);
        state.telemetry.publish(CrdWatchEvent::new(
            WatchedCrdKind::Stack,
            KEEL_PREVIEWS_NAMESPACE,
            &entry.preview_stack.stack.metadata.name,
            CrdReconcileStatus::Deleted,
            format!("Garbage-collected ephemeral preview for rejected PR #{pr_id}"),
            Some(closed.head_sha.clone()),
        ));
        plan
    } else {
        GcPlan::default()
    };

    Ok(Json(MergeProposalResponse {
        proposal: closed,
        gc_plan,
    }))
}

/// `POST /api/v1/proposals/{pr_id}/reopen`
///
/// Reopens a previously closed PR proposal via the configured [`ForgeProvider`].
pub async fn reopen_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    require_pr_tenant_admin_access(&auth, &state, &pr_id, Some(PullRequestState::Closed)).await?;
    let reopened = state.forge.reopen_pull_request(&pr_id).await?;

    Ok(Json(MergeProposalResponse {
        proposal: reopened,
        gc_plan: GcPlan::default(),
    }))
}

/// Query parameters for `GET /api/v1/stacks`.
#[derive(Debug, Deserialize)]
pub struct ListStacksQuery {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

/// Response body for `GET /api/v1/stacks`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StacksResponse {
    pub stacks: Vec<StackSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_backend: Option<String>,
}

/// `GET /api/v1/stacks`
pub async fn list_stacks(
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListStacksQuery>,
) -> Result<Json<StacksResponse>, ServerError> {
    if let Some(ref tenant) = query.tenant {
        auth.require_tenant_access(tenant)?;
    }
    let mut stacks = state
        .state_provider
        .list_stacks(query.tenant.as_deref(), query.provider.as_deref())
        .await?;
    if query.tenant.is_none() && !auth.is_admin() {
        stacks.retain(|s| {
            let tenant = s.namespace.strip_prefix("tenant-").unwrap_or(&s.namespace);
            auth.require_tenant_access(tenant).is_ok()
        });
    }

    let flux_repo_url = tokio::fs::read_to_string(
        state
            .repo_workdir
            .join("clusters/management/flux-system/git-repository.yaml"),
    )
    .await
    .ok()
    .and_then(|raw| serde_yml::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|v| {
        v.get("spec")
            .and_then(|s| s.get("url"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
    });

    let forge_provider = tokio::fs::read_to_string(state.repo_workdir.join(".keel/config.yaml"))
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("forge")
                .and_then(|f| f.get("provider"))
                .and_then(|p| p.as_str())
                .map(str::to_string)
        });

    Ok(Json(StacksResponse {
        stacks,
        git_repo_url: flux_repo_url.or_else(|| Some(state.remote_url.clone())),
        forge_provider,
        state_backend: Some(state.backend_url.clone()),
    }))
}

/// `GET /api/v1/stacks/{namespace}/{name}/diagnostics`
///
/// Returns live PKO `Stack` conditions, workspace pod status, recent Kubernetes events, and logs.
pub async fn get_stack_diagnostics(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<StackDiagnostics>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let diag = state
        .state_provider
        .stack_diagnostics(&namespace, &name)
        .await?
        .unwrap_or(StackDiagnostics {
            name,
            namespace,
            metadata: None,
            conditions: Vec::new(),
            workspace_pod: None,
            workspace_phase: None,
            events: Vec::new(),
            logs: Vec::new(),
        });
    Ok(Json(diag))
}

/// `GET /api/v1/releases/{namespace}/{name}/diagnostics`
///
/// Returns live Flux `HelmRelease` / `Kustomization` conditions, pod status, recent Kubernetes events, and logs.
pub async fn get_release_diagnostics(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<StackDiagnostics>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let diag = state
        .state_provider
        .release_diagnostics(&namespace, &name)
        .await?
        .unwrap_or(StackDiagnostics {
            name,
            namespace,
            metadata: None,
            conditions: Vec::new(),
            workspace_pod: None,
            workspace_phase: None,
            events: Vec::new(),
            logs: Vec::new(),
        });
    Ok(Json(diag))
}

/// Response body for `DELETE /api/v1/stacks/{namespace}/{name}` and `DELETE /api/v1/releases/{namespace}/{name}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResourceResponse {
    pub id: String,
    pub namespace: String,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

/// `DELETE /api/v1/stacks/{namespace}/{name}`
///
/// Opens a PR removing a tenant PKO `Stack` directory (`tenants/<tenant>/infra/<name>` or bundle item)
/// and pruning its entries from `tenants/kustomization.yaml`.
pub async fn delete_stack(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<DeleteResourceResponse>, ServerError> {
    delete_gitops_resource_pr(auth, state, namespace, name, "infra").await
}

/// `DELETE /api/v1/releases/{namespace}/{name}`
///
/// Opens a PR removing a tenant Flux `HelmRelease` / `Kustomization` directory (`tenants/<tenant>/apps/<name>` or bundle item)
/// and pruning its entries from `tenants/kustomization.yaml`.
pub async fn delete_release(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<DeleteResourceResponse>, ServerError> {
    if matches!(
        namespace.as_str(),
        "keel-system" | "flux-system" | "kyverno"
    ) {
        return Err(ServerError::Forbidden(
            "Protected management-plane base addon cannot be deleted via API".to_string(),
        ));
    }
    delete_gitops_resource_pr(auth, state, namespace, name, "apps").await
}

async fn delete_gitops_resource_pr(
    auth: AuthSession,
    state: AppState,
    namespace: String,
    name: String,
    subdir: &str,
) -> Result<Json<DeleteResourceResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    if name == "management-cluster-core" || namespace == "flux-system" {
        return Err(ServerError::BadRequest(
            "Cannot delete protected control-plane resource".to_string(),
        ));
    }

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    validate_dns_label(tenant, "tenant")?;
    auth.require_tenant_access(tenant)?;

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let action_label = format!("delete-{name}");
    let branch = build_preview_branch(tenant, &action_label, timestamp)?;

    let (commit_sha, removed_rel_dir) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

        // Locate either `tenants/<tenant>/<subdir>/<name>` or a matching bundle item `<bundle>/<item>`
        let Some(resolved) =
            resolve_tenant_resource_dir(&state.repo_workdir, tenant, subdir, &name).await
        else {
            return Err(ServerError::NotFound(format!(
                "Resource '{name}' not found in tenant '{tenant}'"
            )));
        };
        let rel_dir = resolved.rel_dir;

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        tokio::fs::remove_dir_all(state.repo_workdir.join(&rel_dir)).await?;
        let mut staged_paths = vec![rel_dir.clone()];

        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        if tokio::fs::try_exists(&tenants_kustomization_path)
            .await
            .unwrap_or(false)
        {
            let existing = tokio::fs::read_to_string(&tenants_kustomization_path)
                .await
                .unwrap_or_default();
            let prefix_in_tenants = rel_dir
                .strip_prefix("tenants/")
                .unwrap_or(&rel_dir)
                .to_string();
            let resources: Vec<String> = serde_yml::from_str::<Value>(&existing)
                .ok()
                .and_then(|v| v.get("resources").and_then(|r| r.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .filter(|r| !r.starts_with(&format!("{prefix_in_tenants}/")))
                .collect();
            tokio::fs::write(
                &tenants_kustomization_path,
                serialize_tenants_kustomization(&resources)?,
            )
            .await?;
            staged_paths.push("tenants/kustomization.yaml".to_string());
        }

        state
            .git_runner
            .add(&state.repo_workdir, &staged_paths)
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let kind_label = if subdir == "infra" {
            "stack"
        } else {
            "release"
        };
        let commit_msg = format!("feat({tenant}): delete {kind_label} {name}");
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        (sha, rel_dir)
    };

    let kind_label = if subdir == "infra" {
        "stack"
    } else {
        "release"
    };
    let pr_req = CreatePrRequest::new(
        format!("[Delete] feat({tenant}): remove {kind_label} {name}"),
        format!(
            "[Delete] Automated decommission proposal for `{name}` in `{namespace}`\n\n- Commit: `{commit_sha}`\n- Removed: `{removed_rel_dir}`"
        ),
        &branch,
    )
    .with_tenant(tenant)
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    Ok(Json(DeleteResourceResponse {
        id: name,
        namespace,
        proposal,
        branch,
    }))
}

struct ResolvedTenantResource {
    rel_dir: String,
    bundle_item: Option<(String, String)>,
}

async fn resolve_tenant_resource_dir(
    repo_workdir: &std::path::Path,
    tenant: &str,
    subdir: &str,
    name: &str,
) -> Option<ResolvedTenantResource> {
    let direct_rel_dir = format!("tenants/{tenant}/{subdir}/{name}");
    if tokio::fs::try_exists(repo_workdir.join(&direct_rel_dir))
        .await
        .unwrap_or(false)
    {
        return Some(ResolvedTenantResource {
            rel_dir: direct_rel_dir,
            bundle_item: None,
        });
    }

    let bundles_dir = repo_workdir.join("tenants").join(tenant).join("bundles");
    let mut b_entries = tokio::fs::read_dir(&bundles_dir).await.ok()?;
    while let Ok(Some(b_entry)) = b_entries.next_entry().await {
        let b_name = b_entry.file_name();
        let Some(b_str) = b_name.to_str() else {
            continue;
        };
        if let Some(item_suffix) = name.strip_prefix(&format!("{b_str}-")) {
            let candidate = format!("tenants/{tenant}/bundles/{b_str}/{item_suffix}");
            let candidate_abs = repo_workdir.join(&candidate);
            if !tokio::fs::try_exists(&candidate_abs).await.unwrap_or(false) {
                continue;
            }
            let kind_matches = match subdir {
                "infra" => tokio::fs::try_exists(candidate_abs.join("stack.yaml"))
                    .await
                    .unwrap_or(false),
                "apps" => {
                    tokio::fs::try_exists(candidate_abs.join("helmrelease.yaml"))
                        .await
                        .unwrap_or(false)
                        || tokio::fs::try_exists(candidate_abs.join("helmrelease-canary.yaml"))
                            .await
                            .unwrap_or(false)
                        || tokio::fs::try_exists(candidate_abs.join("manifests.yaml"))
                            .await
                            .unwrap_or(false)
                }
                _ => true,
            };
            if kind_matches {
                return Some(ResolvedTenantResource {
                    rel_dir: candidate,
                    bundle_item: Some((b_str.to_string(), item_suffix.to_string())),
                });
            }
        }
    }
    None
}

/// `GET /api/v1/stacks/{namespace}/{name}/config`
///
/// Reads `tenants/<tenant>/infra/<name>/values.yaml` (or matching bundle item) + `Pulumi.yaml` / `stack.yaml`
/// so the UI can pre-fill the Catalog Wizard RJSF form when editing an existing infra stack.
pub async fn get_stack_config(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReleaseConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    let Some(resolved) =
        resolve_tenant_resource_dir(&state.repo_workdir, tenant, "infra", &name).await
    else {
        return Err(ServerError::NotFound(format!(
            "Stack '{name}' not found in tenant '{tenant}'"
        )));
    };
    let infra_dir = state.repo_workdir.join(&resolved.rel_dir);

    let values_path = infra_dir.join("values.yaml");
    let values = if tokio::fs::try_exists(&values_path).await.unwrap_or(false) {
        let raw = tokio::fs::read_to_string(&values_path).await?;
        serde_yml::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    let mut catalog_id = name.clone();
    if tokio::fs::try_exists(
        state
            .repo_workdir
            .join("catalog/infra")
            .join(&name)
            .join("catalog.yaml"),
    )
    .await
    .unwrap_or(false)
    {
        catalog_id = name.clone();
    } else {
        let stack_path = infra_dir.join("stack.yaml");
        if let Ok(raw) = tokio::fs::read_to_string(&stack_path).await
            && let Ok(doc) = serde_yml::from_str::<Value>(&raw)
            && let Some(provider) = doc
                .pointer("/metadata/labels/keel.io~1provider")
                .and_then(Value::as_str)
            && let Ok(mut entries) =
                tokio::fs::read_dir(state.repo_workdir.join("catalog/infra")).await
        {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let candidate = entry.path().join("catalog.yaml");
                if let Ok(cat_raw) = tokio::fs::read_to_string(&candidate).await
                    && let Ok(CatalogManifest::InfraBlueprint(bp)) = parse_catalog(&cat_raw)
                    && bp.metadata.provider == provider
                {
                    catalog_id = bp.metadata.id;
                    break;
                }
            }
        }
    }

    Ok(Json(ReleaseConfigResponse {
        catalog_id,
        tenant: tenant.to_string(),
        name,
        version: None,
        target_cluster: None,
        target_namespace: None,
        canary: None,
        action_engine: None,
        values,
    }))
}

/// Response body for `GET /api/v1/releases/{namespace}/{name}/config`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseConfigResponse {
    pub catalog_id: String,
    pub tenant: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_engine: Option<String>,
    pub values: Value,
}

/// `GET /api/v1/releases/{namespace}/{name}/config`
///
/// Reads the persisted GitOps configuration (`values.yaml` + `helmrelease*.yaml` / `flux-kustomization.yaml` / `stack.yaml`)
/// for an existing application release so the Catalog Wizard can pre-fill an update PR without exposing secrets.
pub async fn get_release_config(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReleaseConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    let app_dir =
        match resolve_tenant_resource_dir(&state.repo_workdir, tenant, "apps", &name).await {
            Some(resolved) => state.repo_workdir.join(&resolved.rel_dir),
            None => state
                .repo_workdir
                .join("tenants")
                .join(tenant)
                .join("apps")
                .join(&name),
        };

    let (values, hr_doc, is_canary, action_engine) = {
        let _guard = state.git_lock.lock().await;
        let values_path = app_dir.join("values.yaml");
        let hr_path = app_dir.join("helmrelease.yaml");
        let hr_canary_path = app_dir.join("helmrelease-canary.yaml");
        let canary_cr_path = app_dir.join("canary.yaml");
        let fk_path = app_dir.join("flux-kustomization.yaml");
        let stack_path = app_dir.join("stack.yaml");
        let manifests_path = app_dir.join("manifests.yaml");
        let cm_path = app_dir.join("values-configmap.yaml");

        let has_values = tokio::fs::try_exists(&values_path).await.unwrap_or(false);
        let has_canary_hr = tokio::fs::try_exists(&hr_canary_path)
            .await
            .unwrap_or(false);
        let has_canary_cr = tokio::fs::try_exists(&canary_cr_path)
            .await
            .unwrap_or(false);
        let has_fk = tokio::fs::try_exists(&fk_path).await.unwrap_or(false);
        let has_stack = tokio::fs::try_exists(&stack_path).await.unwrap_or(false);
        let has_manifests = tokio::fs::try_exists(&manifests_path)
            .await
            .unwrap_or(false);
        let has_cm = tokio::fs::try_exists(&cm_path).await.unwrap_or(false);

        let active_hr_path = if has_canary_hr {
            Some(hr_canary_path)
        } else if tokio::fs::try_exists(&hr_path).await.unwrap_or(false) {
            Some(hr_path)
        } else if has_fk {
            Some(fk_path)
        } else if has_stack {
            Some(stack_path)
        } else {
            None
        };

        if !has_values && active_hr_path.is_none() && !has_manifests && !has_cm {
            return Err(ServerError::NotFound(format!(
                "Release '{name}' in namespace '{namespace}' not found in GitOps repository"
            )));
        }

        let values = if has_values {
            let raw = tokio::fs::read_to_string(&values_path).await?;
            match serde_yml::from_str::<Value>(&raw) {
                Ok(Value::Object(map)) => Value::Object(map),
                _ => Value::Object(serde_json::Map::new()),
            }
        } else if has_cm {
            let raw = tokio::fs::read_to_string(&cm_path).await?;
            serde_yml::from_str::<Value>(&raw)
                .ok()
                .and_then(|d| d.get("data").cloned())
                .filter(Value::is_object)
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
        } else {
            Value::Object(serde_json::Map::new())
        };

        let hr_doc = if let Some(path) = active_hr_path {
            let raw = tokio::fs::read_to_string(&path).await?;
            serde_yml::from_str::<Value>(&raw).ok()
        } else {
            None
        };

        let engine = if has_stack && has_manifests {
            Some("pulumi".to_string())
        } else if has_fk || has_manifests {
            Some("flux".to_string())
        } else {
            None
        };

        (values, hr_doc, has_canary_hr || has_canary_cr, engine)
    };

    let chart_name = hr_doc
        .as_ref()
        .and_then(|d| {
            d.pointer("/spec/chart/spec/chart")
                .or_else(|| d.pointer("/metadata/labels/keel.io~1catalog-id"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let version = hr_doc
        .as_ref()
        .and_then(|d| d.pointer("/spec/chart/spec/version"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let target_cluster = hr_doc
        .as_ref()
        .and_then(|d| {
            d.pointer("/spec/kubeConfig/secretRef/name")
                .or_else(|| d.pointer("/spec/config/kubeconfigSecretName"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let target_namespace = hr_doc
        .as_ref()
        .and_then(|d| d.pointer("/spec/targetNamespace"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut catalog_id = chart_name.clone().unwrap_or_else(|| name.clone());
    if let Some(ref chart) = chart_name {
        let direct = state
            .repo_workdir
            .join("catalog/apps")
            .join(chart)
            .join("catalog.yaml");
        if tokio::fs::try_exists(&direct).await.unwrap_or(false) {
            catalog_id = chart.clone();
        } else if let Ok(mut entries) =
            tokio::fs::read_dir(state.repo_workdir.join("catalog/apps")).await
        {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let candidate = entry.path().join("catalog.yaml");
                if let Ok(raw) = tokio::fs::read_to_string(&candidate).await
                    && let Ok(CatalogManifest::Application(app)) = parse_catalog(&raw)
                    && app.spec.upstream.chart.as_deref() == Some(chart.as_str())
                {
                    catalog_id = app.metadata.id;
                    break;
                }
            }
        }
    }

    Ok(Json(ReleaseConfigResponse {
        catalog_id,
        tenant: tenant.to_string(),
        name,
        version,
        target_cluster,
        target_namespace,
        canary: if is_canary { Some(true) } else { None },
        action_engine,
        values,
    }))
}

/// Response body for `GET /api/v1/bundles/{namespace}/{name}/config`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleConfigResponse {
    pub tenant: String,
    pub name: String,
    pub values: Value,
}

/// `GET /api/v1/bundles/{namespace}/{name}/config`
///
/// Reads non-secret `values.yaml` per item under `tenants/<tenant>/bundles/<name>/<item>/`
/// so the UI can pre-fill the RJSF form when editing an existing bundle deployment.
pub async fn get_bundle_config(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<BundleConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    let bundle_dir = state
        .repo_workdir
        .join("tenants")
        .join(tenant)
        .join("bundles")
        .join(&name);

    if !tokio::fs::try_exists(&bundle_dir).await.unwrap_or(false) {
        return Err(ServerError::NotFound(format!(
            "Bundle '{name}' not found in tenant '{tenant}'"
        )));
    }

    let mut values_map = serde_json::Map::new();
    let mut entries = tokio::fs::read_dir(&bundle_dir).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let item_path = entry.path();
        if !item_path.is_dir() {
            continue;
        }
        let Some(item_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let values_path = item_path.join("values.yaml");
        let item_values = if tokio::fs::try_exists(&values_path).await.unwrap_or(false) {
            let raw = tokio::fs::read_to_string(&values_path).await?;
            serde_yml::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
        } else {
            json!({})
        };
        values_map.insert(item_id, item_values);
    }

    Ok(Json(BundleConfigResponse {
        tenant: tenant.to_string(),
        name,
        values: Value::Object(values_map),
    }))
}

/// Response body for `POST /api/v1/stacks/{namespace}/{name}/restart`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartStackResponse {
    pub restarted: bool,
    pub namespace: String,
    pub name: String,
    pub message: String,
}

/// `POST /api/v1/stacks/{namespace}/{name}/restart`
///
/// Triggers a `kubectl rollout restart` equivalent on `<name>-workspace` StatefulSet.
pub async fn restart_stack_workspace(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<RestartStackResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;

    let message = state
        .state_provider
        .restart_stack_workspace(&namespace, &name)
        .await?;
    Ok(Json(RestartStackResponse {
        restarted: true,
        namespace,
        name,
        message,
    }))
}

fn default_ssh_user_for_provider(provider: &str) -> &'static str {
    match provider.trim().to_ascii_lowercase().as_str() {
        "aws" | "proxmox" => "ubuntu",
        "azure" => "azureuser",
        "gcp" => "debian",
        "hetzner" => "root",
        _ => "ubuntu",
    }
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

pub(crate) fn clean_candidate_ssh_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if trimmed.starts_with('[')
        && let Ok(json_val) = serde_json::from_str::<Value>(trimmed)
    {
        let mut candidates = Vec::new();
        collect_hosts_from_json_value(&json_val, &mut candidates);
        return pick_preferred_host(candidates);
    }
    clean_single_ssh_host(trimmed)
}

fn extract_ssh_host_from_yaml_value(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => clean_candidate_ssh_host(s),
        Value::Array(_) => {
            let mut candidates = Vec::new();
            collect_hosts_from_json_value(val, &mut candidates);
            pick_preferred_host(candidates)
        }
        _ => None,
    }
}

pub(crate) fn clean_candidate_ssh_user(raw: &str) -> Option<String> {
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

fn parse_candidate_ssh_port(val: &Value) -> Option<u16> {
    match val {
        Value::Number(n) => n
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0),
        Value::String(s) => s
            .trim()
            .trim_matches('"')
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0),
        _ => None,
    }
}

async fn resolve_stack_ssh_info(
    state: &AppState,
    auth: &AuthSession,
    namespace: &str,
    name: &str,
) -> Result<(StackSshInfoResponse, Option<String>), ServerError> {
    validate_dns_label(namespace, "namespace")?;
    validate_dns_label(name, "name")?;
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(namespace);
    validate_dns_label(tenant, "tenant")?;
    auth.require_tenant_access(tenant)?;

    let saved_ssh_key = if let Ok(key) = state.secret_store.get_or_create_session_key().await {
        state
            .secret_store
            .get_user_git_creds(&auth.claims.sub, &key)
            .await
            .ok()
            .flatten()
            .and_then(|c| c.ssh_private_key)
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
    } else {
        None
    };
    let has_saved_ssh_key = saved_ssh_key.is_some();

    let live_outputs = state.state_provider.stack_outputs(namespace, name).await;

    let infra_dir =
        match resolve_tenant_resource_dir(&state.repo_workdir, tenant, "infra", name).await {
            Some(resolved) => state.repo_workdir.join(resolved.rel_dir),
            None => state
                .repo_workdir
                .join("tenants")
                .join(tenant)
                .join("infra")
                .join(name),
        };

    let stack_doc = tokio::fs::read_to_string(infra_dir.join("stack.yaml"))
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<Value>(&raw).ok());
    let values_doc = tokio::fs::read_to_string(infra_dir.join("values.yaml"))
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<Value>(&raw).ok());
    let pulumi_doc = tokio::fs::read_to_string(infra_dir.join("Pulumi.yaml"))
        .await
        .ok()
        .and_then(|raw| serde_yml::from_str::<Value>(&raw).ok());

    let mut provider = stack_doc
        .as_ref()
        .and_then(|d| {
            d.pointer("/metadata/labels/keel.io~1provider")
                .or_else(|| d.pointer("/spec/config/keel:provider"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);

    if provider.is_none()
        && let Ok(stacks) = state.state_provider.list_stacks(Some(tenant), None).await
    {
        provider = stacks
            .into_iter()
            .find(|s| s.name == name)
            .and_then(|s| s.provider);
    }

    let provider = provider.unwrap_or_else(|| {
        let lower = name.to_ascii_lowercase();
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
        && let Some(obj) = values_doc.as_ref().and_then(Value::as_object)
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
            .as_ref()
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(Value::as_object)
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
            .as_ref()
            .and_then(|d| d.get("outputs"))
            .and_then(Value::as_object)
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
    let ssh_host = ssh_host.unwrap_or_else(|| format!("{name}.{tenant}.local"));

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
        && let Some(obj) = values_doc.as_ref().and_then(Value::as_object)
    {
        for key in USER_KEYS {
            if let Some(val) = obj.get(*key).and_then(Value::as_str)
                && let Some(cleaned) = clean_candidate_ssh_user(val)
            {
                ssh_user = Some(cleaned);
                break;
            }
        }
    }
    if ssh_user.is_none()
        && let Some(obj) = stack_doc
            .as_ref()
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(Value::as_object)
    {
        for key in USER_KEYS {
            if let Some(val) = obj.get(*key).and_then(Value::as_str)
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
            && let Some(p) = parse_candidate_ssh_port(&Value::String(val.clone()))
        {
            ssh_port = Some(p);
            break;
        }
    }
    if ssh_port.is_none()
        && let Some(obj) = values_doc.as_ref().and_then(Value::as_object)
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
            .as_ref()
            .and_then(|d| d.pointer("/spec/config"))
            .and_then(Value::as_object)
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
    let cli_command = format!("keel infra ssh {name} --tenant {tenant}");
    let cli_exec_command = format!("keel infra ssh {name} --tenant {tenant} --exec");

    Ok((
        StackSshInfoResponse {
            stack_name: name.to_string(),
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
        },
        saved_ssh_key,
    ))
}

/// `GET /api/v1/stacks/{namespace}/{name}/ssh-info`
///
/// Resolves SSH target metadata (host, user, port, copyable commands, and whether the user has a
/// saved SSH key) for a provisioned VM stack without exposing any private key material (`NFR-SEC-05`).
pub async fn get_stack_ssh_info(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<StackSshInfoResponse>, ServerError> {
    let (info, _saved_key) = resolve_stack_ssh_info(&state, &auth, &namespace, &name).await?;
    Ok(Json(info))
}

/// `GET /api/v1/stacks/{namespace}/{name}/ssh/ws`
///
/// Upgrades an authenticated tenant-authorized request to an interactive WebSocket SSH terminal bridge.
pub async fn stack_ssh_ws(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ServerError> {
    let (ssh_info, saved_ssh_key) =
        resolve_stack_ssh_info(&state, &auth, &namespace, &name).await?;
    tracing::info!(
        target: "keel::audit",
        user = %auth.claims.sub,
        tenant = %ssh_info.tenant,
        namespace = %ssh_info.namespace,
        stack = %ssh_info.stack_name,
        ssh_host = %ssh_info.ssh_host,
        ssh_user = %ssh_info.ssh_user,
        ssh_port = ssh_info.ssh_port,
        "WebSocket SSH session initiated"
    );
    let user_sub = auth.claims.sub.clone();
    Ok(ws.on_upgrade(move |socket| {
        crate::ssh::handle_stack_ssh_ws(socket, ssh_info, saved_ssh_key, user_sub)
    }))
}

/// Response body for `POST /api/v1/releases/{namespace}/{name}/reconcile`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileReleaseResponse {
    pub reconciled: bool,
    pub namespace: String,
    pub name: String,
    pub message: String,
}

/// `POST /api/v1/releases/{namespace}/{name}/reconcile`
///
/// Annotates a Flux `HelmRelease` (or `Kustomization`) with `reconcile.fluxcd.io/requestedAt`
/// to trigger immediate reconciliation.
pub async fn reconcile_release(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReconcileReleaseResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;

    let message = state
        .state_provider
        .reconcile_release(&namespace, &name)
        .await?;
    Ok(Json(ReconcileReleaseResponse {
        reconciled: true,
        namespace,
        name,
        message,
    }))
}

/// Query parameters for `GET /api/v1/releases`.
#[derive(Debug, Deserialize)]
pub struct ListReleasesQuery {
    #[serde(default)]
    pub tenant: Option<String>,
}

/// Response body for `GET /api/v1/releases`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleasesResponse {
    pub releases: Vec<ReleaseSummary>,
}

/// `GET /api/v1/releases`
pub async fn list_releases(
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListReleasesQuery>,
) -> Result<Json<ReleasesResponse>, ServerError> {
    if let Some(ref tenant) = query.tenant {
        auth.require_tenant_access(tenant)?;
    }
    let mut releases = state
        .state_provider
        .list_releases(query.tenant.as_deref())
        .await?;
    if query.tenant.is_none() && !auth.is_admin() {
        releases.retain(|r| {
            let tenant = r.namespace.strip_prefix("tenant-").unwrap_or(&r.namespace);
            auth.require_tenant_access(tenant).is_ok()
        });
    }
    Ok(Json(ReleasesResponse { releases }))
}

/// Query parameters for `GET /api/v1/events/stream`.
#[derive(Debug, Deserialize)]
pub struct EventsStreamQuery {
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `GET /api/v1/events/stream`
///
/// Streams real-time `CrdWatchEvent` JSON frames over Server-Sent Events (SSE).
pub async fn events_stream(
    _auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<EventsStreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let raw_stream: std::pin::Pin<Box<dyn Stream<Item = CrdWatchEvent> + Send>> =
        if let Some(ns) = query.namespace.filter(|s| !s.trim().is_empty()) {
            Box::pin(state.telemetry.subscribe_namespace_stream(ns))
        } else {
            Box::pin(state.telemetry.subscribe_stream())
        };

    let sse_stream = raw_stream.map(|ev| {
        let data = ev.to_sse_json().unwrap_or_else(|_| "{}".to_string());
        Ok(Event::default().event("crd-watch").data(data))
    });

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

/// Request body for `PUT /api/v1/profile/git-credentials`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateGitCredentialsRequest {
    #[serde(default)]
    pub ssh_private_key: Option<String>,
    #[serde(default)]
    pub personal_access_token: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// Response body for `PUT /api/v1/profile/git-credentials` (never echoes secret material).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateGitCredentialsResponse {
    pub stored: bool,
    pub namespace: String,
    pub secret_name: String,
    pub has_ssh_key: bool,
    pub has_personal_access_token: bool,
}

/// `PUT /api/v1/profile/git-credentials`
///
/// Encrypts the user's SSH private key or Forge PAT with `AES-256-GCM` and persists it
/// in `keel-system/user-git-creds-<sub_hash>` via `SecretStateStore::put_user_git_creds`.
/// Never logs or echoes secret material (`NFR-SEC-05`).
pub async fn put_git_credentials(
    auth: AuthSession,
    State(state): State<AppState>,
    Json(req): Json<UpdateGitCredentialsRequest>,
) -> Result<Json<UpdateGitCredentialsResponse>, ServerError> {
    let ssh_private_key = req
        .ssh_private_key
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let personal_access_token = req
        .personal_access_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let username = req
        .username
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if ssh_private_key.is_none() && personal_access_token.is_none() {
        return Err(ServerError::BadRequest(
            "At least one of sshPrivateKey or personalAccessToken must be provided".to_string(),
        ));
    }

    let has_ssh_key = ssh_private_key.is_some();
    let has_personal_access_token = personal_access_token.is_some();

    let creds = UserGitCredentials::new(ssh_private_key, personal_access_token, username);
    let encryption_key = state.secret_store.get_or_create_session_key().await?;
    state
        .secret_store
        .put_user_git_creds(&auth.claims.sub, &creds, &encryption_key)
        .await?;

    let secret_name = user_git_creds_secret_name(&auth.claims.sub)?;
    tracing::info!(
        sub = %auth.claims.sub,
        secret_name = %secret_name,
        has_ssh_key,
        has_personal_access_token,
        "Stored AES-256-GCM encrypted user Git credentials"
    );

    Ok(Json(UpdateGitCredentialsResponse {
        stored: true,
        namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
        secret_name,
        has_ssh_key,
        has_personal_access_token,
    }))
}

/// Request body for `POST /api/v1/management/addons/{id}/upgrade`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeManagementAddonRequest {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub values: Value,
}

/// Response body for `POST /api/v1/management/addons/{id}/upgrade` and `rollback`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementAddonProposalResponse {
    pub id: String,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

async fn find_core_addon_manifest(
    repo_workdir: &Path,
    id: &str,
) -> Result<(PathBuf, String, Value), ServerError> {
    let dir = repo_workdir.join("clusters/management/core-addons");
    let mut entries = tokio::fs::read_dir(&dir).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ServerError::NotFound(format!(
                "Core addon '{id}' not found under clusters/management/core-addons/"
            ))
        } else {
            ServerError::Io(err)
        }
    })?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"));
        if !is_yaml {
            continue;
        }
        let raw = tokio::fs::read_to_string(&path).await?;
        let Ok(doc) = serde_yml::from_str::<Value>(&raw) else {
            continue;
        };
        let meta_name = doc
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str());
        if meta_name == Some(id) {
            let file_name = path
                .file_name()
                .and_then(|f| f.to_str())
                .ok_or_else(|| ServerError::Internal("Invalid core addon filename".to_string()))?;
            let rel_path = format!("clusters/management/core-addons/{file_name}");
            return Ok((path, rel_path, doc));
        }
    }

    Err(ServerError::NotFound(format!(
        "Core addon '{id}' not found under clusters/management/core-addons/"
    )))
}

fn merge_json_values(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target_map), Value::Object(patch_map)) => {
            for (k, v) in patch_map {
                match target_map.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        merge_json_values(existing, v);
                    }
                    _ => {
                        target_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (target_slot, patch_val) => {
            *target_slot = patch_val.clone();
        }
    }
}

/// `POST /api/v1/management/addons/{id}/upgrade`
///
/// Updates a protected management-plane core addon manifest under
/// `clusters/management/core-addons/` on a new branch and opens a PR via `ForgeProvider`.
pub async fn upgrade_management_addon(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<UpgradeManagementAddonRequest>,
) -> Result<(StatusCode, Json<ManagementAddonProposalResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = format!("chore/core-addon-{id}-{timestamp}");

    let (commit_sha, rel_path) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

        let (abs_path, rel_path, mut doc) =
            find_core_addon_manifest(&state.repo_workdir, &id).await?;

        if let Some(version) = req
            .version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let spec_obj = doc
                .as_object_mut()
                .and_then(|root| {
                    root.entry("spec")
                        .or_insert_with(|| Value::Object(Default::default()))
                        .as_object_mut()
                })
                .ok_or_else(|| {
                    ServerError::Internal(
                        "Core addon manifest root or spec is not a mapping".to_string(),
                    )
                })?;
            let chart_obj = spec_obj
                .entry("chart")
                .or_insert_with(|| Value::Object(Default::default()))
                .as_object_mut()
                .ok_or_else(|| {
                    ServerError::Internal(
                        "Core addon manifest spec.chart is not a mapping".to_string(),
                    )
                })?;
            let chart_spec_obj = chart_obj
                .entry("spec")
                .or_insert_with(|| Value::Object(Default::default()))
                .as_object_mut()
                .ok_or_else(|| {
                    ServerError::Internal(
                        "Core addon manifest spec.chart.spec is not a mapping".to_string(),
                    )
                })?;
            chart_spec_obj.insert("version".to_string(), Value::String(version.to_string()));
        }

        if req.values.is_object() {
            let spec_obj = doc
                .as_object_mut()
                .and_then(|root| {
                    root.entry("spec")
                        .or_insert_with(|| Value::Object(Default::default()))
                        .as_object_mut()
                })
                .ok_or_else(|| {
                    ServerError::Internal(
                        "Core addon manifest root or spec is not a mapping".to_string(),
                    )
                })?;
            match spec_obj.get_mut("values") {
                Some(existing) if existing.is_object() => {
                    merge_json_values(existing, &req.values);
                }
                _ => {
                    spec_obj.insert("values".to_string(), req.values.clone());
                }
            }
        }

        let metadata_obj = doc
            .as_object_mut()
            .and_then(|root| {
                root.entry("metadata")
                    .or_insert_with(|| Value::Object(Default::default()))
                    .as_object_mut()
            })
            .ok_or_else(|| {
                ServerError::Internal("Core addon manifest metadata is not a mapping".to_string())
            })?;
        let labels_obj = metadata_obj
            .entry("labels")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| {
                ServerError::Internal("Core addon manifest labels is not a mapping".to_string())
            })?;
        labels_obj.insert(
            "keel.io/protect".to_string(),
            Value::String("true".to_string()),
        );

        let updated_yaml = serde_yml::to_string(&doc).map_err(|e| {
            ServerError::Internal(format!("Failed to serialize core addon YAML: {e}"))
        })?;

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;
        tokio::fs::write(&abs_path, &updated_yaml).await?;
        state
            .git_runner
            .add(&state.repo_workdir, [rel_path.as_str()])
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("chore(core-addon): upgrade {id}");
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        (sha, rel_path)
    };

    let pr_req = CreatePrRequest::new(
        format!("chore(core-addon): upgrade {id}"),
        format!(
            "Automated management-plane core addon upgrade for `{id}`\n\n- Commit: `{commit_sha}`\n- Path: `{rel_path}`"
        ),
        &branch,
    )
    .with_tenant("management")
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    Ok((
        StatusCode::CREATED,
        Json(ManagementAddonProposalResponse {
            id,
            proposal,
            branch,
        }),
    ))
}

/// `POST /api/v1/management/addons/{id}/rollback`
///
/// Reverts a management-plane core addon manifest under `clusters/management/core-addons/`
/// to its previous Git revision on a new branch and opens a PR via `ForgeProvider`.
pub async fn rollback_management_addon(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<(StatusCode, Json<ManagementAddonProposalResponse>), ServerError> {
    auth.require_admin()?;
    validate_dns_label(&id, "id")?;

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = format!("revert/core-addon-{id}-{timestamp}");

    let (commit_sha, prev_sha, rel_path) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

        let (abs_path, rel_path, _doc) = find_core_addon_manifest(&state.repo_workdir, &id).await?;

        let commits = state
            .git_runner
            .log_file_commits(&state.repo_workdir, &rel_path, 2)
            .await?;
        if commits.len() < 2 {
            return Err(ServerError::BadRequest(
                "No previous revision found to roll back to".to_string(),
            ));
        }
        let prev_sha = commits[1].clone();

        let prev_yaml = state
            .git_runner
            .show_file_at_ref(&state.repo_workdir, &prev_sha, &rel_path)
            .await?
            .ok_or_else(|| {
                ServerError::Internal(format!(
                    "Previous revision '{prev_sha}:{rel_path}' not found"
                ))
            })?;

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;
        tokio::fs::write(&abs_path, &prev_yaml).await?;
        state
            .git_runner
            .add(&state.repo_workdir, [rel_path.as_str()])
            .await?;

        let author = CommitAuthor::new(
            auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
            auth.claims.email.as_deref().unwrap_or("keel@keel.local"),
        );
        let commit_msg = format!("revert(core-addon): rollback {id} to {prev_sha}");
        let sha = state
            .git_runner
            .commit(&state.repo_workdir, &commit_msg, &author)
            .await?;
        state
            .git_runner
            .push(&state.repo_workdir, "origin", &branch)
            .await?;

        let _ = state
            .git_runner
            .checkout_branch(&state.repo_workdir, "main")
            .await;
        (sha, prev_sha, rel_path)
    };

    let pr_req = CreatePrRequest::new(
        format!("revert(core-addon): rollback {id}"),
        format!(
            "Automated management-plane core addon rollback for `{id}`\n\n- Target revision: `{prev_sha}`\n- Commit: `{commit_sha}`\n- Path: `{rel_path}`"
        ),
        &branch,
    )
    .with_tenant("management")
    .with_author(format!(
        "{} <{}>",
        auth.claims.name.as_deref().unwrap_or(&auth.claims.sub),
        auth.claims.email.as_deref().unwrap_or("keel@keel.local")
    ));
    let proposal = state.forge.create_pull_request(pr_req).await?;

    Ok((
        StatusCode::CREATED,
        Json(ManagementAddonProposalResponse {
            id,
            proposal,
            branch,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keel_core::{AuthPhase, KEEL_SYSTEM_NAMESPACE};
    use keel_git::{CommitAuthor, GitRunner, LocalGitProvider};
    use keel_kube::{InMemorySecretStore, TelemetryMux};
    use tempfile::TempDir;

    use super::*;
    use crate::auth::SessionClaims;

    fn test_auth() -> AuthSession {
        AuthSession {
            claims: SessionClaims {
                sub: "admin@keel.local".to_string(),
                email: Some("admin@keel.local".to_string()),
                name: Some("Keel Admin".to_string()),
                groups: vec!["keel-admins".to_string()],
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        }
    }

    async fn setup_test_state() -> (TempDir, AppState, PathBuf) {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let bare_repo = temp_dir.path().join("remote.git");
        let workdir = temp_dir.path().join("workdir");

        let git = GitRunner::new();
        git.init_bare(&bare_repo).await.expect("init bare");
        git.clone(bare_repo.to_str().unwrap(), &workdir)
            .await
            .expect("clone");

        tokio::fs::create_dir_all(workdir.join("catalog/core/s3-state-backend"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("catalog/core/s3-state-backend/catalog.yaml"),
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: s3-state-backend
  name: SeaweedFS S3 Backend
  description: Seed S3-compatible state backend
  category: storage
  runtime: helm
spec:
  upstream:
    repoURL: https://seaweedfs.github.io/seaweedfs/helm
    chart: seaweedfs
    version: "4.53.0"
  schema:
    type: object
    properties:
      s3:
        type: object
"#,
        )
        .await
        .unwrap();

        tokio::fs::create_dir_all(workdir.join("clusters/management/core-addons"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("clusters/management/core-addons/s3-seaweedfs.yaml"),
            r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: s3-state-backend
  namespace: keel-system
  labels:
    app.kubernetes.io/managed-by: keel
    keel.io/protect: "true"
spec:
  interval: 10m
  chart:
    spec:
      chart: seaweedfs
      version: "4.53.0"
  values:
    s3:
      enabled: true
      enableAuth: true
    master:
      replicas: 1
"#,
        )
        .await
        .unwrap();

        git.add(&workdir, ["."]).await.unwrap();
        git.commit(
            &workdir,
            "chore: initial core addon seed",
            &CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
        git.push(&workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        let forge = Arc::new(LocalGitProvider::new(&bare_repo));
        let secret_store = Arc::new(InMemorySecretStore::new());
        let state = AppState::new(
            forge,
            secret_store,
            TelemetryMux::default(),
            git,
            &workdir,
            bare_repo.to_string_lossy().to_string(),
            AuthPhase::OidcActive,
        );

        (temp_dir, state, workdir)
    }

    #[tokio::test]
    async fn get_catalog_returns_core_addons() {
        let (_tmp, state, _workdir) = setup_test_state().await;
        let Json(resp) = get_catalog(test_auth(), State(state))
            .await
            .expect("get_catalog succeeds");

        assert_eq!(resp.core_addons.len(), 1);
        assert_eq!(resp.core_addons[0].metadata.id, "s3-state-backend");
    }

    #[tokio::test]
    async fn delete_release_rejects_protected_management_namespaces() {
        let (_tmp, state, _workdir) = setup_test_state().await;

        for ns in [KEEL_SYSTEM_NAMESPACE, "flux-system", "kyverno"] {
            let err = delete_release(
                test_auth(),
                State(state.clone()),
                AxumPath((ns.to_string(), "s3-state-backend".to_string())),
            )
            .await
            .expect_err("protected namespace must be rejected");

            match err {
                ServerError::Forbidden(msg) => {
                    assert_eq!(
                        msg,
                        "Protected management-plane base addon cannot be deleted via API"
                    );
                }
                other => panic!("expected Forbidden, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn upgrade_management_addon_creates_branch_and_pr() {
        let (_tmp, state, workdir) = setup_test_state().await;

        let req = UpgradeManagementAddonRequest {
            version: Some("4.54.0".to_string()),
            values: json!({
                "s3": {
                    "enableAuth": false
                },
                "filer": {
                    "replicas": 2
                }
            }),
        };

        let (status, Json(resp)) = upgrade_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
            Json(req),
        )
        .await
        .expect("upgrade_management_addon succeeds");

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(resp.id, "s3-state-backend");
        assert!(
            resp.branch
                .starts_with("chore/core-addon-s3-state-backend-")
        );
        assert_eq!(resp.proposal.source_branch, resp.branch);

        // Inspect updated file on the pushed branch
        state
            .git_runner
            .checkout_branch(&workdir, &resp.branch)
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(
            workdir.join("clusters/management/core-addons/s3-seaweedfs.yaml"),
        )
        .await
        .unwrap();
        let doc: Value = serde_yml::from_str(&raw).unwrap();
        assert_eq!(doc["spec"]["chart"]["spec"]["version"], "4.54.0");
        assert_eq!(doc["spec"]["values"]["s3"]["enabled"], true);
        assert_eq!(doc["spec"]["values"]["s3"]["enableAuth"], false);
        assert_eq!(doc["spec"]["values"]["filer"]["replicas"], 2);
        assert_eq!(doc["metadata"]["labels"]["keel.io/protect"], "true");
    }

    #[tokio::test]
    async fn rollback_management_addon_requires_two_commits_and_reverts_when_present() {
        let (_tmp, state, workdir) = setup_test_state().await;

        // With only 1 commit, rollback must return BadRequest (400)
        let err = rollback_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
        )
        .await
        .expect_err("rollback with 1 commit should fail");
        match err {
            ServerError::BadRequest(msg) => {
                assert_eq!(msg, "No previous revision found to roll back to");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }

        // Create a second commit on main modifying s3-seaweedfs.yaml
        let addon_path = workdir.join("clusters/management/core-addons/s3-seaweedfs.yaml");
        let original_yaml = tokio::fs::read_to_string(&addon_path).await.unwrap();
        let updated_yaml = original_yaml.replace("4.53.0", "4.54.0");
        tokio::fs::write(&addon_path, &updated_yaml).await.unwrap();
        state
            .git_runner
            .add(
                &workdir,
                ["clusters/management/core-addons/s3-seaweedfs.yaml"],
            )
            .await
            .unwrap();
        state
            .git_runner
            .commit(
                &workdir,
                "chore(core-addon): bump s3-state-backend to 4.54.0",
                &CommitAuthor::new("Keel Admin", "admin@keel.local"),
            )
            .await
            .unwrap();
        state
            .git_runner
            .push(&workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();

        // Now rollback should succeed and restore 4.53.0 on a revert branch + PR
        let (status, Json(resp)) = rollback_management_addon(
            test_auth(),
            State(state.clone()),
            AxumPath("s3-state-backend".to_string()),
        )
        .await
        .expect("rollback with 2 commits succeeds");

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(resp.id, "s3-state-backend");
        assert!(
            resp.branch
                .starts_with("revert/core-addon-s3-state-backend-")
        );
        assert_eq!(resp.proposal.source_branch, resp.branch);

        state
            .git_runner
            .checkout_branch(&workdir, &resp.branch)
            .await
            .unwrap();
        let reverted_raw = tokio::fs::read_to_string(&addon_path).await.unwrap();
        assert_eq!(reverted_raw, original_yaml);
    }

    #[test]
    fn serialize_tenants_kustomization_ends_with_newline_and_preserves_prior_entries() {
        let initial = serialize_tenants_kustomization(&[
            "default/rbac.yaml".to_string(),
            "default/apps/e2e-lab-podinfo/helmrelease-canary.yaml".to_string(),
        ])
        .unwrap();
        assert!(initial.ends_with('\n'));

        let updated = serialize_tenants_kustomization(&[
            "default/rbac.yaml".to_string(),
            "default/apps/e2e-lab-podinfo/helmrelease-canary.yaml".to_string(),
            "acme/apps/acme-podinfo/helmrelease.yaml".to_string(),
        ])
        .unwrap();
        assert!(updated.ends_with('\n'));
        assert!(
            updated.starts_with(&initial),
            "appending a new tenant entry must not alter existing lines"
        );

        let parsed: serde_json::Value = serde_yml::from_str(&updated).unwrap();
        assert_eq!(parsed["resources"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn register_and_deploy_manifest_catalog_app_with_flux_and_pulumi_engines() {
        let (_tmp, state, workdir) = setup_test_state().await;

        let raw_manifests = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: ${app_name:=demo}-cfg
data:
  LOG_LEVEL: ${log_level:=info}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ${app_name:=demo}
spec:
  replicas: ${replicas:=2}
"#;

        let reg_req = RegisterCatalogAppRequest {
            kind: "app".to_string(),
            id: "raw-demo".to_string(),
            name: "Raw Demo App".to_string(),
            category: "Workloads".to_string(),
            runtime: Some(AppRuntime::Manifest),
            repo_url: None,
            chart: None,
            version: None,
            path: None,
            flagger_supported: false,
            provider: None,
            pulumi_runtime: None,
            entrypoint: None,
            protect_by_default: false,
            terraform_provider: None,
            pulumi_yaml: None,
            manifests_yaml: Some(raw_manifests.to_string()),
            default_engine: Some("flux".to_string()),
            description: Some("Multi-doc raw K8s manifest app".to_string()),
            items: None,
            schema: None,
        };

        let (status, Json(reg_resp)) =
            register_catalog_app(test_auth(), State(state.clone()), Json(reg_req))
                .await
                .expect("register manifest catalog app succeeds");
        assert_eq!(status, StatusCode::CREATED);
        let app_dto = reg_resp.application.expect("application dto present");
        assert_eq!(app_dto.application.metadata.runtime, AppRuntime::Manifest);
        assert_eq!(app_dto.default_engine.as_deref(), Some("flux"));
        assert!(
            app_dto
                .manifests_yaml
                .as_deref()
                .unwrap()
                .contains("kind: Deployment")
        );

        // Merge the catalog registration PR so `raw-demo` is on `main`
        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(reg_resp.proposal.id),
        )
        .await
        .expect("merge catalog PR");

        // Verify GET /api/v1/catalog returns manifests_yaml and default_engine
        let Json(cat_resp) = get_catalog(test_auth(), State(state.clone()))
            .await
            .expect("get_catalog succeeds");
        let found = cat_resp
            .applications
            .iter()
            .find(|a| a.application.metadata.id == "raw-demo")
            .expect("raw-demo in catalog");
        assert_eq!(found.default_engine.as_deref(), Some("flux"));
        assert!(found.manifests_yaml.is_some());
        assert!(cat_resp.manifests_yaml.contains_key("raw-demo"));

        // 1. Create proposal with Flux engine
        let flux_prop_req = CreateProposalRequest {
            tenant: "default".to_string(),
            catalog_id: "raw-demo".to_string(),
            kind: Some("app".to_string()),
            name: Some("raw-flux-inst".to_string()),
            title: None,
            body: None,
            values: json!({
                "app_name": "flux-svc",
                "log_level": "debug",
                "replicas": "3"
            }),
            canary: false,
            target_cluster: None,
            version: None,
            target_namespace: None,
            action_engine: Some("flux".to_string()),
        };
        let (p_status, Json(flux_prop_resp)) =
            create_proposal(test_auth(), State(state.clone()), Json(flux_prop_req))
                .await
                .expect("create flux manifest proposal");
        assert_eq!(p_status, StatusCode::CREATED);
        assert_eq!(
            flux_prop_resp.report.resources[0].resource_type,
            "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization"
        );

        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(flux_prop_resp.proposal.id),
        )
        .await
        .expect("merge flux manifest PR");

        let flux_app_dir = workdir.join("tenants/default/apps/raw-flux-inst");
        assert!(flux_app_dir.join("manifests.yaml").is_file());
        assert!(flux_app_dir.join("values-configmap.yaml").is_file());
        assert!(flux_app_dir.join("kustomization.yaml").is_file());
        assert!(flux_app_dir.join("flux-kustomization.yaml").is_file());

        let fk_raw = tokio::fs::read_to_string(flux_app_dir.join("flux-kustomization.yaml"))
            .await
            .unwrap();
        assert!(fk_raw.contains("substituteFrom"));
        assert!(fk_raw.contains("raw-flux-inst-values"));

        let Json(rel_cfg) = get_release_config(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "raw-flux-inst".to_string())),
        )
        .await
        .expect("get_release_config for flux manifest app");
        assert_eq!(rel_cfg.action_engine.as_deref(), Some("flux"));
        assert_eq!(rel_cfg.values["app_name"], "flux-svc");

        // 2. Create proposal with Pulumi engine
        let pulumi_prop_req = CreateProposalRequest {
            tenant: "default".to_string(),
            catalog_id: "raw-demo".to_string(),
            kind: Some("app".to_string()),
            name: Some("raw-pulumi-inst".to_string()),
            title: None,
            body: None,
            values: json!({
                "app_name": "pulumi-svc",
                "log_level": "warn",
                "replicas": "4"
            }),
            canary: false,
            target_cluster: None,
            version: None,
            target_namespace: None,
            action_engine: Some("pulumi".to_string()),
        };
        let (p2_status, Json(pulumi_prop_resp)) =
            create_proposal(test_auth(), State(state.clone()), Json(pulumi_prop_req))
                .await
                .expect("create pulumi manifest proposal");
        assert_eq!(p2_status, StatusCode::CREATED);
        assert!(
            pulumi_prop_resp
                .report
                .resources
                .iter()
                .any(|r| r.resource_type == "kubernetes:yaml/v2:ConfigGroup")
        );
        assert!(
            pulumi_prop_resp
                .report
                .resources
                .iter()
                .any(|r| r.resource_type == "kubernetes:apps/v1:Deployment")
        );

        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(pulumi_prop_resp.proposal.id),
        )
        .await
        .expect("merge pulumi manifest PR");

        let pulumi_app_dir = workdir.join("tenants/default/apps/raw-pulumi-inst");
        assert!(pulumi_app_dir.join("manifests.yaml").is_file());
        assert!(pulumi_app_dir.join("Pulumi.yaml").is_file());
        assert!(pulumi_app_dir.join("stack.yaml").is_file());
        assert!(pulumi_app_dir.join("kustomization.yaml").is_file());

        let Json(pulumi_rel_cfg) = get_release_config(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "raw-pulumi-inst".to_string())),
        )
        .await
        .expect("get_release_config for pulumi manifest app");
        assert_eq!(pulumi_rel_cfg.action_engine.as_deref(), Some("pulumi"));
        assert_eq!(pulumi_rel_cfg.values["app_name"], "pulumi-svc");

        // 3. Update existing manifest catalog item via PUT /api/v1/catalog/app/raw-demo
        let updated_manifests = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: ${app_name:=demo}-cfg
data:
  LOG_LEVEL: ${log_level:=info}
  API_KEY: ${api_key}
"#;
        let update_req = UpdateCatalogItemRequest {
            name: "Raw Demo App v2".to_string(),
            category: "Workloads".to_string(),
            description: Some("Updated manifest app with secret".to_string()),
            runtime: Some(AppRuntime::Manifest),
            repo_url: Some(String::new()),
            chart: None,
            version: None,
            path: None,
            flagger_supported: Some(false),
            provider: None,
            pulumi_runtime: None,
            entrypoint: None,
            protect_by_default: None,
            pulumi_yaml: None,
            manifests_yaml: Some(updated_manifests.to_string()),
            default_engine: Some("pulumi".to_string()),
            items: None,
            schema: Some(json!({
                "type": "object",
                "properties": {
                    "app_name": { "type": "string" },
                    "log_level": { "type": "string" },
                    "api_key": { "type": "string", "writeOnly": true, "format": "password" }
                }
            })),
        };
        let (up_status, Json(up_resp)) = update_catalog_item(
            test_auth(),
            State(state.clone()),
            AxumPath(("app".to_string(), "raw-demo".to_string())),
            Json(update_req),
        )
        .await
        .expect("update_catalog_item for manifest app");
        assert_eq!(up_status, StatusCode::OK);
        assert_eq!(
            up_resp
                .application
                .as_ref()
                .and_then(|a| a.default_engine.as_deref()),
            Some("pulumi")
        );
        let _ = merge_proposal(
            test_auth(),
            State(state.clone()),
            AxumPath(up_resp.proposal.id),
        )
        .await
        .expect("merge catalog update PR");

        //Verify list_releases includes both Flux and Pulumi manifest releases
        let Json(releases_resp) = list_releases(
            test_auth(),
            State(state.clone()),
            Query(ListReleasesQuery {
                tenant: Some("default".to_string()),
            }),
        )
        .await
        .expect("list_releases succeeds");
        assert!(
            releases_resp
                .releases
                .iter()
                .any(|r| r.name == "raw-flux-inst" && r.runtime == "manifest")
        );
        assert!(
            releases_resp
                .releases
                .iter()
                .any(|r| r.name == "raw-pulumi-inst" && r.runtime == "manifest")
        );

        // Verify synthesize_report_from_git_diff handles flux-kustomization.yaml and stack.yaml under apps/
        let sample_diff = "diff --git a/tenants/default/apps/raw-flux-inst/flux-kustomization.yaml b/tenants/default/apps/raw-flux-inst/flux-kustomization.yaml\nnew file mode 100644\n--- /dev/null\n+++ b/tenants/default/apps/raw-flux-inst/flux-kustomization.yaml\ndiff --git a/tenants/default/apps/raw-pulumi-inst/stack.yaml b/tenants/default/apps/raw-pulumi-inst/stack.yaml\nnew file mode 100644\n--- /dev/null\n+++ b/tenants/default/apps/raw-pulumi-inst/stack.yaml\n";
        let diff_report = synthesize_report_from_git_diff("99", sample_diff);
        assert_eq!(diff_report.resources.len(), 2);
        assert_eq!(
            diff_report.resources[0].resource_type,
            "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization"
        );
        assert_eq!(
            diff_report.resources[1].resource_type,
            "pulumi:pulumi.com/v1:Stack"
        );
    }

    #[tokio::test]
    async fn stack_ssh_info_resolves_configured_and_fallback_targets_and_simulated_shell() {
        let (_tmp, state, workdir) = setup_test_state().await;

        // 1. Fallback across multiple providers when stack directory is not yet materialized on disk
        for (stack_name, expected_provider, expected_user) in [
            ("proxmox-vm-1", "proxmox", "ubuntu"),
            ("aws-vm-1", "aws", "ubuntu"),
            ("azure-vm-1", "azure", "azureuser"),
            ("gcp-vm-1", "gcp", "debian"),
            ("hetzner-vm-1", "hetzner", "root"),
        ] {
            let Json(fallback_info) = get_stack_ssh_info(
                test_auth(),
                State(state.clone()),
                AxumPath(("tenant-default".to_string(), stack_name.to_string())),
            )
            .await
            .expect("get_stack_ssh_info fallback");
            assert_eq!(fallback_info.stack_name, stack_name);
            assert_eq!(fallback_info.provider, expected_provider);
            assert_eq!(fallback_info.ssh_user, expected_user);
            assert_eq!(
                fallback_info.ssh_host,
                format!("{stack_name}.default.local")
            );
        }

        // 2. Store user SSH key and create stacks with explicit ipAddress/ciuser/sshPort in values.yaml, stack.yaml, and Pulumi.yaml
        let _ = put_git_credentials(
            test_auth(),
            State(state.clone()),
            Json(UpdateGitCredentialsRequest {
                ssh_private_key: Some(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\ntest\n-----END OPENSSH PRIVATE KEY-----"
                        .to_string(),
                ),
                personal_access_token: None,
                username: Some("admin".to_string()),
            }),
        )
        .await
        .expect("store user ssh key");

        let vm_dir = workdir.join("tenants/default/infra/hetzner-node");
        tokio::fs::create_dir_all(&vm_dir).await.unwrap();
        tokio::fs::write(
            vm_dir.join("values.yaml"),
            "ipAddress: 203.0.113.42/24\nsshPort: 2222\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            vm_dir.join("stack.yaml"),
            r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: hetzner-node
  namespace: tenant-default
  labels:
    keel.io/provider: hetzner
spec:
  stack: organization/hetzner-vm/default-hetzner-node
"#,
        )
        .await
        .unwrap();

        let Json(vm_info) = get_stack_ssh_info(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "hetzner-node".to_string())),
        )
        .await
        .expect("get_stack_ssh_info configured");
        assert_eq!(vm_info.provider, "hetzner");
        assert_eq!(vm_info.ssh_host, "203.0.113.42");
        assert_eq!(vm_info.ssh_user, "root");
        assert_eq!(vm_info.ssh_port, 2222);
        assert_eq!(vm_info.ssh_command, "ssh -p 2222 root@203.0.113.42");
        assert!(vm_info.has_saved_ssh_key);

        // Stack with sshHost/sshUser/sshPort in stack.yaml spec.config and Pulumi.yaml outputs
        let stack_cfg_dir = workdir.join("tenants/default/infra/custom-box");
        tokio::fs::create_dir_all(&stack_cfg_dir).await.unwrap();
        tokio::fs::write(
            stack_cfg_dir.join("stack.yaml"),
            r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: custom-box
  namespace: tenant-default
spec:
  stack: organization/custom/default-custom-box
  config:
    keel:provider: aws
    sshUser: ec2-user
    sshPort: "2200"
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            stack_cfg_dir.join("Pulumi.yaml"),
            "name: custom-box\nruntime: yaml\noutputs:\n  publicIp: 198.51.100.77\n",
        )
        .await
        .unwrap();

        let Json(custom_info) = get_stack_ssh_info(
            test_auth(),
            State(state.clone()),
            AxumPath(("tenant-default".to_string(), "custom-box".to_string())),
        )
        .await
        .expect("get_stack_ssh_info from stack.yaml + Pulumi.yaml");
        assert_eq!(custom_info.provider, "aws");
        assert_eq!(custom_info.ssh_host, "198.51.100.77");
        assert_eq!(custom_info.ssh_user, "ec2-user");
        assert_eq!(custom_info.ssh_port, 2200);

        // 3. Verify SSH capability gating across providers
        assert!(crate::ssh::is_ssh_supported_infra(
            &vm_info.provider,
            &vm_info.stack_name
        ));
        assert!(crate::ssh::is_ssh_supported_infra(
            &custom_info.provider,
            &custom_info.stack_name
        ));
        assert!(!crate::ssh::is_ssh_supported_infra(
            "kind",
            "management-cluster-core"
        ));
        assert!(!crate::ssh::is_ssh_supported_infra(
            "kubernetes",
            "postgres-managed"
        ));
    }

    #[test]
    fn clean_candidate_ssh_host_extracts_external_ip_from_arrays_and_rejects_invalid() {
        assert_eq!(
            clean_candidate_ssh_host(r#"[["127.0.0.1"], ["192.168.1.150"]]"#).as_deref(),
            Some("192.168.1.150")
        );
        assert_eq!(
            clean_candidate_ssh_host(r#"["169.254.1.1", "203.0.113.5/24"]"#).as_deref(),
            Some("203.0.113.5")
        );
        assert_eq!(
            clean_candidate_ssh_host(r#"["127.0.0.1"]"#).as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(clean_candidate_ssh_host("dhcp"), None);
        assert_eq!(clean_candidate_ssh_host("-oProxyCommand=evil"), None);
        assert_eq!(clean_candidate_ssh_host("192.168.1.0/24"), None);
    }
}
