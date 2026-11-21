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
use keel_core::{
    KEEL_SYSTEM_NAMESPACE, MANAGEMENT_CORE_ADDONS_DIR, ManagementAddonKind, ManagementAddonRef,
    apply_management_addon_upgrade, disallowed_value_changes, ensure_management_addon_protected,
    ensure_management_addon_upgradable, management_addon_chart, management_addon_desired_version,
    management_addon_ref, render_management_addon_document, replace_yaml_document,
    validate_dns_label, validate_management_addon_document,
};
use keel_git::PullRequestInfo;
use keel_kube::{CrdWatchEvent, UserGitCredentials, user_git_creds_secret_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::state::{AppState, MANAGEMENT_TENANT, ManagementAddonKey, ReleaseSummary};

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

/// One management-plane core add-on (`clusters/management/core-addons/*.yaml` Flux
/// `HelmRelease` or Flux `Kustomization`) with its live Flux status, matched strictly by
/// `(kind, namespace, name)`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManagementAddonStatus {
    /// Flux object `metadata.name` (equals the `catalog/core` application id).
    pub id: String,
    pub name: String,
    pub namespace: String,
    /// Flux object kind reconciling the add-on (`HelmRelease` or `Kustomization`).
    pub kind: ManagementAddonKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart: Option<String>,
    /// Chart version (`HelmRelease`) or first `spec.images[].newTag` (`Kustomization`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_version: Option<String>,
    /// Live status label (`Ready`, `Reconciling`, `Error`, ...) or `None` when not tracked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `true` when a live Flux object with the same kind/namespace/name was observed.
    pub tracked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<String>,
    /// `true` when more than one Flux object in Git is named `id`; upgrade/rollback of a
    /// conflicting add-on is refused with `409` until the duplicate is removed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub conflict: bool,
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

fn is_yaml_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
}

/// Lists `clusters/management/core-addons/*.yaml` sorted by file name (deterministic matching).
async fn core_addon_yaml_files(repo_workdir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries = tokio::fs::read_dir(repo_workdir.join(MANAGEMENT_CORE_ADDONS_DIR)).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if is_yaml_path(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Parses every document of a core add-on file, keeping its index (as numbered by
/// [`keel_core::split_yaml_documents`]) and verbatim text. Unparseable documents are skipped.
fn parse_documents(raw: &str) -> impl Iterator<Item = (usize, String, Value)> + '_ {
    keel_core::split_yaml_documents(raw)
        .into_iter()
        .enumerate()
        .filter_map(|(idx, text)| {
            let val = serde_yml::from_str::<Value>(&text).ok()?;
            Some((idx, text, val))
        })
}

/// One add-on Flux object located in a core add-on file: document index (as numbered by
/// [`keel_core::split_yaml_documents`]), verbatim text, parsed value and identity.
type AddonDocument = (usize, String, Value, ManagementAddonRef);

/// Finds every add-on Flux object (`HelmRelease` / Flux `Kustomization` named `id`) in `raw`,
/// optionally constrained to an exact `(kind, namespace)`. Never matches Services,
/// ServiceAccounts or other supporting documents that share the add-on name.
fn find_addon_documents(
    raw: &str,
    id: &str,
    expect: Option<&ManagementAddonRef>,
) -> Vec<AddonDocument> {
    parse_documents(raw)
        .filter_map(|(idx, text, val)| {
            let addon = management_addon_ref(&val)?;
            let matches = addon.name == id
                && expect.is_none_or(|e| e.kind == addon.kind && e.namespace == addon.namespace);
            matches.then_some((idx, text, val, addon))
        })
        .collect()
}

/// `409` for an add-on id defined by more than one Flux object. Only repository-relative paths
/// are reported (never the server's workdir).
fn duplicate_addon_conflict(id: &str, first: &str, second: &str) -> ServerError {
    let location = if first == second {
        format!("more than once in {first}")
    } else {
        format!("in both {first} and {second}")
    };
    ServerError::Conflict(format!(
        "Core add-on '{id}' is defined by more than one Flux object ({location}); remove the \
         duplicate in Git before upgrading or rolling it back"
    ))
}

/// Flux `Kustomization`s that wire the GitOps tree itself (`clusters/management/flux-system`).
/// They are infrastructure roots, never management add-ons, even if mis-placed under
/// `core-addons/`.
pub const MANAGEMENT_ROOT_KUSTOMIZATIONS: [&str; 3] =
    ["flux-system", "core-addons", "keel-tenants"];

/// `true` for an infrastructure root Flux `Kustomization` in `flux-system`.
pub fn is_management_root_kustomization(
    kind: ManagementAddonKind,
    namespace: &str,
    name: &str,
) -> bool {
    kind == ManagementAddonKind::Kustomization
        && namespace == keel_core::FLUX_SYSTEM_NAMESPACE
        && MANAGEMENT_ROOT_KUSTOMIZATIONS.contains(&name)
}

/// Reads desired management add-ons (Flux `HelmRelease`s and Flux `Kustomization`s in
/// `keel-system` / `flux-system`) from Git.
pub(crate) async fn read_desired_core_addons(repo_workdir: &Path) -> Vec<ManagementAddonStatus> {
    let Ok(files) = core_addon_yaml_files(repo_workdir).await else {
        return Vec::new();
    };
    let mut addons = Vec::new();
    for path in files {
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        for (_, _, val) in parse_documents(&raw) {
            let Some(addon) = management_addon_ref(&val) else {
                continue;
            };
            if is_management_root_kustomization(addon.kind, &addon.namespace, &addon.name) {
                continue;
            }
            addons.push(ManagementAddonStatus {
                id: addon.name.clone(),
                name: addon.name,
                namespace: addon.namespace,
                kind: addon.kind,
                chart: management_addon_chart(&val),
                desired_version: management_addon_desired_version(&val),
                status: None,
                tracked: false,
                last_synced: None,
                conflict: false,
            });
        }
    }
    addons.sort_by(|a, b| (&a.namespace, &a.id, a.kind).cmp(&(&b.namespace, &b.id, b.kind)));
    // Every Flux object is listed; ids defined more than once (in one file or across files, of any
    // kind/namespace) are flagged instead of silently deduplicated, matching the 409 on upgrade.
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for addon in &addons {
        *counts.entry(addon.id.clone()).or_default() += 1;
    }
    for addon in &mut addons {
        addon.conflict = counts.get(&addon.id).is_some_and(|n| *n > 1);
    }
    addons
}

/// Overlays live status onto desired add-ons strictly by `(kind, namespace, name)` — never by
/// chart name, so a tenant release using the same chart (e.g. `tenant-x/flagger`) cannot
/// masquerade as the management add-on.
pub fn overlay_management_addon_statuses(
    addons: &mut [ManagementAddonStatus],
    live: &std::collections::HashMap<ManagementAddonKey, keel_kube::LiveResourceInfo>,
) {
    for addon in addons.iter_mut() {
        let key = (addon.kind, addon.namespace.clone(), addon.name.clone());
        if let Some(info) = live.get(&key) {
            addon.status = Some(live_status_label(&info.status).to_string());
            addon.tracked = true;
            addon.last_synced = info.last_synced.clone();
        }
    }
}

/// Maps a management add-on onto the `GET /api/v1/releases` shape so the Topology view can
/// watch, diagnose and reconcile it like any other application. Management add-ons are always
/// protected (they live in `flux-system` / `keel-system` and carry `keel.io/protect`).
pub(crate) fn management_addon_release(addon: ManagementAddonStatus) -> ReleaseSummary {
    let runtime = match addon.kind {
        ManagementAddonKind::HelmRelease => "helm",
        ManagementAddonKind::Kustomization => "kustomize",
    };
    ReleaseSummary {
        chart: addon.chart.clone().unwrap_or_else(|| addon.id.clone()),
        version: addon.desired_version.clone(),
        runtime: runtime.to_string(),
        tenant: Some(MANAGEMENT_TENANT.to_string()),
        last_synced: addon.last_synced,
        status: addon.status.unwrap_or_else(|| "Pending".to_string()),
        management_addon: true,
        protected: true,
        catalog_id: Some(addon.id),
        addon_kind: Some(addon.kind),
        desired_version: addon.desired_version,
        conflict: addon.conflict,
        name: addon.name,
        namespace: addon.namespace,
        ..ReleaseSummary::default()
    }
}

/// `GET /api/v1/management/addons`
///
/// Read-only, admin-only view of the protected management-plane core add-ons with live Flux
/// `HelmRelease` / `Kustomization` status from `keel-system` / `flux-system`.
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

/// The add-on's Flux object located inside a (possibly multi-document) core add-on file.
#[derive(Debug)]
struct CoreAddonManifest {
    abs_path: PathBuf,
    rel_path: String,
    /// Full file contents (all documents).
    raw: String,
    /// Index of the add-on document (as numbered by `split_yaml_documents`).
    doc_index: usize,
    /// Verbatim text of the add-on document.
    doc_text: String,
    doc: Value,
    addon: ManagementAddonRef,
}

fn core_addon_not_found(id: &str) -> ServerError {
    ServerError::NotFound(format!(
        "Core add-on '{id}' (Flux HelmRelease or Kustomization) not found under \
         {MANAGEMENT_CORE_ADDONS_DIR}/"
    ))
}

async fn find_core_addon_manifest(
    repo_workdir: &Path,
    id: &str,
) -> Result<CoreAddonManifest, ServerError> {
    let files = core_addon_yaml_files(repo_workdir)
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => core_addon_not_found(id),
            _ => ServerError::Io(err),
        })?;

    let mut found: Option<CoreAddonManifest> = None;
    for abs_path in files {
        let raw = tokio::fs::read_to_string(&abs_path).await?;
        let mut matches = find_addon_documents(&raw, id, None).into_iter();
        let Some((doc_index, doc_text, doc, addon)) = matches.next() else {
            continue;
        };
        let file_name = abs_path
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or_else(|| ServerError::Internal("Invalid core addon filename".to_string()))?;
        let rel_path = format!("{MANAGEMENT_CORE_ADDONS_DIR}/{file_name}");
        if matches.next().is_some() {
            return Err(duplicate_addon_conflict(id, &rel_path, &rel_path));
        }
        if let Some(existing) = &found {
            return Err(duplicate_addon_conflict(id, &existing.rel_path, &rel_path));
        }
        found = Some(CoreAddonManifest {
            abs_path,
            rel_path,
            raw,
            doc_index,
            doc_text,
            doc,
            addon,
        });
    }
    found.ok_or_else(|| core_addon_not_found(id))
}

/// Renders `updated` from the add-on document's `base_text` (verbatim when unchanged, a targeted
/// version edit when possible, otherwise re-serialized with its leading comments) and splices it
/// into the current file, leaving every other document byte-for-byte unchanged.
fn render_updated_core_addon_file(
    manifest: &CoreAddonManifest,
    base_text: &str,
    updated: &Value,
) -> Result<String, ServerError> {
    let replacement = render_management_addon_document(base_text, updated)?;
    replace_yaml_document(&manifest.raw, manifest.doc_index, &replacement)
        .ok_or_else(|| ServerError::Internal("Core add-on document index out of range".to_string()))
}

/// `POST /api/v1/management/addons/{id}/upgrade`
///
/// Updates only the add-on's Flux object (`HelmRelease`: chart version + values; Flux
/// `Kustomization`: `spec.images[].newTag`) inside its core add-on file under
/// `clusters/management/core-addons/` on a new branch and opens a PR via `ForgeProvider`.
/// Supporting documents in the same file are preserved verbatim, version-only upgrades keep the
/// add-on document's comments and key order, and value changes outside the add-on's allowlist
/// (`keel_core::management_addon_mutable_values`) are rejected with 400.
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

    let (commit_sha, rel_path, kind) = {
        let _guard = state.git_lock.lock().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;

        let mut manifest = find_core_addon_manifest(&state.repo_workdir, &id).await?;
        apply_management_addon_upgrade(
            &id,
            &mut manifest.doc,
            req.version.as_deref(),
            &req.values,
        )?;
        let updated_yaml =
            render_updated_core_addon_file(&manifest, &manifest.doc_text, &manifest.doc)?;

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;
        tokio::fs::write(&manifest.abs_path, &updated_yaml).await?;

        let commit_msg = format!("chore(core-addon): upgrade {id}");
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            [manifest.rel_path.as_str()],
        )
        .await?;
        (sha, manifest.rel_path, manifest.addon.kind)
    };

    let proposal = open_pr_for_session(
        &state,
        &auth,
        "management",
        &branch,
        format!("chore(core-addon): upgrade {id}"),
        format!(
            "Automated management-plane core addon upgrade for `{id}` (Flux {kind})\n\n- Commit: `{commit_sha}`\n- Path: `{rel_path}`"
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

fn protected_copy(doc: &Value) -> Value {
    let mut copy = doc.clone();
    // Non-mapping documents are never add-ons; leave them unchanged for the comparison.
    let _ = ensure_management_addon_protected(&mut copy);
    copy
}

/// How many commits touching a core add-on file are searched for a previous add-on revision.
const ROLLBACK_SEARCH_DEPTH: usize = 25;

/// `POST /api/v1/management/addons/{id}/rollback`
///
/// Restores only the add-on's Flux object to its most recent earlier Git revision that differs
/// from the current one (other documents in the file are kept as they are now) on a new branch
/// and opens a PR via `ForgeProvider`. The restored document always keeps the
/// `keel.io/protect: "true"` label and must carry valid versions; rollbacks that would change
/// values outside the add-on's allowlist are rejected with 400.
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

        let manifest = find_core_addon_manifest(&state.repo_workdir, &id).await?;
        // A rollback replaces the pod just like an upgrade does (e.g. an ephemeral Forgejo).
        ensure_management_addon_upgradable(&id, &manifest.doc)
            .map_err(|e| ServerError::BadRequest(e.to_string()))?;

        let commits = state
            .git_runner
            .log_file_commits(
                &state.repo_workdir,
                &manifest.rel_path,
                ROLLBACK_SEARCH_DEPTH,
            )
            .await?;
        let mut target: Option<(String, String, Value)> = None;
        for sha in commits {
            let Some(prev_raw) = state
                .git_runner
                .show_file_at_ref(&state.repo_workdir, &sha, &manifest.rel_path)
                .await?
            else {
                continue;
            };
            let mut prev_matches = find_addon_documents(&prev_raw, &id, Some(&manifest.addon));
            if prev_matches.len() != 1 {
                // Absent, or ambiguous at that revision: not a safe rollback target.
                continue;
            }
            let (_, prev_text, prev_doc, _) = prev_matches.remove(0);
            // Compare with the protect label asserted on both sides: a revision that differs
            // only by a missing label is not a meaningful rollback target.
            if protected_copy(&prev_doc) != protected_copy(&manifest.doc) {
                target = Some((sha, prev_text, prev_doc));
                break;
            }
        }
        let Some((prev_sha, prev_text, prev_doc)) = target else {
            return Err(ServerError::BadRequest(format!(
                "No previous revision of core add-on '{id}' found to roll back to"
            )));
        };

        let null = Value::Null;
        let disallowed = disallowed_value_changes(
            &id,
            manifest.doc.pointer("/spec/values").unwrap_or(&null),
            prev_doc.pointer("/spec/values").unwrap_or(&null),
        );
        if !disallowed.is_empty() {
            return Err(ServerError::BadRequest(format!(
                "Rolling back core add-on '{id}' to {prev_sha} would change locked settings ({}); \
                 edit the manifest in Git directly instead",
                disallowed.join(", ")
            )));
        }
        validate_management_addon_document(&id, &prev_doc).map_err(|e| {
            ServerError::BadRequest(format!(
                "Cannot roll back core add-on '{id}' to {prev_sha}: {e}"
            ))
        })?;
        // Never let a rollback drop the anti-lockout marker (AGENTS.md guardrail 7).
        let mut restored = prev_doc;
        ensure_management_addon_protected(&mut restored)?;
        let updated_yaml = render_updated_core_addon_file(&manifest, &prev_text, &restored)?;

        state
            .git_runner
            .checkout_new_branch(&state.repo_workdir, &branch)
            .await?;
        tokio::fs::write(&manifest.abs_path, &updated_yaml).await?;

        let commit_msg = format!("revert(core-addon): rollback {id} to {prev_sha}");
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            [manifest.rel_path.as_str()],
        )
        .await?;
        (sha, prev_sha, manifest.rel_path)
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
        live: HashMap<ManagementAddonKey, LiveResourceInfo>,
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
        async fn management_addon_statuses(&self) -> HashMap<ManagementAddonKey, LiveResourceInfo> {
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
        // Infrastructure root Kustomizations mis-placed under `core-addons/` are never add-ons.
        std::fs::write(
            tmp.path().join("clusters/management/core-addons/roots.yaml"),
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: core-addons\n  namespace: flux-system\nspec:\n  path: ./clusters/management/core-addons\n---\napiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: flux-system\n  namespace: flux-system\nspec:\n  path: ./clusters/management\n---\napiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: keel-tenants\n  namespace: flux-system\nspec:\n  path: ./tenants\n",
        )
        .unwrap();
        let Json(resp) = list_management_addons(session(&["keel-admins"]), State(state))
            .await
            .unwrap();
        assert!(!resp.live_status_available);
        for root in ["core-addons", "flux-system", "keel-tenants"] {
            assert!(
                resp.addons.iter().all(|a| a.id != root),
                "root Kustomization {root} listed as an add-on"
            );
        }
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
        let hr = ManagementAddonKind::HelmRelease;
        let mut live = HashMap::new();
        live.insert(
            (hr, "keel-system".to_string(), "kyverno".to_string()),
            info(LiveResourceStatus::Ready),
        );
        // A tenant release using the same chart must never be reported as the mgmt add-on.
        live.insert(
            (
                hr,
                "tenant-default".to_string(),
                "e2e-lab-flagger".to_string(),
            ),
            info(LiveResourceStatus::Ready),
        );
        live.insert(
            (hr, "tenant-default".to_string(), "flagger".to_string()),
            info(LiveResourceStatus::Ready),
        );
        // A Flux Kustomization sharing the add-on's namespace/name is a different object.
        live.insert(
            (
                ManagementAddonKind::Kustomization,
                "keel-system".to_string(),
                "flagger".to_string(),
            ),
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
            kind: ManagementAddonKind::HelmRelease,
            chart: Some("flagger".into()),
            desired_version: Some("1.40.0".into()),
            status: None,
            tracked: false,
            last_synced: None,
            conflict: false,
        };
        let v = serde_json::to_value(&addon).unwrap();
        assert_eq!(v["desiredVersion"], "1.40.0");
        assert_eq!(v["kind"], "HelmRelease");
        assert_eq!(v["tracked"], false);
        assert!(v.get("status").is_none());
        assert!(v.get("conflict").is_none());
    }

    // --- Multi-document, kind-aware upgrade / rollback --------------------------------------

    const CERT_MANAGER_MULTI: &str = r#"# cert-manager core add-on (header comment must survive upgrades)
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: cert-manager
  namespace: keel-system
  labels:
    keel.io/protect: "true"
spec:
  interval: 10m
  chart:
    spec:
      chart: cert-manager
      version: "1.18.2"
  values:
    crds:
      enabled: true
---
# Self-signed bootstrap issuer
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: keel-selfsigned
spec:
  selfSigned: {}   # inline comment kept verbatim
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: keel-ca
  namespace: keel-system
spec:
  isCA: true
  commonName: keel-ca
  secretName: keel-ca
  issuerRef:
    name: keel-selfsigned
    kind: ClusterIssuer
"#;

    const PKO_MULTI: &str = r#"apiVersion: v1
kind: ServiceAccount
metadata:
  name: keel-management-pko
  namespace: flux-system
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: pulumi-kubernetes-operator
  namespace: keel-previews
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: pulumi-kubernetes-operator
  namespace: keel-system
spec:
  chart:
    spec:
      chart: pulumi-kubernetes-operator
      version: "2.3.0"
"#;

    const RAUTHY_KUSTOMIZATION: &str = r#"# Rauthy plain-manifest add-on (manifests under clusters/management/addons/rauthy)
apiVersion: v1
kind: Service
metadata:
  name: rauthy
  namespace: keel-system
spec:
  ports:
    - port: 8080
---
apiVersion: kustomize.toolkit.fluxcd.io/v1
kind: Kustomization
metadata:
  name: rauthy
  namespace: flux-system
spec:
  interval: 10m
  path: ./clusters/management/addons/rauthy
  prune: false
  sourceRef:
    kind: GitRepository
    name: flux-system
  images:
    - name: ghcr.io/sebadob/rauthy
      newTag: "0.37.2"
"#;

    /// Only a Service named `dex` — never a management add-on.
    const SERVICE_ONLY: &str = "apiVersion: v1\nkind: Service\nmetadata:\n  name: dex\n  namespace: keel-system\nspec:\n  ports:\n    - port: 5556\n";

    /// kustomize.config.k8s.io Kustomization listing the core add-on files.
    const KUSTOMIZE_CONFIG: &str = "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nmetadata:\n  name: rauthy\nresources:\n  - cert-manager.yaml\n";

    fn admin() -> AuthSession {
        session(&["keel-admins"])
    }

    async fn git_state() -> (tempfile::TempDir, AppState, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let workdir = tmp.path().join("workdir");
        let git = GitRunner::new();
        git.init_bare(&bare).await.unwrap();
        git.clone(bare.to_str().unwrap(), &workdir).await.unwrap();
        let core = workdir.join(MANAGEMENT_CORE_ADDONS_DIR);
        tokio::fs::create_dir_all(&core).await.unwrap();
        for (file, body) in [
            ("cert-manager.yaml", CERT_MANAGER_MULTI),
            ("pko.yaml", PKO_MULTI),
            ("rauthy.yaml", RAUTHY_KUSTOMIZATION),
            ("dex-service.yaml", SERVICE_ONLY),
            ("kustomization.yaml", KUSTOMIZE_CONFIG),
        ] {
            tokio::fs::write(core.join(file), body).await.unwrap();
        }
        commit_and_push(&git, &workdir, "chore: seed core add-ons").await;
        let state = AppState::new(
            Arc::new(LocalGitProvider::new(&bare)),
            Arc::new(InMemorySecretStore::new()),
            TelemetryMux::default(),
            git,
            &workdir,
            bare.to_string_lossy().to_string(),
            AuthPhase::OidcActive,
        );
        (tmp, state, workdir)
    }

    async fn commit_and_push(git: &GitRunner, workdir: &Path, msg: &str) {
        git.add(workdir, ["."]).await.unwrap();
        git.commit(
            workdir,
            msg,
            &keel_git::CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
        git.push(workdir, "origin", "HEAD:refs/heads/main")
            .await
            .unwrap();
    }

    /// Reads `file` from `branch` and switches the worktree back to `main`.
    async fn read_on_branch(state: &AppState, workdir: &Path, branch: &str, file: &str) -> String {
        state
            .git_runner
            .checkout_branch(workdir, branch)
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(workdir.join(MANAGEMENT_CORE_ADDONS_DIR).join(file))
            .await
            .unwrap();
        state
            .git_runner
            .checkout_branch(workdir, "main")
            .await
            .unwrap();
        raw
    }

    async fn upgrade(
        state: &AppState,
        id: &str,
        version: Option<&str>,
        values: Value,
    ) -> Result<ManagementAddonProposalResponse, ServerError> {
        upgrade_management_addon(
            admin(),
            State(state.clone()),
            AxumPath(id.to_string()),
            Json(UpgradeManagementAddonRequest {
                version: version.map(str::to_owned),
                values,
            }),
        )
        .await
        .map(|(_, Json(resp))| resp)
    }

    #[tokio::test]
    async fn upgrade_preserves_supporting_documents_verbatim() {
        let (_tmp, state, workdir) = git_state().await;
        let resp = upgrade(
            &state,
            "cert-manager",
            Some("1.19.1"),
            serde_json::json!({"prometheus": {"enabled": true}}),
        )
        .await
        .unwrap();
        let raw = read_on_branch(&state, &workdir, &resp.branch, "cert-manager.yaml").await;

        let before = keel_core::split_yaml_documents(CERT_MANAGER_MULTI);
        let after = keel_core::split_yaml_documents(&raw);
        assert_eq!(after.len(), 3, "{raw}");
        assert_eq!(after[1], before[1], "ClusterIssuer must be untouched");
        assert_eq!(after[2], before[2], "Certificate must be untouched");
        assert!(after[0].starts_with("# cert-manager core add-on"));
        let hr: Value = serde_yml::from_str(&after[0]).unwrap();
        assert_eq!(hr.pointer("/spec/chart/spec/version").unwrap(), "1.19.1");
        assert_eq!(hr.pointer("/spec/values/crds/enabled").unwrap(), true);
        assert_eq!(hr.pointer("/spec/values/prometheus/enabled").unwrap(), true);
    }

    #[tokio::test]
    async fn upgrade_finds_helmrelease_after_service_accounts() {
        let (_tmp, state, workdir) = git_state().await;
        let resp = upgrade(
            &state,
            "pulumi-kubernetes-operator",
            Some("2.4.0"),
            Value::Null,
        )
        .await
        .unwrap();
        let raw = read_on_branch(&state, &workdir, &resp.branch, "pko.yaml").await;
        let before = keel_core::split_yaml_documents(PKO_MULTI);
        let after = keel_core::split_yaml_documents(&raw);
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[0]);
        // The ServiceAccount sharing the add-on's name is not the add-on.
        assert_eq!(after[1], before[1]);
        let hr: Value = serde_yml::from_str(&after[2]).unwrap();
        assert_eq!(hr["kind"], "HelmRelease");
        assert_eq!(hr.pointer("/spec/chart/spec/version").unwrap(), "2.4.0");
        assert_eq!(
            hr.pointer("/metadata/labels/keel.io~1protect").unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn version_only_upgrade_keeps_document_comments_and_key_order() {
        let (_tmp, state, workdir) = git_state().await;
        let resp = upgrade(&state, "cert-manager", Some("1.19.1"), Value::Null)
            .await
            .unwrap();
        let raw = read_on_branch(&state, &workdir, &resp.branch, "cert-manager.yaml").await;
        assert_eq!(
            raw,
            CERT_MANAGER_MULTI.replace("version: \"1.18.2\"", "version: \"1.19.1\"")
        );
    }

    #[tokio::test]
    async fn duplicate_flux_objects_in_one_file_conflict_and_are_flagged() {
        let (_tmp, state, workdir) = git_state().await;
        let dup = format!(
            "{FLAGGER}---\napiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: flagger\n  namespace: flux-system\nspec:\n  chart:\n    spec:\n      chart: flagger\n      version: \"1.41.0\"\n"
        );
        tokio::fs::write(
            workdir
                .join(MANAGEMENT_CORE_ADDONS_DIR)
                .join("flagger.yaml"),
            dup,
        )
        .await
        .unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: duplicate flagger").await;

        let err = upgrade(&state, "flagger", Some("1.42.0"), Value::Null)
            .await
            .unwrap_err();
        let ServerError::Conflict(msg) = &err else {
            panic!("expected 409, got {err:?}");
        };
        assert!(
            msg.contains("more than once in clusters/management/core-addons/flagger.yaml"),
            "{msg}"
        );
        assert!(!msg.contains(&workdir.display().to_string()), "{msg}");
        let err = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("flagger".to_string()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ServerError::Conflict(_)), "{err:?}");

        let Json(resp) = list_management_addons(admin(), State(state)).await.unwrap();
        let flagger: Vec<_> = resp.addons.iter().filter(|a| a.id == "flagger").collect();
        assert_eq!(flagger.len(), 2);
        assert!(flagger.iter().all(|a| a.conflict));
        assert!(
            resp.addons
                .iter()
                .filter(|a| a.id != "flagger")
                .all(|a| !a.conflict)
        );
    }

    #[tokio::test]
    async fn duplicates_across_files_report_relative_paths() {
        let (_tmp, state, workdir) = git_state().await;
        tokio::fs::write(
            workdir
                .join(MANAGEMENT_CORE_ADDONS_DIR)
                .join("zz-rauthy-copy.yaml"),
            RAUTHY_KUSTOMIZATION,
        )
        .await
        .unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: copy rauthy").await;
        let err = upgrade(&state, "rauthy", Some("0.38.0"), Value::Null)
            .await
            .unwrap_err();
        let ServerError::Conflict(msg) = &err else {
            panic!("expected 409, got {err:?}");
        };
        assert!(
            msg.contains("clusters/management/core-addons/rauthy.yaml and clusters/management/core-addons/zz-rauthy-copy.yaml"),
            "{msg}"
        );
        assert!(!msg.contains(&workdir.display().to_string()), "{msg}");
    }

    #[tokio::test]
    async fn kustomization_addon_upgrades_image_tag_and_rejects_values() {
        let (_tmp, state, workdir) = git_state().await;
        let resp = upgrade(&state, "rauthy", Some("0.38.0"), Value::Null)
            .await
            .unwrap();
        let raw = read_on_branch(&state, &workdir, &resp.branch, "rauthy.yaml").await;
        let before = keel_core::split_yaml_documents(RAUTHY_KUSTOMIZATION);
        let after = keel_core::split_yaml_documents(&raw);
        assert_eq!(
            after[0], before[0],
            "Service named like the add-on is untouched"
        );
        let ks: Value = serde_yml::from_str(&after[1]).unwrap();
        assert_eq!(ks["kind"], "Kustomization");
        assert_eq!(ks.pointer("/spec/images/0/newTag").unwrap(), "0.38.0");
        assert_eq!(
            ks.pointer("/spec/path").unwrap(),
            "./clusters/management/addons/rauthy"
        );
        assert!(ks.pointer("/spec/chart").is_none());

        let err = upgrade(&state, "rauthy", None, serde_json::json!({"replicas": 3}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ServerError::Core(keel_core::CoreError::Validation(m)) if m.contains("Kustomization")),
            "{err:?}"
        );
        let err = upgrade(&state, "rauthy", Some("0.38.0\nevil: x"), Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ServerError::Core(keel_core::CoreError::Validation(_))
        ));
    }

    #[tokio::test]
    async fn non_flux_documents_named_like_the_id_are_not_addons() {
        let (_tmp, state, _workdir) = git_state().await;
        let err = upgrade(&state, "dex", Some("2.0.0"), Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, ServerError::NotFound(_)), "{err:?}");
        let err =
            rollback_management_addon(admin(), State(state.clone()), AxumPath("dex".to_string()))
                .await
                .unwrap_err();
        assert!(matches!(err, ServerError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn listing_includes_kustomization_addons_with_kind() {
        let (_tmp, state, _workdir) = git_state().await;
        let mut live = HashMap::new();
        live.insert(
            (
                ManagementAddonKind::Kustomization,
                "flux-system".to_string(),
                "rauthy".to_string(),
            ),
            info(LiveResourceStatus::Ready),
        );
        let mut state = state;
        state.state_provider = Arc::new(StubProvider { live });
        let Json(resp) = list_management_addons(admin(), State(state)).await.unwrap();
        let ids: Vec<&str> = resp.addons.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["rauthy", "cert-manager", "pulumi-kubernetes-operator"]
        );
        let rauthy = &resp.addons[0];
        assert_eq!(rauthy.kind, ManagementAddonKind::Kustomization);
        assert_eq!(rauthy.namespace, "flux-system");
        assert_eq!(rauthy.desired_version.as_deref(), Some("0.37.2"));
        assert!(rauthy.chart.is_none());
        assert!(rauthy.tracked);
        assert_eq!(rauthy.status.as_deref(), Some("Ready"));
        assert!(!resp.addons[1].tracked);
    }

    #[tokio::test]
    async fn rollback_restores_only_the_addon_document() {
        let (_tmp, state, workdir) = git_state().await;
        let path = workdir
            .join(MANAGEMENT_CORE_ADDONS_DIR)
            .join("cert-manager.yaml");

        // Commit 2: bump the HelmRelease. Commit 3: edit only the ClusterIssuer document.
        let bumped = CERT_MANAGER_MULTI.replace("\"1.18.2\"", "\"1.19.0\"");
        tokio::fs::write(&path, &bumped).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: bump cert-manager").await;
        let issuer_edit = bumped.replace(
            "name: keel-selfsigned\nspec:",
            "name: keel-selfsigned\n  labels:\n    team: platform\nspec:",
        );
        assert_ne!(issuer_edit, bumped);
        tokio::fs::write(&path, &issuer_edit).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: label issuer").await;

        let (_, Json(resp)) = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("cert-manager".to_string()),
        )
        .await
        .unwrap();
        let raw = read_on_branch(&state, &workdir, &resp.branch, "cert-manager.yaml").await;
        let expected = issuer_edit.replace("\"1.19.0\"", "\"1.18.2\"");
        assert_eq!(raw, expected);
    }

    #[tokio::test]
    async fn rollback_reasserts_protect_label_and_validates_versions() {
        let (_tmp, state, workdir) = git_state().await;
        let path = workdir
            .join(MANAGEMENT_CORE_ADDONS_DIR)
            .join("cert-manager.yaml");
        let unlabelled =
            CERT_MANAGER_MULTI.replace("  labels:\n    keel.io/protect: \"true\"\n", "");
        assert_ne!(unlabelled, CERT_MANAGER_MULTI);
        tokio::fs::write(&path, &unlabelled).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: unlabelled").await;
        let bumped = CERT_MANAGER_MULTI.replace("\"1.18.2\"", "\"1.19.0\"");
        tokio::fs::write(&path, &bumped).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: bump").await;

        let (_, Json(resp)) = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("cert-manager".to_string()),
        )
        .await
        .unwrap();
        let raw = read_on_branch(&state, &workdir, &resp.branch, "cert-manager.yaml").await;
        let docs = keel_core::split_yaml_documents(&raw);
        let hr: Value = serde_yml::from_str(&docs[0]).unwrap();
        assert_eq!(hr.pointer("/spec/chart/spec/version").unwrap(), "1.18.2");
        assert_eq!(
            hr.pointer("/metadata/labels/keel.io~1protect").unwrap(),
            "true"
        );
        assert_eq!(
            docs[1],
            keel_core::split_yaml_documents(CERT_MANAGER_MULTI)[1]
        );

        // A historical revision with an invalid version is not restored.
        let ranged = CERT_MANAGER_MULTI.replace("\"1.18.2\"", "\">=1.0.0\"");
        tokio::fs::write(&path, &ranged).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: ranged").await;
        tokio::fs::write(&path, &bumped).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: bump again").await;
        let err = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("cert-manager".to_string()),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ServerError::BadRequest(m) if m.contains("Invalid add-on version")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rollback_without_previous_addon_revision_is_rejected() {
        let (_tmp, state, workdir) = git_state().await;
        // Only a supporting document changed: there is no earlier add-on revision.
        let path = workdir
            .join(MANAGEMENT_CORE_ADDONS_DIR)
            .join("cert-manager.yaml");
        let edited = CERT_MANAGER_MULTI.replace("commonName: keel-ca", "commonName: keel-root-ca");
        tokio::fs::write(&path, &edited).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: rename ca").await;
        let err = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("cert-manager".to_string()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ServerError::BadRequest(_)), "{err:?}");
    }

    /// The seed `flux2` HelmRelease bootstrap writes to `core-addons/flux2.yaml`.
    const SEED_FLUX2: &str = include_str!("../../../../deploy/seed/core-addons/flux2.yaml");

    #[tokio::test]
    async fn flux2_self_upgrade_and_rollback_round_trip_through_the_api() {
        let (_tmp, state, workdir) = git_state().await;
        let path = workdir.join(MANAGEMENT_CORE_ADDONS_DIR).join("flux2.yaml");
        tokio::fs::write(&path, SEED_FLUX2).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: add flux2").await;

        // A PR cannot weaken the multitenancy lockdown or the CRD retention policy.
        for values in [
            serde_json::json!({"multitenancy": {"enabled": false}}),
            serde_json::json!({"multitenancy": {"defaultServiceAccount": "helm-controller"}}),
            serde_json::json!({"crds": {"annotations": {"helm.sh/resource-policy": "delete"}}}),
        ] {
            let err = upgrade(&state, "flux2", Some("2.21.0"), values.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(err, ServerError::Core(keel_core::CoreError::Validation(_))),
                "{values}: {err:?}"
            );
        }

        // An allowlisted upgrade (version + log level) keeps every locked value and the
        // protect label; merging it then rolling back restores the seed release verbatim.
        let resp = upgrade(
            &state,
            "flux2",
            Some("2.21.0"),
            serde_json::json!({"logLevel": "debug"}),
        )
        .await
        .unwrap();
        let upgraded = read_on_branch(&state, &workdir, &resp.branch, "flux2.yaml").await;
        let doc: Value = serde_yml::from_str(&upgraded).unwrap();
        assert_eq!(doc.pointer("/spec/chart/spec/version").unwrap(), "2.21.0");
        assert_eq!(doc.pointer("/spec/values/logLevel").unwrap(), "debug");
        assert_eq!(
            doc.pointer("/spec/values/multitenancy/enabled").unwrap(),
            true
        );
        assert_eq!(
            doc.pointer("/metadata/labels/keel.io~1protect").unwrap(),
            "true"
        );
        assert_eq!(
            doc.pointer("/spec/upgrade/remediation/strategy").unwrap(),
            "rollback"
        );
        tokio::fs::write(&path, &upgraded).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: merge flux2 upgrade").await;

        let (_, Json(resp)) =
            rollback_management_addon(admin(), State(state.clone()), AxumPath("flux2".to_string()))
                .await
                .unwrap();
        let restored = read_on_branch(&state, &workdir, &resp.branch, "flux2.yaml").await;
        assert_eq!(restored, SEED_FLUX2);
    }

    /// The seed `forgejo.yaml` (`HelmRepository` + `HelmRelease`, both named `forgejo`, plus the
    /// `forgejo-ingress` HTTPRoute) as bootstrap renders it.
    fn seed_forgejo(persistent: bool) -> String {
        let seed = include_str!("../../../../deploy/seed/core-addons/forgejo.yaml")
            .replace("${FORGEJO_ROOT_URL}", "https://forgejo.keel.local:8888/")
            .replace("${FORGEJO_HOST}", "forgejo.keel.local");
        if persistent {
            let ephemeral = "    persistence:\n      enabled: false\n";
            assert!(seed.contains(ephemeral));
            seed.replace(ephemeral, "    persistence:\n      enabled: true\n")
        } else {
            seed
        }
    }

    fn yaml_chunks(raw: &str) -> Vec<&str> {
        raw.split("\n---\n").collect()
    }

    #[tokio::test]
    async fn ephemeral_forgejo_is_never_upgraded_or_rolled_back_through_the_api() {
        let (_tmp, state, workdir) = git_state().await;
        let path = workdir
            .join(MANAGEMENT_CORE_ADDONS_DIR)
            .join("forgejo.yaml");
        let seed = seed_forgejo(false);
        tokio::fs::write(&path, &seed).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: add forgejo").await;

        // Version-only and resources-only upgrades both replace the emptyDir pod.
        for (version, values) in [
            (Some("17.4.0"), serde_json::json!({})),
            (
                None,
                serde_json::json!({"resources": {"limits": {"memory": "1Gi"}}}),
            ),
            (None, serde_json::json!({"persistence": {"enabled": true}})),
        ] {
            let err = upgrade(&state, "forgejo", version, values.clone())
                .await
                .unwrap_err();
            match err {
                ServerError::Core(keel_core::CoreError::Validation(msg)) => {
                    assert!(msg.contains("forgejo dump"), "{msg}")
                }
                other => panic!("{values}: {other:?}"),
            }
        }

        // A Git-side bump does not make a rollback (another pod replacement) acceptable.
        let bumped = seed.replace("version: \"17.3.3\"", "version: \"17.4.0\"");
        assert_ne!(bumped, seed);
        tokio::fs::write(&path, &bumped).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: bump forgejo").await;
        let err = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("forgejo".to_string()),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ServerError::BadRequest(msg) if msg.contains("without persistence")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn persistent_forgejo_upgrade_and_rollback_round_trip_through_the_api() {
        let (_tmp, state, workdir) = git_state().await;
        let path = workdir
            .join(MANAGEMENT_CORE_ADDONS_DIR)
            .join("forgejo.yaml");
        let seed = seed_forgejo(true);
        tokio::fs::write(&path, &seed).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: add forgejo").await;

        // Locked values: the admin Secret, OAuth2 settings, and persistence enablement.
        for values in [
            serde_json::json!({"gitea": {"admin": {"existingSecret": "attacker"}}}),
            serde_json::json!({"gitea": {"config": {"oauth2_client": {"ACCOUNT_LINKING": "login"}}}}),
            serde_json::json!({"persistence": {"enabled": false}}),
        ] {
            let err = upgrade(&state, "forgejo", Some("17.4.0"), values.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(err, ServerError::Core(keel_core::CoreError::Validation(_))),
                "{values}: {err:?}"
            );
        }

        // A version bump edits only the HelmRelease, never the same-named HelmRepository or
        // the HTTPRoute.
        let resp = upgrade(&state, "forgejo", Some("17.4.0"), serde_json::json!({}))
            .await
            .unwrap();
        let upgraded = read_on_branch(&state, &workdir, &resp.branch, "forgejo.yaml").await;
        let (before, after) = (yaml_chunks(&seed), yaml_chunks(&upgraded));
        assert_eq!(before.len(), 3);
        assert_eq!(after.len(), 3);
        assert_eq!(
            after[0], before[0],
            "HelmRepository must be written back verbatim"
        );
        assert_eq!(
            after[2], before[2],
            "HTTPRoute must be written back verbatim"
        );
        let repo: Value = serde_yml::from_str(after[0]).unwrap();
        assert_eq!(repo["kind"], "HelmRepository");
        let hr: Value = serde_yml::from_str(after[1]).unwrap();
        assert_eq!(hr["kind"], "HelmRelease");
        assert_eq!(hr.pointer("/spec/chart/spec/version").unwrap(), "17.4.0");
        assert_eq!(
            upgraded,
            seed.replace("version: \"17.3.3\"", "version: \"17.4.0\"")
        );

        // Merge, then roll back: the seed comes back verbatim, protect label included.
        tokio::fs::write(&path, &upgraded).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: merge forgejo upgrade").await;
        let (_, Json(resp)) = rollback_management_addon(
            admin(),
            State(state.clone()),
            AxumPath("forgejo".to_string()),
        )
        .await
        .unwrap();
        let restored = read_on_branch(&state, &workdir, &resp.branch, "forgejo.yaml").await;
        assert_eq!(restored, seed);
        let hr: Value = serde_yml::from_str(yaml_chunks(&restored)[1]).unwrap();
        assert_eq!(
            hr.pointer("/metadata/labels/keel.io~1protect").unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn rollback_cannot_restore_changed_locked_values() {
        let (_tmp, state, workdir) = git_state().await;
        let path = workdir.join(MANAGEMENT_CORE_ADDONS_DIR).join("flux2.yaml");
        let insecure = "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: flux2\n  namespace: flux-system\nspec:\n  chart:\n    spec:\n      chart: flux2\n      version: \"2.19.0\"\n  values:\n    multitenancy:\n      enabled: false\n";
        tokio::fs::write(&path, insecure).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: add flux2").await;
        let secure = insecure
            .replace("enabled: false", "enabled: true")
            .replace("2.19.0", "2.20.0");
        tokio::fs::write(&path, &secure).await.unwrap();
        commit_and_push(&state.git_runner, &workdir, "chore: lock flux2").await;

        let err =
            rollback_management_addon(admin(), State(state.clone()), AxumPath("flux2".to_string()))
                .await
                .unwrap_err();
        assert!(
            matches!(&err, ServerError::BadRequest(m) if m.contains("/multitenancy")),
            "{err:?}"
        );
        // And the API cannot flip it either.
        let err = upgrade(
            &state,
            "flux2",
            Some("2.21.0"),
            serde_json::json!({"multitenancy": {"enabled": false}}),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ServerError::Core(keel_core::CoreError::Validation(_))
        ));
    }
}
