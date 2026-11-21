//! Catalog listing, registration, updates, and upstream Helm schema introspection (`SDD.md` §7.3).

use std::collections::HashMap;
use std::path::Path;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use chrono::Utc;
use keel_core::{
    AppRuntime, BundleItemKind, CATALOG_API_VERSION, CATALOG_APPLICATION_KIND, CATALOG_BUNDLE_KIND,
    CATALOG_INFRA_BLUEPRINT_KIND, CatalogAppMetadata, CatalogAppSpec, CatalogApplication,
    CatalogBlueprintMetadata, CatalogBlueprintSpec, CatalogBundle, CatalogBundleItem,
    CatalogBundleMetadata, CatalogBundleSpec, CatalogInfraBlueprint, CatalogManifest,
    InfraResourceKind, ManifestEngine, ProgressiveDeliverySpec, UpstreamSource,
    extract_manifest_variables, parse_catalog_application, parse_catalog_blueprint,
    parse_catalog_bundle, validate_dns_label, validate_k8s_manifests_yaml,
};
use keel_git::PullRequestInfo;
use keel_pko::build_preview_branch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;

use super::{
    find_catalog_files, load_catalog_manifest, load_provider_registry, open_pr_for_session,
    safe_relative_path, stage_commit_push_and_return_to_main,
};

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

#[derive(Debug, Deserialize)]
pub struct GetCatalogQuery {
    pub tenant: Option<String>,
}

/// `GET /api/v1/catalog`
///
/// Scans `catalog/apps/*/catalog.yaml` and `catalog/infra/*/catalog.yaml` in `repo_workdir`,
/// filtering infrastructure blueprints by enabled providers in `providers/registry.yaml`.
pub async fn get_catalog(
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<GetCatalogQuery>,
) -> Result<Json<CatalogResponse>, ServerError> {
    let registry = load_provider_registry(&state.repo_workdir).await?;

    let enabled_providers: Vec<String> = registry
        .enabled_providers()
        .into_iter()
        .map(|p| p.id.clone())
        .collect();

    let (mut applications, manifests_yaml) =
        load_catalog_items_from_dir(&state.repo_workdir.join("catalog/apps")).await?;

    let filter_tenant = query
        .tenant
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"));

    let is_item_visible = |item_tenant: Option<&str>| -> bool {
        match item_tenant {
            Some(item_t) => {
                let has_access = auth.is_admin() || auth.require_tenant_access(item_t).is_ok();
                has_access && filter_tenant.is_none_or(|f_t| item_t == f_t)
            }
            None => true,
        }
    };

    applications.retain(|a| is_item_visible(a.application.metadata.tenant.as_deref()));

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
            Ok(mut bp) => {
                if is_item_visible(bp.metadata.tenant.as_deref()) {
                    let is_kubernetes_builtin = bp.metadata.provider == "kubernetes"
                        && !registry.providers.iter().any(|p| p.id == "kubernetes");
                    if registry.is_enabled(&bp.metadata.provider) || is_kubernetes_builtin {
                        bp.metadata.resource_kind = Some(bp.effective_resource_kind());
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
            Ok(bundle) => {
                if is_item_visible(bundle.metadata.tenant.as_deref()) {
                    bundles.push(bundle);
                }
            }
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
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub resource_kind: Option<InfraResourceKind>,
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
    validate_dns_label(&req.id, "id")?;

    let normalized_tenant = req
        .tenant
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        .map(str::to_string);

    if let Some(ref t) = normalized_tenant {
        validate_dns_label(t, "tenant")?;
        auth.require_tenant_admin(t)?;
    } else {
        auth.require_admin()?;
    }

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
                None if runtime == AppRuntime::Helm => {
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
                    protected: req.protected,
                    tenant: normalized_tenant.clone(),
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

                let commit_msg = format!("feat(catalog): add application {}", req.id);
                stage_commit_push_and_return_to_main(
                    &state,
                    &auth,
                    &branch,
                    &commit_msg,
                    &staged_files,
                )
                .await?
            };

            let proposal = open_pr_for_session(
                &state,
                &auth,
                "catalog",
                &branch,
                format!("feat(catalog): register application {}", req.id),
                format!(
                    "Automated catalog registration for `{}` (`{}`)\n\n- Commit: `{}`\n- Path: `{}`",
                    req.id, application.metadata.name, commit_sha, rel_file
                ),
            )
            .await?;

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
                    protected: req.protected,
                    tenant: normalized_tenant.clone(),
                    resource_kind: req.resource_kind,
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

                let commit_msg = format!("feat(catalog): add blueprint {}", req.id);
                stage_commit_push_and_return_to_main(
                    &state,
                    &auth,
                    &branch,
                    &commit_msg,
                    [&rel_catalog, &rel_pulumi],
                )
                .await?
            };

            let proposal = open_pr_for_session(
                &state,
                &auth,
                "catalog",
                &branch,
                format!("feat(catalog): register blueprint {}", req.id),
                format!(
                    "Automated catalog blueprint registration for `{}` (`{}`)\n\n- Commit: `{}`\n- Catalog: `{}`\n- Entrypoint: `{}`",
                    req.id, blueprint.metadata.name, commit_sha, rel_catalog, rel_pulumi
                ),
            )
            .await?;

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
                    protected: req.protected,
                    tenant: normalized_tenant.clone(),
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

                let commit_msg = format!("feat(catalog): add bundle {}", req.id);
                stage_commit_push_and_return_to_main(
                    &state,
                    &auth,
                    &branch,
                    &commit_msg,
                    [&rel_catalog],
                )
                .await?
            };

            let proposal = open_pr_for_session(
                &state,
                &auth,
                "catalog",
                &branch,
                format!("feat(catalog): register bundle {}", req.id),
                format!(
                    "Automated catalog bundle registration for `{}` (`{}`)\n\n- Commit: `{}`\n- Catalog: `{}`",
                    req.id, bundle.metadata.name, commit_sha, rel_catalog
                ),
            )
            .await?;

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
    repo_workdir: &Path,
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
    repo_workdir: &Path,
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
    #[serde(default)]
    pub protected: Option<bool>,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub resource_kind: Option<InfraResourceKind>,
}

/// `PUT /api/v1/catalog/{kind}/{id}`
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
    AxumPath((kind, id)): AxumPath<(String, String)>,
    Json(req): Json<UpdateCatalogItemRequest>,
) -> Result<(StatusCode, Json<RegisterCatalogAppResponse>), ServerError> {
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
        let manifest =
            load_catalog_manifest(&state.repo_workdir, &id, Some(normalized_kind)).await?;

        let existing_tenant = manifest.tenant();
        if let Some(t) = existing_tenant {
            auth.require_tenant_admin(t)?;
        } else {
            auth.require_admin()?;
        }

        let new_tenant = if let Some(ref raw_new_t) = req.tenant {
            let trimmed = raw_new_t.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
                if existing_tenant.is_some() {
                    auth.require_admin()?;
                }
                Some(None)
            } else {
                validate_dns_label(trimmed, "tenant")?;
                auth.require_tenant_admin(trimmed)?;
                Some(Some(trimmed.to_string()))
            }
        } else {
            None
        };

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
                if let Some(nt) = &new_tenant {
                    app.metadata.tenant = nt.clone();
                }
                if let Some(p) = req.protected {
                    app.metadata.protected = p;
                }
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
                if let Some(nt) = &new_tenant {
                    bp.metadata.tenant = nt.clone();
                }
                if let Some(p) = req.protected {
                    bp.metadata.protected = p;
                }
                if let Some(rk) = req.resource_kind {
                    bp.metadata.resource_kind = Some(rk);
                }
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
                if let Some(nt) = &new_tenant {
                    bundle.metadata.tenant = nt.clone();
                }
                if let Some(p) = req.protected {
                    bundle.metadata.protected = p;
                }
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

        let commit_msg = format!("feat(catalog): update {normalized_kind} {id}");
        let sha =
            stage_commit_push_and_return_to_main(&state, &auth, &branch, &commit_msg, &staged)
                .await?;
        (sha, app_out, bp_out, bundle_out, title_name)
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        "catalog",
        &branch,
        format!("feat(catalog): update {normalized_kind} {id}"),
        format!(
            "Automated catalog update for `{id}` (`{display_name}`)\n\n- Commit: `{commit_sha}`\n- Path: `{rel_file}`"
        ),
    )
    .await?;

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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteCatalogItemResponse {
    pub id: String,
    pub kind: String,
    pub proposal: PullRequestInfo,
    pub branch: String,
}

/// `DELETE /api/v1/catalog/{kind}/{id}`
pub async fn delete_catalog_item(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((kind, id)): AxumPath<(String, String)>,
) -> Result<Json<DeleteCatalogItemResponse>, ServerError> {
    validate_dns_label(&id, "id")?;
    let normalized_kind = match kind.as_str() {
        "app" | "application" => "app",
        "infra" | "infrastructure" | "blueprint" => "infra",
        "bundle" => "bundle",
        other => {
            return Err(ServerError::BadRequest(format!(
                "Unsupported catalog kind '{other}'; expected 'app', 'infra', or 'bundle'"
            )));
        }
    };

    let rel_dir = match normalized_kind {
        "app" => format!("catalog/apps/{id}"),
        "infra" => format!("catalog/infra/{id}"),
        "bundle" => format!("catalog/bundles/{id}"),
        _ => unreachable!(),
    };
    let rel_file = format!("{rel_dir}/catalog.yaml");

    let timestamp = super::next_unique_branch_timestamp();
    let branch = build_preview_branch("catalog", &format!("delete-{id}"), timestamp)?;

    let (_commit_sha, display_name) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

        let abs_dir = state.repo_workdir.join(&rel_dir);
        let abs_file = state.repo_workdir.join(&rel_file);
        if !tokio::fs::try_exists(&abs_file).await.unwrap_or(false) {
            return Err(ServerError::NotFound(format!(
                "Catalog {normalized_kind} '{id}' not found"
            )));
        }
        let _existing_yaml = tokio::fs::read_to_string(&abs_file).await?;
        let manifest =
            load_catalog_manifest(&state.repo_workdir, &id, Some(normalized_kind)).await?;

        if manifest.is_protected() {
            return Err(ServerError::BadRequest(format!(
                "Catalog item '{id}' is protected and cannot be deleted"
            )));
        }

        if let Some(t) = manifest.tenant() {
            auth.require_tenant_admin(t)?;
        } else {
            auth.require_admin()?;
        }

        let name = match manifest {
            CatalogManifest::Application(app) => app.metadata.name,
            CatalogManifest::InfraBlueprint(bp) => bp.metadata.name,
            CatalogManifest::Bundle(b) => b.metadata.name,
        };

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;

        tokio::fs::remove_dir_all(&abs_dir).await.map_err(|e| {
            ServerError::Internal(format!("Failed to delete catalog directory: {e}"))
        })?;

        let commit_message = format!("feat(catalog): delete {normalized_kind} {id}");
        let commit_sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_message,
            &[rel_dir.as_str()],
        )
        .await?;

        (commit_sha, name)
    };

    let title = format!("Delete catalog {normalized_kind}: {display_name}");
    let description = format!("This PR deletes the `{id}` catalog {normalized_kind}.");
    let proposal =
        open_pr_for_session(&state, &auth, "catalog", &branch, title, description).await?;

    Ok(Json(DeleteCatalogItemResponse {
        id,
        kind: normalized_kind.to_string(),
        proposal,
        branch,
    }))
}
