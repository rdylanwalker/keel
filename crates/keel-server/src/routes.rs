//! REST and Server-Sent Events (SSE) API handlers (`SDD.md` §7.3).

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use axum::Json;
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
    KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE, ProgressiveDeliverySpec, ProviderRegistry,
    SopsManager, UpstreamSource, parse_catalog, parse_catalog_application, parse_catalog_blueprint,
    parse_catalog_bundle, partition_values_by_schema, render_helm_values_yaml, tenant_namespace,
    validate_dns_label, validate_schema_values,
};
use keel_git::{
    CommitAuthor, CreatePrRequest, PullRequestComment, PullRequestInfo, PullRequestState,
};
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, FluxDependencyRef, FluxHealthCheckRef,
    FluxKustomizationSpecInput, HelmChartSourceKind, HelmChartSourceRef, HelmReleaseSpecInput,
    HelmValuesSecretRef, RemoteKubeConfigRef, StackDiagnostics, TenantAgeKey,
    TenantIsolationConfig, TenantManifestBundle, UserGitCredentials, WatchedCrdKind,
    synthesize_flux_kustomization_yaml, synthesize_helm_release_yaml, user_git_creds_secret_name,
};
use keel_pko::{
    ChangeOp, EphemeralPreviewParams, GcPlan, PkoStack, PreviewReport, PreviewResourceCandidate,
    PreviewSummary, ResourceDiff, TenantStackParams, build_preview_branch, format_markdown_report,
    plan_preview_gc, synthesize_ephemeral_preview, synthesize_tenant_stack,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::{AppState, CachedPreview, ReleaseSummary, StackSummary};

/// `GET /healthz`
pub async fn healthz() -> &'static str {
    "OK"
}

/// `GET /metrics`
pub async fn metrics(State(state): State<AppState>) -> Response {
    let phase = *state.auth_phase.read().await;
    let phase_label = match phase {
        keel_core::AuthPhase::Bootstrap => "bootstrap",
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

/// Response body for `GET /api/v1/catalog`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogResponse {
    pub applications: Vec<CatalogApplication>,
    pub blueprints: Vec<CatalogInfraBlueprint>,
    #[serde(default)]
    pub bundles: Vec<CatalogBundle>,
    pub enabled_providers: Vec<String>,
}

/// `GET /api/v1/catalog`
///
/// Scans `catalog/apps/*/catalog.yaml` and `catalog/infra/*/catalog.yaml` in `repo_workdir`,
/// filtering infrastructure blueprints by enabled providers in `providers/registry.yaml`.
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

    let mut applications = Vec::new();
    for path in find_catalog_files(&state.repo_workdir.join("catalog/apps")).await {
        let yaml = tokio::fs::read_to_string(&path).await?;
        match parse_catalog_application(&yaml) {
            Ok(app) => applications.push(app),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "Skipping invalid CatalogApplication");
            }
        }
    }
    applications.sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));

    let mut blueprints = Vec::new();
    for path in find_catalog_files(&state.repo_workdir.join("catalog/infra")).await {
        let yaml = tokio::fs::read_to_string(&path).await?;
        match parse_catalog_blueprint(&yaml) {
            Ok(bp) => {
                if registry.is_enabled(&bp.metadata.provider) {
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
        blueprints,
        bundles,
        enabled_providers,
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
    pub description: Option<String>,
    #[serde(default)]
    pub items: Option<Vec<CatalogBundleItem>>,
    #[serde(default)]
    pub schema: Option<Value>,
}

/// Response body for `POST /api/v1/catalog`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterCatalogAppResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<CatalogApplication>,
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
    validate_dns_label(&req.id, "id")?;

    match req.kind.as_str() {
        "app" => {
            let runtime = req.runtime.ok_or_else(|| {
                ServerError::BadRequest("runtime is required when kind is 'app'".to_string())
            })?;
            let repo_url = req.repo_url.ok_or_else(|| {
                ServerError::BadRequest("repoUrl is required when kind is 'app'".to_string())
            })?;

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
                None => default_app_schema(),
            };

            let application = CatalogApplication {
                api_version: CATALOG_API_VERSION.to_string(),
                kind: CATALOG_APPLICATION_KIND.to_string(),
                metadata: CatalogAppMetadata {
                    id: req.id.clone(),
                    name: req.name,
                    description: None,
                    category: req.category,
                    runtime,
                },
                spec: CatalogAppSpec {
                    upstream: UpstreamSource {
                        repo_url,
                        chart: req.chart,
                        version: req.version,
                        path: req.path,
                        git_ref: None,
                    },
                    progressive_delivery: ProgressiveDeliverySpec {
                        flagger_supported: req.flagger_supported,
                        default_canary: false,
                    },
                    companion_infrastructure: Vec::new(),
                    schema,
                    ui_schema: None,
                },
            };
            application.validate()?;

            let catalog_yaml = serde_yml::to_string(&application)
                .map_err(|e| ServerError::Internal(e.to_string()))?;

            let rel_dir = format!("catalog/apps/{}", req.id);
            let rel_file = format!("{rel_dir}/catalog.yaml");
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

                state
                    .git_runner
                    .add(&state.repo_workdir, [&rel_file])
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
                    application: Some(application),
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
                    description: None,
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
            let pulumi_yaml = serde_yml::to_string(&Value::Object(pulumi_doc))
                .map_err(|e| ServerError::Internal(e.to_string()))?;

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
                custom_schema
            } else {
                let mut props = serde_json::Map::new();
                let mut required = Vec::with_capacity(items.len());
                for item in &items {
                    validate_dns_label(&item.id, "items[].id")?;
                    validate_dns_label(&item.catalog_ref, "items[].catalogRef")?;
                    let kind_hint = match item.kind {
                        BundleItemKind::Infra => "infra",
                        BundleItemKind::App => "app",
                    };
                    let ref_manifest = load_catalog_manifest(
                        &state.repo_workdir,
                        &item.catalog_ref,
                        Some(kind_hint),
                    )
                    .await?;
                    let (ref_title, ref_schema) = match ref_manifest {
                        CatalogManifest::Application(a) => (a.metadata.name, a.spec.schema),
                        CatalogManifest::InfraBlueprint(b) => (b.metadata.name, b.spec.schema),
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
                json!({
                    "type": "object",
                    "required": required,
                    "properties": props,
                })
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

async fn fetch_upstream_helm_schema(
    repo_url: &str,
    chart: &str,
    version: Option<&str>,
) -> Result<(Value, String), ServerError> {
    let repo_url = repo_url.trim();
    let chart = chart.trim();
    if !(repo_url.starts_with("http://")
        || repo_url.starts_with("https://")
        || repo_url.starts_with("oci://"))
    {
        return Err(ServerError::BadRequest(
            "repoUrl must start with http://, https://, or oci://".to_string(),
        ));
    }
    if chart.is_empty() || chart.starts_with('-') || chart.contains("..") || chart.contains(' ') {
        return Err(ServerError::BadRequest(
            "invalid Helm chart name".to_string(),
        ));
    }
    if let Some(v) = version {
        let v = v.trim();
        if v.starts_with('-') || v.contains(' ') {
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

    let output = tokio::process::Command::new("helm")
        .args(&args)
        .output()
        .await
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
    _auth: AuthSession,
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
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
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

        state
            .git_runner
            .add(&state.repo_workdir, [&rel_file])
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
        state
            .git_runner
            .add(&state.repo_workdir, [&rel_dir])
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

    let (schema, action_label, is_app) = match &manifest {
        CatalogManifest::Application(app) => {
            (&app.spec.schema, format!("app-{}", app.metadata.id), true)
        }
        CatalogManifest::InfraBlueprint(bp) => {
            (&bp.spec.schema, format!("infra-{}", bp.metadata.id), false)
        }
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

    // Encrypt extracted secret fields via SOPS + age before touching Git (`SDD.md` §6.3)
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
                let generated = TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
                state
                    .secret_store
                    .store_tenant_age_key(&req.tenant, &generated)
                    .await?;
                generated
            }
        };
        Some(
            SopsManager::new()
                .encrypt_yaml(&age_key.public_key, plaintext_secrets)
                .await?,
        )
    } else {
        None
    };

    let timestamp = Utc::now().timestamp().max(1) as u64;
    let branch = build_preview_branch(&req.tenant, &action_label, timestamp)?;
    let tenant_ns = tenant_namespace(&req.tenant)?;

    let (rel_dir, manifest_yaml, preview_resource_type) = match &manifest {
        CatalogManifest::Application(app) => {
            let rel_dir = format!("tenants/{}/apps/{instance_name}", req.tenant);
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
                hr_input.kube_config = Some(RemoteKubeConfigRef::new(target));
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
                "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
            )
        }
        CatalogManifest::InfraBlueprint(bp) => {
            let rel_dir = format!("tenants/{}/infra/{instance_name}", req.tenant);
            let mut stack_params = TenantStackParams::new(
                &req.tenant,
                &instance_name,
                format!("organization/{instance_name}/{}", req.tenant),
                &state.backend_url,
                "keel-gitops",
                &rel_dir,
            );
            stack_params
                .config
                .insert("keel:provider".to_string(), bp.metadata.provider.clone());
            let stack = synthesize_tenant_stack(&stack_params)?;
            let stack_yaml = stack.to_yaml()?;
            (rel_dir, stack_yaml, "pulumi:pulumi.com/v1:Stack")
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

        let manifest_filename = if is_app {
            if req.canary {
                "helmrelease-canary.yaml"
            } else {
                "helmrelease.yaml"
            }
        } else {
            "stack.yaml"
        };
        let manifest_path = target_dir.join(manifest_filename);
        tokio::fs::write(&manifest_path, &manifest_yaml).await?;
        staged_paths.push(format!("{rel_dir}/{manifest_filename}"));

        let mut extra_kustomize_resources: Vec<String> = Vec::new();
        if let CatalogManifest::Application(app) = &manifest {
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
                        "name": format!("{instance_name}-repo"),
                        "namespace": tenant_ns,
                        "labels": {
                            "app.kubernetes.io/managed-by": "keel",
                            "keel.io/tenant": req.tenant,
                        }
                    },
                    "spec": repo_spec,
                });
                let helm_repo_yaml = serde_yml::to_string(&helm_repo_doc).map_err(|e| {
                    ServerError::Internal(format!("failed to serialize helm-repository.yaml: {e}"))
                })?;
                let helm_repo_path = target_dir.join("helm-repository.yaml");
                tokio::fs::write(&helm_repo_path, helm_repo_yaml).await?;
                staged_paths.push(format!("{rel_dir}/helm-repository.yaml"));
                extra_kustomize_resources.push(format!(
                    "{}/apps/{instance_name}/helm-repository.yaml",
                    req.tenant
                ));
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
        let manifest_rel_to_tenants = format!(
            "{}/{}/{instance_name}/{manifest_filename}",
            req.tenant,
            if is_app { "apps" } else { "infra" }
        );
        for res in extra_kustomize_resources {
            if !resources.contains(&res) {
                resources.push(res);
            }
        }
        if !resources.contains(&manifest_rel_to_tenants) {
            resources.push(manifest_rel_to_tenants);
        }
        let kustomization_doc = serde_json::json!({
            "apiVersion": "kustomize.config.k8s.io/v1beta1",
            "kind": "Kustomization",
            "resources": resources,
        });
        tokio::fs::create_dir_all(state.repo_workdir.join("tenants")).await?;
        tokio::fs::write(
            &tenants_kustomization_path,
            serde_yml::to_string(&kustomization_doc).map_err(|e| {
                ServerError::Internal(format!(
                    "failed to serialize tenants/kustomization.yaml: {e}"
                ))
            })?,
        )
        .await?;
        staged_paths.push("tenants/kustomization.yaml".to_string());

        if let Some(enc_yaml) = encrypted_secrets_yaml {
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
    let preview_params = EphemeralPreviewParams::for_action(
        &pr.id,
        &req.tenant,
        &action_label,
        timestamp,
        &state.remote_url,
        &rel_dir,
        format!("organization/{instance_name}-pr-{}/{}", pr.id, req.tenant),
        &state.backend_url,
    )?;
    let preview_stack = synthesize_ephemeral_preview(&preview_params)?;

    let report = PreviewReport {
        resources: vec![ResourceDiff {
            urn: format!(
                "urn:pulumi:pr-{}::{}::{}::{}",
                pr.id, req.tenant, preview_resource_type, instance_name
            ),
            resource_type: preview_resource_type.to_string(),
            name: instance_name.clone(),
            op: ChangeOp::Create,
            changed_fields: vec!["spec".to_string()],
        }],
        summary: PreviewSummary {
            create: 1,
            ..PreviewSummary::default()
        },
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
    let timestamp = Utc::now().timestamp().max(1) as u64;
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
    for item in &ordered_items {
        item_kinds.insert(item.id.as_str(), item.kind);
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
            Some(
                SopsManager::new()
                    .encrypt_yaml(&age_key.public_key, plaintext_secrets)
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
                    hr_input.kube_config = Some(RemoteKubeConfigRef::new(format!(
                        "{instance_name}-{cluster_item}-kubeconfig"
                    )));
                } else if let Some(target) = req
                    .target_cluster
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "local")
                {
                    hr_input.kube_config = Some(RemoteKubeConfigRef::new(target));
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
                let mut extras = Vec::new();
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
                }

                // Item-level kustomization.yaml so the Flux Kustomization applies only K8s manifests
                let mut item_k_res = vec!["helmrelease.yaml".to_string()];
                if !extras.is_empty() {
                    item_k_res.push("helm-repository.yaml".to_string());
                }
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
                    "helmrelease.yaml".to_string(),
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
                let mut stack_params = TenantStackParams::new(
                    &req.tenant,
                    &item_resource_name,
                    format!("organization/{item_resource_name}/{}", req.tenant),
                    &state.backend_url,
                    "keel-gitops",
                    &item_rel_dir,
                );
                stack_params
                    .config
                    .insert("keel:provider".to_string(), bp.metadata.provider.clone());
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
        let kustomization_doc = serde_json::json!({
            "apiVersion": "kustomize.config.k8s.io/v1beta1",
            "kind": "Kustomization",
            "resources": resources,
        });
        tokio::fs::create_dir_all(state.repo_workdir.join("tenants")).await?;
        tokio::fs::write(
            &tenants_kustomization_path,
            serde_yml::to_string(&kustomization_doc).map_err(|e| {
                ServerError::Internal(format!(
                    "failed to serialize tenants/kustomization.yaml: {e}"
                ))
            })?,
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

    let preview_params = EphemeralPreviewParams::for_action(
        &pr.id,
        &req.tenant,
        &action_label,
        timestamp,
        &state.remote_url,
        &bundle_rel_dir,
        format!("organization/{instance_name}-pr-{}/{}", pr.id, req.tenant),
        &state.backend_url,
    )?;
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

/// `GET /api/v1/proposals/{pr_id}/preview`
pub async fn get_proposal_preview(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<ProposalPreviewResponse>, ServerError> {
    let pr = state.forge.get_pull_request(&pr_id).await?;
    let report = state
        .preview_cache
        .read()
        .await
        .get(&pr_id)
        .map(|c| c.report.clone())
        .unwrap_or_default();

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

/// `POST /api/v1/proposals/{pr_id}/merge`
pub async fn merge_proposal(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
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
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
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
    _auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListStacksQuery>,
) -> Result<Json<StacksResponse>, ServerError> {
    let stacks = state
        .state_provider
        .list_stacks(query.tenant.as_deref(), query.provider.as_deref())
        .await?;

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

    let direct_rel_dir = format!("tenants/{tenant}/{subdir}/{name}");
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
        let mut target_rel_dir = None;
        if tokio::fs::try_exists(state.repo_workdir.join(&direct_rel_dir))
            .await
            .unwrap_or(false)
        {
            target_rel_dir = Some(direct_rel_dir.clone());
        } else {
            let bundles_dir = state
                .repo_workdir
                .join("tenants")
                .join(tenant)
                .join("bundles");
            if let Ok(mut b_entries) = tokio::fs::read_dir(&bundles_dir).await {
                while let Ok(Some(b_entry)) = b_entries.next_entry().await {
                    let b_name = b_entry.file_name();
                    let Some(b_str) = b_name.to_str() else {
                        continue;
                    };
                    if let Some(item_suffix) = name.strip_prefix(&format!("{b_str}-")) {
                        let candidate = format!("tenants/{tenant}/bundles/{b_str}/{item_suffix}");
                        if tokio::fs::try_exists(state.repo_workdir.join(&candidate))
                            .await
                            .unwrap_or(false)
                        {
                            target_rel_dir = Some(candidate);
                            break;
                        }
                    }
                }
            }
        }

        let Some(rel_dir) = target_rel_dir else {
            return Err(ServerError::NotFound(format!(
                "Resource '{name}' not found in tenant '{tenant}'"
            )));
        };

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
            let kustomization_doc = json!({
                "apiVersion": "kustomize.config.k8s.io/v1beta1",
                "kind": "Kustomization",
                "resources": resources,
            });
            tokio::fs::write(
                &tenants_kustomization_path,
                serde_yml::to_string(&kustomization_doc).map_err(|e| {
                    ServerError::Internal(format!(
                        "failed to serialize tenants/kustomization.yaml: {e}"
                    ))
                })?,
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

/// `GET /api/v1/stacks/{namespace}/{name}/config`
///
/// Reads `tenants/<tenant>/infra/<name>/values.yaml` + `Pulumi.yaml` / `stack.yaml`
/// so the UI can pre-fill the Catalog Wizard RJSF form when editing an existing infra stack.
pub async fn get_stack_config(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReleaseConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    let infra_dir = state
        .repo_workdir
        .join("tenants")
        .join(tenant)
        .join("infra")
        .join(&name);

    if !tokio::fs::try_exists(&infra_dir).await.unwrap_or(false) {
        return Err(ServerError::NotFound(format!(
            "Stack '{name}' not found in tenant '{tenant}'"
        )));
    }

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
    pub values: Value,
}

/// `GET /api/v1/releases/{namespace}/{name}/config`
///
/// Reads the persisted GitOps configuration (`values.yaml` + `helmrelease*.yaml`) for an existing
/// application release so the Catalog Wizard can pre-fill an update PR without exposing secrets.
pub async fn get_release_config(
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReleaseConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    let app_dir = state
        .repo_workdir
        .join("tenants")
        .join(tenant)
        .join("apps")
        .join(&name);

    let (values, hr_doc) = {
        let _guard = state.git_lock.lock().await;
        let values_path = app_dir.join("values.yaml");
        let hr_path = app_dir.join("helmrelease.yaml");
        let hr_canary_path = app_dir.join("helmrelease-canary.yaml");

        let has_values = tokio::fs::try_exists(&values_path).await.unwrap_or(false);
        let active_hr_path = if tokio::fs::try_exists(&hr_path).await.unwrap_or(false) {
            Some(hr_path)
        } else if tokio::fs::try_exists(&hr_canary_path)
            .await
            .unwrap_or(false)
        {
            Some(hr_canary_path)
        } else {
            None
        };

        if !has_values && active_hr_path.is_none() {
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
        } else {
            Value::Object(serde_json::Map::new())
        };

        let hr_doc = if let Some(path) = active_hr_path {
            let raw = tokio::fs::read_to_string(&path).await?;
            serde_yml::from_str::<Value>(&raw).ok()
        } else {
            None
        };

        (values, hr_doc)
    };

    let chart_name = hr_doc
        .as_ref()
        .and_then(|d| d.pointer("/spec/chart/spec/chart"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let version = hr_doc
        .as_ref()
        .and_then(|d| d.pointer("/spec/chart/spec/version"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let target_cluster = hr_doc
        .as_ref()
        .and_then(|d| d.pointer("/spec/kubeConfig/secretRef/name"))
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
    _auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<RestartStackResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

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
    _auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListReleasesQuery>,
) -> Result<Json<ReleasesResponse>, ServerError> {
    let releases = state
        .state_provider
        .list_releases(query.tenant.as_deref())
        .await?;
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
