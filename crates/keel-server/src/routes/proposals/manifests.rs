//! Manifest layout of proposed apps: Flux manifest-app files, Helm app extras, canaries and
//! file staging helpers.

use std::path::Path;

use keel_core::{
    CatalogApplication, gateway_route_hostnames, render_manifest_template,
    verify_rendered_manifest_structure,
};
use keel_kube::{
    FlaggerCanarySpecInput, FluxDependencyRef, FluxKustomizationSpecInput, HelmReleaseSpecInput,
    HelmValuesSecretRef, RemoteKubeConfigRef, synthesize_flux_kustomization_yaml,
    synthesize_remote_target_namespace, synthesize_remote_target_namespace_yaml,
};
use serde_json::{Value, json};

use crate::error::ServerError;
use crate::proposal_repo::ScopeRepo;
use crate::routes::{
    create_dir_all_contained, read_contained_to_string, safe_relative_path, write_contained,
};
use crate::state::AppState;

use super::guards::ensure_tenant_route_hostnames_allowed;
use super::secrets::{StoredManifestAppSecret, resolve_tenant_app_secret};

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
    keel_core::yaml::to_string(&cm_doc)
        .map_err(|e| ServerError::Internal(format!("failed to serialize values ConfigMap: {e}")))
}

/// Serializes a `kustomize.config.k8s.io/v1beta1` `kustomization.yaml` listing `resources`.
fn kustomize_resources_yaml(resources: &[&str]) -> Result<String, ServerError> {
    keel_core::yaml::to_string(&json!({
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": resources,
    }))
    .map_err(|e| ServerError::Internal(format!("failed to serialize kustomization.yaml: {e}")))
}

/// Inputs for [`build_flux_manifest_app_files`].
pub(super) struct FluxManifestAppLayoutInput<'a> {
    pub(super) tenant: &'a str,
    pub(super) tenant_ns: &'a str,
    /// App / app `Kustomization` name (`<instance>` or `<bundle>-<item>`).
    pub(super) name: &'a str,
    /// Repo-relative app directory (`tenants/<t>/apps/<name>` or a bundle item directory).
    pub(super) app_dir: &'a str,
    pub(super) catalog_id: &'a str,
    pub(super) rendered_manifests: &'a str,
    pub(super) normal_values: &'a Value,
    /// SOPS-encrypted `<name>-secrets` `Secret` manifest, when the app has secret fields.
    pub(super) encrypted_secret_yaml: Option<&'a str>,
    pub(super) kube_config: Option<RemoteKubeConfigRef>,
    pub(super) target_namespace: Option<String>,
    /// Namespaces of a multi-namespace app ([`CatalogApplication::multi_namespace_placement`]);
    /// empty for single-namespace apps.
    pub(super) multi_namespaces: Vec<String>,
    pub(super) depends_on: Vec<FluxDependencyRef>,
    pub(super) extra_labels: std::collections::BTreeMap<String, String>,
}

/// Committed file layout of a Flux-engine manifest app, relative to its directory.
#[derive(Debug)]
pub(super) struct FluxManifestAppFiles {
    /// `(relative path, contents)`, parents before children.
    pub(super) files: Vec<(String, String)>,
    /// Relative paths (files or directories) to remove when present: artifacts of a previous
    /// local/remote or secret/no-secret variant.
    pub(super) stale: Vec<&'static str>,
    /// The app `Kustomization` (`flux-kustomization.yaml`) YAML.
    pub(super) app_kustomization_yaml: String,
}

/// File names of the two Flux `Kustomization`s committed for a Flux-engine manifest app.
pub(super) const FLUX_MANIFEST_APP_KUSTOMIZATION_FILES: [&str; 2] =
    ["config-kustomization.yaml", "flux-kustomization.yaml"];

/// Builds the committed layout of a Flux-engine manifest app (`SDD.md` §5.1):
///
/// ```text
/// <app_dir>/config-kustomization.yaml   Kustomization <name>-config (management cluster, SOPS)
/// <app_dir>/config/kustomization.yaml   values-configmap.yaml [+ secrets.enc.yaml]
/// <app_dir>/flux-kustomization.yaml     Kustomization <name> (dependsOn <name>-config)
/// <app_dir>/kustomization.yaml          [namespace.yaml (remote only)] + manifests.yaml
/// ```
///
/// Substitution sources live on the management cluster (where Flux reads `substituteFrom`), so a
/// remote app never ships the `<name>-secrets` Secret to the workload cluster; only the rendered
/// manifests (with substituted values) are applied there.
pub(super) fn build_flux_manifest_app_files(
    input: FluxManifestAppLayoutInput<'_>,
) -> Result<FluxManifestAppFiles, ServerError> {
    let remote = input.kube_config.is_some();
    // A multi-namespace app (`spec.placement.additionalNamespaces`, remote only) names every
    // object's namespace itself: no `targetNamespace` (it would collapse them into one), and all
    // of its namespaces are created first.
    let multi_namespace = remote && !input.multi_namespaces.is_empty();
    // Remote apps always pin a target namespace (default: the tenant namespace) and create it
    // first, like remote canaries: nothing else creates it on the workload cluster.
    let target_namespace = if multi_namespace {
        None
    } else if remote {
        Some(
            input
                .target_namespace
                .clone()
                .unwrap_or_else(|| input.tenant_ns.to_string()),
        )
    } else {
        input.target_namespace.clone()
    };
    let has_secrets = input.encrypted_secret_yaml.is_some();
    let (config_k_yaml, app_k_yaml) =
        keel_kube::synthesize_flux_manifest_app_kustomizations(&keel_kube::FluxManifestAppInput {
            tenant_id: input.tenant.to_string(),
            name: input.name.to_string(),
            source_name: "keel-gitops".to_string(),
            app_dir: input.app_dir.to_string(),
            kube_config: input.kube_config.clone(),
            target_namespace: target_namespace.clone(),
            has_secrets,
            depends_on: input.depends_on,
            extra_labels: input.extra_labels,
        })?;

    let mut files = Vec::new();
    let mut app_resources = Vec::new();
    let remote_namespaces: Vec<&str> = if multi_namespace {
        input.multi_namespaces.iter().map(String::as_str).collect()
    } else if remote {
        target_namespace.as_deref().into_iter().collect()
    } else {
        Vec::new()
    };
    if !remote_namespaces.is_empty() {
        let namespaces = remote_namespaces
            .iter()
            .map(|ns| synthesize_remote_target_namespace(input.tenant, ns))
            .collect::<Result<Vec<_>, _>>()?;
        let namespace_yaml =
            keel_core::to_yaml_documents(namespaces).map_err(keel_core::CoreError::from)?;
        files.push(("namespace.yaml".to_string(), namespace_yaml));
        app_resources.push("namespace.yaml");
    }
    files.push((
        "manifests.yaml".to_string(),
        input.rendered_manifests.to_string(),
    ));
    app_resources.push("manifests.yaml");
    files.push((
        "kustomization.yaml".to_string(),
        kustomize_resources_yaml(&app_resources)?,
    ));

    let config_dir = keel_kube::MANIFEST_APP_CONFIG_DIR;
    let mut config_resources = vec!["values-configmap.yaml"];
    files.push((
        format!("{config_dir}/values-configmap.yaml"),
        build_values_configmap_manifest_yaml(
            &keel_kube::manifest_app_values_configmap_name(input.name),
            input.tenant_ns,
            input.tenant,
            input.catalog_id,
            input.normal_values,
        )?,
    ));
    if let Some(enc) = input.encrypted_secret_yaml {
        files.push((format!("{config_dir}/secrets.enc.yaml"), enc.to_string()));
        config_resources.push("secrets.enc.yaml");
    }
    files.push((
        format!("{config_dir}/kustomization.yaml"),
        kustomize_resources_yaml(&config_resources)?,
    ));
    files.push((
        FLUX_MANIFEST_APP_KUSTOMIZATION_FILES[0].to_string(),
        config_k_yaml,
    ));
    files.push((
        FLUX_MANIFEST_APP_KUSTOMIZATION_FILES[1].to_string(),
        app_k_yaml.clone(),
    ));

    let mut stale = Vec::new();
    if !remote {
        stale.push("namespace.yaml");
    }
    if !has_secrets {
        stale.push("config/secrets.enc.yaml");
    }
    Ok(FluxManifestAppFiles {
        files,
        stale,
        app_kustomization_yaml: app_k_yaml,
    })
}

/// Removes each existing `names` entry (file or directory) under `rel_dir` and stages the removal.
pub(super) async fn remove_stale_entries(
    repo_workdir: &Path,
    rel_dir: &str,
    names: &[&str],
    staged_paths: &mut Vec<String>,
) -> Result<(), ServerError> {
    let dir = repo_workdir.join(rel_dir);
    for name in names {
        let path = dir.join(name);
        let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
        staged_paths.push(format!("{rel_dir}/{name}"));
    }
    Ok(())
}

/// Writes `files` (paths relative to `rel_dir`, creating parent directories) and stages them.
pub(super) async fn write_relative_files(
    repo_workdir: &Path,
    rel_dir: &str,
    files: &[(String, String)],
    staged_paths: &mut Vec<String>,
) -> Result<(), ServerError> {
    let dir = repo_workdir.join(rel_dir);
    for (name, contents) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            create_dir_all_contained(repo_workdir, parent).await?;
        }
        write_contained(repo_workdir, &path, contents).await?;
        staged_paths.push(format!("{rel_dir}/{name}"));
    }
    Ok(())
}

/// Remote canary layout (`<dir>/canary/` + `canary-kustomization.yaml`, Kustomization
/// `<name>-canary` with `kubeConfig`/`targetNamespace`), relative to the app directory. The
/// Flagger `Canary` and its NetworkPolicy must run next to the HelmRelease's workload on the
/// remote cluster; the namespace is shipped first so they never race Helm's `createNamespace`.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_remote_canary_files(
    tenant: &str,
    name: &str,
    app_dir: &str,
    kube_config: &RemoteKubeConfigRef,
    target_namespace: &str,
    canary_yaml: String,
    network_policy_yaml: String,
    extra_labels: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, ServerError> {
    let mut fk_input = FluxKustomizationSpecInput::new(
        tenant,
        format!("{name}-canary"),
        "keel-gitops",
        format!("./{app_dir}/canary"),
    );
    fk_input.kube_config = Some(kube_config.clone());
    fk_input.target_namespace = Some(target_namespace.to_string());
    fk_input.extra_labels = extra_labels.clone();
    Ok(vec![
        (
            "canary/namespace.yaml".to_string(),
            synthesize_remote_target_namespace_yaml(tenant, target_namespace)?,
        ),
        ("canary/canary.yaml".to_string(), canary_yaml),
        (
            "canary/networkpolicy-canary.yaml".to_string(),
            network_policy_yaml,
        ),
        (
            "canary/kustomization.yaml".to_string(),
            kustomize_resources_yaml(&[
                "namespace.yaml",
                "canary.yaml",
                "networkpolicy-canary.yaml",
            ])?,
        ),
        (
            "canary-kustomization.yaml".to_string(),
            synthesize_flux_kustomization_yaml(&fk_input)?,
        ),
    ])
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

/// Renders a `runtime: manifest` catalog app's template with `normal_values` for `rel_dir`,
/// failing closed (before anything is committed) when rendered values add documents or change
/// kinds, or when a route claims a platform host or another app's hostname.
pub(super) async fn render_manifest_app(
    state: &AppState,
    repo: &ScopeRepo,
    app: &CatalogApplication,
    normal_values: &Value,
    rel_dir: &str,
) -> Result<String, ServerError> {
    let raw_manifests = load_catalog_app_raw_manifests(&state.repo_workdir, app).await;
    let mut rendered = render_manifest_template(&raw_manifests, normal_values)?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    let rendered_docs = verify_rendered_manifest_structure(&raw_manifests, &rendered)?;
    // Tenant routes attach to the shared `keel-ingress` listeners: never let them claim a
    // platform host (IdP issuer, keel-server, Forgejo) or another workload's hostname.
    let route_hostnames = gateway_route_hostnames(&rendered_docs);
    ensure_tenant_route_hostnames_allowed(state, repo, rel_dir, &route_hostnames).await?;
    Ok(rendered)
}

/// File (relative to the app directory) of the Keel-generated `NetworkPolicy` of a Helm app.
const HELM_APP_NETWORK_POLICY_FILE: &str = "networkpolicy.yaml";

/// File (relative to the app directory) of a Helm app's SOPS-encrypted `<name>-secrets` `Secret`.
pub(super) const HELM_APP_SECRETS_FILE: &str = "secrets.enc.yaml";

/// Inputs for [`prepare_helm_app_extras`].
pub(super) struct HelmAppExtrasInput<'a> {
    /// Repository holding the tenant's content.
    pub(super) repo: &'a ScopeRepo,
    pub(super) tenant: &'a str,
    pub(super) tenant_ns: &'a str,
    /// `HelmRelease` / instance name (`<instance>` or `<bundle>-<item>`).
    pub(super) name: &'a str,
    /// Repo-relative app directory.
    pub(super) app_dir: &'a str,
    pub(super) app: &'a CatalogApplication,
    pub(super) partitioned: &'a keel_core::PartitionedValues,
    pub(super) stored_secret: Option<&'a StoredManifestAppSecret>,
    /// `true` when the release targets a remote workload cluster.
    pub(super) remote: bool,
    /// `spec.targetNamespace` of the `HelmRelease`, when set.
    pub(super) target_namespace: Option<&'a str>,
}

/// Keel-generated companions of a Helm app (`runtime: helm`, `SDD.md` §5.1):
///
/// ```text
/// <app_dir>/secrets.enc.yaml     Secret <name>-secrets: one values fragment per secret field
/// <app_dir>/networkpolicy.yaml    NetworkPolicy allow-<name>-app (spec.networkPolicy, local only)
/// ```
///
/// The `Secret` is listed directly in the tenant index ([`crate::proposal_repo::TenantIndex`]):
/// the tenant repository's `tenants/<id>/kustomization.yaml`, applied on the management cluster by
/// the tenant's `keel-tenant-root` Kustomization as `tenant-<id>-sa` (SOPS decryption with
/// `tenant-<id>/sops-age`), also for bundle items. Only the Local Git provider (D8) lists it in
/// the platform `tenants/kustomization.yaml`, applied by the root `keel-tenants` Kustomization.
pub(super) struct HelmAppExtras {
    /// `(relative path, contents)`, parents before children.
    pub(super) files: Vec<(String, String)>,
    /// Files applied with the `HelmRelease` (tenants / bundle item `kustomization.yaml`).
    pub(super) resources: Vec<&'static str>,
    /// Files registered directly in the tenant index (`keel-tenant-root`; Local Git provider:
    /// `tenants/kustomization.yaml`, applied by `keel-tenants`).
    pub(super) tenant_resources: Vec<&'static str>,
    /// Relative paths of other variants (`secrets.enc.yaml` once no secret is stored, the
    /// `NetworkPolicy` of a remote release) removed when present.
    pub(super) stale: Vec<&'static str>,
    /// `spec.valuesFrom` entries (one per stored secret fragment).
    values_from: Vec<HelmValuesSecretRef>,
    /// Hostnames of the chart's enabled Gateway routes (`spec.gatewayRoutes`).
    route_hostnames: Vec<String>,
    /// Non-secret values for `HelmRelease.spec.values` (`spec.lockedValues` merged, route
    /// `parentRefs` pinned).
    release_values: Value,
}

impl HelmAppExtras {
    /// Applies the `HelmRelease` side: values, secret `valuesFrom` refs (replacing stale refs to
    /// the app's own secret) and the route hostnames annotation.
    pub(super) fn apply_to(&self, hr_input: &mut HelmReleaseSpecInput) {
        let secret_name = keel_kube::manifest_app_secrets_name(&hr_input.release_name);
        hr_input
            .values_from_secrets
            .retain(|r| r.name != secret_name);
        hr_input
            .values_from_secrets
            .extend(self.values_from.iter().cloned());
        hr_input.values = self
            .release_values
            .as_object()
            .filter(|o| !o.is_empty())
            .map(|_| self.release_values.clone());
        if !self.route_hostnames.is_empty() {
            hr_input.annotations.insert(
                keel_kube::ROUTE_HOSTNAMES_ANNOTATION.to_string(),
                self.route_hostnames.join(","),
            );
        }
    }
}

/// Fails closed (400, before anything is committed) on Helm values that would escape the catalog
/// app's guarantees:
///
/// * apps with `spec.lockedValues` / `spec.gatewayRoutes` ([`CatalogApplication::strict_values`])
///   only accept values declared by their schema (no second chart route, `extraDeploy`, sidecars
///   or ServiceAccount overrides);
/// * no value may name the tenant reconciler ServiceAccount (`tenant-<id>-sa`, namespace admin
///   with an automounted token), which would hand its token to the chart's pods;
/// * on Edit, `spec.immutableValues` (PersistentVolumeClaim sizes, storage classes, claim names)
///   must equal the committed `values.yaml`: Kubernetes rejects those changes, so the Helm
///   upgrade would fail forever.
async fn ensure_helm_values_within_catalog(
    input: &HelmAppExtrasInput<'_>,
) -> Result<(), ServerError> {
    let app = input.app;
    if app.strict_values() {
        let mut undeclared =
            keel_core::undeclared_value_paths(&app.spec.schema, &input.partitioned.normal_values);
        undeclared.extend(keel_core::undeclared_value_paths(
            &app.spec.schema,
            &input.partitioned.secret_values,
        ));
        if !undeclared.is_empty() {
            undeclared.sort();
            undeclared.dedup();
            return Err(ServerError::BadRequest(format!(
                "Values not declared by the '{}' catalog schema are not accepted (Keel fixes the \
                 rest of the chart configuration): {}",
                app.metadata.id,
                undeclared.join(", ")
            )));
        }
    }

    let reconciler_sa = keel_kube::default_tenant_service_account_name(input.tenant)?;
    fn names_string(value: &Value, needle: &str) -> bool {
        match value {
            Value::String(s) => s.trim() == needle,
            Value::Array(items) => items.iter().any(|v| names_string(v, needle)),
            Value::Object(map) => map.values().any(|v| names_string(v, needle)),
            _ => false,
        }
    }
    if names_string(&input.partitioned.normal_values, &reconciler_sa) {
        return Err(ServerError::BadRequest(format!(
            "Values may not reference the tenant reconciler ServiceAccount '{reconciler_sa}'"
        )));
    }

    if !app.spec.immutable_values.is_empty() {
        let committed_path = input.repo.workdir.join(input.app_dir).join("values.yaml");
        if let Ok(raw) = tokio::fs::read_to_string(&committed_path).await {
            let committed: Value = noyalib::from_str(&raw).unwrap_or(Value::Null);
            let changed = keel_core::changed_immutable_values(
                &app.spec.immutable_values,
                &committed,
                &input.partitioned.normal_values,
            );
            if !changed.is_empty() {
                return Err(ServerError::BadRequest(format!(
                    "{} cannot be changed after install (PersistentVolumeClaim settings are \
                     immutable or need a manual volume expansion); keep the installed values",
                    changed.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Builds the [`HelmAppExtras`] of a Helm app, failing closed (before anything is committed)
/// on unsafe route hostnames or unsupported secret fields.
///
/// Secret fields are committed as one SOPS-encrypted `Secret` key per field (a Helm values
/// fragment) and merged by helm-controller through `spec.valuesFrom` (no `targetPath`, so values
/// keep their exact content); the plaintext never reaches Git or the `HelmRelease` object. A
/// secret field left blank on Edit keeps its stored value.
pub(super) async fn prepare_helm_app_extras(
    state: &AppState,
    input: HelmAppExtrasInput<'_>,
) -> Result<HelmAppExtras, ServerError> {
    ensure_helm_values_within_catalog(&input).await?;
    let mut release_values = input.partitioned.normal_values.clone();
    if let Some(locked) = &input.app.spec.locked_values {
        keel_core::merge_json_values(&mut release_values, locked);
    }
    let route_hostnames =
        keel_core::resolve_helm_gateway_routes(&input.app.spec.gateway_routes, &mut release_values)
            .map_err(|e| ServerError::BadRequest(e.to_string()))?;
    if input.remote && !route_hostnames.is_empty() {
        // The route would be pinned to the management cluster's `keel-ingress` Gateway, which
        // does not exist on a workload cluster (which may not even serve the Gateway API CRDs).
        return Err(ServerError::BadRequest(format!(
            "'{}' routes through the shared keel-ingress Gateway of the management cluster; \
             disable the route ({}) when installing into a remote workload cluster and expose \
             the app through that cluster's own ingress",
            input.app.metadata.id,
            input
                .app
                .spec
                .gateway_routes
                .iter()
                .filter_map(|r| r.enabled_value.as_deref())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    ensure_tenant_route_hostnames_allowed(state, input.repo, input.app_dir, &route_hostnames)
        .await?;

    let mut extras = HelmAppExtras {
        files: Vec::new(),
        resources: Vec::new(),
        tenant_resources: Vec::new(),
        stale: Vec::new(),
        values_from: Vec::new(),
        route_hostnames,
        release_values,
    };

    let fragments = keel_core::helm_secret_value_fragments(
        &input.partitioned.secret_values,
        &input.partitioned.secret_field_paths,
    )
    .map_err(|e| ServerError::BadRequest(e.to_string()))?;
    let secret_name = keel_kube::manifest_app_secrets_name(input.name);
    let resolved = resolve_tenant_app_secret(
        state,
        input.tenant,
        input.tenant_ns,
        &secret_name,
        &input.partitioned.secret_field_paths,
        fragments,
        // Helm apps leave unset secrets to the chart defaults.
        &[],
        input.stored_secret,
    )
    .await?;
    match resolved {
        Some((ciphertext, keys)) => {
            extras
                .files
                .push((HELM_APP_SECRETS_FILE.to_string(), ciphertext));
            extras.tenant_resources.push(HELM_APP_SECRETS_FILE);
            extras.values_from = keys
                .into_iter()
                .map(|key| HelmValuesSecretRef {
                    name: secret_name.clone(),
                    values_key: Some(key),
                    target_path: None,
                    optional: None,
                })
                .collect();
        }
        None => extras.stale.push(HELM_APP_SECRETS_FILE),
    }

    // Tenant namespaces on the management cluster are `default-deny-all`. Remote workload
    // clusters manage their own network policy (the file would land on the management cluster).
    match (&input.app.spec.network_policy, input.remote) {
        (Some(np), false) => {
            let release = match input.target_namespace {
                Some(ns) => format!("{ns}-{}", input.name),
                None => input.name.to_string(),
            };
            extras.files.push((
                HELM_APP_NETWORK_POLICY_FILE.to_string(),
                keel_kube::synthesize_helm_app_network_policy_yaml(
                    input.tenant,
                    input.name,
                    &release,
                    np,
                )?,
            ));
            extras.resources.push(HELM_APP_NETWORK_POLICY_FILE);
        }
        _ => extras.stale.push(HELM_APP_NETWORK_POLICY_FILE),
    }
    Ok(extras)
}

/// The Flux `HelmRepository` (`<name>-repo` in `tenant_ns`) of a Helm app's upstream chart
/// repository (`type: oci` for `oci://` URLs), or `None` when the catalog entry has no
/// `repoUrl`. `bundle_item` is the `(bundle, item)` pair labelling a bundle item.
pub(super) fn helm_repository_yaml(
    app: &CatalogApplication,
    tenant: &str,
    tenant_ns: &str,
    name: &str,
    bundle_item: Option<(&str, &str)>,
) -> Result<Option<String>, ServerError> {
    let repo_url = app.spec.upstream.repo_url.trim();
    if repo_url.is_empty() {
        return Ok(None);
    }
    let mut repo_spec = json!({
        "interval": "5m",
        "url": repo_url,
    });
    if repo_url.starts_with("oci://")
        && let Some(obj) = repo_spec.as_object_mut()
    {
        obj.insert("type".to_string(), json!("oci"));
    }
    let mut labels = serde_json::Map::new();
    labels.insert("app.kubernetes.io/managed-by".to_string(), json!("keel"));
    labels.insert("keel.io/tenant".to_string(), json!(tenant));
    if let Some((bundle, item)) = bundle_item {
        labels.insert("keel.io/bundle".to_string(), json!(bundle));
        labels.insert("keel.io/bundle-item".to_string(), json!(item));
    }
    let doc = json!({
        "apiVersion": "source.toolkit.fluxcd.io/v1",
        "kind": "HelmRepository",
        "metadata": {
            "name": format!("{name}-repo"),
            "namespace": tenant_ns,
            "labels": Value::Object(labels),
        },
        "spec": repo_spec,
    });
    keel_core::yaml::to_string(&doc).map(Some).map_err(|e| {
        ServerError::Internal(format!("failed to serialize helm-repository.yaml: {e}"))
    })
}

/// Flagger `Canary` input of a Helm app: the service port from the `service.port` value (else
/// 9898 for the podinfo chart, 80 otherwise) and the `ingressHost` value as its host.
pub(super) fn helm_app_canary_input(
    tenant: &str,
    name: &str,
    app: &CatalogApplication,
    normal_values: &Value,
) -> FlaggerCanarySpecInput {
    let mut canary_input = FlaggerCanarySpecInput::new(tenant, name);
    let chart_name = app
        .spec
        .upstream
        .chart
        .as_deref()
        .unwrap_or(&app.metadata.id);
    canary_input.service_port = normal_values
        .get("service")
        .and_then(|s| s.get("port"))
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)
        .unwrap_or(if chart_name == "podinfo" { 9898 } else { 80 });
    if let Some(host) = normal_values
        .get("ingressHost")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        canary_input.hosts.push(host.to_string());
    }
    canary_input
}

/// `spec.dependsOn` names of the committed Flux `Kustomization` at `rel_path` (minus the
/// manifest-app `-config` dependency, which the layout re-adds); empty when absent/unparsable.
pub(super) async fn read_flux_depends_on(
    repo_workdir: &Path,
    rel_path: &str,
) -> Vec<FluxDependencyRef> {
    let Ok(raw) = read_contained_to_string(repo_workdir, &repo_workdir.join(rel_path)).await else {
        return Vec::new();
    };
    let Ok(doc) = noyalib::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let own_config = doc
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .map(keel_kube::manifest_app_config_kustomization_name);
    doc.pointer("/spec/dependsOn")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| d.get("name").and_then(Value::as_str))
        .filter(|name| own_config.as_deref() != Some(*name))
        .map(|name| FluxDependencyRef {
            name: name.to_string(),
        })
        .collect()
}
