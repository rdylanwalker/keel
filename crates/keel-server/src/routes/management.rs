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

/// One management-plane core add-on (`clusters/management/core-addons/*.yaml` HelmRelease)
/// with its live Flux status, matched strictly by `(namespace, name)`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManagementAddonStatus {
    /// HelmRelease `metadata.name` (equals the `catalog/core` application id).
    pub id: String,
    pub name: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_version: Option<String>,
    /// Live status label (`Ready`, `Reconciling`, `Error`, ...) or `None` when not tracked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `true` when a live HelmRelease with the same namespace/name was observed.
    pub tracked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<String>,
}

/// Response body for `GET /api/v1/management/addons`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementAddonsResponse {
    pub addons: Vec<ManagementAddonStatus>,
    /// `false` when the server runs without a live Kubernetes connection (Git-only view).
    pub live_status_available: bool,
}

fn live_status_label(status: &keel_kube::LiveResourceStatus) -> &'static str {
    match status {
        keel_kube::LiveResourceStatus::Synced => "Synced",
        keel_kube::LiveResourceStatus::Ready => "Ready",
        keel_kube::LiveResourceStatus::Reconciling => "Reconciling",
        keel_kube::LiveResourceStatus::Deleting => "Deleting",
        keel_kube::LiveResourceStatus::Error => "Error",
    }
}

/// Reads desired management add-on HelmReleases (`keel-system` / `flux-system`) from Git.
async fn read_desired_core_addons(repo_workdir: &Path) -> Vec<ManagementAddonStatus> {
    let dir = repo_workdir.join("clusters/management/core-addons");
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return Vec::new();
    };
    let mut addons = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"));
        if !is_yaml {
            continue;
        }
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        for doc in keel_core::split_yaml_documents(&raw) {
            let Ok(val) = serde_yml::from_str::<Value>(&doc) else {
                continue;
            };
            if val.get("kind").and_then(Value::as_str) != Some("HelmRelease") {
                continue;
            }
            let meta = val.get("metadata");
            let Some(name) = meta.and_then(|m| m.get("name")).and_then(Value::as_str) else {
                continue;
            };
            let namespace = meta
                .and_then(|m| m.get("namespace"))
                .and_then(Value::as_str)
                .unwrap_or(KEEL_SYSTEM_NAMESPACE);
            if namespace != KEEL_SYSTEM_NAMESPACE && namespace != "flux-system" {
                continue;
            }
            let chart_spec = val
                .get("spec")
                .and_then(|s| s.get("chart"))
                .and_then(|c| c.get("spec"));
            let chart = chart_spec
                .and_then(|c| c.get("chart"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let desired_version = chart_spec.and_then(|c| c.get("version")).and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.as_number().map(|n| n.to_string()))
            });
            addons.push(ManagementAddonStatus {
                id: name.to_string(),
                name: name.to_string(),
                namespace: namespace.to_string(),
                chart,
                desired_version,
                status: None,
                tracked: false,
                last_synced: None,
            });
        }
    }
    addons.sort_by(|a, b| (&a.namespace, &a.id).cmp(&(&b.namespace, &b.id)));
    addons.dedup_by(|a, b| a.namespace == b.namespace && a.id == b.id);
    addons
}

/// Overlays live status onto desired add-ons strictly by `(namespace, name)` — never by chart
/// name, so a tenant release using the same chart (e.g. `tenant-x/flagger`) cannot masquerade
/// as the management add-on.
pub fn overlay_management_addon_statuses(
    addons: &mut [ManagementAddonStatus],
    live: &std::collections::HashMap<(String, String), keel_kube::LiveResourceInfo>,
) {
    for addon in addons.iter_mut() {
        if let Some(info) = live.get(&(addon.namespace.clone(), addon.name.clone())) {
            addon.status = Some(live_status_label(&info.status).to_string());
            addon.tracked = true;
            addon.last_synced = info.last_synced.clone();
        }
    }
}

/// `GET /api/v1/management/addons`
///
/// Read-only, admin-only view of the protected management-plane core add-ons with live Flux
/// `HelmRelease` status from `keel-system` / `flux-system`.
pub async fn list_management_addons(
    auth: AuthSession,
    State(state): State<AppState>,
) -> Result<Json<ManagementAddonsResponse>, ServerError> {
    auth.require_admin()?;
    let mut addons = read_desired_core_addons(&state.repo_workdir).await;
    let live = state.state_provider.management_addon_statuses().await;
    overlay_management_addon_statuses(&mut addons, &live);
    Ok(Json(ManagementAddonsResponse {
        addons,
        live_status_available: state.state_provider.has_live_status(),
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

#[cfg(test)]
mod addon_status_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use keel_core::AuthPhase;
    use keel_git::{GitRunner, LocalGitProvider};
    use keel_kube::{InMemorySecretStore, LiveResourceInfo, LiveResourceStatus, TelemetryMux};

    use super::*;
    use crate::auth::SessionClaims;
    use crate::state::{ClusterStateProvider, ReleaseSummary, StackSummary};

    fn session(groups: &[&str]) -> AuthSession {
        AuthSession {
            claims: SessionClaims {
                sub: "user@keel.local".to_string(),
                email: None,
                name: None,
                groups: groups.iter().map(|g| g.to_string()).collect(),
                auth_phase: AuthPhase::OidcActive,
                exp: Utc::now().timestamp() + 3600,
            },
        }
    }

    const FLAGGER: &str = "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: flagger\n  namespace: keel-system\nspec:\n  chart:\n    spec:\n      chart: flagger\n      version: \"1.40.0\"\n";
    const KYVERNO: &str = "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: kyverno\n  namespace: keel-system\nspec:\n  chart:\n    spec:\n      chart: kyverno\n      version: 3.3.0\n---\napiVersion: source.toolkit.fluxcd.io/v1\nkind: HelmRepository\nmetadata:\n  name: kyverno\n  namespace: keel-system\n";

    fn state_with(dir: &Path, provider: Option<Arc<dyn ClusterStateProvider>>) -> AppState {
        let core = dir.join("clusters/management/core-addons");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(core.join("flagger.yaml"), FLAGGER).unwrap();
        std::fs::write(core.join("kyverno.yaml"), KYVERNO).unwrap();
        std::fs::write(core.join("README.md"), "not yaml").unwrap();
        let mut state = AppState::new(
            Arc::new(LocalGitProvider::new(dir.join("remote.git"))),
            Arc::new(InMemorySecretStore::new()),
            TelemetryMux::default(),
            GitRunner::new(),
            dir,
            "file:///dev/null",
            AuthPhase::OidcActive,
        );
        if let Some(p) = provider {
            state.state_provider = p;
        }
        state
    }

    struct StubProvider {
        live: HashMap<(String, String), LiveResourceInfo>,
    }

    #[async_trait]
    impl ClusterStateProvider for StubProvider {
        async fn list_stacks(
            &self,
            _tenant: Option<&str>,
            _provider: Option<&str>,
        ) -> Result<Vec<StackSummary>, ServerError> {
            Ok(Vec::new())
        }
        async fn list_releases(
            &self,
            _tenant: Option<&str>,
        ) -> Result<Vec<ReleaseSummary>, ServerError> {
            Ok(Vec::new())
        }
        async fn management_addon_statuses(&self) -> HashMap<(String, String), LiveResourceInfo> {
            self.live.clone()
        }
        fn has_live_status(&self) -> bool {
            true
        }
    }

    fn info(status: LiveResourceStatus) -> LiveResourceInfo {
        LiveResourceInfo {
            last_synced: Some("2026-11-20T10:00:00Z".to_string()),
            ..LiveResourceInfo::from_status(status)
        }
    }

    #[tokio::test]
    async fn offline_provider_reports_untracked_addons_from_git() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with(tmp.path(), None);
        let Json(resp) = list_management_addons(session(&["keel-admins"]), State(state))
            .await
            .unwrap();
        assert!(!resp.live_status_available);
        assert_eq!(resp.addons.len(), 2);
        let flagger = resp.addons.iter().find(|a| a.id == "flagger").unwrap();
        assert_eq!(flagger.namespace, "keel-system");
        assert_eq!(flagger.chart.as_deref(), Some("flagger"));
        assert_eq!(flagger.desired_version.as_deref(), Some("1.40.0"));
        assert!(!flagger.tracked);
        assert!(flagger.status.is_none());
        let kyverno = resp.addons.iter().find(|a| a.id == "kyverno").unwrap();
        assert_eq!(kyverno.desired_version.as_deref(), Some("3.3.0"));
    }

    #[tokio::test]
    async fn live_status_matches_by_namespace_and_name_not_chart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut live = HashMap::new();
        live.insert(
            ("keel-system".to_string(), "kyverno".to_string()),
            info(LiveResourceStatus::Ready),
        );
        // A tenant release using the same chart must never be reported as the mgmt add-on.
        live.insert(
            ("tenant-default".to_string(), "e2e-lab-flagger".to_string()),
            info(LiveResourceStatus::Ready),
        );
        live.insert(
            ("tenant-default".to_string(), "flagger".to_string()),
            info(LiveResourceStatus::Ready),
        );
        let state = state_with(tmp.path(), Some(Arc::new(StubProvider { live })));
        let Json(resp) = list_management_addons(session(&["keel-admins"]), State(state))
            .await
            .unwrap();
        assert!(resp.live_status_available);
        let kyverno = resp.addons.iter().find(|a| a.id == "kyverno").unwrap();
        assert!(kyverno.tracked);
        assert_eq!(kyverno.status.as_deref(), Some("Ready"));
        assert_eq!(kyverno.last_synced.as_deref(), Some("2026-11-20T10:00:00Z"));
        let flagger = resp.addons.iter().find(|a| a.id == "flagger").unwrap();
        assert!(!flagger.tracked);
        assert!(flagger.status.is_none());
    }

    #[tokio::test]
    async fn non_admin_is_forbidden() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with(tmp.path(), None);
        let err = list_management_addons(session(&["tenant-acme-admin"]), State(state))
            .await
            .unwrap_err();
        assert!(matches!(err, ServerError::Forbidden(_)));
    }

    #[tokio::test]
    async fn missing_core_addons_dir_yields_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_desired_core_addons(tmp.path()).await.is_empty());
    }

    #[test]
    fn serializes_camel_case_and_omits_unknown_status() {
        let addon = ManagementAddonStatus {
            id: "flagger".into(),
            name: "flagger".into(),
            namespace: "keel-system".into(),
            chart: Some("flagger".into()),
            desired_version: Some("1.40.0".into()),
            status: None,
            tracked: false,
            last_synced: None,
        };
        let v = serde_json::to_value(&addon).unwrap();
        assert_eq!(v["desiredVersion"], "1.40.0");
        assert_eq!(v["tracked"], false);
        assert!(v.get("status").is_none());
    }
}
