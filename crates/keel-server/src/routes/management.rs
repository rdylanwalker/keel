//! Operational health/metrics, SSE event stream, user Git credential storage,
//! and protected management-plane core addon upgrade/rollback endpoints (`SDD.md` §7.3).

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
use keel_core::{KEEL_SYSTEM_NAMESPACE, validate_dns_label};
use keel_git::PullRequestInfo;
use keel_kube::{CrdWatchEvent, UserGitCredentials, user_git_creds_secret_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::AppState;

use super::{open_pr_for_session, stage_commit_push_and_return_to_main};

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
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<EventsStreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let raw_stream: std::pin::Pin<Box<dyn Stream<Item = CrdWatchEvent> + Send>> =
        if let Some(ns) = query.namespace.filter(|s| !s.trim().is_empty()) {
            if !auth.is_admin() {
                let allowed = ns
                    .strip_prefix("tenant-")
                    .is_some_and(|t| auth.require_tenant_access(t).is_ok());
                if !allowed {
                    Box::pin(futures::stream::empty())
                } else {
                    Box::pin(state.telemetry.subscribe_namespace_stream(ns))
                }
            } else {
                Box::pin(state.telemetry.subscribe_namespace_stream(ns))
            }
        } else if auth.is_admin() {
            Box::pin(state.telemetry.subscribe_stream())
        } else {
            let auth_clone = auth.clone();
            Box::pin(state.telemetry.subscribe_stream().filter(move |ev| {
                let allowed = ev
                    .namespace
                    .strip_prefix("tenant-")
                    .is_some_and(|t| auth_clone.require_tenant_access(t).is_ok());
                futures::future::ready(allowed)
            }))
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

        let commit_msg = format!("chore(core-addon): upgrade {id}");
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            [rel_path.as_str()],
        )
        .await?;
        (sha, rel_path)
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        "management",
        &branch,
        format!("chore(core-addon): upgrade {id}"),
        format!(
            "Automated management-plane core addon upgrade for `{id}`\n\n- Commit: `{commit_sha}`\n- Path: `{rel_path}`"
        ),
    )
    .await?;

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

        let commit_msg = format!("revert(core-addon): rollback {id} to {prev_sha}");
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            [rel_path.as_str()],
        )
        .await?;
        (sha, prev_sha, rel_path)
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        "management",
        &branch,
        format!("revert(core-addon): rollback {id}"),
        format!(
            "Automated management-plane core addon rollback for `{id}`\n\n- Target revision: `{prev_sha}`\n- Commit: `{commit_sha}`\n- Path: `{rel_path}`"
        ),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(ManagementAddonProposalResponse {
            id,
            proposal,
            branch,
        }),
    ))
}
