//! Stack, release, and bundle workload queries, diagnostics, live operations, and SSH bridge endpoints (`SDD.md` §7.3).

use axum::Json;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::IntoResponse;
use chrono::Utc;
use keel_core::{
    CatalogManifest, StackSshInfoResponse, parse_catalog, resolve_ssh_target_from_sources,
    validate_dns_label,
};
use keel_git::PullRequestInfo;
use keel_kube::StackDiagnostics;
use keel_pko::build_preview_branch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::{AppState, ReleaseSummary, StackSummary};

use super::{
    open_pr_for_session, resolve_tenant_resource_dir, serialize_tenants_kustomization,
    stage_commit_push_and_return_to_main,
};

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
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<StackDiagnostics>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    if let Some(tenant) = namespace.strip_prefix("tenant-") {
        auth.require_tenant_access(tenant)?;
    } else {
        auth.require_admin()?;
    }

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
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<StackDiagnostics>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    if let Some(tenant) = namespace.strip_prefix("tenant-") {
        auth.require_tenant_access(tenant)?;
    } else {
        auth.require_admin()?;
    }

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
    let kind_label = if subdir == "infra" {
        "stack"
    } else {
        "release"
    };

    let (commit_sha, removed_rel_dir) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

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

        let commit_msg = format!("feat({tenant}): delete {kind_label} {name}");
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            &staged_paths,
        )
        .await?;
        (sha, rel_dir)
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        tenant,
        &branch,
        format!("[Delete] feat({tenant}): remove {kind_label} {name}"),
        format!(
            "[Delete] Automated decommission proposal for `{name}` in `{namespace}`\n\n- Commit: `{commit_sha}`\n- Removed: `{removed_rel_dir}`"
        ),
    )
    .await?;

    Ok(Json(DeleteResourceResponse {
        id: name,
        namespace,
        proposal,
        branch,
    }))
}

/// `GET /api/v1/stacks/{namespace}/{name}/config`
///
/// Reads `tenants/<tenant>/infra/<name>/values.yaml` (or matching bundle item) + `Pulumi.yaml` / `stack.yaml`
/// so the UI can pre-fill the Catalog Wizard RJSF form when editing an existing infra stack.
pub async fn get_stack_config(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReleaseConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;
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
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReleaseConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;
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
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<BundleConfigResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;
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

/// Response body for `POST /api/v1/stacks/{namespace}/{name}/reconcile`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileStackResponse {
    pub reconciled: bool,
    pub namespace: String,
    pub name: String,
    pub message: String,
}

/// `POST /api/v1/stacks/{namespace}/{name}/reconcile`
///
/// Annotates a PKO v2 `Stack` with `pulumi.com/reconcile-request` to trigger
/// immediate reconciliation without restarting the workspace pod.
pub async fn reconcile_stack(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ReconcileStackResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;

    let message = state
        .state_provider
        .reconcile_stack(&namespace, &name)
        .await?;
    Ok(Json(ReconcileStackResponse {
        reconciled: true,
        namespace,
        name,
        message,
    }))
}

/// Response body for `POST /api/v1/stacks/{namespace}/{name}/force-delete`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceDeleteStackResponse {
    pub force_deleted: bool,
    pub namespace: String,
    pub name: String,
    pub message: String,
}

/// `POST /api/v1/stacks/{namespace}/{name}/force-delete`
///
/// Clears finalizers on a stuck/terminating PKO v2 `Stack` and deletes it if not already terminating.
pub async fn force_delete_stack(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<ForceDeleteStackResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    if namespace == "flux-system" && name == keel_pko::MANAGEMENT_CORE_STACK_NAME {
        return Err(ServerError::Forbidden(
            "Cannot force-delete protected management-cluster-core stack".to_string(),
        ));
    }
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;

    let message = state
        .state_provider
        .force_delete_stack(&namespace, &name)
        .await?;
    Ok(Json(ForceDeleteStackResponse {
        force_deleted: true,
        namespace,
        name,
        message,
    }))
}

/// Response body for `POST /api/v1/stacks/{namespace}/{name}/unlock`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockStackResponse {
    pub unlocked: bool,
    pub namespace: String,
    pub name: String,
    pub message: String,
}

/// `POST /api/v1/stacks/{namespace}/{name}/unlock`
///
/// Cancels a stale Pulumi state lock via `pulumi cancel` in the Stack's workspace pod
/// and requests immediate reconciliation.
pub async fn unlock_stack(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<UnlockStackResponse>, ServerError> {
    validate_dns_label(&namespace, "namespace")?;
    validate_dns_label(&name, "name")?;
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;

    let message = state.state_provider.unlock_stack(&namespace, &name).await?;
    Ok(Json(UnlockStackResponse {
        unlocked: true,
        namespace,
        name,
        message,
    }))
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

    let mut provider_override = None;
    if stack_doc
        .as_ref()
        .and_then(|d| {
            d.pointer("/metadata/labels/keel.io~1provider")
                .or_else(|| d.pointer("/spec/config/keel:provider"))
        })
        .and_then(Value::as_str)
        .is_none()
        && let Ok(stacks) = state.state_provider.list_stacks(Some(tenant), None).await
    {
        provider_override = stacks
            .into_iter()
            .find(|s| s.name == name)
            .and_then(|s| s.provider);
    }

    let info = resolve_ssh_target_from_sources(
        name,
        namespace,
        tenant,
        provider_override,
        &live_outputs,
        values_doc.as_ref(),
        stack_doc.as_ref(),
        pulumi_doc.as_ref(),
        has_saved_ssh_key,
    );

    Ok((info, saved_ssh_key))
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
