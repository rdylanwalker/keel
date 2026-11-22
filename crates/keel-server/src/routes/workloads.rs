//! Stack, release, and bundle workload queries, diagnostics, live operations, and SSH bridge endpoints (`SDD.md` §7.3).

use axum::Json;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::IntoResponse;
use chrono::Utc;
use keel_core::{
    CatalogManifest, InfraResourceKind, StackSshInfoResponse, parse_catalog,
    resolve_ssh_target_from_sources, validate_dns_label,
};
use keel_kube::StackDiagnostics;
use keel_pko::build_preview_branch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::{AppState, MANAGEMENT_TENANT, ReleaseSummary, StackSummary};

use crate::proposal_repo::{ScopeRepo, TenantIndex};

use super::{RepoReader, resolve_tenant_resource_dir, stage_commit_push_in};

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
    pub proposal: crate::proposal_repo::ScopedProposal,
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
    if is_management_namespace(&namespace) || namespace == "kyverno" {
        return Err(ServerError::Forbidden(format!(
            "'{namespace}/{name}' is a protected management-plane add-on and cannot be deleted \
             via the API; use the management add-on upgrade/rollback PR endpoints \
             (/api/v1/management/addons/{{id}}) to change it"
        )));
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

    let repo = ScopeRepo::for_tenant(&state, tenant)?;
    let (commit_sha, removed_rel_dir) = {
        let _guard = repo.lock(&state).await;
        repo.sync_main(&state).await?;
        crate::routes::proposals::ensure_tenant_repo_initialised(&repo).await?;
        let workdir = repo.workdir.as_path();

        let Some(resolved) = resolve_tenant_resource_dir(workdir, tenant, subdir, &name).await
        else {
            return Err(ServerError::NotFound(format!(
                "Resource '{name}' not found in tenant '{tenant}'"
            )));
        };
        let rel_dir = resolved.rel_dir;
        if subdir == "apps" && declares_protected_flux_object(&workdir.join(&rel_dir)) {
            return Err(ServerError::Forbidden(format!(
                "Release '{name}' in '{namespace}' is labelled `keel.io/protect: \"true\"` and \
                 cannot be deleted via the API; remove the label in Git first"
            )));
        }

        state
            .git_runner
            .checkout_new_branch(workdir, &branch)
            .await?;

        tokio::fs::remove_dir_all(workdir.join(&rel_dir)).await?;
        let mut staged_paths = vec![rel_dir.clone()];

        let index = TenantIndex::new(&repo, tenant);
        if tokio::fs::try_exists(workdir.join(index.rel_path()))
            .await
            .unwrap_or(false)
        {
            let prefix_in_tenants = rel_dir
                .strip_prefix("tenants/")
                .unwrap_or(&rel_dir)
                .to_string();
            let resources: Vec<String> = index
                .load(workdir)
                .await
                .into_iter()
                .filter(|r| !r.starts_with(&format!("{prefix_in_tenants}/")))
                .collect();
            staged_paths.push(index.store(workdir, &resources).await?);
        }

        let commit_msg = format!("feat({tenant}): delete {kind_label} {name}");
        let sha = stage_commit_push_in(&state, workdir, &auth, &branch, &commit_msg, &staged_paths)
            .await?;
        (sha, rel_dir)
    };

    let proposal = repo
        .open_pull_request(
            &auth,
            tenant,
            &branch,
            format!("chore({tenant}): delete {kind_label} {name}"),
            format!(
                "Automated decommission proposal for `{name}` in `{namespace}`\n\n- Commit: `{commit_sha}`\n- Removed: `{removed_rel_dir}`"
            ),
        )
        .await?;
    state.proposal_cache.invalidate(&repo.repository().label);
    let proposal = repo.scoped(proposal);

    Ok(Json(DeleteResourceResponse {
        id: name,
        namespace,
        proposal,
        branch,
    }))
}

/// The tenant's own repository when tenants have dedicated repositories (`None` for the Local
/// Git provider, whose tenants live in the platform repository).
fn tenant_repository(state: &AppState, tenant: &str) -> Result<Option<ScopeRepo>, ServerError> {
    if !state.tenant_repos.is_dedicated() {
        return Ok(None);
    }
    Ok(Some(ScopeRepo::for_tenant(state, tenant)?))
}

/// `true` when any Flux `HelmRelease` / `Kustomization` manifest under `dir` carries the
/// `keel.io/protect: "true"` anti-lockout label.
fn declares_protected_flux_object(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            return declares_protected_flux_object(&path);
        }
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
        if !is_yaml {
            return false;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return false;
        };
        keel_core::split_yaml_documents(&raw).iter().any(|doc| {
            serde_yml::from_str::<Value>(doc).ok().is_some_and(|val| {
                let kind = val.get("kind").and_then(Value::as_str);
                matches!(kind, Some("HelmRelease" | "Kustomization"))
                    && val
                        .pointer("/metadata/labels/keel.io~1protect")
                        .and_then(Value::as_str)
                        == Some("true")
            })
        })
    })
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

    let not_found =
        || ServerError::NotFound(format!("Stack '{name}' not found in tenant '{tenant}'"));
    // A tenant's own repository: its committed `main`, read from the synced clone.
    if let Some(repo) = tenant_repository(&state, tenant)? {
        let _guard = repo.lock(&state).await;
        repo.sync_main_for_read(&state).await?;
        let reader = RepoReader::committed_in(&state, &repo.workdir)
            .await
            .ok_or_else(not_found)?;
        return read_stack_config(&reader, tenant, &name)
            .await
            .map(Json)
            .ok_or_else(not_found);
    }
    // Prefer committed `origin/main` state read without touching the shared worktree (`OPEN-13`).
    if let Some(reader) = RepoReader::committed(&state).await
        && let Some(resp) = read_stack_config(&reader, tenant, &name).await
    {
        return Ok(Json(resp));
    }
    // Fall back to the checked-out worktree, read under the shared git lock so concurrent
    // branch checkouts by mutating handlers cannot interleave with these reads.
    let _guard = state.git_lock.lock().await;
    let reader = RepoReader::Worktree {
        root: &state.repo_workdir,
    };
    read_stack_config(&reader, tenant, &name)
        .await
        .map(Json)
        .ok_or_else(|| {
            ServerError::NotFound(format!("Stack '{name}' not found in tenant '{tenant}'"))
        })
}

/// Reads `values.yaml` + catalog id for an infra stack through `reader`; `None` if the stack dir is absent.
async fn read_stack_config(
    reader: &RepoReader<'_>,
    tenant: &str,
    name: &str,
) -> Option<ReleaseConfigResponse> {
    let resolved = reader
        .resolve_tenant_resource_dir(tenant, "infra", name)
        .await?;
    let dir = resolved.rel_dir;

    let values = match reader.read(&format!("{dir}/values.yaml")).await {
        Some(raw) => serde_yml::from_str::<Value>(&raw).unwrap_or_else(|_| json!({})),
        None => json!({}),
    };

    let mut catalog_id = name.to_string();
    if !reader
        .exists(&format!("catalog/infra/{name}/catalog.yaml"))
        .await
        && let Some(raw) = reader.read(&format!("{dir}/stack.yaml")).await
        && let Ok(doc) = serde_yml::from_str::<Value>(&raw)
        && let Some(provider) = doc
            .pointer("/metadata/labels/keel.io~1provider")
            .and_then(Value::as_str)
    {
        for entry in reader.list_dir("catalog/infra").await {
            if let Some(cat_raw) = reader
                .read(&format!("catalog/infra/{entry}/catalog.yaml"))
                .await
                && let Ok(CatalogManifest::InfraBlueprint(bp)) = parse_catalog(&cat_raw)
                && bp.metadata.provider == provider
            {
                catalog_id = bp.metadata.id;
                break;
            }
        }
    }

    Some(ReleaseConfigResponse {
        catalog_id,
        tenant: tenant.to_string(),
        name: name.to_string(),
        version: None,
        target_cluster: None,
        target_namespace: None,
        canary: None,
        action_engine: None,
        values,
    })
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
    if is_management_namespace(&namespace) {
        auth.require_admin()?;
        return Err(management_addon_config_conflict(&namespace, &name));
    }

    let tenant = namespace.strip_prefix("tenant-").unwrap_or(&namespace);
    auth.require_tenant_access(tenant)?;
    // The tenant's repository (its own clone, or the platform checkout for the Local Git
    // provider), read under its lock so mutating handlers cannot interleave.
    let repo = ScopeRepo::for_tenant(&state, tenant)?;
    let repo_guard = repo.lock(&state).await;
    repo.sync_main_for_read(&state).await?;
    let root = repo.workdir.as_path();
    let app_dir = match resolve_tenant_resource_dir(root, tenant, "apps", &name).await {
        Some(resolved) => root.join(&resolved.rel_dir),
        None => root.join("tenants").join(tenant).join("apps").join(&name),
    };

    let (values, hr_doc, is_canary, action_engine) = {
        let _guard = repo_guard;
        let values_path = app_dir.join("values.yaml");
        let hr_path = app_dir.join("helmrelease.yaml");
        let hr_canary_path = app_dir.join("helmrelease-canary.yaml");
        let canary_cr_path = app_dir.join("canary.yaml");
        let fk_path = app_dir.join("flux-kustomization.yaml");
        let stack_path = app_dir.join("stack.yaml");
        let manifests_path = app_dir.join("manifests.yaml");
        // Flux manifest apps keep their values ConfigMap under `config/` (pre-split layout: app root).
        let cm_path = {
            let split = app_dir.join("config/values-configmap.yaml");
            if tokio::fs::try_exists(&split).await.unwrap_or(false) {
                split
            } else {
                app_dir.join("values-configmap.yaml")
            }
        };

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
    let repo = ScopeRepo::for_tenant(&state, tenant)?;
    let _repo_guard = repo.lock(&state).await;
    repo.sync_main_for_read(&state).await?;
    let bundle_dir = repo
        .workdir
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

/// Authorizes an out-of-band live Stack operation (`restart`, `force-delete`, `unlock`):
/// tenant namespaces require tenant-admin (or cluster admin); any other namespace requires
/// cluster admin (`OPEN-11`). Returns the tenant id (or the namespace for non-tenant namespaces).
fn require_stack_operator<'a>(
    auth: &AuthSession,
    namespace: &'a str,
) -> Result<&'a str, ServerError> {
    match namespace.strip_prefix("tenant-") {
        Some(tenant) => {
            auth.require_tenant_admin(tenant)?;
            Ok(tenant)
        }
        None => {
            auth.require_admin()?;
            Ok(namespace)
        }
    }
}

/// Emits a `keel::audit` record for an out-of-band live Stack operation.
fn audit_stack_operation(
    auth: &AuthSession,
    action: &str,
    tenant: &str,
    namespace: &str,
    name: &str,
    outcome: Result<&String, &ServerError>,
) {
    match outcome {
        Ok(_) => tracing::warn!(
            target: "keel::audit",
            user = %auth.claims.sub,
            action,
            tenant,
            namespace,
            stack = name,
            outcome = "success",
            "Out-of-band live Stack operation executed"
        ),
        Err(err) => tracing::warn!(
            target: "keel::audit",
            user = %auth.claims.sub,
            action,
            tenant,
            namespace,
            stack = name,
            outcome = "failure",
            error = %err,
            "Out-of-band live Stack operation failed"
        ),
    }
}

/// Resolves `namespace/name` against the declared (Git) + live Stack inventory so out-of-band
/// operations on unknown stacks return `404` before any "live cluster required" (`503`) check.
/// Returns the matched stacks list for further guards. Inventory read failures do not block.
async fn require_known_stack(
    state: &AppState,
    namespace: &str,
    name: &str,
) -> Result<Vec<crate::state::StackSummary>, ServerError> {
    match state
        .state_provider
        .list_stacks(namespace.strip_prefix("tenant-"), None)
        .await
    {
        Ok(stacks) => {
            if stacks
                .iter()
                .any(|s| s.namespace == namespace && s.name == name)
            {
                Ok(stacks)
            } else {
                Err(ServerError::NotFound(format!(
                    "Stack {namespace}/{name} not found in Git or the live cluster"
                )))
            }
        }
        Err(err) => {
            tracing::warn!(namespace, name, error = %err, "stack inventory unavailable; skipping existence check");
            Ok(Vec::new())
        }
    }
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
    let tenant = require_stack_operator(&auth, &namespace)?;
    if let Err(err) = require_known_stack(&state, &namespace, &name).await {
        audit_stack_operation(&auth, "restart", tenant, &namespace, &name, Err(&err));
        return Err(err);
    }

    let outcome = state
        .state_provider
        .restart_stack_workspace(&namespace, &name)
        .await;
    audit_stack_operation(
        &auth,
        "restart",
        tenant,
        &namespace,
        &name,
        outcome.as_ref(),
    );
    let message = outcome?;
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
    require_known_stack(&state, &namespace, &name).await?;

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
    let tenant = require_stack_operator(&auth, &namespace)?;
    if namespace == "flux-system" && name == keel_pko::MANAGEMENT_CORE_STACK_NAME {
        let err = ServerError::Conflict(
            "Cannot force-delete protected management-cluster-core stack".to_string(),
        );
        audit_stack_operation(&auth, "force-delete", tenant, &namespace, &name, Err(&err));
        return Err(err);
    }

    let stacks = match require_known_stack(&state, &namespace, &name).await {
        Ok(stacks) => stacks,
        Err(err) => {
            audit_stack_operation(&auth, "force-delete", tenant, &namespace, &name, Err(&err));
            return Err(err);
        }
    };
    // Refuse protected stacks declared in Git as well (the live guard also checks the cluster object).
    if stacks
        .iter()
        .any(|s| s.namespace == namespace && s.name == name && s.protect)
    {
        let err = ServerError::Conflict(format!(
            "Cannot force-delete protected Stack {namespace}/{name}; remove `{}` / `spec.config.protect` via a GitOps PR first",
            keel_pko::stack::LABEL_PROTECT
        ));
        audit_stack_operation(&auth, "force-delete", tenant, &namespace, &name, Err(&err));
        return Err(err);
    }

    let outcome = state
        .state_provider
        .force_delete_stack(&namespace, &name)
        .await;
    audit_stack_operation(
        &auth,
        "force-delete",
        tenant,
        &namespace,
        &name,
        outcome.as_ref(),
    );
    let message = outcome?;
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
    let tenant = require_stack_operator(&auth, &namespace)?;
    if let Err(err) = require_known_stack(&state, &namespace, &name).await {
        audit_stack_operation(&auth, "unlock", tenant, &namespace, &name, Err(&err));
        return Err(err);
    }

    let outcome = state.state_provider.unlock_stack(&namespace, &name).await;
    audit_stack_operation(&auth, "unlock", tenant, &namespace, &name, outcome.as_ref());
    let message = outcome?;
    Ok(Json(UnlockStackResponse {
        unlocked: true,
        namespace,
        name,
        message,
    }))
}

/// Resolved SSH target plus server-side-only session material.
struct ResolvedStackSsh {
    info: StackSshInfoResponse,
    saved_ssh_key: Option<String>,
    /// Expected host key / fingerprint published by the stack (`sshHostKeyFingerprint`, ...).
    expected_host_key: Option<String>,
}

/// Error from SSH target resolution: a regular [`ServerError`] or `422 Unprocessable Entity` for
/// stacks that exist but do not represent an SSH-reachable VM (`OPEN-17`).
#[derive(Debug)]
pub enum SshInfoError {
    Server(ServerError),
    Unsupported(String),
}

impl From<ServerError> for SshInfoError {
    fn from(err: ServerError) -> Self {
        Self::Server(err)
    }
}

impl IntoResponse for SshInfoError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Server(err) => err.into_response(),
            Self::Unsupported(msg) => (
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                Json(crate::error::ErrorResponse {
                    error: msg,
                    details: None,
                    correlation_id: None,
                }),
            )
                .into_response(),
        }
    }
}

/// Reads `stack.yaml`, `values.yaml`, and `Pulumi.yaml` documents for an infra stack via `reader`.
async fn read_stack_ssh_docs(
    reader: &RepoReader<'_>,
    tenant: &str,
    name: &str,
) -> Option<(Option<Value>, Option<Value>, Option<Value>)> {
    let dir = reader
        .resolve_tenant_resource_dir(tenant, "infra", name)
        .await?
        .rel_dir;
    let parse = |raw: Option<String>| raw.and_then(|r| serde_yml::from_str::<Value>(&r).ok());
    Some((
        parse(reader.read(&format!("{dir}/stack.yaml")).await),
        parse(reader.read(&format!("{dir}/values.yaml")).await),
        parse(reader.read(&format!("{dir}/Pulumi.yaml")).await),
    ))
}

async fn resolve_stack_ssh_info(
    state: &AppState,
    auth: &AuthSession,
    namespace: &str,
    name: &str,
) -> Result<ResolvedStackSsh, SshInfoError> {
    validate_dns_label(namespace, "namespace").map_err(ServerError::from)?;
    validate_dns_label(name, "name").map_err(ServerError::from)?;
    let tenant = namespace.strip_prefix("tenant-").unwrap_or(namespace);
    validate_dns_label(tenant, "tenant").map_err(ServerError::from)?;
    auth.require_tenant_access(tenant)?;

    // The stack must exist (in Git or live) and be a VM; never fabricate a target (`OPEN-17`).
    let summary = state
        .state_provider
        .list_stacks(Some(tenant), None)
        .await?
        .into_iter()
        .find(|s| s.name == name && s.namespace == namespace)
        .ok_or_else(|| {
            ServerError::NotFound(format!(
                "Stack '{name}' not found in namespace '{namespace}'"
            ))
        })?;
    if let Some(kind) = summary.resource_kind
        && kind != InfraResourceKind::Vm
    {
        return Err(SshInfoError::Unsupported(format!(
            "SSH not supported for {kind} stack '{name}'; only VM stacks expose SSH targets"
        )));
    }

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

    // Prefer committed `origin/main` (no worktree access); fall back to the worktree under `git_lock`.
    let committed = match tenant_repository(state, tenant).map_err(SshInfoError::from)? {
        Some(repo) => {
            let _guard = repo.lock(state).await;
            repo.sync_main_for_read(state)
                .await
                .map_err(SshInfoError::from)?;
            match RepoReader::committed_in(state, &repo.workdir).await {
                Some(reader) => Some(
                    read_stack_ssh_docs(&reader, tenant, name)
                        .await
                        .unwrap_or((None, None, None)),
                ),
                None => Some((None, None, None)),
            }
        }
        None => match RepoReader::committed(state).await {
            Some(reader) => read_stack_ssh_docs(&reader, tenant, name).await,
            None => None,
        },
    };
    let (stack_doc, values_doc, pulumi_doc) = match committed {
        Some(docs) => docs,
        None => {
            let _guard = state.git_lock.lock().await;
            let reader = RepoReader::Worktree {
                root: &state.repo_workdir,
            };
            read_stack_ssh_docs(&reader, tenant, name)
                .await
                .unwrap_or((None, None, None))
        }
    };

    let provider_override = if stack_doc
        .as_ref()
        .and_then(|d| {
            d.pointer("/metadata/labels/keel.io~1provider")
                .or_else(|| d.pointer("/spec/config/keel:provider"))
        })
        .and_then(Value::as_str)
        .is_none()
    {
        summary.provider.clone()
    } else {
        None
    };

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
    if !crate::ssh::is_ssh_supported_infra(&info.provider, name) {
        return Err(SshInfoError::Unsupported(format!(
            "SSH not supported for {} stack '{name}'",
            if info.provider.is_empty() {
                "this"
            } else {
                info.provider.as_str()
            }
        )));
    }
    let expected_host_key = keel_core::models::resolve_ssh_host_key_from_sources(
        &live_outputs,
        values_doc.as_ref(),
        stack_doc.as_ref(),
    );

    Ok(ResolvedStackSsh {
        info,
        saved_ssh_key,
        expected_host_key,
    })
}

/// `GET /api/v1/stacks/{namespace}/{name}/ssh-info`
///
/// Resolves SSH target metadata (host, user, port, copyable commands, and whether the user has a
/// saved SSH key) for a provisioned VM stack without exposing any private key material (`NFR-SEC-05`).
pub async fn get_stack_ssh_info(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
) -> Result<Json<StackSshInfoResponse>, SshInfoError> {
    let resolved = resolve_stack_ssh_info(&state, &auth, &namespace, &name).await?;
    Ok(Json(resolved.info))
}

/// `GET /api/v1/stacks/{namespace}/{name}/ssh/ws`
///
/// Upgrades an authenticated tenant-authorized request to an interactive WebSocket SSH terminal bridge.
///
/// Authorization: any tenant member may open a session, but only to the stack-resolved target
/// (host/port pinned from Pulumi outputs / GitOps manifests). Supplying a custom target (only
/// possible when the stack has no resolved endpoint) additionally requires tenant-admin
/// privileges, and such targets must resolve to public addresses. When the stack publishes an
/// expected host key (`sshHostKeyFingerprint` / `sshHostKey`), the server key is enforced.
pub async fn stack_ssh_ws(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath((namespace, name)): AxumPath<(String, String)>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, SshInfoError> {
    let ResolvedStackSsh {
        info: ssh_info,
        saved_ssh_key,
        expected_host_key,
    } = resolve_stack_ssh_info(&state, &auth, &namespace, &name).await?;
    let policy = crate::ssh::SshSessionPolicy {
        allow_unresolved_target_override: auth.is_tenant_admin(&ssh_info.tenant),
        expected_host_key,
    };
    tracing::info!(
        target: "keel::audit",
        user = %auth.claims.sub,
        tenant = %ssh_info.tenant,
        namespace = %ssh_info.namespace,
        stack = %ssh_info.stack_name,
        ssh_host = %ssh_info.ssh_host,
        ssh_user = %ssh_info.ssh_user,
        ssh_port = ssh_info.ssh_port,
        host_key_pinned = policy.expected_host_key.is_some(),
        "WebSocket SSH session initiated"
    );
    let user_sub = auth.claims.sub.clone();
    Ok(ws.on_upgrade(move |socket| {
        crate::ssh::handle_stack_ssh_ws(socket, ssh_info, saved_ssh_key, user_sub, policy)
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
    // Tenant namespaces need tenant access; any other namespace (e.g. management add-ons in
    // `flux-system`) requires cluster admin. keel-server's `flux-system` Role + the Kyverno
    // `protect-control-plane` policy only permit the `reconcile.fluxcd.io/requestedAt` patch.
    match namespace.strip_prefix("tenant-") {
        Some(tenant) => auth.require_tenant_access(tenant)?,
        None => auth.require_admin()?,
    }

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
///
/// Tenant releases are always filtered to the caller's tenants. For cluster admins the
/// protected management-plane add-ons (`clusters/management/core-addons/`) are appended as
/// releases with `managementAddon: true` when no `tenant` filter is set; `?tenant=management`
/// (admin-only) returns only those add-ons. Non-admins never see them.
pub async fn list_releases(
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListReleasesQuery>,
) -> Result<Json<ReleasesResponse>, ServerError> {
    if query.tenant.as_deref() == Some(MANAGEMENT_TENANT) {
        auth.require_admin()?;
        let releases = state
            .state_provider
            .list_management_addon_releases()
            .await?;
        return Ok(Json(ReleasesResponse { releases }));
    }
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
    if query.tenant.is_none() && auth.is_admin() {
        let addons = state
            .state_provider
            .list_management_addon_releases()
            .await?;
        releases.retain(|r| {
            !addons
                .iter()
                .any(|a| a.namespace == r.namespace && a.name == r.name)
        });
        releases.extend(addons);
    }
    Ok(Json(ReleasesResponse { releases }))
}

/// `true` for a release in a management-plane namespace (`flux-system` / `keel-system`), i.e. a
/// protected management add-on that is never edited or deleted through the tenant release API.
fn is_management_namespace(namespace: &str) -> bool {
    namespace == keel_core::FLUX_SYSTEM_NAMESPACE || namespace == keel_core::KEEL_SYSTEM_NAMESPACE
}

/// `409` pointing callers at the management add-on PR endpoints.
fn management_addon_config_conflict(namespace: &str, name: &str) -> ServerError {
    ServerError::Conflict(format!(
        "'{namespace}/{name}' is a protected management-plane add-on; change its version or \
         values via POST /api/v1/management/addons/{name}/upgrade (or /rollback), which opens a \
         reviewed GitOps PR"
    ))
}
