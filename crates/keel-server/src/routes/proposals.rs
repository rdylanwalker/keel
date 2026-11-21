//! PR proposal synthesis, ephemeral PKO preview reports, and merge/reject/reopen endpoints (`SDD.md` §7.3).

use std::path::Path;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use keel_core::{
    AppRuntime, BundleItemKind, CatalogApplication, CatalogBundle, CatalogManifest,
    KEEL_PREVIEWS_NAMESPACE, ManifestEngine, SopsManager, partition_values_by_schema,
    render_helm_values_yaml, render_manifest_template, tenant_namespace, validate_dns_label,
    validate_k8s_manifests_yaml, validate_schema_values,
};
use keel_git::{
    PullRequestComment, PullRequestInfo, PullRequestState, parse_proposal_branch_tenant,
};
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, FlaggerCanarySpecInput, FluxDependencyRef,
    FluxHealthCheckRef, FluxKustomizationSpecInput, FluxSubstituteReference,
    GITHUB_TOKEN_SECRET_NAME, HelmChartSourceKind, HelmChartSourceRef, HelmReleaseSpecInput,
    HelmValuesSecretRef, RemoteKubeConfigRef, TenantAgeKey, WatchedCrdKind,
    generate_flux_kustomization_manifest, synthesize_canary_gateway_network_policy_yaml,
    synthesize_flagger_canary_yaml, synthesize_flux_kustomization_yaml,
    synthesize_helm_release_yaml,
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
use crate::state::{AppState, CachedPreview};

use super::{
    attach_infra_secret_refs, load_catalog_manifest, load_provider_registry,
    next_unique_branch_timestamp, open_pr_for_session, resolve_sops_recipients,
    resolve_tenant_resource_dir, safe_relative_path, serialize_tenants_kustomization,
    stage_commit_push_and_return_to_main,
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

async fn read_pulumi_project_name(pulumi_yaml_path: &Path, fallback: &str) -> String {
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

    let pr = open_pr_for_session(&state, &auth, &req.tenant, &branch, pr_title, pr_body).await?;

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

    let pr = open_pr_for_session(&state, &auth, &req.tenant, &branch, pr_title, pr_body).await?;

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

    PreviewReport { resources, summary }
}

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

    if merged.title.starts_with("[Delete]")
        && let Some((_, raw_stack)) = merged.title.split_once("remove stack ")
    {
        let stack_name = raw_stack.trim();
        let tenant = merged
            .tenant
            .as_deref()
            .or_else(|| parse_proposal_branch_tenant(&merged.source_branch))
            .unwrap_or("default");
        if let Ok(tenant_ns) = tenant_namespace(tenant) {
            let _ = state
                .state_provider
                .delete_tenant_stack(&tenant_ns, stack_name)
                .await;
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
        if let Ok(policy_val) = keel_kube::build_restrict_pulumi_providers_policy(&enabled_ids) {
            let _ = state
                .state_provider
                .apply_cluster_policy(
                    keel_kube::kyverno::POLICY_RESTRICT_PULUMI_PROVIDERS,
                    &policy_val,
                )
                .await;
        }
    }

    let cached = state.preview_cache.write().await.remove(&pr_id);
    let gc_plan = if let Some(entry) = cached {
        let plan = plan_preview_gc(&entry.candidates, &[pr_id.as_str()]);
        let _ = state.state_provider.execute_preview_gc(&plan).await;
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
        let _ = state.state_provider.execute_preview_gc(&plan).await;
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
