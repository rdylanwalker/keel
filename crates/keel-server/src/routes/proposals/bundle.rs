//! Bundle proposals: every item under `tenants/<tenant>/bundles/<name>/`, one PR.

use axum::Json;
use axum::http::StatusCode;
use keel_core::{
    AppRuntime, BundleItemKind, CatalogBundle, CatalogManifest, ManifestEngine,
    apply_schema_defaults, render_helm_values_yaml, tenant_namespace, validate_dns_label,
    validate_schema_values_with_secrets,
};
use keel_kube::{
    FluxDependencyRef, FluxHealthCheckRef, FluxKustomizationSpecInput, HelmChartSourceKind,
    HelmChartSourceRef, HelmReleaseSpecInput, HelmValuesSecretRef, RemoteKubeConfigRef,
    synthesize_canary_gateway_network_policy_yaml, synthesize_flagger_canary_yaml,
    synthesize_flux_kustomization_yaml, synthesize_helm_release_yaml,
};
use keel_pko::{
    ChangeOp, PreviewReport, PreviewRunStatus, PreviewSource, PreviewSummary, ResourceDiff,
    TenantStackParams, build_preview_branch, synthesize_ephemeral_preview, synthesize_tenant_stack,
};
use serde_json::{Value, json};

use crate::auth::AuthSession;
use crate::error::ServerError;
use crate::proposal_repo::{ScopeRepo, TenantIndex};
use crate::routes::{
    create_dir_all_contained, load_catalog_manifest_for_tenant, next_unique_branch_timestamp,
    read_contained_optional, stage_commit_push_in, write_contained,
};
use crate::state::AppState;

use super::guards::{
    ensure_cluster_singleton_not_installed, ensure_flux_kustomization_names_unclaimed,
    ensure_no_derived_name_sibling, ensure_rendered_manifest_placement, is_cluster_singleton_app,
    management_twin_placement_hint, remote_target_cluster, reserved_flux_kustomization_names,
};
use super::manifests::{
    FLUX_MANIFEST_APP_KUSTOMIZATION_FILES, FluxManifestAppLayoutInput, HelmAppExtrasInput,
    build_flux_manifest_app_files, build_remote_canary_files, helm_app_canary_input,
    helm_repository_yaml, prepare_helm_app_extras, remove_stale_entries, render_manifest_app,
    write_relative_files,
};
use super::pr::{OpenedProposal, PushedProposal, open_proposal_pr, publish_proposal};
use super::secrets::{
    EffectiveSecrets, drop_blank_stored_secret_values, effective_non_secret_paths,
    partition_effective_secrets, read_secret_overrides, read_stored_manifest_app_secret,
    relax_stored_secret_requirements_with, resolve_manifest_app_secret, write_secret_overrides,
};
use super::tenant_stack::{
    apply_tenant_source_trust, attach_tenant_stack_credentials, read_pulumi_project_name,
    resolve_tenant_stack_source, tenant_stack_backend_url, tenant_stack_git_source,
};
use super::{CreateProposalRequest, CreateProposalResponse, ProposalGuards};

/// Bundle proposals: every item lives under `tenants/<tenant>/bundles/<name>/` of the tenant's
/// repository, so a bundle is always exactly one PR in one repository (D7: a change spanning
/// several repositories would be one PR per repository; catalog bundles never span tenants).
pub(super) async fn create_bundle_proposal(
    auth: AuthSession,
    state: AppState,
    repo: ScopeRepo,
    repo_guard: ProposalGuards,
    mut req: CreateProposalRequest,
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
    if req.merge_stored_values {
        return Err(ServerError::BadRequest(
            "mergeStoredValues is not supported for bundles: submit the complete bundle values"
                .to_string(),
        ));
    }
    let bundle_rel_dir = format!("tenants/{}/bundles/{instance_name}", req.tenant);
    // First install fills schema defaults (bundle schema here, each item's schema below); an
    // edit submits the complete desired values, so removed keys never come back as defaults.
    let is_new_bundle = !tokio::fs::try_exists(repo.workdir.join(&bundle_rel_dir))
        .await
        .unwrap_or(false);
    if is_new_bundle {
        req.values = apply_schema_defaults(&bundle.spec.schema, &req.values);
    }

    // "Not secret" overrides (AST-2) are bundle paths `<item>.<path>`: split them per item.
    let mut requested_item_overrides: Option<std::collections::BTreeMap<&str, Vec<String>>> = None;
    if let Some(paths) = req.non_secret_paths.as_deref() {
        let mut per_item: std::collections::BTreeMap<&str, Vec<String>> = bundle
            .spec
            .items
            .iter()
            .map(|item| (item.id.as_str(), Vec::new()))
            .collect();
        for path in paths {
            let path = path.trim();
            let target = path
                .split_once('.')
                .and_then(|(id, rest)| per_item.get_mut(id).map(|v| (v, rest)));
            match target {
                Some((list, rest)) if !rest.is_empty() => list.push(rest.to_string()),
                _ => {
                    return Err(ServerError::BadRequest(format!(
                        "Not-secret path '{}' does not name a bundle item value (expected \
                         <item>.<path>)",
                        path.escape_default()
                    )));
                }
            }
        }
        requested_item_overrides = Some(per_item);
    }

    // Edit: secret fields of committed manifest-app and Helm items left blank or omitted keep
    // their stored values (declared secrets and stored heuristic secrets alike).
    let mut stored_item_secrets = std::collections::BTreeMap::new();
    let mut validation_schema = bundle.spec.schema.clone();
    for item in &bundle.spec.items {
        let Some(stored) = read_stored_manifest_app_secret(
            &repo.workdir,
            &format!("{bundle_rel_dir}/{}", item.id),
        )
        .await
        else {
            continue;
        };
        if let Some(item_schema) = validation_schema
            .get_mut("properties")
            .and_then(|p| p.get_mut(&item.id))
        {
            // The item's secrets are found in its self-contained schema: a `$ref` into the
            // bundle's `$defs` must resolve (AST-3).
            let standalone = keel_core::standalone_subschema(&bundle.spec.schema, item_schema);
            let overrides = match &requested_item_overrides {
                Some(per_item) => per_item.get(item.id.as_str()).cloned().unwrap_or_default(),
                None => {
                    read_secret_overrides(&repo.workdir, &format!("{bundle_rel_dir}/{}", item.id))
                        .await
                }
            };
            let stored_heuristic =
                keel_core::stored_heuristic_secret_paths(&standalone, &stored.keys, &overrides);
            if let Some(item_values) = req.values.get_mut(&item.id) {
                *item_values =
                    drop_blank_stored_secret_values(&standalone, item_values, &stored.keys);
                *item_values =
                    keel_core::drop_blank_values_at_paths(item_values, &stored_heuristic);
            }
            let mut relaxed = relax_stored_secret_requirements_with(
                &standalone,
                &stored.valued_keys,
                &stored_heuristic,
            );
            keel_core::restore_standalone_keywords(&mut relaxed, item_schema);
            *item_schema = relaxed;
        }
        stored_item_secrets.insert(item.id.clone(), stored);
    }
    validate_schema_values_with_secrets(
        &validation_schema,
        &req.values,
        &keel_core::heuristic_secret_pointers(&validation_schema, &req.values),
    )?;

    // Every Flux Kustomization the bundle may create must be unique within the tenant.
    let mut reserved_names: Vec<String> = Vec::new();
    for item in &bundle.spec.items {
        // Items are resolved later; conservatively reserve the manifest-app names for all.
        for name in reserved_flux_kustomization_names(&format!("{instance_name}-{}", item.id), true)
        {
            if reserved_names.contains(&name) {
                return Err(ServerError::BadRequest(format!(
                    "Bundle '{}' items derive the same Flux Kustomization name '{name}' (item ids must not differ only by a '-config'/'-canary' suffix)",
                    bundle.metadata.id
                )));
            }
            reserved_names.push(name);
        }
    }
    ensure_flux_kustomization_names_unclaimed(
        &repo.workdir,
        &req.tenant,
        &bundle_rel_dir,
        &reserved_names,
    )
    .await?;
    if is_new_bundle {
        for item in &bundle.spec.items {
            ensure_no_derived_name_sibling(
                &repo.workdir,
                &req.tenant,
                &bundle_rel_dir,
                &format!("{instance_name}-{}", item.id),
                true,
            )
            .await?;
        }
    }

    let ordered_items = bundle.topological_order()?;
    let tenant_ns = tenant_namespace(&req.tenant)?;
    let timestamp = next_unique_branch_timestamp();
    let branch = build_preview_branch(&req.tenant, &action_label, timestamp)?;

    struct PreparedBundleItem {
        id: String,
        depends_on: Vec<String>,
        item_rel_dir: String,
        resource_name: String,
        values_yaml: String,
        primary_filename: String,
        primary_yaml: String,
        extra_files: Vec<(String, String)>,
        flux_kustomization_yaml: String,
        /// Additional files (relative to the item directory) registered in
        /// `tenants/kustomization.yaml` (a manifest app's `config-kustomization.yaml`, a Helm
        /// app's `secrets.enc.yaml`).
        extra_kustomizations: Vec<&'static str>,
        /// Item-relative files/directories of other variants removed on update.
        stale: Vec<&'static str>,
        resource_type: &'static str,
        /// The item's "not secret" overrides ([`keel_core::SECRET_OVERRIDES_FILE`]).
        non_secret_paths: Vec<String>,
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
    let mut secret_notices: Vec<keel_core::SecretNotice> = Vec::new();
    // Items deployed as manifest apps (no HelmRelease to `dependsOn`); filled in topological order.
    let mut manifest_app_items = std::collections::HashSet::new();
    // Cluster singleton items of this request by (catalog ID, kubeconfig Secret) -> item ID: the
    // committed-tree guard skips the bundle's own directory, so repeats within one request are
    // caught here.
    let mut singleton_items: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
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
        let ref_manifest = load_catalog_manifest_for_tenant(
            &state.repo_workdir,
            &item.catalog_ref,
            Some(kind_hint),
            &req.tenant,
        )
        .await?;

        // The item's self-contained schema: its bundle-schema property with the bundle's
        // `$defs`/`definitions` (local `$ref`s resolve against the bundle document, AST-3),
        // else the referenced catalog item's schema.
        let sub_schema_owned = bundle.item_schema(&item.id).unwrap_or_else(|| {
            match &ref_manifest {
                CatalogManifest::Application(a) => &a.spec.schema,
                CatalogManifest::InfraBlueprint(b) => &b.spec.schema,
                CatalogManifest::Bundle(b) => &b.spec.schema,
            }
            .clone()
        });
        let sub_schema = &sub_schema_owned;

        let mut item_values = if is_new_bundle {
            apply_schema_defaults(sub_schema, &item_values)
        } else {
            item_values
        };
        // Effective secret rules (AST-2): declared plus name-heuristic secrets, minus the
        // item's "not secret" overrides; blank stored secrets keep their stored values (also
        // when the bundle schema does not declare the item: catalog schema fallback).
        let item_overrides = effective_non_secret_paths(
            sub_schema,
            requested_item_overrides.as_ref().map(|per_item| {
                per_item
                    .get(item.id.as_str())
                    .map_or(&[][..], Vec::as_slice)
            }),
            read_secret_overrides(&repo.workdir, &item_rel_dir).await,
        )
        .map_err(|e| prefix_item_error(&item.id, e))?;
        let EffectiveSecrets {
            partitioned,
            notices: item_notices,
            non_secret_paths: item_non_secret_paths,
        } = partition_effective_secrets(
            sub_schema,
            &mut item_values,
            stored_item_secrets.get(&item.id),
            item_overrides,
            false,
        )
        .map_err(|e| prefix_item_error(&item.id, e))?;
        secret_notices.extend(item_notices.into_iter().map(|mut n| {
            n.path = format!("{}.{}", item.id, n.path);
            n
        }));
        // Owned: `ref_manifest` is consumed below; a manifest-app item checks its unset optional
        // secrets against it.
        let item_schema = sub_schema.clone();
        let values_yaml = render_helm_values_yaml(&partitioned.normal_values)?;
        let mut helm_item_tenant_resources: Vec<&'static str> = Vec::new();
        let mut bundle_labels = std::collections::BTreeMap::new();
        bundle_labels.insert("keel.io/bundle".to_string(), instance_name.clone());
        bundle_labels.insert("keel.io/bundle-item".to_string(), item.id.clone());
        // Which bundle catalog item the deployment came from (its instance name may differ):
        // `GET /bundles/{ns}/{name}/config` masks the item with that bundle's schema.
        bundle_labels.insert(
            "keel.io/bundle-catalog-id".to_string(),
            bundle.metadata.id.clone(),
        );

        let (
            primary_filename,
            primary_yaml,
            extra_files,
            health_check,
            manifest_app_kustomization,
            stale,
            resource_type,
        ) = match ref_manifest {
            CatalogManifest::Application(app) => {
                if item.canary && !app.spec.progressive_delivery.flagger_supported {
                    return Err(ServerError::BadRequest(format!(
                        "Catalog application '{}' does not support Flagger canary rollouts (flaggerSupported=false)",
                        app.metadata.id
                    )));
                }
                let item_kube_config = if let Some(cluster_item) = &item.target_cluster_from {
                    Some(RemoteKubeConfigRef::with_key(
                        format!("{instance_name}-{cluster_item}-kubeconfig"),
                        "kubeconfig",
                    ))
                } else {
                    remote_target_cluster(req.target_cluster.as_deref())
                        .map(|target| RemoteKubeConfigRef::with_key(target, "kubeconfig"))
                };
                if item_kube_config.is_none() && app.spec.placement.requires_remote_cluster() {
                    let hint =
                        management_twin_placement_hint(&state.repo_workdir, &app.metadata.id).await;
                    return Err(ServerError::BadRequest(format!(
                        "Bundle item '{}' references catalog application '{}' with spec.placement.scope: cluster (cluster-scoped resources); it requires targetClusterFrom or a remote targetCluster{hint}",
                        item.id, app.metadata.id
                    )));
                }
                // Operators that own cluster-wide objects run once per workload cluster.
                if let Some(kc) = &item_kube_config
                    && is_cluster_singleton_app(&state.repo_workdir, &app).await
                {
                    ensure_cluster_singleton_not_installed(
                        &repo.workdir,
                        &req.tenant,
                        &bundle_rel_dir,
                        &app.metadata.id,
                        &kc.secret_name,
                    )
                    .await?;
                    if let Some(first) = singleton_items.insert(
                        (app.metadata.id.clone(), kc.secret_name.clone()),
                        item.id.clone(),
                    ) {
                        return Err(ServerError::Conflict(format!(
                            "Bundle items '{first}' and '{}' both install catalog application \
                             '{}', which installs cluster-wide objects, on target cluster '{}'; \
                             install it once per cluster",
                            item.id, app.metadata.id, kc.secret_name
                        )));
                    }
                }
                // `spec.placement.namespace` defaults/pins the item's remote namespace.
                let item_target_namespace = app
                    .resolve_target_namespace(item.target_namespace.as_deref())
                    .map_err(|e| {
                        ServerError::BadRequest(format!("Bundle item '{}': {e}", item.id))
                    })?;
                if let Some(target_ns) = &item_target_namespace
                    && item_kube_config.is_none()
                    && target_ns != &tenant_ns
                {
                    return Err(ServerError::BadRequest(format!(
                        "Bundle item '{}' targetNamespace '{target_ns}' requires targetClusterFrom or remote targetCluster",
                        item.id
                    )));
                }
                if app.metadata.runtime == AppRuntime::Manifest {
                    manifest_app_items.insert(item.id.clone());
                    if app.spec.default_engine == Some(ManifestEngine::Pulumi) {
                        return Err(ServerError::BadRequest(format!(
                            "Bundle item '{}' references manifest app '{}' whose default engine is pulumi; bundles deploy manifest apps with the flux engine only",
                            item.id, app.metadata.id
                        )));
                    }
                    // Neither is wired for manifest apps: fail closed instead of ignoring them.
                    if item.canary {
                        return Err(ServerError::BadRequest(format!(
                            "Bundle item '{}' references manifest app '{}'; Flagger canary rollouts are only generated for Helm apps",
                            item.id, app.metadata.id
                        )));
                    }
                    if !item.bind_values.is_empty() {
                        return Err(ServerError::BadRequest(format!(
                            "Bundle item '{}' references manifest app '{}'; bindValues are only supported for Helm apps",
                            item.id, app.metadata.id
                        )));
                    }
                    let rendered = render_manifest_app(
                        &state,
                        &repo,
                        &app,
                        &partitioned.normal_values,
                        &item_rel_dir,
                    )
                    .await?;
                    ensure_rendered_manifest_placement(
                        &app.metadata.id,
                        &rendered,
                        item_kube_config.is_some(),
                    )?;
                    app.ensure_manifest_namespaces(&rendered).map_err(|e| {
                        ServerError::BadRequest(format!("Bundle item '{}': {e}", item.id))
                    })?;
                    let encrypted_secret = resolve_manifest_app_secret(
                        &state,
                        &req.tenant,
                        &tenant_ns,
                        &keel_kube::manifest_app_secrets_name(&item_resource_name),
                        &item_schema,
                        &partitioned.secret_field_paths,
                        &partitioned.secret_values,
                        stored_item_secrets.get(&item.id),
                    )
                    .await?;
                    let mut labels = bundle_labels.clone();
                    labels.insert("keel.io/runtime".to_string(), "manifest".to_string());
                    labels.insert("keel.io/engine".to_string(), "flux".to_string());
                    labels.insert("keel.io/catalog-id".to_string(), app.metadata.id.clone());
                    let files = build_flux_manifest_app_files(FluxManifestAppLayoutInput {
                        tenant: &req.tenant,
                        tenant_ns: &tenant_ns,
                        name: &item_resource_name,
                        app_dir: &item_rel_dir,
                        catalog_id: &app.metadata.id,
                        rendered_manifests: &rendered,
                        normal_values: &partitioned.normal_values,
                        encrypted_secret_yaml: encrypted_secret.as_deref(),
                        kube_config: item_kube_config,
                        target_namespace: item_target_namespace.clone(),
                        multi_namespaces: app.multi_namespace_placement(),
                        depends_on: item
                            .depends_on
                            .iter()
                            .map(|dep| FluxDependencyRef {
                                name: format!("{instance_name}-{dep}"),
                            })
                            .collect(),
                        extra_labels: labels,
                    })?;
                    let mut stale = vec![
                        "helmrelease.yaml",
                        "helmrelease-canary.yaml",
                        "helm-repository.yaml",
                        "canary.yaml",
                        "networkpolicy-canary.yaml",
                        "canary",
                        "canary-kustomization.yaml",
                    ];
                    stale.extend(files.stale.iter().copied());
                    (
                        "manifests.yaml".to_string(),
                        rendered,
                        files.files,
                        None,
                        Some(files.app_kustomization_yaml),
                        stale,
                        "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization",
                    )
                } else {
                    let helm_extras = prepare_helm_app_extras(
                        &state,
                        HelmAppExtrasInput {
                            repo: &repo,
                            tenant: &req.tenant,
                            tenant_ns: &tenant_ns,
                            name: &item_resource_name,
                            app_dir: &item_rel_dir,
                            app: &app,
                            partitioned: &partitioned,
                            stored_secret: stored_item_secrets.get(&item.id),
                            remote: item_kube_config.is_some(),
                            target_namespace: item_target_namespace.as_deref(),
                            canary: item.canary,
                        },
                    )
                    .await?;
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
                    hr_input.crds = app.spec.crd_policy;
                    hr_input.extra_labels = bundle_labels.clone();
                    // Identifies the item's catalog app for the once-per-cluster guard.
                    hr_input
                        .extra_labels
                        .insert("keel.io/catalog-id".to_string(), app.metadata.id.clone());
                    hr_input.kube_config = item_kube_config;
                    hr_input.target_namespace = item_target_namespace.clone();

                    for dep in &item.depends_on {
                        if item_kinds.get(dep.as_str()) == Some(&BundleItemKind::App) {
                            // A manifest-app dependency has no HelmRelease; the item
                            // Kustomization's `dependsOn` already orders it.
                            if manifest_app_items.contains(dep) {
                                continue;
                            }
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

                    helm_extras.apply_to(&mut hr_input);

                    let hr_yaml = synthesize_helm_release_yaml(&hr_input)?;
                    let primary_filename = if item.canary {
                        "helmrelease-canary.yaml".to_string()
                    } else {
                        "helmrelease.yaml".to_string()
                    };
                    let mut extras = helm_extras.files.clone();
                    let mut item_k_res = vec![primary_filename.clone()];
                    item_k_res.extend(helm_extras.resources.iter().map(|f| (*f).to_string()));
                    helm_item_tenant_resources = helm_extras.tenant_resources.clone();
                    if let Some(helm_repo_yaml) = helm_repository_yaml(
                        &app,
                        &req.tenant,
                        &tenant_ns,
                        &item_resource_name,
                        Some((&instance_name, &item.id)),
                    )? {
                        extras.push(("helm-repository.yaml".to_string(), helm_repo_yaml));
                        item_k_res.push("helm-repository.yaml".to_string());
                    }

                    if item.canary {
                        let mut canary_input = helm_app_canary_input(
                            &req.tenant,
                            &item_resource_name,
                            &app,
                            &partitioned.normal_values,
                        );
                        canary_input.extra_labels = bundle_labels.clone();
                        let canary_yaml = synthesize_flagger_canary_yaml(&canary_input)?;
                        let np_yaml = synthesize_canary_gateway_network_policy_yaml(
                            &req.tenant,
                            &item_resource_name,
                            canary_input.service_port,
                        )?;
                        if let Some(remote_kc) = &hr_input.kube_config {
                            // The Canary must run next to the HelmRelease's workload on the remote
                            // cluster (never the management cluster): own `<item>-canary`
                            // Kustomization with kubeConfig/targetNamespace, like single apps.
                            let remote_target_ns = hr_input
                                .target_namespace
                                .clone()
                                .unwrap_or_else(|| tenant_ns.clone());
                            extras.extend(build_remote_canary_files(
                                &req.tenant,
                                &item_resource_name,
                                &item_rel_dir,
                                remote_kc,
                                &remote_target_ns,
                                canary_yaml,
                                np_yaml,
                                &bundle_labels,
                            )?);
                            item_k_res.push("canary-kustomization.yaml".to_string());
                        } else {
                            extras.push(("canary.yaml".to_string(), canary_yaml));
                            item_k_res.push("canary.yaml".to_string());
                            extras.push(("networkpolicy-canary.yaml".to_string(), np_yaml));
                            item_k_res.push("networkpolicy-canary.yaml".to_string());
                        }
                    }

                    let item_k_doc = serde_json::json!({
                        "apiVersion": "kustomize.config.k8s.io/v1beta1",
                        "kind": "Kustomization",
                        "resources": item_k_res,
                    });
                    extras.push((
                        "kustomization.yaml".to_string(),
                        keel_core::yaml::to_string(&item_k_doc).map_err(|e| {
                            ServerError::Internal(format!(
                                "failed to serialize kustomization.yaml: {e}"
                            ))
                        })?,
                    ));

                    let mut stale = match (item.canary, hr_input.kube_config.is_some()) {
                        (true, true) => vec![
                            "helmrelease.yaml",
                            "canary.yaml",
                            "networkpolicy-canary.yaml",
                        ],
                        (true, false) => {
                            vec!["helmrelease.yaml", "canary", "canary-kustomization.yaml"]
                        }
                        (false, _) => vec![
                            "helmrelease-canary.yaml",
                            "canary.yaml",
                            "networkpolicy-canary.yaml",
                            "canary",
                            "canary-kustomization.yaml",
                        ],
                    };
                    stale.extend(["manifests.yaml", "namespace.yaml"]);
                    stale.extend(helm_extras.stale.iter().copied());
                    (
                        primary_filename,
                        hr_yaml,
                        extras,
                        Some(FluxHealthCheckRef {
                            api_version: "helm.toolkit.fluxcd.io/v2".to_string(),
                            kind: "HelmRelease".to_string(),
                            name: item_resource_name.clone(),
                            namespace: Some(tenant_ns.clone()),
                        }),
                        None,
                        stale,
                        "flux:helm.toolkit.fluxcd.io/v2:HelmRelease",
                    )
                }
            }
            CatalogManifest::InfraBlueprint(bp) => {
                // Bundle infra items pass only plaintext config to their Stack: a value treated
                // as secret by its key name would silently vanish, so fail closed instead.
                let heuristic: Vec<&str> = secret_notices
                    .iter()
                    .filter(|n| n.reason == keel_core::SecretNoticeReason::HeuristicSecret)
                    .filter_map(|n| n.path.strip_prefix(&format!("{}.", item.id)))
                    .collect();
                if !heuristic.is_empty() {
                    return Err(ServerError::BadRequest(format!(
                        "Bundle item '{}' (infrastructure blueprint '{}') has values whose key \
                         looks like a secret ({}), which bundle infrastructure items cannot \
                         store encrypted; mark them not secret with nonSecretPaths if they are \
                         not secrets",
                        item.id,
                        bp.metadata.id,
                        heuristic.join(", ")
                    )));
                }
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
                let stack_source = resolve_tenant_stack_source(&state, &req.tenant).await?;
                let mut stack_params = TenantStackParams::new(
                    &req.tenant,
                    &item_resource_name,
                    format!(
                        "organization/{pulumi_project}/{}-{item_resource_name}",
                        req.tenant
                    ),
                    tenant_stack_backend_url(&state, &req.tenant)?,
                    tenant_stack_git_source(&stack_source, &item_rel_dir)?,
                );
                stack_params
                    .config
                    .insert("keel:provider".to_string(), bp.metadata.provider.clone());
                // Where the item's kubeconfig Secret lives is bundle wiring, not a value: it is
                // applied after the item values below, so a schema default such as
                // canary-gateway-config's `kubeconfigSecretName: workload-dev-kubeconfig` cannot
                // point the stack at a Secret the bundle's cluster item never writes.
                let kubeconfig_wiring = if bp.spec.outputs.iter().any(|o| o == "kubeconfig")
                    || target_cluster_sources.contains(item.id.as_str())
                {
                    Some(format!("{item_resource_name}-kubeconfig"))
                } else if let Some(cluster_item) = &item.target_cluster_from {
                    Some(format!("{instance_name}-{cluster_item}-kubeconfig"))
                } else {
                    remote_target_cluster(req.target_cluster.as_deref()).map(str::to_string)
                };
                if bp.metadata.provider == "kind" {
                    stack_params.docker_host = state.docker_proxy_url.clone();
                }
                attach_tenant_stack_credentials(
                    &state,
                    &req.tenant,
                    &tenant_ns,
                    &mut stack_params.env_refs,
                )
                .await?;
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
                if let Some(secret_name) = kubeconfig_wiring {
                    stack_params
                        .config
                        .insert("kubeconfigSecretName".to_string(), secret_name);
                    stack_params
                        .config
                        .insert("kubeconfigSecretNamespace".to_string(), tenant_ns.clone());
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
                        keel_core::yaml::to_string(&item_k_doc).map_err(|e| {
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
                    Some(FluxHealthCheckRef {
                        api_version: "pulumi.com/v1".to_string(),
                        kind: "Stack".to_string(),
                        name: item_resource_name.clone(),
                        namespace: Some(tenant_ns.clone()),
                    }),
                    None,
                    Vec::new(),
                    "pulumi:pulumi.com/v1:Stack",
                )
            }
            CatalogManifest::Bundle(_) => {
                return Err(ServerError::BadRequest(format!(
                    "Nested CatalogBundle '{}' is not supported inside bundle items",
                    item.catalog_ref
                )));
            }
        };

        let extra_kustomizations = if manifest_app_kustomization.is_some() {
            vec![FLUX_MANIFEST_APP_KUSTOMIZATION_FILES[0]]
        } else {
            helm_item_tenant_resources
        };
        let flux_kustomization_yaml = if let Some(app_fk) = manifest_app_kustomization {
            app_fk
        } else {
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
            fk_input.health_checks = health_check.into_iter().collect();
            fk_input.extra_labels = bundle_labels;
            synthesize_flux_kustomization_yaml(&fk_input)?
        };

        prepared_items.push(PreparedBundleItem {
            non_secret_paths: item_non_secret_paths,
            id: item.id.clone(),
            depends_on: item.depends_on.clone(),
            item_rel_dir,
            resource_name: item_resource_name,
            values_yaml,
            primary_filename,
            primary_yaml,
            extra_files,
            flux_kustomization_yaml,
            extra_kustomizations,
            stale,
            resource_type,
        });
    }

    let (commit_sha, is_update) = {
        let workdir = repo.workdir.as_path();
        state
            .git_runner
            .checkout_new_branch(workdir, &branch)
            .await?;

        let target_dir = workdir.join(&bundle_rel_dir);
        let is_update = tokio::fs::try_exists(&target_dir).await.unwrap_or(false);
        create_dir_all_contained(workdir, &target_dir).await?;

        let mut staged_paths = Vec::new();
        let mut kustomize_additions = Vec::new();

        for item in &prepared_items {
            let item_dir = workdir.join(&item.item_rel_dir);
            create_dir_all_contained(workdir, &item_dir).await?;

            write_contained(workdir, &item_dir.join("values.yaml"), &item.values_yaml).await?;
            staged_paths.push(format!("{}/values.yaml", item.item_rel_dir));
            write_secret_overrides(
                workdir,
                &item.item_rel_dir,
                &item.non_secret_paths,
                &mut staged_paths,
            )
            .await?;

            write_contained(
                workdir,
                &item_dir.join(&item.primary_filename),
                &item.primary_yaml,
            )
            .await?;
            staged_paths.push(format!("{}/{}", item.item_rel_dir, item.primary_filename));

            remove_stale_entries(workdir, &item.item_rel_dir, &item.stale, &mut staged_paths)
                .await?;
            write_relative_files(
                workdir,
                &item.item_rel_dir,
                &item.extra_files,
                &mut staged_paths,
            )
            .await?;

            write_contained(
                workdir,
                &item_dir.join("flux-kustomization.yaml"),
                &item.flux_kustomization_yaml,
            )
            .await?;
            staged_paths.push(format!("{}/flux-kustomization.yaml", item.item_rel_dir));
            for fk_file in &item.extra_kustomizations {
                kustomize_additions.push(format!(
                    "{}/bundles/{instance_name}/{}/{fk_file}",
                    req.tenant, item.id
                ));
            }
            kustomize_additions.push(format!(
                "{}/bundles/{instance_name}/{}/flux-kustomization.yaml",
                req.tenant, item.id
            ));
        }

        let index = TenantIndex::new(&repo, &req.tenant);
        let mut resources: Vec<String> = index.load(workdir).await;
        for res in kustomize_additions {
            if !resources.contains(&res) {
                resources.push(res);
            }
        }
        let bundle_prefix = format!("{}/bundles/{instance_name}/", req.tenant);
        resources.retain(|r| {
            !r.starts_with(&bundle_prefix) || workdir.join("tenants").join(r).is_file()
        });
        staged_paths.push(index.store(workdir, &resources).await?);

        let commit_msg = format!("feat({}): propose bundle {}", req.tenant, instance_name);
        let sha = stage_commit_push_in(&state, workdir, &auth, &branch, &commit_msg, &staged_paths)
            .await?;
        (sha, is_update)
    };
    drop(repo_guard);

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
    let OpenedProposal {
        pr,
        preview_key,
        preview_params,
    } = open_proposal_pr(
        &state,
        &auth,
        &repo,
        &req,
        PushedProposal {
            branch: &branch,
            timestamp,
            action_label: &action_label,
            rel_dir: &bundle_rel_dir,
            instance_name: &instance_name,
            commit_sha: &commit_sha,
            verb: if is_update {
                "update bundle"
            } else {
                "deploy bundle"
            },
            summary: &summary,
        },
    )
    .await?;

    // Bundles span several Stacks/HelmReleases with `dependsOn` ordering; a single PKO preview
    // cannot evaluate them, so the bundle preview Stack is synthesized but never applied (and
    // never carries control-plane admin S3 keys, OPEN-9).
    let preview_stack = synthesize_ephemeral_preview(&preview_params)?;

    let resources: Vec<ResourceDiff> = prepared_items
        .iter()
        .map(|item| {
            let rtype = item.resource_type;
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
                    preview_key, req.tenant, rtype, item.resource_name
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
    secret_notices.sort();
    body.secret_notices = secret_notices;
    Ok((status, Json(body)))
}

/// Names the bundle item in a client error about its values.
fn prefix_item_error(item_id: &str, err: ServerError) -> ServerError {
    match err {
        ServerError::BadRequest(msg) => {
            ServerError::BadRequest(format!("Bundle item '{item_id}': {msg}"))
        }
        other => other,
    }
}
