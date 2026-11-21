//! PR proposal synthesis, ephemeral PKO preview reports, and merge/reject/reopen endpoints (`SDD.md` §7.3).

use std::path::Path;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use keel_core::{
    AppRuntime, BundleItemKind, CatalogApplication, CatalogBundle, CatalogManifest,
    KEEL_PREVIEWS_NAMESPACE, ManifestEngine, SopsManager, partition_values_by_schema,
    render_helm_values_yaml, render_manifest_template, tenant_namespace, validate_dns_label,
    validate_k8s_manifests_yaml, validate_schema_values, verify_rendered_manifest_structure,
};
use keel_git::{
    PullRequestComment, PullRequestInfo, PullRequestState, parse_proposal_branch_tenant,
    validate_commit_sha,
};
use keel_kube::{
    FlaggerCanarySpecInput, FluxDependencyRef, FluxHealthCheckRef, FluxKustomizationSpecInput,
    FluxSubstituteReference, GITHUB_TOKEN_SECRET_NAME, HelmChartSourceKind, HelmChartSourceRef,
    HelmReleaseSpecInput, HelmValuesSecretRef, RemoteKubeConfigRef, TenantAgeKey,
    generate_flux_kustomization_manifest, synthesize_canary_gateway_network_policy_yaml,
    synthesize_flagger_canary_yaml, synthesize_flux_kustomization_yaml,
    synthesize_helm_release_yaml, synthesize_remote_target_namespace_yaml,
};
use keel_pko::{
    ChangeOp, EphemeralPreviewParams, GcPlan, PkoStack, PreviewReport, PreviewResourceCandidate,
    PreviewRunStatus, PreviewSecretCopy, PreviewSource, PreviewSummary, ResourceDiff,
    TenantStackParams, attach_github_token_env_ref, attach_s3_credentials_env_refs,
    build_preview_branch, format_markdown_report, synthesize_ephemeral_preview,
    synthesize_k8s_manifest_pulumi_yaml, synthesize_tenant_stack,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::preview_lifecycle::launch_pko_preview;
use crate::state::{AppState, CachedPreview};

use super::{
    attach_infra_secret_refs, create_dir_all_contained, load_catalog_manifest,
    load_provider_registry, next_unique_branch_timestamp, open_pr_for_session,
    read_contained_optional, read_contained_to_string, resolve_sops_recipients,
    resolve_tenant_resource_dir, safe_relative_path, serialize_tenants_kustomization,
    stage_commit_push_and_return_to_main, write_contained,
};

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
    auth: AuthSession,
    State(state): State<AppState>,
    Query(query): Query<ListProposalsQuery>,
) -> Result<Json<ProposalsListResponse>, ServerError> {
    if let Some(ref t) = query.tenant
        && !auth.is_admin()
    {
        auth.require_tenant_access(t)?;
    }

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
    let mut proposals = state
        .forge
        .list_pull_requests(query.tenant.as_deref(), pr_state)
        .await?;
    if query.tenant.is_none() && !auth.is_admin() {
        proposals.retain(|p| {
            p.tenant
                .as_deref()
                .or_else(|| parse_proposal_branch_tenant(&p.source_branch))
                .is_some_and(|t| auth.require_tenant_access(t).is_ok())
        });
    }
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

/// Resolves the Pulumi project name a tenant Stack must use: the `name` of the Pulumi.yaml that
/// is actually committed next to the Stack. That is the blueprint's `Pulumi.yaml`, or, when the
/// blueprint ships none, the generated one named after the instance (`fallback`). The catalog
/// display name is never used. The result is sanitized to Pulumi's project-name charset.
///
/// The read is contained to `repo_workdir`; an escaping symlink is treated as absent.
async fn read_pulumi_project_name(
    repo_workdir: &Path,
    pulumi_yaml_path: &Path,
    fallback: &str,
) -> String {
    let raw = read_contained_to_string(repo_workdir, pulumi_yaml_path)
        .await
        .ok();
    pulumi_project_name_from_yaml(raw.as_deref(), fallback)
}

fn pulumi_project_name_from_yaml(raw: Option<&str>, fallback: &str) -> String {
    let declared = raw
        .and_then(|r| serde_yml::from_str::<serde_json::Value>(r).ok())
        .and_then(|doc| {
            doc.get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    sanitize_pulumi_project_name(declared.as_deref().unwrap_or(fallback))
}

/// Maps a name to Pulumi's project-name charset (`[A-Za-z0-9_.-]`), collapsing other runs to `-`.
fn sanitize_pulumi_project_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
            out.push(c);
            last_dash = c == '-';
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Returns the tenant-scoped Pulumi DIY backend (`s3://<bucket>/tenants/<id>?...`) so tenant
/// Stack state lands under the prefix the tenant's scoped SeaweedFS identity can access.
fn tenant_stack_backend_url(state: &AppState, tenant: &str) -> Result<String, ServerError> {
    keel_kube::tenant::tenant_s3_backend_url(&state.backend_url, tenant).map_err(|e| {
        ServerError::Internal(format!(
            "cannot derive tenant state backend for `{tenant}`: {e}"
        ))
    })
}

/// Ensures `tenant_ns` holds the tenant's state credentials before a Pulumi Stack is committed:
///
/// * a mirror of the tenant's own scoped S3 identity, attached to the Stack's `envRefs`;
/// * the tenant state encryption passphrase (`keel-pulumi-passphrase`) the Stack workspace
///   sources `PULUMI_CONFIG_PASSPHRASE` from.
///
/// Fails closed with `409 Conflict` when either is missing (after one lazy provisioning attempt),
/// rather than committing a Stack that can never reach or decrypt its state (and never falling
/// back to the control-plane admin keys or an empty passphrase).
async fn attach_tenant_state_credentials(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
) -> Result<(), ServerError> {
    attach_tenant_s3_credentials(state, tenant, tenant_ns, env_refs).await?;
    require_tenant_pulumi_passphrase(state, tenant, tenant_ns).await
}

/// Fails closed (`409`) unless `tenant_ns` holds a usable Pulumi state passphrase. A missing
/// passphrase is provisioned lazily only through the tenant provisioner (which creates it
/// alongside the scoped S3 identity); the server never invents one on the proposal path.
async fn require_tenant_pulumi_passphrase(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
) -> Result<(), ServerError> {
    if !state.require_pulumi_passphrase {
        return Ok(());
    }
    let present = |r: Result<bool, keel_kube::KubeError>| match r {
        Ok(p) => Ok(p),
        Err(e) => {
            tracing::error!(tenant, error = %e, "failed to read tenant Pulumi state passphrase");
            Err(ServerError::Conflict(format!(
                "tenant `{tenant}` Pulumi state passphrase is unreadable; re-run tenant provisioning"
            )))
        }
    };
    if present(state.secret_store.has_pulumi_passphrase(tenant_ns).await)? {
        return Ok(());
    }
    if provision_missing_tenant_identity(state, tenant).await?
        && present(state.secret_store.has_pulumi_passphrase(tenant_ns).await)?
    {
        return Ok(());
    }
    Err(ServerError::Conflict(format!(
        "tenant `{tenant}` has no Pulumi state passphrase (`{tenant_ns}/{}`, key `{}`); \
         run tenant provisioning before creating Pulumi stacks",
        keel_pko::PULUMI_PASSPHRASE_SECRET_NAME,
        keel_pko::PULUMI_PASSPHRASE_SECRET_KEY
    )))
}

/// Ensures `tenant_ns` holds a mirror of the tenant's own scoped S3 identity and attaches it to
/// the tenant Stack's `envRefs` (`409` when the tenant has no scoped state credentials).
async fn attach_tenant_s3_credentials(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
    env_refs: &mut std::collections::BTreeMap<String, keel_pko::PkoEnvRef>,
) -> Result<(), ServerError> {
    let Some(ref s3_secret) = state.s3_credentials_secret else {
        return Ok(());
    };
    match state
        .secret_store
        .ensure_s3_credentials_in_namespace(tenant_ns)
        .await
    {
        Ok(true) => {
            attach_s3_credentials_env_refs(env_refs, s3_secret);
            Ok(())
        }
        Ok(false) => {
            if provision_missing_tenant_identity(state, tenant).await?
                && let Ok(true) = state
                    .secret_store
                    .ensure_s3_credentials_in_namespace(tenant_ns)
                    .await
            {
                attach_s3_credentials_env_refs(env_refs, s3_secret);
                return Ok(());
            }
            Err(ServerError::Conflict(format!(
                "tenant `{tenant}` has no scoped state credentials \
                 (`{tenant_ns}/{}`); run tenant provisioning before creating Pulumi stacks",
                keel_kube::tenant::TENANT_S3_CREDENTIALS_SECRET_NAME
            )))
        }
        Err(e) => {
            tracing::error!(tenant, error = %e, "failed to mirror tenant scoped state credentials");
            Err(ServerError::Conflict(format!(
                "tenant `{tenant}` scoped state credentials are unusable; re-run tenant provisioning"
            )))
        }
    }
}

/// Lazily provisions a tenant's scoped S3 identity when a provisioner is configured (e.g. the
/// tenant was onboarded through the API before its namespace existed). Returns `Ok(true)` when an
/// identity is now in place.
async fn provision_missing_tenant_identity(
    state: &AppState,
    tenant: &str,
) -> Result<bool, ServerError> {
    let Some(provisioner) = state.tenant_s3_provisioner.as_ref() else {
        return Ok(false);
    };
    match provisioner.provision_tenant_s3_identity(tenant).await {
        Ok(keel_kube::TenantS3ProvisionOutcome::NamespaceMissing) => Ok(false),
        Ok(outcome) => {
            tracing::info!(
                tenant,
                ?outcome,
                "provisioned tenant scoped S3 state identity"
            );
            Ok(true)
        }
        Err(e) => {
            tracing::error!(tenant, error = %e, "tenant scoped S3 identity provisioning failed");
            Err(ServerError::ServiceUnavailable(format!(
                "could not provision scoped state credentials for tenant `{tenant}`; retry later"
            )))
        }
    }
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

/// Renders schema secret values into `stringData` form (nulls dropped, scalars stringified).
fn render_secret_string_data(secret_values: &Value) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if let Some(obj) = secret_values.as_object() {
        for (key, val) in obj {
            let rendered = match val {
                Value::Null => continue,
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Array(_) | Value::Object(_) => val.to_string(),
            };
            out.insert(key.clone(), rendered);
        }
    }
    out
}

/// Secrets the preview runner already mirrors with tenant-scoped values (state backend, forge
/// token); tenant Stack refs to them are never copied into a preview.
fn preview_runner_managed_secrets(state: &AppState) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::from([
        keel_kube::S3_CREDENTIALS_SECRET_NAME.to_string(),
        keel_kube::GITHUB_TOKEN_SECRET_NAME.to_string(),
        keel_kube::tenant::TENANT_S3_CREDENTIALS_SECRET_NAME.to_string(),
    ]);
    if let Some(name) = &state.s3_credentials_secret {
        out.insert(name.clone());
    }
    out
}

/// Plans provider-credential Secret copies for a tenant preview and rewrites its refs. The PR's
/// own `<instance>-secrets` values (not yet decrypted into the tenant namespace by Flux for a
/// brand-new stack) are attached as `proposed_data` so the preview evaluates the PR.
fn plan_preview_credentials(
    state: &AppState,
    params: &mut EphemeralPreviewParams,
    tenant_stack: &PkoStack,
    instance_secret_name: &str,
    proposed_secret_values: &Value,
) -> Result<Vec<PreviewSecretCopy>, ServerError> {
    let mut copies =
        params.inherit_tenant_credentials(tenant_stack, &preview_runner_managed_secrets(state))?;
    let proposed = render_secret_string_data(proposed_secret_values);
    for copy in &mut copies {
        if copy.source_name == instance_secret_name && !proposed.is_empty() {
            copy.proposed_data = Some(
                proposed
                    .iter()
                    .filter(|(k, _)| copy.keys.contains(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            );
        }
    }
    Ok(copies)
}

fn build_infra_secret_manifest_yaml(
    secret_name: &str,
    namespace: &str,
    secret_values: &Value,
) -> Result<(String, Vec<String>), ServerError> {
    let mut string_data = serde_json::Map::new();
    let mut keys = Vec::new();
    for (key, rendered) in render_secret_string_data(secret_values) {
        keys.push(key.clone());
        string_data.insert(key, Value::String(rendered));
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
    if let Ok(content) = read_contained_to_string(repo_workdir, &default_path).await {
        return content;
    }
    if let Some(rel_path) = app.spec.upstream.path.as_deref()
        && let Some(resolved) = safe_relative_path(&app_dir, rel_path)
        && let Ok(content) = read_contained_to_string(repo_workdir, &resolved).await
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
    if req.tenant == crate::state::MANAGEMENT_TENANT {
        auth.require_admin()?;
        return Err(ServerError::Conflict(
            "Management-plane add-ons are not tenant applications; change them via \
             POST /api/v1/management/addons/{id}/upgrade (or /rollback), which opens a reviewed \
             GitOps PR"
                .to_string(),
        ));
    }
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
                        .map_err(|err| {
                            tracing::warn!(tenant = %req.tenant, error = %err, "Could not generate tenant SOPS age key");
                            ServerError::ServiceUnavailable(format!(
                                "Tenant '{}' has no SOPS age key configured and one could not be generated ({err}); install `age-keygen` and `sops` on the keel-server host",
                                req.tenant
                            ))
                        })?;
                let generated = TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
                let _ = state
                    .secret_store
                    .store_tenant_age_key(&req.tenant, &generated)
                    .await;
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
            let mut rendered =
                render_manifest_template(&raw_manifests, &partitioned.normal_values)?;
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            // Fail closed before anything is committed: rendered values must not add documents
            // or change resource kinds relative to the catalog-authored template.
            verify_rendered_manifest_structure(&raw_manifests, &rendered)?;
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
                        tenant_stack_backend_url(&state, &req.tenant)?,
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
                    attach_tenant_state_credentials(
                        &state,
                        &req.tenant,
                        &tenant_ns,
                        &mut stack_params.env_refs,
                    )
                    .await?;
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
                    && let Ok(raw) = read_contained_to_string(&state.repo_workdir, &hr_path).await
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
            let pulumi_project = read_pulumi_project_name(
                &state.repo_workdir,
                &blueprint_pulumi_path,
                &instance_name,
            )
            .await;
            let mut stack_params = TenantStackParams::new(
                &req.tenant,
                &instance_name,
                format!(
                    "organization/{pulumi_project}/{}-{instance_name}",
                    req.tenant
                ),
                tenant_stack_backend_url(&state, &req.tenant)?,
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
            attach_tenant_state_credentials(
                &state,
                &req.tenant,
                &tenant_ns,
                &mut stack_params.env_refs,
            )
            .await?;
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
                && let Ok(raw) = read_contained_to_string(
                    &state.repo_workdir,
                    &state.repo_workdir.join(&rel_dir).join("stack.yaml"),
                )
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
            let mut stack = synthesize_tenant_stack(&stack_params)?;
            stack
                .metadata
                .labels
                .insert("keel.io/catalog-id".to_string(), bp.metadata.id.clone());
            stack.metadata.labels.insert(
                "keel.io/resource-kind".to_string(),
                bp.effective_resource_kind().as_str().to_string(),
            );
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
        create_dir_all_contained(&state.repo_workdir, &target_dir).await?;

        let mut staged_paths = Vec::new();
        let values_path = target_dir.join("values.yaml");
        write_contained(&state.repo_workdir, &values_path, &values_yaml).await?;
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
        write_contained(&state.repo_workdir, &manifest_path, &manifest_yaml).await?;
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
                write_contained(
                    &state.repo_workdir,
                    &target_dir.join("manifests.yaml"),
                    rendered_manifests,
                )
                .await?;
                staged_paths.push(format!("{rel_dir}/manifests.yaml"));
            }
            match eng {
                ManifestEngine::Flux => {
                    let mut app_k_resources = vec![
                        "manifests.yaml".to_string(),
                        "values-configmap.yaml".to_string(),
                    ];
                    if let Some(ref cm_yaml) = values_configmap_yaml {
                        write_contained(
                            &state.repo_workdir,
                            &target_dir.join("values-configmap.yaml"),
                            cm_yaml,
                        )
                        .await?;
                        staged_paths.push(format!("{rel_dir}/values-configmap.yaml"));
                    }
                    if let Some(ref enc_k8s_secret) = encrypted_infra_k8s_secret_yaml {
                        write_contained(
                            &state.repo_workdir,
                            &target_dir.join("secrets.enc.yaml"),
                            enc_k8s_secret,
                        )
                        .await?;
                        staged_paths.push(format!("{rel_dir}/secrets.enc.yaml"));
                        app_k_resources.push("secrets.enc.yaml".to_string());
                    }
                    let app_k_doc = serde_json::json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": app_k_resources,
                    });
                    write_contained(
                        &state.repo_workdir,
                        &target_dir.join("kustomization.yaml"),
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
                        write_contained(
                            &state.repo_workdir,
                            &target_dir.join("Pulumi.yaml"),
                            pulumi_yaml,
                        )
                        .await?;
                        staged_paths.push(format!("{rel_dir}/Pulumi.yaml"));
                    }
                    let mut app_k_resources = vec!["stack.yaml".to_string()];
                    if let Some(ref enc_k8s_secret) = encrypted_infra_k8s_secret_yaml {
                        write_contained(
                            &state.repo_workdir,
                            &target_dir.join("secret.enc.yaml"),
                            enc_k8s_secret,
                        )
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
                    write_contained(
                        &state.repo_workdir,
                        &target_dir.join("kustomization.yaml"),
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
                write_contained(&state.repo_workdir, &helm_repo_path, helm_repo_yaml).await?;
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
                    let remote_target_ns = remote_app_target_ns
                        .clone()
                        .unwrap_or_else(|| tenant_ns.clone());
                    let canary_dir = target_dir.join("canary");
                    create_dir_all_contained(&state.repo_workdir, &canary_dir).await?;
                    // On the workload cluster the target namespace is otherwise only created by
                    // the sibling HelmRelease (`install.createNamespace`); ship it here so the
                    // canary objects never race Helm (kustomize-controller applies Namespaces
                    // first). `prune: disabled` + `ssa: IfNotPresent` keep it from deleting or
                    // fighting the namespace Helm may also create.
                    write_contained(
                        &state.repo_workdir,
                        &canary_dir.join("namespace.yaml"),
                        synthesize_remote_target_namespace_yaml(&req.tenant, &remote_target_ns)?,
                    )
                    .await?;
                    write_contained(
                        &state.repo_workdir,
                        &canary_dir.join("canary.yaml"),
                        canary_yaml,
                    )
                    .await?;
                    write_contained(
                        &state.repo_workdir,
                        &canary_dir.join("networkpolicy-canary.yaml"),
                        np_yaml,
                    )
                    .await?;
                    let canary_k_doc = serde_json::json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": ["namespace.yaml", "canary.yaml", "networkpolicy-canary.yaml"],
                    });
                    let canary_k_yaml = serde_yml::to_string(&canary_k_doc).map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to serialize canary/kustomization.yaml: {e}"
                        ))
                    })?;
                    write_contained(
                        &state.repo_workdir,
                        &canary_dir.join("kustomization.yaml"),
                        canary_k_yaml,
                    )
                    .await?;
                    staged_paths.push(format!("{rel_dir}/canary/namespace.yaml"));
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
                    fk_input.target_namespace = Some(remote_target_ns);
                    let fk_yaml = synthesize_flux_kustomization_yaml(&fk_input)?;
                    write_contained(
                        &state.repo_workdir,
                        &target_dir.join("canary-kustomization.yaml"),
                        fk_yaml,
                    )
                    .await?;
                    staged_paths.push(format!("{rel_dir}/canary-kustomization.yaml"));
                    bundle_item_kustomize_resources.push("canary-kustomization.yaml".to_string());
                    extra_kustomize_resources.push(format!(
                        "{}/apps/{instance_name}/canary-kustomization.yaml",
                        req.tenant
                    ));
                } else {
                    let canary_path = target_dir.join("canary.yaml");
                    write_contained(&state.repo_workdir, &canary_path, canary_yaml).await?;
                    staged_paths.push(format!("{rel_dir}/canary.yaml"));
                    bundle_item_kustomize_resources.push("canary.yaml".to_string());
                    extra_kustomize_resources
                        .push(format!("{}/apps/{instance_name}/canary.yaml", req.tenant));

                    let np_path = target_dir.join("networkpolicy-canary.yaml");
                    write_contained(&state.repo_workdir, &np_path, np_yaml).await?;
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
            let pulumi_yaml = if let Some(content) =
                read_contained_optional(&state.repo_workdir, &blueprint_pulumi_path).await?
            {
                content
            } else {
                format!(
                    "name: {instance_name}\nruntime: yaml\ndescription: Keel provisioned {catalog_id} stack for tenant {tenant}\noutputs:\n  status: provisioned\n",
                    catalog_id = req.catalog_id,
                    tenant = req.tenant
                )
            };
            let target_pulumi_path = target_dir.join("Pulumi.yaml");
            write_contained(&state.repo_workdir, &target_pulumi_path, pulumi_yaml).await?;
            staged_paths.push(format!("{rel_dir}/Pulumi.yaml"));

            if let Some(enc_k8s_secret) = &encrypted_infra_k8s_secret_yaml {
                let secret_path = target_dir.join("secret.enc.yaml");
                write_contained(&state.repo_workdir, &secret_path, enc_k8s_secret).await?;
                staged_paths.push(format!("{rel_dir}/secret.enc.yaml"));
                bundle_item_kustomize_resources.push("secret.enc.yaml".to_string());
                extra_kustomize_resources.push(format!(
                    "{}/infra/{instance_name}/secret.enc.yaml",
                    req.tenant
                ));
            }
        }

        let tenants_kustomization_path = state.repo_workdir.join("tenants/kustomization.yaml");
        let existing_kustomization = if tokio::fs::try_exists(&tenants_kustomization_path)
            .await
            .unwrap_or(false)
        {
            read_contained_to_string(&state.repo_workdir, &tenants_kustomization_path)
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
            write_contained(
                &state.repo_workdir,
                &target_dir.join("kustomization.yaml"),
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
        create_dir_all_contained(&state.repo_workdir, &state.repo_workdir.join("tenants")).await?;
        write_contained(
            &state.repo_workdir,
            &tenants_kustomization_path,
            serialize_tenants_kustomization(&resources)?,
        )
        .await?;
        staged_paths.push("tenants/kustomization.yaml".to_string());

        if manifest_engine.is_none()
            && let Some(enc_yaml) = encrypted_secrets_yaml
        {
            let enc_path = target_dir.join("secrets.enc.yaml");
            write_contained(&state.repo_workdir, &enc_path, enc_yaml).await?;
            staged_paths.push(format!("{rel_dir}/secrets.enc.yaml"));
        }

        let commit_msg = format!(
            "feat({}): propose {} {}",
            req.tenant,
            if is_app { "app" } else { "infra" },
            instance_name
        );
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            &staged_paths,
        )
        .await?;
        (sha, is_update)
    };

    let verb = match (is_update, is_app) {
        (true, _) => "update",
        (false, true) => "deploy",
        (false, false) => "provision",
    };
    let pr_title = proposal_pr_title(req.title.as_deref(), &req.tenant, verb, &instance_name);
    let summary = format!(
        "Automated proposal for tenant `{}` (`{}`)\n\n- Action: {}\n- Commit: `{}`\n- Path: `{}`",
        req.tenant,
        req.catalog_id,
        if is_update {
            "Update existing instance"
        } else {
            "Create new instance"
        },
        commit_sha,
        rel_dir
    );
    let pr_body = proposal_pr_body(
        req.title.as_deref(),
        &pr_title,
        req.body.as_deref(),
        &summary,
    );

    let pr = open_pr_for_session(&state, &auth, &req.tenant, &branch, pr_title, pr_body).await?;
    audit_proposal_event("create", &auth, Some(&req.tenant), &pr);

    // Ephemeral PKO v2 preview in `keel-previews` (`SDD.md` §4.2): only Pulumi-backed changes
    // (infra blueprints, Pulumi-engine manifest apps) have a program PKO can preview.
    let pko_backed = !is_app || manifest_engine == Some(ManifestEngine::Pulumi);
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
    preview_params.head_sha = Some(commit_sha.clone());
    let mut preview_blocker: Option<String> = None;
    let mut preview_secret_copies: Vec<PreviewSecretCopy> = Vec::new();
    if pko_backed {
        populate_stack_config_from_values(&mut preview_params.config, &partitioned.normal_values);
        if let CatalogManifest::InfraBlueprint(bp) = &manifest
            && bp.spec.outputs.iter().any(|o| o == "kubeconfig")
        {
            preview_params.config.insert(
                "kubeconfigSecretName".to_string(),
                format!("{instance_name}-kubeconfig"),
            );
            preview_params
                .config
                .insert("kubeconfigSecretNamespace".to_string(), tenant_ns.clone());
        }
        if let Ok(tenant_stack) = PkoStack::from_yaml(&manifest_yaml) {
            preview_params.inherit_tenant_stack(&tenant_stack);
            preview_secret_copies = plan_preview_credentials(
                &state,
                &mut preview_params,
                &tenant_stack,
                &infra_secret_name,
                &partitioned.secret_values,
            )?;
        }
        // The preview workspace always decrypts with the tenant's state passphrase (copied into
        // `keel-previews` per preview); never an empty one.
        let passphrase_copy = preview_params.passphrase_secret_copy()?;
        if !preview_secret_copies
            .iter()
            .any(|c| c.target_name == passphrase_copy.target_name)
        {
            preview_secret_copies.push(passphrase_copy);
        }
        preview_blocker =
            attach_preview_cluster_inputs(&state, &req.tenant, &mut preview_params).await;
    }
    let preview_stack = synthesize_ephemeral_preview(&preview_params)?;
    let run = match (pko_backed, preview_blocker) {
        (false, _) => PreviewRunStatus::not_executed(
            "Flux-managed change: no Pulumi program to preview; report synthesized from the proposed manifests",
        ),
        (true, Some(reason)) => PreviewRunStatus::not_executed(reason),
        (true, None) => launch_pko_preview(&state, &preview_stack, &preview_secret_copies).await,
    };

    let report = synthesize_proposal_report(
        &pr.id,
        &req.tenant,
        preview_resource_type,
        &instance_name,
        rendered_manifest_template_yaml.as_deref(),
        run,
    );

    let markdown = simulated_preview_markdown(&report, &pr.id);
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

/// Resolves cluster-side preview inputs for `tenant` (OPEN-9): the tenant's **scoped** S3 state
/// identity mirrored into `keel-previews` (never the control-plane admin keys) and the tenant's
/// cluster-reachable Flux Git source + read-only credentials. Returns a human-readable reason
/// when the PKO preview cannot run (rendered as `notExecuted`).
async fn attach_preview_cluster_inputs(
    state: &AppState,
    tenant: &str,
    params: &mut EphemeralPreviewParams,
) -> Option<String> {
    let Some(cluster) = state.preview_cluster.clone() else {
        return Some("No live cluster connected: PKO preview not executed".to_string());
    };
    let prereqs = match cluster.ensure_preview_prerequisites(tenant).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(tenant, error = %e, "preview prerequisites unavailable");
            return Some(format!(
                "Preview prerequisites unavailable for tenant `{tenant}`: PKO preview not executed"
            ));
        }
    };
    if let Some(url) = prereqs.git_url {
        params.repo_url = url;
        params.git_secret_name = prereqs.git_secret;
    }
    if params.backend_url.starts_with("s3://") {
        match prereqs.s3_secret {
            Some(secret) => attach_s3_credentials_env_refs(&mut params.env_refs, &secret),
            None if state.s3_credentials_secret.is_some() => {
                return Some(format!(
                    "Tenant `{tenant}` has no scoped state credentials: PKO preview not executed"
                ));
            }
            None => {}
        }
    }
    if matches!(
        state
            .secret_store
            .ensure_github_token_in_namespace(KEEL_PREVIEWS_NAMESPACE)
            .await,
        Ok(true)
    ) {
        attach_github_token_env_ref(&mut params.env_refs, GITHUB_TOKEN_SECRET_NAME);
    }
    params.docker_host = state.docker_proxy_url.clone();
    None
}

/// Builds the creation-time report: a control-plane synthesis (`source: synthesized`) listing
/// the proposed top-level object plus every rendered manifest document (UI-14), annotated with
/// the PKO run status. Real PKO results replace it once the run finishes.
fn synthesize_proposal_report(
    pr_id: &str,
    tenant: &str,
    resource_type: &str,
    instance_name: &str,
    rendered_manifests: Option<&str>,
    run: PreviewRunStatus,
) -> PreviewReport {
    let mut resources = vec![ResourceDiff {
        urn: format!("urn:pulumi:pr-{pr_id}::{tenant}::{resource_type}::{instance_name}"),
        resource_type: resource_type.to_string(),
        name: instance_name.to_string(),
        op: ChangeOp::Create,
        changed_fields: vec!["spec".to_string()],
    }];
    if let Some(rendered) = rendered_manifests
        && let Ok(docs) = validate_k8s_manifests_yaml(rendered)
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
                .unwrap_or(instance_name);
            let k8s_type = format!("kubernetes:{api_v}:{kind}");
            resources.push(ResourceDiff {
                urn: format!("urn:pulumi:pr-{pr_id}::{tenant}::{k8s_type}::{name}"),
                resource_type: k8s_type,
                name: name.to_string(),
                op: ChangeOp::Create,
                changed_fields: vec!["metadata".to_string(), "spec".to_string()],
            });
        }
    }
    PreviewReport {
        summary: PreviewSummary {
            create: resources.len(),
            ..PreviewSummary::default()
        },
        resources,
        source: PreviewSource::Synthesized,
        run: Some(run),
    }
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
                        .map_err(|err| {
                            tracing::warn!(tenant = %req.tenant, error = %err, "Could not generate tenant SOPS age key");
                            ServerError::ServiceUnavailable(format!(
                                "Tenant '{}' has no SOPS age key configured and one could not be generated ({err}); install `age-keygen` and `sops` on the keel-server host",
                                req.tenant
                            ))
                        })?;
                    let generated =
                        TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
                    let _ = state
                        .secret_store
                        .store_tenant_age_key(&req.tenant, &generated)
                        .await;
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
                let pulumi_project = read_pulumi_project_name(
                    &state.repo_workdir,
                    &blueprint_pulumi_path,
                    &item_resource_name,
                )
                .await;
                let mut stack_params = TenantStackParams::new(
                    &req.tenant,
                    &item_resource_name,
                    format!(
                        "organization/{pulumi_project}/{}-{item_resource_name}",
                        req.tenant
                    ),
                    tenant_stack_backend_url(&state, &req.tenant)?,
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
                attach_tenant_state_credentials(
                    &state,
                    &req.tenant,
                    &tenant_ns,
                    &mut stack_params.env_refs,
                )
                .await?;
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
                let mut stack = synthesize_tenant_stack(&stack_params)?;
                stack
                    .metadata
                    .labels
                    .insert("keel.io/catalog-id".to_string(), bp.metadata.id.clone());
                stack.metadata.labels.insert(
                    "keel.io/resource-kind".to_string(),
                    bp.effective_resource_kind().as_str().to_string(),
                );
                let stack_yaml = stack.to_yaml()?;

                let blueprint_pulumi_path = state
                    .repo_workdir
                    .join("catalog/infra")
                    .join(&item.catalog_ref)
                    .join("Pulumi.yaml");
                let pulumi_yaml = if let Some(content) =
                    read_contained_optional(&state.repo_workdir, &blueprint_pulumi_path).await?
                {
                    content
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
        create_dir_all_contained(&state.repo_workdir, &target_dir).await?;

        let mut staged_paths = Vec::new();
        let mut kustomize_additions = Vec::new();

        for item in &prepared_items {
            let item_dir = state.repo_workdir.join(&item.item_rel_dir);
            create_dir_all_contained(&state.repo_workdir, &item_dir).await?;

            write_contained(
                &state.repo_workdir,
                &item_dir.join("values.yaml"),
                &item.values_yaml,
            )
            .await?;
            staged_paths.push(format!("{}/values.yaml", item.item_rel_dir));

            write_contained(
                &state.repo_workdir,
                &item_dir.join(&item.primary_filename),
                &item.primary_yaml,
            )
            .await?;
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
                write_contained(&state.repo_workdir, &item_dir.join(fname), content).await?;
                staged_paths.push(format!("{}/{fname}", item.item_rel_dir));
            }

            if let Some(enc_yaml) = &item.encrypted_secrets_yaml {
                write_contained(
                    &state.repo_workdir,
                    &item_dir.join("secrets.enc.yaml"),
                    enc_yaml,
                )
                .await?;
                staged_paths.push(format!("{}/secrets.enc.yaml", item.item_rel_dir));
            }

            write_contained(
                &state.repo_workdir,
                &item_dir.join("flux-kustomization.yaml"),
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
            read_contained_to_string(&state.repo_workdir, &tenants_kustomization_path)
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
        create_dir_all_contained(&state.repo_workdir, &state.repo_workdir.join("tenants")).await?;
        write_contained(
            &state.repo_workdir,
            &tenants_kustomization_path,
            serialize_tenants_kustomization(&resources)?,
        )
        .await?;
        staged_paths.push("tenants/kustomization.yaml".to_string());

        let commit_msg = format!("feat({}): propose bundle {}", req.tenant, instance_name);
        let sha = stage_commit_push_and_return_to_main(
            &state,
            &auth,
            &branch,
            &commit_msg,
            &staged_paths,
        )
        .await?;
        (sha, is_update)
    };

    let pr_title = proposal_pr_title(
        req.title.as_deref(),
        &req.tenant,
        if is_update {
            "update bundle"
        } else {
            "deploy bundle"
        },
        &instance_name,
    );
    let summary = format!(
        "Automated bundle proposal for tenant `{}` (`{}`)\n\n- Action: {}\n- Items: {}\n- Commit: `{}`\n- Path: `{}`",
        req.tenant,
        req.catalog_id,
        if is_update {
            "Update existing bundle"
        } else {
            "Create new bundle"
        },
        prepared_items
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>()
            .join(" -> "),
        commit_sha,
        bundle_rel_dir
    );
    let pr_body = proposal_pr_body(
        req.title.as_deref(),
        &pr_title,
        req.body.as_deref(),
        &summary,
    );

    let pr = open_pr_for_session(&state, &auth, &req.tenant, &branch, pr_title, pr_body).await?;
    audit_proposal_event("create", &auth, Some(&req.tenant), &pr);

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
    // Bundles span several Stacks/HelmReleases with `dependsOn` ordering; a single PKO preview
    // cannot evaluate them, so the bundle preview Stack is synthesized but never applied (and
    // never carries control-plane admin S3 keys, OPEN-9).
    preview_params.head_sha = Some(commit_sha.clone());
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
        source: PreviewSource::Synthesized,
        run: Some(PreviewRunStatus::not_executed(
            "Bundle proposal: per-item PKO previews are not executed; report synthesized from the bundle plan",
        )),
    };

    let markdown = simulated_preview_markdown(&report, &pr.id);
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

pub(crate) fn synthesize_report_from_git_diff(pr_id: &str, git_diff: &str) -> PreviewReport {
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

    PreviewReport {
        resources,
        summary,
        source: PreviewSource::Synthesized,
        run: None,
    }
}

/// Banner prepended to PR comments for control-plane-synthesized previews.
const SIMULATED_PREVIEW_NOTICE: &str = "> **Simulated diff — PKO preview not executed.** This report was synthesized by the Keel control plane from the proposed manifests; it is not the output of a `pulumi preview` run and does not include policy evaluation.\n\n";

/// Renders the PR comment markdown, flagging synthesized reports as simulated.
fn simulated_preview_markdown(report: &PreviewReport, pr_id: &str) -> String {
    let body = format_markdown_report(report, pr_id);
    if report.source != PreviewSource::Synthesized {
        return body;
    }
    let pko_in_flight = report.run.as_ref().is_some_and(|r| {
        matches!(
            r.phase,
            keel_pko::PreviewPhase::Pending | keel_pko::PreviewPhase::Running
        )
    });
    if pko_in_flight {
        format!("{PROVISIONAL_PREVIEW_NOTICE}{body}")
    } else {
        format!("{SIMULATED_PREVIEW_NOTICE}{body}")
    }
}

/// Banner for the creation-time comment while the real PKO preview is still running.
const PROVISIONAL_PREVIEW_NOTICE: &str = "> **Provisional diff — PKO preview running.** This summary was synthesized by the Keel control plane from the proposed manifests; the real `pulumi preview` results will be posted as a follow-up comment when the ephemeral PKO preview completes.\n\n";

/// `GET /api/v1/proposals/{pr_id}/preview`
pub async fn get_proposal_preview(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<ProposalPreviewResponse>, ServerError> {
    let pr = state.forge.get_pull_request(&pr_id).await?;
    if !auth.is_admin() {
        if let Some(t) = pr
            .tenant
            .as_deref()
            .or_else(|| parse_proposal_branch_tenant(&pr.source_branch))
        {
            auth.require_tenant_access(t)?;
        } else {
            auth.require_admin()?;
        }
    }
    // Cluster-persisted PKO report first (survives restarts / shared across replicas), then the
    // optional in-memory cache, then a synthesis from the Git diff.
    let (cluster_report, live_run) =
        crate::preview_lifecycle::load_cluster_preview(&state, &pr_id).await;
    let cached_report = state
        .preview_cache
        .read()
        .await
        .get(&pr_id)
        .map(|c| c.report.clone());

    let git_diff = compute_pr_git_diff(&state, &pr).await;

    let report = match (cluster_report, cached_report) {
        (Some(r), _) => r,
        (None, cached) => {
            let mut r = match cached {
                Some(r) => r,
                None => match git_diff.as_deref() {
                    Some(diff) => synthesize_report_from_git_diff(&pr_id, diff),
                    None => PreviewReport::default(),
                },
            };
            if let Some(live) = live_run {
                r.run = Some(live);
            }
            r
        }
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

/// Rejects path traversal (`..` segments), absolute paths, backslashes, and NUL/control characters
/// so that prefix checks such as `tenants/<id>/` cannot be bypassed.
fn is_safe_repo_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('~')
        && !path.contains('\\')
        && !path.contains("..")
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|seg| seg == ".")
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
        if !is_safe_repo_relative_path(trimmed) {
            return Err(ServerError::Forbidden(format!(
                "Tenant administrators cannot manage PRs touching unsafe path '{trimmed}'"
            )));
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
                    if !is_safe_repo_relative_path(entry) || !entry.starts_with(&kustomize_prefix) {
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
///
/// For tenant administrators, returns the exact source-branch head SHA whose file scope was
/// verified, so callers can pin the subsequent forge operation to that commit.
async fn require_pr_tenant_admin_access(
    auth: &AuthSession,
    state: &AppState,
    pr_id: &str,
    expected_state: Option<PullRequestState>,
) -> Result<Option<String>, ServerError> {
    if auth.is_admin() {
        return Ok(None);
    }

    // Direct lookup: paginated list scans could miss older PRs (OPEN-18).
    let pr = state.forge.get_pull_request(pr_id).await?;
    if let Some(expected) = expected_state
        && pr.state != expected
    {
        return Err(ServerError::NotFound(format!(
            "Pull request '{pr_id}' is not {expected}"
        )));
    }

    let tenant = parse_proposal_branch_tenant(&pr.source_branch).ok_or_else(|| {
        ServerError::Forbidden(format!(
            "PR #{pr_id} branch '{}' is not scoped to a tenant",
            pr.source_branch
        ))
    })?;
    auth.require_tenant_admin(tenant)?;
    if pr.target_branch != "main" {
        return Err(ServerError::Forbidden(format!(
            "Tenant administrators can only manage PRs targeting 'main' (PR #{pr_id} targets '{}')",
            pr.target_branch
        )));
    }

    if !tokio::fs::try_exists(state.repo_workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        return Err(ServerError::Forbidden(
            "Git workdir unavailable to verify PR file scope for tenant administrator".to_string(),
        ));
    }

    let (head_sha, changed_files, kustomization_diff) = {
        let _git_guard = state.git_lock.lock().await;
        let _ = state.git_runner.fetch(&state.repo_workdir, "origin").await;
        // Resolve the branch head once; the scope check and the merge are both pinned to it so a
        // push between verification and merge cannot smuggle in unverified changes.
        let head_sha = state
            .git_runner
            .rev_parse(&state.repo_workdir, &format!("origin/{}", pr.source_branch))
            .await
            .ok()
            .filter(|sha| validate_commit_sha(sha).is_ok())
            .ok_or_else(|| {
                ServerError::Forbidden(format!(
                    "Unable to resolve head commit of PR #{pr_id} for tenant administrator"
                ))
            })?;
        let range = format!("origin/{}...{head_sha}", pr.target_branch);
        let files = state
            .git_runner
            .diff_name_only(&state.repo_workdir, &range)
            .await
            .map_err(|_| {
                ServerError::Forbidden(format!(
                    "Unable to verify changed files on PR #{pr_id} for tenant administrator"
                ))
            })?;

        let k_diff = if files.iter().any(|f| f == "tenants/kustomization.yaml") {
            state
                .git_runner
                .diff_file_zero_context(&state.repo_workdir, &range, "tenants/kustomization.yaml")
                .await
                .ok()
        } else {
            None
        };
        (head_sha, files, k_diff)
    };

    validate_tenant_admin_pr_paths(tenant, &changed_files, kustomization_diff.as_deref())?;
    Ok(Some(head_sha))
}

/// Resolves the head SHA a cluster-admin merge is pinned to: the fetched `origin/<branch>` commit
/// when a Git workdir is available, else the forge-reported PR head. Returns `None` only when no
/// valid commit id can be determined (the forge then merges its current head).
async fn resolve_merge_head_sha(
    state: &AppState,
    pr_id: &str,
) -> Result<Option<String>, ServerError> {
    let pr = state.forge.get_pull_request(pr_id).await?;
    if tokio::fs::try_exists(state.repo_workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        let _git_guard = state.git_lock.lock().await;
        let _ = state.git_runner.fetch(&state.repo_workdir, "origin").await;
        if let Ok(sha) = state
            .git_runner
            .rev_parse(&state.repo_workdir, &format!("origin/{}", pr.source_branch))
            .await
            && validate_commit_sha(&sha).is_ok()
        {
            return Ok(Some(sha));
        }
    }
    Ok(validate_commit_sha(&pr.head_sha).ok().map(str::to_string))
}

/// `POST /api/v1/proposals/{pr_id}/merge`
pub async fn merge_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(pr_id): AxumPath<String>,
) -> Result<Json<MergeProposalResponse>, ServerError> {
    let verified_sha =
        require_pr_tenant_admin_access(&auth, &state, &pr_id, Some(PullRequestState::Open)).await?;
    let head_sha = match verified_sha {
        Some(sha) => Some(sha),
        None => resolve_merge_head_sha(&state, &pr_id).await?,
    };
    // Derive live Stack deletions from the PR's actual diff (removed `tenants/<id>/**/stack.yaml`
    // at the pinned head), never from free-form title text (OPEN-11).
    let stack_deletions = match head_sha.as_deref() {
        Some(sha)
            if tokio::fs::try_exists(state.repo_workdir.join(".git"))
                .await
                .unwrap_or(false) =>
        {
            let pr = state.forge.get_pull_request(&pr_id).await?;
            let _git_guard = state.git_lock.lock().await;
            plan_stack_deletions_for_pr(&state, &pr, sha).await
        }
        _ => Vec::new(),
    };

    // The forge refuses the merge if the PR head moved after verification.
    let merged = state
        .forge
        .merge_pull_request(&pr_id, head_sha.as_deref())
        .await?;
    audit_proposal_event("merge", &auth, pr_tenant(&merged), &merged);

    for del in &stack_deletions {
        match state
            .state_provider
            .delete_tenant_stack(&del.namespace, &del.name)
            .await
        {
            Ok(_) => tracing::info!(
                target: "keel::audit",
                action = "stack.delete",
                user = %auth.claims.sub,
                tenant = %del.tenant,
                stack = %del.name,
                pr_id = %merged.id,
                "deleted Stack removed by merged proposal"
            ),
            Err(e) => tracing::warn!(
                tenant = %del.tenant,
                stack = %del.name,
                pr_id = %merged.id,
                error = %e,
                "failed to delete Stack removed by merged proposal"
            ),
        }
    }

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

    if let Ok(registry) = load_provider_registry(&state.repo_workdir).await {
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

    let gc_plan = crate::preview_lifecycle::gc_preview_for_pr(&state, &pr_id, "merged").await;

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
    audit_proposal_event("close", &auth, pr_tenant(&closed), &closed);

    let gc_plan = crate::preview_lifecycle::gc_preview_for_pr(&state, &pr_id, "rejected").await;

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
    audit_proposal_event("reopen", &auth, pr_tenant(&reopened), &reopened);

    Ok(Json(MergeProposalResponse {
        proposal: reopened,
        gc_plan: GcPlan::default(),
    }))
}

// ============================================================================
// Git diff, deletion planning, titles & audit helpers (OPEN-2, OPEN-11, OPEN-13, UI-15, CLI-15)
// ============================================================================

/// Private ref namespace used by read-only handlers so their fetches never race the
/// `refs/remotes/origin/*` updates performed by mutation paths under `git_lock`.
pub(crate) const READ_REF_NAMESPACE: &str = "keel-read/origin";

/// Repeated read-path fetches of the same branch are coalesced within this window.
const READ_FETCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on a read-path network fetch; the git child is killed on expiry.
const READ_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

type FetchDebounceMap = std::collections::HashMap<(std::path::PathBuf, String), std::time::Instant>;

static LAST_READ_FETCH: std::sync::LazyLock<std::sync::Mutex<FetchDebounceMap>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(FetchDebounceMap::new()));

/// Fetches `branches` into [`READ_REF_NAMESPACE`] **without** taking `git_lock`, debounced per
/// `(workdir, branch)` and bounded by a short timeout. Failures are logged and tolerated: callers
/// fall back to the last known `origin/*` refs.
pub(crate) async fn fetch_branches_for_read(state: &AppState, branches: &[&str]) {
    let now = std::time::Instant::now();
    let due: Vec<&str> = {
        let mut map = LAST_READ_FETCH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.retain(|_, at| now.duration_since(*at) < READ_FETCH_DEBOUNCE * 6);
        branches
            .iter()
            .copied()
            .filter(|b| {
                let key = (state.repo_workdir.clone(), (*b).to_string());
                match map.get(&key) {
                    Some(at) if now.duration_since(*at) < READ_FETCH_DEBOUNCE => false,
                    _ => {
                        map.insert(key, now);
                        true
                    }
                }
            })
            .collect()
    };
    if due.is_empty() {
        return;
    }
    if let Err(e) = state
        .git_runner
        .fetch_branches_isolated(
            &state.repo_workdir,
            "origin",
            &due,
            READ_REF_NAMESPACE,
            READ_FETCH_TIMEOUT,
        )
        .await
    {
        tracing::debug!(error = %e, "read-path git fetch failed; using last known refs");
    }
}

/// Resolves the freshest locally known ref for remote `branch`: the read-path namespace when
/// present, else `origin/<branch>`.
pub(crate) async fn resolve_read_ref(state: &AppState, branch: &str) -> String {
    let isolated = format!("refs/{READ_REF_NAMESPACE}/{branch}");
    if state
        .git_runner
        .ref_exists(&state.repo_workdir, &isolated)
        .await
        .unwrap_or(false)
    {
        isolated
    } else {
        format!("origin/{branch}")
    }
}

/// Computes the diff a PR actually proposes (OPEN-2), never a two-dot diff against current `main`:
///
/// - open/closed PRs: `<target>...<head>` (merge-base three-dot),
/// - merged PRs: `<recorded base>...<head>` when the forge recorded a base, else the merge
///   commit's first-parent diff, else three-dot against the target.
///
/// Reads refs/objects only (no checkout), so it does not need `git_lock`.
pub(crate) async fn compute_pr_git_diff(state: &AppState, pr: &PullRequestInfo) -> Option<String> {
    if !tokio::fs::try_exists(state.repo_workdir.join(".git"))
        .await
        .unwrap_or(false)
    {
        return None;
    }
    let mut branches = vec![pr.target_branch.as_str()];
    if pr.state != PullRequestState::Merged {
        branches.push(pr.source_branch.as_str());
    }
    fetch_branches_for_read(state, &branches).await;

    let target_ref = resolve_read_ref(state, &pr.target_branch).await;
    let runner = &state.git_runner;
    let dir = &state.repo_workdir;
    let head_sha = validate_commit_sha(&pr.head_sha).ok();
    let diff = match pr.state {
        PullRequestState::Merged => {
            let base = pr
                .base_sha
                .as_deref()
                .and_then(|b| validate_commit_sha(b).ok());
            let merge = pr
                .merge_commit_sha
                .as_deref()
                .and_then(|m| validate_commit_sha(m).ok());
            let mut out = None;
            if let (Some(base), Some(head)) = (base, head_sha) {
                out = runner.diff_refs(dir, base, head).await.ok();
            }
            if out.as_deref().is_none_or(str::is_empty)
                && let Some(merge) = merge
            {
                out = runner.diff_first_parent(dir, merge).await.ok();
            }
            if out.as_deref().is_none_or(str::is_empty)
                && let Some(head) = head_sha
            {
                out = runner.diff_refs(dir, &target_ref, head).await.ok();
            }
            out
        }
        PullRequestState::Open | PullRequestState::Closed => {
            let source_ref = resolve_read_ref(state, &pr.source_branch).await;
            match runner.diff_refs(dir, &target_ref, &source_ref).await {
                Ok(d) => Some(d),
                Err(_) => match head_sha {
                    Some(head) => runner.diff_refs(dir, &target_ref, head).await.ok(),
                    None => None,
                },
            }
        }
    };
    diff.filter(|d| !d.trim().is_empty())
}

/// A PKO `Stack` removed by a PR, derived from the PR's actual diff (never from its title).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StackDeletion {
    pub tenant: String,
    pub namespace: String,
    pub name: String,
}

/// Extracts `(tenant, path)` for deleted `stack.yaml` manifests under `tenants/<tenant>/...`.
fn deleted_stack_manifest_paths(deleted_files: &[String]) -> Vec<(String, String)> {
    deleted_files
        .iter()
        .map(|p| p.trim())
        .filter(|p| is_safe_repo_relative_path(p))
        .filter_map(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            if parts.len() >= 4
                && parts[0] == "tenants"
                && parts.last() == Some(&"stack.yaml")
                && validate_dns_label(parts[1], "tenant").is_ok()
            {
                Some((parts[1].to_string(), p.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Parses a deleted `stack.yaml` (as it existed at the merge-base) into a [`StackDeletion`],
/// requiring `kind: Stack` and a namespace equal to the owning tenant's namespace.
fn parse_deleted_stack_manifest(tenant: &str, raw_yaml: &str) -> Option<StackDeletion> {
    let expected_ns = tenant_namespace(tenant).ok()?;
    let doc: Value = serde_yml::from_str(raw_yaml).ok()?;
    if doc.get("kind").and_then(Value::as_str) != Some("Stack") {
        return None;
    }
    let meta = doc.get("metadata")?;
    let name = meta.get("name").and_then(Value::as_str)?.trim();
    validate_dns_label(name, "stack").ok()?;
    let namespace = meta
        .get("namespace")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or(expected_ns.as_str());
    if namespace != expected_ns {
        tracing::warn!(
            tenant,
            namespace,
            stack = name,
            "Ignoring deleted Stack manifest whose namespace does not match its tenant directory"
        );
        return None;
    }
    Some(StackDeletion {
        tenant: tenant.to_string(),
        namespace: expected_ns,
        name: name.to_string(),
    })
}

/// Plans live `Stack` deletions for a PR from its real diff (OPEN-11): every `stack.yaml` removed
/// under `tenants/<id>/` between the merge-base and `head_sha`. Must be called with `git_lock`
/// held after `origin` was fetched.
async fn plan_stack_deletions_for_pr(
    state: &AppState,
    pr: &PullRequestInfo,
    head_sha: &str,
) -> Vec<StackDeletion> {
    let target_ref = format!("origin/{}", pr.target_branch);
    let runner = &state.git_runner;
    let dir = &state.repo_workdir;
    let Ok(deleted) = runner.diff_deleted_files(dir, &target_ref, head_sha).await else {
        return Vec::new();
    };
    let candidates = deleted_stack_manifest_paths(&deleted);
    if candidates.is_empty() {
        return Vec::new();
    }
    let Ok(base) = runner.merge_base(dir, &target_ref, head_sha).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (tenant, path) in candidates {
        if let Ok(Some(raw)) = runner.read_file_at_ref(dir, &base, &path).await
            && let Some(del) = parse_deleted_stack_manifest(&tenant, &raw)
        {
            out.push(del);
        }
    }
    out
}

/// Returns `true` when `title` already follows Conventional Commits (`type(scope)!: subject`).
pub(crate) fn is_conventional_commit_title(title: &str) -> bool {
    const TYPES: &[&str] = &[
        "feat", "fix", "chore", "docs", "refactor", "revert", "perf", "test", "ci", "build",
        "style",
    ];
    let Some((head, subject)) = title.split_once(": ") else {
        return false;
    };
    if subject.trim().is_empty() {
        return false;
    }
    let head = head.strip_suffix('!').unwrap_or(head);
    let (kind, scope) = match head.split_once('(') {
        Some((k, rest)) => match rest.strip_suffix(')') {
            Some(scope) if !scope.is_empty() && !scope.contains(['(', ')']) => (k, Some(scope)),
            _ => return false,
        },
        None => (head, None),
    };
    TYPES.contains(&kind) && scope.is_none_or(|s| !s.chars().any(char::is_whitespace))
}

/// Builds the PR title for a tenant workload proposal (UI-15): a caller-supplied title is kept
/// only when it is already a Conventional Commit; otherwise the server-generated
/// `feat(<tenant>): <verb> <name>` title is used so UI and CLI proposals are uniform.
pub(crate) fn proposal_pr_title(
    requested: Option<&str>,
    tenant: &str,
    verb: &str,
    name: &str,
) -> String {
    match requested.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) if is_conventional_commit_title(t) => t.to_string(),
        _ => format!("feat({tenant}): {verb} {name}"),
    }
}

/// Builds an informative PR body: caller-supplied body (or a non-conventional caller title that
/// was replaced by the generated title) followed by the server-generated summary.
pub(crate) fn proposal_pr_body(
    requested_title: Option<&str>,
    final_title: &str,
    requested_body: Option<&str>,
    summary: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = requested_title.map(str::trim).filter(|t| !t.is_empty())
        && t != final_title
    {
        parts.push(format!("**{t}**"));
    }
    if let Some(b) = requested_body.map(str::trim).filter(|b| !b.is_empty()) {
        parts.push(b.to_string());
    }
    parts.push(summary.to_string());
    parts.join("\n\n")
}

/// Emits a structured audit log line for proposal lifecycle actions (NFR-OBS). Never logs secrets.
pub(crate) fn audit_proposal_event(
    action: &str,
    auth: &AuthSession,
    tenant: Option<&str>,
    pr: &PullRequestInfo,
) {
    tracing::info!(
        target: "keel::audit",
        action,
        user = %auth.claims.sub,
        tenant = tenant.unwrap_or("-"),
        pr_id = %pr.id,
        branch = %pr.source_branch,
        state = %pr.state,
        "proposal {action}"
    );
}

fn pr_tenant(pr: &PullRequestInfo) -> Option<&str> {
    pr.tenant
        .as_deref()
        .or_else(|| parse_proposal_branch_tenant(&pr.source_branch))
}

#[cfg(test)]
mod proposal_workflow_helper_tests {
    use super::*;

    #[test]
    fn pulumi_project_name_matches_committed_pulumi_yaml_never_display_name() {
        // Blueprint ships a Pulumi.yaml: its `name` wins.
        assert_eq!(
            pulumi_project_name_from_yaml(Some("name: kind-cluster\nruntime: yaml\n"), "kc1"),
            "kind-cluster"
        );
        // No blueprint Pulumi.yaml: the generated one is named after the instance.
        assert_eq!(pulumi_project_name_from_yaml(None, "kc1"), "kc1");
        assert_eq!(
            pulumi_project_name_from_yaml(Some("runtime: yaml\n"), "kc1"),
            "kc1"
        );
        assert_eq!(
            pulumi_project_name_from_yaml(Some(": not yaml ["), "kc1"),
            "kc1"
        );
        // Names outside Pulumi's charset are sanitized (no spaces in `organization/<p>/<s>`).
        assert_eq!(
            pulumi_project_name_from_yaml(Some("name: Local kind Cluster\n"), "kc1"),
            "Local-kind-Cluster"
        );
        assert_eq!(sanitize_pulumi_project_name("  a//b__c.d "), "a-b__c.d");
        assert_eq!(sanitize_pulumi_project_name("***"), "project");
    }

    #[test]
    fn conventional_titles_are_detected_and_enforced() {
        for ok in [
            "feat(acme): deploy redis",
            "fix(acme): bump chart",
            "chore(catalog): delete app podinfo",
            "feat!: breaking",
            "revert(core-addon): rollback kyverno",
        ] {
            assert!(is_conventional_commit_title(ok), "{ok}");
        }
        for bad in [
            "[New] feat(acme): deploy redis",
            "Deploy Redis for acme",
            "feat(acme):",
            "feat(): x",
            "feat(a b): x",
            "wip(acme): x",
        ] {
            assert!(!is_conventional_commit_title(bad), "{bad}");
        }
        assert_eq!(
            proposal_pr_title(Some("Deploy Redis for acme"), "acme", "deploy", "redis"),
            "feat(acme): deploy redis"
        );
        assert_eq!(
            proposal_pr_title(Some("fix(acme): pin redis"), "acme", "update", "redis"),
            "fix(acme): pin redis"
        );
        assert_eq!(
            proposal_pr_title(None, "acme", "provision", "vm-1"),
            "feat(acme): provision vm-1"
        );
        let body = proposal_pr_body(
            Some("Deploy Redis for acme"),
            "feat(acme): deploy redis",
            Some("please review"),
            "Automated proposal",
        );
        assert_eq!(
            body,
            "**Deploy Redis for acme**\n\nplease review\n\nAutomated proposal"
        );
        assert_eq!(
            proposal_pr_body(None, "feat(acme): deploy redis", None, "S"),
            "S"
        );
    }

    #[test]
    fn stack_deletions_derive_from_deleted_manifests_not_titles() {
        let deleted = vec![
            "tenants/acme/infra/db/stack.yaml".to_string(),
            "tenants/acme/infra/db/values.yaml".to_string(),
            "tenants/acme/bundles/web/vm/stack.yaml".to_string(),
            "tenants/../etc/stack.yaml".to_string(),
            "deploy/stack.yaml".to_string(),
            "tenants/stack.yaml".to_string(),
        ];
        assert_eq!(
            deleted_stack_manifest_paths(&deleted),
            vec![
                (
                    "acme".to_string(),
                    "tenants/acme/infra/db/stack.yaml".to_string()
                ),
                (
                    "acme".to_string(),
                    "tenants/acme/bundles/web/vm/stack.yaml".to_string()
                ),
            ]
        );

        let ok = "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: db\n  namespace: tenant-acme\n";
        assert_eq!(
            parse_deleted_stack_manifest("acme", ok),
            Some(StackDeletion {
                tenant: "acme".to_string(),
                namespace: "tenant-acme".to_string(),
                name: "db".to_string(),
            })
        );
        let no_ns = "kind: Stack\nmetadata:\n  name: web-vm\n";
        assert_eq!(
            parse_deleted_stack_manifest("acme", no_ns).map(|d| d.namespace),
            Some("tenant-acme".to_string())
        );
        // Cross-tenant namespace, wrong kind, or invalid names are ignored.
        let foreign = "kind: Stack\nmetadata:\n  name: db\n  namespace: tenant-globex\n";
        assert!(parse_deleted_stack_manifest("acme", foreign).is_none());
        assert!(
            parse_deleted_stack_manifest("acme", "kind: ConfigMap\nmetadata:\n  name: db\n")
                .is_none()
        );
        assert!(
            parse_deleted_stack_manifest("acme", "kind: Stack\nmetadata:\n  name: Bad_Name\n")
                .is_none()
        );
        assert!(parse_deleted_stack_manifest("acme", ": not yaml :").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn synthesized_preview_markdown_is_marked_simulated() {
        let report = synthesize_report_from_git_diff("7", "");
        assert_eq!(report.source, PreviewSource::Synthesized);
        let md = simulated_preview_markdown(&report, "7");
        assert!(md.starts_with("> **Simulated diff"));
        let pko = PreviewReport {
            source: PreviewSource::Pko,
            ..PreviewReport::default()
        };
        assert!(!simulated_preview_markdown(&pko, "7").contains("Simulated diff"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["source"], "synthesized");
    }

    #[test]
    fn safe_repo_relative_path_rejects_traversal_absolute_and_backslashes() {
        assert!(is_safe_repo_relative_path(
            "tenants/acme/apps/x/values.yaml"
        ));
        assert!(is_safe_repo_relative_path(
            "acme/apps/x/flux-kustomization.yaml"
        ));
        for bad in [
            "",
            "/etc/passwd",
            "~/x",
            "tenants/acme/../other/rbac.yaml",
            "acme/../other",
            "..",
            "tenants\\acme\\x",
            "tenants/acme/./x",
            "tenants/acme/\u{0}x",
        ] {
            assert!(!is_safe_repo_relative_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn tenant_admin_pr_paths_reject_traversal_and_foreign_entries() {
        assert!(
            validate_tenant_admin_pr_paths(
                "acme",
                &files(&["tenants/acme/apps/redis/values.yaml"]),
                None
            )
            .is_ok()
        );
        assert!(
            validate_tenant_admin_pr_paths(
                "acme",
                &files(&["tenants/acme/../globex/apps/redis/values.yaml"]),
                None
            )
            .is_err()
        );
        assert!(
            validate_tenant_admin_pr_paths("acme", &files(&["tenants/acme/rbac.yaml"]), None)
                .is_err()
        );
        assert!(validate_tenant_admin_pr_paths("acme", &files(&["deploy/x.yaml"]), None).is_err());
        assert!(validate_tenant_admin_pr_paths("acme", &[], None).is_err());

        let k = files(&["tenants/kustomization.yaml"]);
        let ok_diff = "--- a/tenants/kustomization.yaml\n+++ b/tenants/kustomization.yaml\n@@ -1 +1 @@\n+  - acme/apps/redis/flux-kustomization.yaml\n";
        assert!(validate_tenant_admin_pr_paths("acme", &k, Some(ok_diff)).is_ok());
        for bad_entry in [
            "acme/../globex/rbac.yaml",
            "/acme/x.yaml",
            "acme\\..\\globex",
            "globex/apps/x.yaml",
        ] {
            let diff = format!("+  - {bad_entry}\n");
            assert!(
                validate_tenant_admin_pr_paths("acme", &k, Some(&diff)).is_err(),
                "{bad_entry:?}"
            );
        }
        assert!(validate_tenant_admin_pr_paths("acme", &k, None).is_err());
    }
}
