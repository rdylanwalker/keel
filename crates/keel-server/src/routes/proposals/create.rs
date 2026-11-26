//! `POST /api/v1/tenants/{tenant}/proposals`: single catalog app or infra blueprint proposals.

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use keel_core::{
    AppRuntime, CatalogManifest, ManifestEngine, apply_schema_defaults, merge_values_patch,
    render_helm_values_yaml, tenant_namespace, validate_dns_label,
};
use keel_kube::{
    FluxDependencyRef, HelmChartSourceKind, HelmChartSourceRef, HelmReleaseSpecInput,
    HelmValuesSecretRef, RemoteKubeConfigRef, synthesize_canary_gateway_network_policy_yaml,
    synthesize_flagger_canary_yaml, synthesize_helm_release_yaml,
};
use keel_pko::{
    PkoStack, PreviewRunStatus, PreviewSecretCopy, TenantStackParams, build_preview_branch,
    synthesize_ephemeral_preview, synthesize_k8s_manifest_pulumi_yaml, synthesize_tenant_stack,
};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::preview_lifecycle::launch_pko_preview;
use crate::proposal_repo::{ScopeRepo, TenantIndex};
use crate::routes::{
    BUNDLE_CATALOG_ID_LABEL, attach_infra_secret_refs, create_dir_all_contained, exists_contained,
    load_catalog_manifest_for_tenant, next_unique_branch_timestamp, read_contained_optional,
    read_contained_to_string, resolve_tenant_resource_dir, stage_commit_push_in, write_contained,
};
use crate::state::AppState;

use super::bundle::create_bundle_proposal;
use super::guards::{
    ensure_app_placement, ensure_cluster_singleton_not_installed,
    ensure_flux_kustomization_names_unclaimed, ensure_no_derived_name_sibling,
    ensure_rendered_manifest_placement, ensure_tenant_repo_initialised, is_cluster_singleton_app,
    remote_target_cluster, reserved_flux_kustomization_names,
};
use super::manifests::{
    FLUX_MANIFEST_APP_KUSTOMIZATION_FILES, FluxManifestAppFiles, FluxManifestAppLayoutInput,
    HelmAppExtrasInput, build_flux_manifest_app_files, build_remote_canary_files,
    helm_app_canary_input, helm_repository_yaml, prepare_helm_app_extras, read_bundle_catalog_id,
    read_flux_depends_on, remove_stale_entries, render_manifest_app, write_relative_files,
};
use super::pr::{OpenedProposal, PushedProposal, open_proposal_pr, publish_proposal};
use super::preview::{ProposedChange, synthesize_proposal_report};
use super::preview_inputs::{
    apply_preview_source_trust, attach_preview_cluster_inputs, plan_preview_credentials,
};
use super::secrets::{
    EffectiveSecrets, INFRA_SECRET_FILE, effective_non_secret_paths, heuristic_secret_hint,
    partition_effective_secrets, read_secret_overrides, read_stored_infra_secret,
    read_stored_manifest_app_secret, render_secret_string_data, resolve_manifest_app_secret,
    resolve_tenant_app_secret, write_secret_overrides,
};
use super::tenant_stack::{
    apply_tenant_source_trust, attach_tenant_stack_credentials,
    ensure_stack_values_split_by_top_level_key, populate_stack_config_from_values,
    read_pulumi_project_name, resolve_tenant_stack_source, tenant_stack_backend_url,
    tenant_stack_git_source,
};
use super::{CreateProposalRequest, CreateProposalResponse, ProposalGuards};

/// `POST /api/v1/tenants/{tenant}/proposals`
///
/// Validates payload against `catalog.yaml` JSONSchema, splits secrets (`writeOnly`/`format: password`),
/// writes tenant manifests (`tenants/<tenant>/...`) into the tenant's own repository (the platform
/// repository only for the development-only Local Git provider), commits on
/// `keel/<tenant>/<action>-<ts>`, pushes, opens the PR there via `ForgeProvider`, synthesizes the
/// ephemeral PKO preview (commit-pinned to the PR head for tenant repositories), and publishes an
/// SSE event.
pub async fn create_proposal(
    auth: AuthSession,
    State(state): State<AppState>,
    AxumPath(scope): AxumPath<String>,
    Json(mut req): Json<CreateProposalRequest>,
) -> Result<(StatusCode, Json<CreateProposalResponse>), ServerError> {
    validate_dns_label(&scope, "tenant")?;
    if !req.tenant.trim().is_empty() && req.tenant.trim() != scope {
        return Err(ServerError::BadRequest(format!(
            "body tenant `{}` does not match the path tenant `{scope}`",
            req.tenant.trim()
        )));
    }
    req.tenant = scope;
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

    // The tenant's repository (its own, or the platform repository for the Local Git provider):
    // held locked and synced to `main` from the first read of tenant content to the push, so
    // validation sees exactly the tree the commit is built on.
    let repo = ScopeRepo::for_tenant(&state, &req.tenant)?;
    // A tenant repository proposal still reads platform content (catalog items and blueprint
    // programs, `.sops.yaml`, route hostnames, the merged tenant stub): the platform checkout is
    // held on freshly synced `main` for the whole proposal, so changes merged on the forge (or by
    // another replica) are seen and no concurrent platform proposal can have a feature branch
    // checked out underneath. Lock order: `git_lock` before any tenant repository lock. (For
    // the Local Git provider `repo.lock` is `git_lock` itself.)
    let platform_guard = if repo.is_dedicated() {
        let guard = state.git_lock.clone().lock_owned().await;
        state
            .git_runner
            .sync_branch_from_remote(&state.repo_workdir, "origin", "main")
            .await?;
        Some(guard)
    } else {
        None
    };
    let repo_guard: ProposalGuards = (platform_guard, repo.lock(&state).await);
    repo.sync_main(&state).await?;
    ensure_tenant_repo_initialised(&repo).await?;

    let manifest = load_catalog_manifest_for_tenant(
        &state.repo_workdir,
        &req.catalog_id,
        req.kind.as_deref(),
        &req.tenant,
    )
    .await?;

    if let CatalogManifest::Bundle(bundle) = manifest {
        return create_bundle_proposal(auth, state, repo, repo_guard, req, *bundle).await;
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
            ensure_app_placement(
                &state.repo_workdir,
                app,
                remote_target_cluster(req.target_cluster.as_deref()),
            )
            .await?;
            // `spec.placement.namespace`: default an omitted targetNamespace to the app's
            // dedicated namespace and reject any other one (never the shared `tenant-<id>`).
            req.target_namespace = app
                .resolve_target_namespace(req.target_namespace.as_deref())
                .map_err(|e| ServerError::BadRequest(e.to_string()))?;
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
    if let CatalogManifest::Application(app) = &manifest
        && app.metadata.runtime == AppRuntime::Manifest
        && req.canary
    {
        return Err(ServerError::BadRequest(format!(
            "Catalog application '{}' is a manifest app; Flagger canary rollouts are only              generated for Helm apps",
            app.metadata.id
        )));
    }

    let subdir = if is_app { "apps" } else { "infra" };
    let resolved_dir =
        resolve_tenant_resource_dir(&repo.workdir, &req.tenant, subdir, &instance_name).await;
    let bundle_item = resolved_dir.as_ref().and_then(|r| r.bundle_item.clone());
    let own_rel_dir = resolved_dir
        .as_ref()
        .map(|r| r.rel_dir.clone())
        .unwrap_or_else(|| format!("tenants/{}/{subdir}/{instance_name}", req.tenant));
    // A bundle item edited on its own keeps the bundle catalog item it was deployed from (the
    // documents are rebuilt below), so its bundle's config read still masks it with the
    // bundle schema.
    let bundle_catalog_id = match bundle_item {
        Some(_) => read_bundle_catalog_id(&repo.workdir, &own_rel_dir).await,
        None => None,
    };
    let bundle_labels = |labels: &mut std::collections::BTreeMap<String, String>| {
        if let Some((bundle_name, item_id)) = &bundle_item {
            labels.insert("keel.io/bundle".to_string(), bundle_name.clone());
            labels.insert("keel.io/bundle-item".to_string(), item_id.clone());
            if let Some(id) = &bundle_catalog_id {
                labels.insert(BUNDLE_CATALOG_ID_LABEL.to_string(), id.clone());
            }
        }
    };
    // Helm apps (`HelmRelease` path): secrets, routes and NetworkPolicy via `HelmAppExtras`.
    // Their secrets are applied by the root tenants Kustomization: no `<name>-config`.
    let is_helm_app = is_app && manifest_engine.is_none();
    if resolved_dir.is_none() {
        ensure_no_derived_name_sibling(
            &repo.workdir,
            &req.tenant,
            &own_rel_dir,
            &instance_name,
            !is_helm_app,
        )
        .await?;
    }
    ensure_flux_kustomization_names_unclaimed(
        &repo.workdir,
        &req.tenant,
        &own_rel_dir,
        &reserved_flux_kustomization_names(&instance_name, !is_helm_app),
    )
    .await?;
    // First install: fill the catalog schema `default`s server-side, so CLI, API and GUI installs
    // commit the same values. An edit (the instance directory exists) submits the complete
    // desired values (keys the user removed must not come back as defaults), unless it asks to
    // merge a partial patch over the committed values (`mergeStoredValues`, the CLI).
    if resolved_dir.is_none() {
        req.values = apply_schema_defaults(schema, &req.values);
    } else if req.merge_stored_values {
        let stored = read_stored_values(&repo.workdir, &own_rel_dir).await?;
        req.values = merge_values_patch(&stored, &req.values);
    }
    // Edit (Flux manifest apps, Helm apps and infrastructure stacks): secret fields left blank or
    // omitted keep the committed values (the UI never receives them); provided ones replace
    // them. Pulumi-engine manifest apps cannot have secret fields (rejected below).
    let stored_manifest_secret = if manifest_engine == Some(ManifestEngine::Flux) || is_helm_app {
        read_stored_manifest_app_secret(&repo.workdir, &own_rel_dir).await
    } else if !is_app {
        read_stored_infra_secret(&repo.workdir, &own_rel_dir).await
    } else {
        None
    };
    // Effective secret rules (AST-2): schema-declared secrets plus values whose key looks like a
    // secret, minus the user's "not secret" overrides (this request's, else the persisted ones).
    let non_secret_paths = effective_non_secret_paths(
        schema,
        req.non_secret_paths.as_deref(),
        read_secret_overrides(&repo.workdir, &own_rel_dir).await,
    )?;
    let EffectiveSecrets {
        partitioned,
        notices: secret_notices,
        non_secret_paths,
    } = partition_effective_secrets(
        schema,
        &mut req.values,
        stored_manifest_secret.as_ref(),
        non_secret_paths,
        true,
    )?;
    let has_secret_fields = partitioned
        .secret_values
        .as_object()
        .is_some_and(|o| !o.is_empty());
    // Secret fields reach a manifest template only through Flux `postBuild` substitution from the
    // SOPS-decrypted `<instance>-secrets` Secret, which the `<instance>-config` Kustomization
    // applies in the tenant namespace of the management cluster (where Flux reads substitution
    // sources), so remote `targetCluster`s work too. The Pulumi engine has no such path: fail
    // closed before encrypting or committing.
    if has_secret_fields && manifest_engine == Some(ManifestEngine::Pulumi) {
        return Err(ServerError::BadRequest(format!(
            "Catalog application '{}' has secret fields, which the Pulumi engine cannot pass into its manifest template; use the flux engine{}",
            req.catalog_id,
            heuristic_secret_hint(&secret_notices)
        )));
    }
    // Multi-namespace manifest apps rely on the Flux layout (namespaces created first, no
    // `targetNamespace`); the Pulumi manifest Stack has no such path.
    if let CatalogManifest::Application(app) = &manifest
        && manifest_engine == Some(ManifestEngine::Pulumi)
        && !app.multi_namespace_placement().is_empty()
    {
        return Err(ServerError::BadRequest(format!(
            "Catalog application '{}' spans several namespaces (spec.placement.additionalNamespaces), which only the flux engine supports",
            req.catalog_id
        )));
    }
    let values_yaml = render_helm_values_yaml(&partitioned.normal_values)?;

    let tenant_ns = tenant_namespace(&req.tenant)?;
    let infra_secret_name = format!("{instance_name}-secrets");

    // Encrypt extracted secret fields via SOPS + age before touching Git (`SDD.md` §6.3).
    // Flux manifest apps and infrastructure stacks commit the `<instance>-secrets` Secret
    // resolved against the stored one (blank secrets keep their stored values); Helm apps
    // resolve theirs in `prepare_helm_app_extras`.
    let (encrypted_infra_k8s_secret_yaml, infra_secret_keys) =
        if manifest_engine == Some(ManifestEngine::Flux) {
            let secret = resolve_manifest_app_secret(
                &state,
                &req.tenant,
                &tenant_ns,
                &infra_secret_name,
                schema,
                &partitioned.secret_field_paths,
                &partitioned.secret_values,
                stored_manifest_secret.as_ref(),
            )
            .await?;
            (secret, Vec::new())
        } else if !is_app {
            match resolve_tenant_app_secret(
                &state,
                &req.tenant,
                &tenant_ns,
                &infra_secret_name,
                &partitioned.secret_field_paths,
                render_secret_string_data(&partitioned.secret_values),
                &[],
                stored_manifest_secret.as_ref(),
            )
            .await?
            {
                Some((ciphertext, keys)) => (Some(ciphertext), keys.into_iter().collect()),
                None => (None, Vec::new()),
            }
        } else {
            (None, Vec::new())
        };
    if !is_app {
        ensure_stack_values_split_by_top_level_key(&partitioned.normal_values, &infra_secret_keys)?;
    }

    let helm_extras = match &manifest {
        CatalogManifest::Application(app) if is_helm_app => {
            let target_namespace = req
                .target_namespace
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Some(
                prepare_helm_app_extras(
                    &state,
                    HelmAppExtrasInput {
                        repo: &repo,
                        tenant: &req.tenant,
                        tenant_ns: &tenant_ns,
                        name: &instance_name,
                        app_dir: &own_rel_dir,
                        app,
                        partitioned: &partitioned,
                        stored_secret: stored_manifest_secret.as_ref(),
                        remote: remote_target_cluster(req.target_cluster.as_deref()).is_some(),
                        target_namespace,
                        canary: req.canary,
                    },
                )
                .await?,
            )
        }
        _ => None,
    };

    let timestamp = next_unique_branch_timestamp();
    let branch = build_preview_branch(&req.tenant, &action_label, timestamp)?;

    let mut rendered_manifest_template_yaml: Option<String> = None;
    let mut synthesized_manifest_pulumi_yaml: Option<String> = None;
    let mut flux_manifest_files: Option<FluxManifestAppFiles> = None;

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
            let rendered =
                render_manifest_app(&state, &repo, app, &partitioned.normal_values, &rel_dir)
                    .await?;
            rendered_manifest_template_yaml = Some(rendered.clone());

            let remote_kc = remote_target_cluster(req.target_cluster.as_deref())
                .map(|t| RemoteKubeConfigRef::with_key(t, "kubeconfig"));
            ensure_rendered_manifest_placement(&app.metadata.id, &rendered, remote_kc.is_some())?;
            app.ensure_manifest_namespaces(&rendered)
                .map_err(|e| ServerError::BadRequest(e.to_string()))?;
            if let Some(kc) = &remote_kc
                && is_cluster_singleton_app(&state.repo_workdir, app).await
            {
                ensure_cluster_singleton_not_installed(
                    &repo.workdir,
                    &req.tenant,
                    &rel_dir,
                    &app.metadata.id,
                    &kc.secret_name,
                )
                .await?;
            }
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
                    let mut labels = std::collections::BTreeMap::new();
                    labels.insert("keel.io/runtime".to_string(), "manifest".to_string());
                    labels.insert("keel.io/engine".to_string(), "flux".to_string());
                    labels.insert("keel.io/catalog-id".to_string(), req.catalog_id.clone());
                    let mut depends_on = Vec::new();
                    bundle_labels(&mut labels);
                    if bundle_item.is_some() {
                        // Keep the bundle ordering of the item's Kustomization on edit.
                        depends_on = read_flux_depends_on(
                            &repo.workdir,
                            &format!("{rel_dir}/flux-kustomization.yaml"),
                        )
                        .await;
                    }
                    let files = build_flux_manifest_app_files(FluxManifestAppLayoutInput {
                        tenant: &req.tenant,
                        tenant_ns: &tenant_ns,
                        name: &instance_name,
                        app_dir: &rel_dir,
                        catalog_id: &req.catalog_id,
                        rendered_manifests: &rendered,
                        normal_values: &partitioned.normal_values,
                        encrypted_secret_yaml: encrypted_infra_k8s_secret_yaml.as_deref(),
                        kube_config: remote_kc.clone(),
                        target_namespace: remote_ns.clone(),
                        multi_namespaces: app.multi_namespace_placement(),
                        depends_on,
                        extra_labels: labels,
                    })?;
                    let fk_yaml = files.app_kustomization_yaml.clone();
                    flux_manifest_files = Some(files);
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

                    let stack_source = resolve_tenant_stack_source(&state, &req.tenant).await?;
                    let mut stack_params = TenantStackParams::new(
                        &req.tenant,
                        &instance_name,
                        format!(
                            "organization/{instance_name}/{}-{instance_name}",
                            req.tenant
                        ),
                        tenant_stack_backend_url(&state, &req.tenant)?,
                        tenant_stack_git_source(&stack_source, &rel_dir)?,
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
                    attach_tenant_stack_credentials(
                        &state,
                        &req.tenant,
                        &tenant_ns,
                        &mut stack_params.env_refs,
                    )
                    .await?;
                    let mut stack = synthesize_tenant_stack(&stack_params)?;
                    apply_tenant_source_trust(&mut stack, &stack_source);
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
            bundle_labels(&mut hr_input.extra_labels);
            if bundle_item.is_some() {
                let abs_dir = repo.workdir.join(&rel_dir);
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
                    && let Ok(raw) = read_contained_to_string(&repo.workdir, &hr_path).await
                    && let Ok(doc) = noyalib::from_str::<Value>(&raw)
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
            hr_input.crds = app.spec.crd_policy;
            if let Some(target) = remote_target_cluster(req.target_cluster.as_deref()) {
                if is_cluster_singleton_app(&state.repo_workdir, app).await {
                    ensure_cluster_singleton_not_installed(
                        &repo.workdir,
                        &req.tenant,
                        &rel_dir,
                        &app.metadata.id,
                        target,
                    )
                    .await?;
                }
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
            if let Some(ref extras) = helm_extras {
                extras.apply_to(&mut hr_input);
            }
            // Identifies the instance's catalog app for the once-per-cluster guard.
            hr_input
                .extra_labels
                .insert("keel.io/catalog-id".to_string(), req.catalog_id.clone());
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
            let stack_source = resolve_tenant_stack_source(&state, &req.tenant).await?;
            let mut stack_params = TenantStackParams::new(
                &req.tenant,
                &instance_name,
                format!(
                    "organization/{pulumi_project}/{}-{instance_name}",
                    req.tenant
                ),
                tenant_stack_backend_url(&state, &req.tenant)?,
                tenant_stack_git_source(&stack_source, &rel_dir)?,
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
            attach_tenant_stack_credentials(
                &state,
                &req.tenant,
                &tenant_ns,
                &mut stack_params.env_refs,
            )
            .await?;
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
                    &repo.workdir,
                    &repo.workdir.join(&rel_dir).join("stack.yaml"),
                )
                .await
                && let Ok(doc) = noyalib::from_str::<Value>(&raw)
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
            apply_tenant_source_trust(&mut stack, &stack_source);
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

    let (commit_sha, is_update, previous_rendered_manifests) = {
        let workdir = repo.workdir.as_path();
        state
            .git_runner
            .checkout_new_branch(workdir, &branch)
            .await?;

        let target_dir = workdir.join(&rel_dir);
        let is_update = exists_contained(workdir, &target_dir).await?;
        // The instance's rendered documents on `main`, so the preview diffs against them. The read
        // is contained: a committed symlink escaping the worktree is refused, never followed.
        let previous_rendered_manifests = if is_update {
            read_contained_optional(workdir, &target_dir.join("manifests.yaml")).await?
        } else {
            None
        };
        create_dir_all_contained(workdir, &target_dir).await?;

        let mut staged_paths = Vec::new();
        let values_path = target_dir.join("values.yaml");
        write_contained(workdir, &values_path, &values_yaml).await?;
        staged_paths.push(format!("{rel_dir}/values.yaml"));
        write_secret_overrides(workdir, &rel_dir, &non_secret_paths, &mut staged_paths).await?;

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
        write_contained(workdir, &manifest_path, &manifest_yaml).await?;
        staged_paths.push(format!("{rel_dir}/{manifest_filename}"));

        if is_app {
            let remote_canary = req.canary && remote_app_kube_config.is_some();
            let mut stale_files: Vec<&str> = match manifest_engine {
                Some(ManifestEngine::Flux) => vec![
                    "helmrelease.yaml",
                    "helmrelease-canary.yaml",
                    "helm-repository.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                    "canary",
                    "canary-kustomization.yaml",
                    "stack.yaml",
                    "Pulumi.yaml",
                    "secret.enc.yaml",
                ],
                Some(ManifestEngine::Pulumi) => vec![
                    "helmrelease.yaml",
                    "helmrelease-canary.yaml",
                    "helm-repository.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                    "canary",
                    "canary-kustomization.yaml",
                    "flux-kustomization.yaml",
                    "config-kustomization.yaml",
                    "config",
                    "namespace.yaml",
                    "values-configmap.yaml",
                    "secrets.enc.yaml",
                ],
                None if remote_canary => vec![
                    "helmrelease.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                ],
                None if req.canary => {
                    vec!["helmrelease.yaml", "canary", "canary-kustomization.yaml"]
                }
                None => vec![
                    "helmrelease-canary.yaml",
                    "canary.yaml",
                    "networkpolicy-canary.yaml",
                    "canary",
                    "canary-kustomization.yaml",
                ],
            };
            if let Some(ref files) = flux_manifest_files {
                stale_files.extend(files.stale.iter().copied());
            }
            if let Some(ref extras) = helm_extras {
                stale_files.extend(extras.stale.iter().copied());
            }
            remove_stale_entries(workdir, &rel_dir, &stale_files, &mut staged_paths).await?;
        }

        let mut extra_kustomize_resources: Vec<String> = Vec::new();
        let mut bundle_item_kustomize_resources: Vec<String> = vec![manifest_filename.to_string()];
        let mut helm_tenant_resources: Vec<String> = Vec::new();

        if let Some(eng) = manifest_engine {
            if let Some(ref rendered_manifests) = rendered_manifest_template_yaml {
                write_contained(
                    workdir,
                    &target_dir.join("manifests.yaml"),
                    rendered_manifests,
                )
                .await?;
                staged_paths.push(format!("{rel_dir}/manifests.yaml"));
            }
            match eng {
                ManifestEngine::Flux => {
                    if let Some(ref files) = flux_manifest_files {
                        write_relative_files(workdir, &rel_dir, &files.files, &mut staged_paths)
                            .await?;
                    }
                }
                ManifestEngine::Pulumi => {
                    if let Some(ref pulumi_yaml) = synthesized_manifest_pulumi_yaml {
                        write_contained(workdir, &target_dir.join("Pulumi.yaml"), pulumi_yaml)
                            .await?;
                        staged_paths.push(format!("{rel_dir}/Pulumi.yaml"));
                    }
                    let mut app_k_resources = vec!["stack.yaml".to_string()];
                    if let Some(ref enc_k8s_secret) = encrypted_infra_k8s_secret_yaml {
                        write_contained(
                            workdir,
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
                        workdir,
                        &target_dir.join("kustomization.yaml"),
                        keel_core::yaml::to_string(&app_k_doc).map_err(|e| {
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
            if let Some(ref extras) = helm_extras {
                write_relative_files(workdir, &rel_dir, &extras.files, &mut staged_paths).await?;
                let rel_dir_in_tenants = rel_dir.strip_prefix("tenants/").unwrap_or(&rel_dir);
                for file in &extras.resources {
                    bundle_item_kustomize_resources.push((*file).to_string());
                    extra_kustomize_resources.push(format!("{rel_dir_in_tenants}/{file}"));
                }
                // Always registered in `tenants/kustomization.yaml` (also for bundle items):
                // the SOPS `Secret` needs `keel-tenants`' decryption.
                for file in &extras.tenant_resources {
                    helm_tenant_resources.push(format!("{rel_dir_in_tenants}/{file}"));
                }
            }
            if let Some(helm_repo_yaml) = helm_repository_yaml(
                app,
                &req.tenant,
                &tenant_ns,
                &instance_name,
                bundle_item
                    .as_ref()
                    .map(|(bundle, item)| (bundle.as_str(), item.as_str())),
            )? {
                let helm_repo_path = target_dir.join("helm-repository.yaml");
                write_contained(workdir, &helm_repo_path, helm_repo_yaml).await?;
                staged_paths.push(format!("{rel_dir}/helm-repository.yaml"));
                bundle_item_kustomize_resources.push("helm-repository.yaml".to_string());
                extra_kustomize_resources.push(format!(
                    "{}/apps/{instance_name}/helm-repository.yaml",
                    req.tenant
                ));
            }
            if req.canary {
                let mut canary_input = helm_app_canary_input(
                    &req.tenant,
                    &instance_name,
                    app,
                    &partitioned.normal_values,
                );
                bundle_labels(&mut canary_input.extra_labels);
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
                    let files = build_remote_canary_files(
                        &req.tenant,
                        &instance_name,
                        &rel_dir,
                        remote_kc,
                        &remote_target_ns,
                        canary_yaml,
                        np_yaml,
                        &canary_input.extra_labels,
                    )?;
                    write_relative_files(workdir, &rel_dir, &files, &mut staged_paths).await?;
                    bundle_item_kustomize_resources.push("canary-kustomization.yaml".to_string());
                    extra_kustomize_resources.push(format!(
                        "{}/apps/{instance_name}/canary-kustomization.yaml",
                        req.tenant
                    ));
                } else {
                    let canary_path = target_dir.join("canary.yaml");
                    write_contained(workdir, &canary_path, canary_yaml).await?;
                    staged_paths.push(format!("{rel_dir}/canary.yaml"));
                    bundle_item_kustomize_resources.push("canary.yaml".to_string());
                    extra_kustomize_resources
                        .push(format!("{}/apps/{instance_name}/canary.yaml", req.tenant));

                    let np_path = target_dir.join("networkpolicy-canary.yaml");
                    write_contained(workdir, &np_path, np_yaml).await?;
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
            write_contained(workdir, &target_pulumi_path, pulumi_yaml).await?;
            staged_paths.push(format!("{rel_dir}/Pulumi.yaml"));

            if let Some(enc_k8s_secret) = &encrypted_infra_k8s_secret_yaml {
                let secret_path = target_dir.join(INFRA_SECRET_FILE);
                write_contained(workdir, &secret_path, enc_k8s_secret).await?;
                staged_paths.push(format!("{rel_dir}/{INFRA_SECRET_FILE}"));
                bundle_item_kustomize_resources.push(INFRA_SECRET_FILE.to_string());
                extra_kustomize_resources.push(format!(
                    "{}/infra/{instance_name}/{INFRA_SECRET_FILE}",
                    req.tenant
                ));
            } else {
                // No secret value left (none stored or submitted, or all marked not secret):
                // the Stack no longer references the Secret.
                remove_stale_entries(workdir, &rel_dir, &[INFRA_SECRET_FILE], &mut staged_paths)
                    .await?;
            }
        }

        let index = TenantIndex::new(&repo, &req.tenant);
        let mut resources: Vec<String> = index.load(workdir).await;
        // The platform repository index also lists the tenant's own baseline and Flux source
        // (Local Git provider); a tenant repository index never does.
        let monorepo_candidates = if index.is_dedicated() {
            Vec::new()
        } else {
            vec![
                format!("{}/rbac.yaml", req.tenant),
                format!("{}/git-repository.yaml", req.tenant),
            ]
        };
        for candidate in monorepo_candidates {
            if tokio::fs::try_exists(workdir.join("tenants").join(&candidate))
                .await
                .unwrap_or(false)
                && !resources.contains(&candidate)
            {
                resources.push(candidate);
            }
        }
        let rel_dir_in_tenants = rel_dir.strip_prefix("tenants/").unwrap_or(&rel_dir);
        for fk_rel in helm_tenant_resources {
            if !resources.contains(&fk_rel) {
                resources.push(fk_rel);
            }
        }
        if bundle_item.is_some() {
            if manifest_engine == Some(ManifestEngine::Flux) {
                // The item directory's `kustomization.yaml` already lists the rendered manifests
                // (written with the manifest-app layout); register both Kustomizations.
                for fk_file in FLUX_MANIFEST_APP_KUSTOMIZATION_FILES {
                    let fk_rel = format!("{rel_dir_in_tenants}/{fk_file}");
                    if !resources.contains(&fk_rel) {
                        resources.push(fk_rel);
                    }
                }
            } else {
                write_contained(
                    workdir,
                    &target_dir.join("kustomization.yaml"),
                    keel_core::yaml::to_string(&json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": bundle_item_kustomize_resources,
                    }))
                    .map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to serialize kustomization.yaml: {e}"
                        ))
                    })?,
                )
                .await?;
                staged_paths.push(format!("{rel_dir}/kustomization.yaml"));

                let fk_rel = format!("{rel_dir_in_tenants}/flux-kustomization.yaml");
                if !resources.contains(&fk_rel) {
                    resources.push(fk_rel);
                }
            }
        } else {
            if manifest_engine == Some(ManifestEngine::Flux) {
                extra_kustomize_resources.push(format!(
                    "{rel_dir_in_tenants}/{}",
                    FLUX_MANIFEST_APP_KUSTOMIZATION_FILES[0]
                ));
            }
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
        // Drop entries for files of this instance that no longer exist (engine / canary / layout
        // switches), so the tenants Kustomization never references a removed file.
        let own_prefix = format!("{rel_dir_in_tenants}/");
        resources
            .retain(|r| !r.starts_with(&own_prefix) || workdir.join("tenants").join(r).is_file());
        staged_paths.push(index.store(workdir, &resources).await?);

        let commit_msg = format!(
            "feat({}): propose {} {}",
            req.tenant,
            if is_app { "app" } else { "infra" },
            instance_name
        );
        let sha = stage_commit_push_in(&state, workdir, &auth, &branch, &commit_msg, &staged_paths)
            .await?;
        (sha, is_update, previous_rendered_manifests)
    };
    drop(repo_guard);

    let verb = match (is_update, is_app) {
        (true, _) => "update",
        (false, true) => "deploy",
        (false, false) => "provision",
    };
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
    let OpenedProposal {
        pr,
        preview_key,
        mut preview_params,
    } = open_proposal_pr(
        &state,
        &auth,
        &repo,
        &req,
        PushedProposal {
            branch: &branch,
            timestamp,
            action_label: &action_label,
            rel_dir: &rel_dir,
            instance_name: &instance_name,
            commit_sha: &commit_sha,
            verb,
            summary: &summary,
        },
    )
    .await?;

    // Ephemeral PKO v2 preview in `keel-previews` (`SDD.md` §4.2): only Pulumi-backed changes
    // (infra blueprints, Pulumi-engine manifest apps) have a program PKO can preview.
    let pko_backed = !is_app || manifest_engine == Some(ManifestEngine::Pulumi);
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
        preview_blocker = attach_preview_cluster_inputs(
            &state,
            &repo,
            &req.tenant,
            &mut preview_params,
            &mut preview_secret_copies,
        )
        .await;
    }
    let mut preview_stack = synthesize_ephemeral_preview(&preview_params)?;
    apply_preview_source_trust(&mut preview_stack.stack, &preview_params);
    let run = match (pko_backed, preview_blocker) {
        (false, _) => PreviewRunStatus::not_executed(
            "Flux-managed change: no Pulumi program to preview; report synthesized from the proposed manifests",
        ),
        (true, Some(reason)) => PreviewRunStatus::not_executed(reason),
        (true, None) => launch_pko_preview(&state, &preview_stack, &preview_secret_copies).await,
    };

    let report = synthesize_proposal_report(
        &preview_key,
        &req.tenant,
        &ProposedChange {
            resource_type: preview_resource_type,
            instance_name: &instance_name,
            is_update,
            rendered_manifests: rendered_manifest_template_yaml.as_deref(),
            previous_rendered_manifests: previous_rendered_manifests.as_deref(),
        },
        run,
    );

    let (status, Json(mut body)) = publish_proposal(
        &state,
        &repo,
        pr,
        preview_key,
        branch,
        preview_stack,
        report,
    )
    .await;
    body.secret_notices = secret_notices;
    Ok((status, Json(body)))
}

/// The committed non-secret values of an existing instance (`<rel_dir>/values.yaml`; an absent or
/// empty file is `{}`). Secret fields are never in it: a blank secret keeps its stored value.
async fn read_stored_values(
    workdir: &std::path::Path,
    rel_dir: &str,
) -> Result<Value, ServerError> {
    let path = workdir.join(rel_dir).join("values.yaml");
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e.into()),
    };
    match noyalib::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(Value::Null) => Ok(json!({})),
        Ok(_) | Err(_) => Err(ServerError::Internal(format!(
            "committed values file `{rel_dir}/values.yaml` is not a YAML mapping"
        ))),
    }
}
