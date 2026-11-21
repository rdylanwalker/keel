//! Provider registry and tenant onboarding/decommissioning endpoints (`SDD.md` §7.3).

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use chrono::Utc;
use keel_core::{SopsManager, tenant_namespace, validate_dns_label};
use keel_git::PullRequestInfo;
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, TenantAgeKey, TenantIsolationConfig, TenantManifestBundle,
    WatchedCrdKind,
};
use keel_pko::build_preview_branch;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;

use super::{
    load_provider_registry, open_pr_for_session, serialize_tenants_kustomization,
    stage_commit_push_and_return_to_main,
};

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

        let commit_msg = format!("feat(providers): {action_verb} provider {id}");
        stage_commit_push_and_return_to_main(&state, &auth, &branch, &commit_msg, [rel_file])
            .await?
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        "providers",
        &branch,
        format!("feat(providers): {action_verb} provider {id}"),
        format!(
            "Automated provider registry update for `{id}` (`enabled={}`)\n\n- Commit: `{commit_sha}`\n- Path: `{rel_file}`",
            req.enabled
        ),
    )
    .await?;

    Ok(Json(ToggleProviderResponse {
        provider,
        proposal,
        branch,
    }))
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
    // `tenant-<id>-admin` is the tenant-admin IdP group; an id ending in `-admin` would make the
    // member group of `<x>-admin` collide with the admin group of `<x>` (OPEN-12).
    if req.id.ends_with("-admin") {
        return Err(ServerError::BadRequest(
            "Tenant id must not end with '-admin' (reserved for the tenant-<id>-admin group)"
                .to_string(),
        ));
    }
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
        if let Err(err) = state
            .secret_store
            .store_tenant_age_key(&req.id, &age_key)
            .await
        {
            tracing::debug!(
                tenant = %req.id,
                error = %err,
                "Deferred storing tenant SOPS age key until tenant namespace exists in cluster"
            );
        }
    }

    // Best-effort tenant Pulumi state passphrase (create-if-absent, never rotated). Deferred to
    // tenant provisioning when the namespace does not exist yet.
    if let Err(err) = state
        .secret_store
        .ensure_pulumi_passphrase(&namespace)
        .await
    {
        tracing::debug!(
            tenant = %req.id,
            error = %err,
            "Deferred tenant Pulumi state passphrase until tenant namespace exists in cluster"
        );
    }

    // Best-effort scoped S3 state identity (`tenants/<id>/*` only). The namespace usually only
    // exists after the onboarding PR merges; the proposal path then provisions lazily.
    if let Some(provisioner) = state.tenant_s3_provisioner.as_ref() {
        match provisioner.provision_tenant_s3_identity(&req.id).await {
            Ok(outcome) => tracing::info!(
                tenant = %req.id,
                ?outcome,
                "Tenant scoped S3 state identity provisioning"
            ),
            Err(err) => tracing::warn!(
                tenant = %req.id,
                error = %err,
                "Deferred tenant scoped S3 state identity provisioning"
            ),
        }
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

        let commit_msg = format!("feat({}): create tenant {}", req.id, req.id);
        stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            [rel_file.as_str(), "tenants/kustomization.yaml"],
        )
        .await?
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        &req.id,
        &branch,
        format!("feat({}): create tenant {}", req.id, req.id),
        format!(
            "Automated tenant onboarding for `{}` (namespace `{}`)\n\n- Commit: `{}`\n- Path: `{}`",
            req.id, namespace, commit_sha, rel_file
        ),
    )
    .await?;

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

        let commit_msg = format!("chore({id}): delete tenant {id}");
        stage_commit_push_and_return_to_main(&state, &auth, &branch, &commit_msg, &staged_paths)
            .await?
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        &id,
        &branch,
        format!("chore({id}): delete tenant {id}"),
        format!(
            "Automated tenant decommission proposal for `{id}`\n\n- Commit: `{commit_sha}`\n- Removed: `{rel_dir}`"
        ),
    )
    .await?;

    Ok(Json(DeleteTenantResponse {
        id,
        proposal,
        branch,
    }))
}
