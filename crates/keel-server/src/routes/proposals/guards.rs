//! Pre-commit guards of a proposal: placement, derived Flux names, route hostnames and
//! tenant repository initialisation.

use std::path::Path;

use keel_core::{
    AppRuntime, CatalogApplication, gateway_route_hostnames, platform_reserved_hostnames,
    validate_dns_label, validate_k8s_manifests_yaml, validate_route_hostnames,
};
use serde_json::Value;

use crate::error::ServerError;
use crate::proposal_repo::{ScopeRepo, TenantIndex};
use crate::routes::{RepoReader, resolve_tenant_resource_dir};
use crate::state::AppState;

/// Remote kubeconfig Secret reference of a request `targetCluster`, or `None` when the install
/// targets the management cluster (absent, blank, or the `local` alias).
pub(super) fn remote_target_cluster(target_cluster: Option<&str>) -> Option<&str> {
    target_cluster
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "local")
}

/// Rejects a rendered manifest app that declares well-known cluster-scoped kinds when it targets
/// the management cluster (`remote == false`). Tenant objects there impersonate `tenant-<id>-sa`
/// (namespace-scoped RBAC), so they would only fail at reconcile; fail early with a clear `400`.
pub(super) fn ensure_rendered_manifest_placement(
    app_id: &str,
    rendered: &str,
    remote: bool,
) -> Result<(), ServerError> {
    if remote {
        return Ok(());
    }
    let kinds = keel_core::manifest_cluster_scoped_kinds(rendered);
    if kinds.is_empty() {
        return Ok(());
    }
    Err(ServerError::BadRequest(format!(
        "Manifest app '{app_id}' declares cluster-scoped kinds ({}) that cannot be installed into a tenant namespace of the management cluster; declare spec.placement.scope: cluster and select a remote workload targetCluster",
        kinds.join(", ")
    )))
}

/// Rejects route hostnames already claimed by another committed app (any tenant: a manifest app's
/// rendered routes, a Helm app's `keel.io/route-hostnames` annotation or the hosts of a
/// management-cluster Flagger `Canary`, see [`claims_route_hostnames`]) under
/// `tenants/`. `own_rel_dir` (the instance being created/edited) is skipped. Reads the checked-out
/// default branch; open PRs are not considered, the Kyverno `restrict-tenant-route-hostnames`
/// policy still guards platform hosts at admission.
async fn ensure_route_hostnames_unclaimed(
    repo_workdir: &Path,
    own_rel_dir: &str,
    route_hostnames: &[String],
) -> Result<(), ServerError> {
    let tenants_root = repo_workdir.join("tenants");
    let own_dir = repo_workdir.join(own_rel_dir);
    let mut stack = vec![tenants_root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                if path != own_dir {
                    stack.push(path);
                }
                continue;
            }
            let file_name = entry.file_name();
            let remote_canary = || {
                path.parent()
                    .and_then(Path::parent)
                    .is_some_and(|app_dir| app_dir.join(REMOTE_CANARY_KUSTOMIZATION).is_file())
            };
            if !file_type.is_file()
                || !claims_route_hostnames(&file_name.to_string_lossy(), remote_canary)
            {
                continue;
            }
            let Ok(contents) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let claimed = claimed_route_hostnames(&file_name.to_string_lossy(), &contents);
            if let Some(host) = conflicting_hostname(route_hostnames, &claimed) {
                let owner = path
                    .parent()
                    .and_then(|p| p.strip_prefix(repo_workdir).ok())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                return Err(ServerError::Conflict(format!(
                    "Route hostname '{host}' is already claimed by '{owner}'; choose another hostname"
                )));
            }
        }
    }
    Ok(())
}

/// Flux `Kustomization` names an instance (or bundle item) named `name` may create in its tenant:
/// the app `Kustomization` (`<name>`), a manifest app's substitution sources (`<name>-config`,
/// only when `manifest_config` — Helm apps never create one) and a remote canary
/// (`<name>-canary`). Reserved regardless of the current engine/canary variant, so a later edit
/// can never collide either.
pub(super) fn reserved_flux_kustomization_names(name: &str, manifest_config: bool) -> Vec<String> {
    let mut names = vec![name.to_string()];
    if manifest_config {
        names.push(keel_kube::manifest_app_config_kustomization_name(name));
    }
    names.push(format!("{name}-canary"));
    names
}

/// Rejects a **new** instance (or bundle item resource) `name` whose derived Flux `Kustomization`
/// names can overlap another instance's: `<base>-config` / `<base>-canary` while an instance
/// `<base>` exists, or `<name>` while an instance `<name>-config` / `<name>-canary` exists. Checked
/// on instance directories (not only committed `Kustomization`s), so a later engine or canary
/// switch of either instance can never produce a duplicate id.
pub(super) async fn ensure_no_derived_name_sibling(
    repo_workdir: &Path,
    tenant: &str,
    own_rel_dir: &str,
    name: &str,
    manifest_config: bool,
) -> Result<(), ServerError> {
    let mut candidates: Vec<String> = ["-config", "-canary"]
        .iter()
        .filter_map(|suffix| name.strip_suffix(suffix))
        .filter(|base| !base.is_empty())
        .map(str::to_string)
        .collect();
    candidates.extend(
        reserved_flux_kustomization_names(name, manifest_config)
            .into_iter()
            .skip(1),
    );
    for candidate in candidates {
        for subdir in ["apps", "infra"] {
            if let Some(found) =
                resolve_tenant_resource_dir(repo_workdir, tenant, subdir, &candidate).await
                && found.rel_dir != own_rel_dir
                && !found.rel_dir.starts_with(&format!("{own_rel_dir}/"))
            {
                return Err(ServerError::Conflict(format!(
                    "Instance name '{name}' would share derived Flux Kustomization names \
                     ('<name>-config', '<name>-canary') with '{}'; choose another name",
                    found.rel_dir
                )));
            }
        }
    }
    Ok(())
}

/// Rejects `names` already used by a committed Flux `Kustomization` of `tenant` outside
/// `own_rel_dir` (the instance being created/edited). Every tenant tree is built by the single
/// root `tenants/` Flux `Kustomization`, so a duplicate `Kustomization tenant-<id>/<name>` id would
/// stop kustomize (and reconciliation) for **all** tenants (`NFR-SEC-02`).
pub(super) async fn ensure_flux_kustomization_names_unclaimed(
    repo_workdir: &Path,
    tenant: &str,
    own_rel_dir: &str,
    names: &[String],
) -> Result<(), ServerError> {
    let own_dir = repo_workdir.join(own_rel_dir);
    let mut stack = vec![repo_workdir.join("tenants").join(tenant)];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                if path != own_dir {
                    stack.push(path);
                }
                continue;
            }
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            // Flux `Kustomization`s are committed as `*-kustomization.yaml` (`flux-`, `config-`,
            // `canary-`); plain `kustomization.yaml` files are kustomize configs.
            if !file_type.is_file() || !file_name.ends_with("-kustomization.yaml") {
                continue;
            }
            let Ok(raw) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let Ok(doc) = noyalib::from_str::<Value>(&raw) else {
                continue;
            };
            let is_flux_kustomization = doc.get("kind").and_then(Value::as_str)
                == Some("Kustomization")
                && doc
                    .get("apiVersion")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.starts_with("kustomize.toolkit.fluxcd.io/"));
            let Some(name) = doc.pointer("/metadata/name").and_then(Value::as_str) else {
                continue;
            };
            if is_flux_kustomization && names.iter().any(|n| n == name) {
                let owner = path
                    .strip_prefix(repo_workdir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                return Err(ServerError::Conflict(format!(
                    "Flux Kustomization '{name}' already exists in tenant '{tenant}' ('{owner}'); \
                     choose another instance name"
                )));
            }
        }
    }
    Ok(())
}

/// `true` when the self-service catalog app `catalog_id` mirrors a management-plane add-on of
/// the same ID (`catalog/core/<id>/catalog.yaml` in the platform repository `platform_workdir`),
/// e.g. `kyverno`, `pulumi-kubernetes-operator`, `flux2` or `keel-server`.
pub(super) async fn is_management_addon_twin(platform_workdir: &Path, catalog_id: &str) -> bool {
    validate_dns_label(catalog_id, "catalog_id").is_ok()
        && tokio::fs::try_exists(
            platform_workdir
                .join("catalog/core")
                .join(catalog_id)
                .join("catalog.yaml"),
        )
        .await
        .unwrap_or(false)
}

/// `true` when a second instance of `app` on one workload cluster must be refused (see
/// [`ensure_cluster_singleton_not_installed`]): every `scope: cluster` manifest app (fixed-name
/// cluster-wide objects, e.g. `keel-server`) and every `scope: cluster` Helm app that mirrors a
/// management-plane add-on (`kyverno`, `pulumi-kubernetes-operator`, `flux2`, `cert-manager`,
/// `envoy-gateway`, `flagger`, `prometheus`: operators that own their CRDs, webhooks and
/// controllers once per cluster). Other `scope: cluster` Helm apps (e.g. `dex`) name their
/// cluster-wide objects after the release and may run more than once.
pub(super) async fn is_cluster_singleton_app(
    platform_workdir: &Path,
    app: &CatalogApplication,
) -> bool {
    app.spec.placement.requires_remote_cluster()
        && (app.metadata.runtime == AppRuntime::Manifest
            || is_management_addon_twin(platform_workdir, &app.metadata.id).await)
}

/// [`CatalogApplication::ensure_placement_allows`] with the management-plane twin hint: a
/// `scope: cluster` twin of a `catalog/core/` add-on proposed onto the management cluster is
/// refused (`400`, unchanged) with a message saying the add-on is already managed there and
/// pointing at Edit on its management add-on row of the Topology Map.
pub(super) async fn ensure_app_placement(
    platform_workdir: &Path,
    app: &CatalogApplication,
    remote_target: Option<&str>,
) -> Result<(), ServerError> {
    let Err(err) = app.ensure_placement_allows(remote_target) else {
        return Ok(());
    };
    let hint = management_twin_placement_hint(platform_workdir, &app.metadata.id).await;
    Err(ServerError::BadRequest(format!("{err}{hint}")))
}

/// Suffix of a management-cluster placement `400` for `catalog_id`: for a twin of a
/// `catalog/core/` add-on, says the add-on is already managed on the management cluster and
/// points at Edit on its management add-on row; empty otherwise.
pub(super) async fn management_twin_placement_hint(
    platform_workdir: &Path,
    catalog_id: &str,
) -> String {
    if is_management_addon_twin(platform_workdir, catalog_id).await {
        format!(
            ". '{catalog_id}' is already managed on the management cluster as a \
             management-plane add-on: change it with Edit on its management add-on row (listed \
             first under Topology Map → Deployed Application Releases, admins only) instead"
        )
    } else {
        String::new()
    }
}

/// Flux objects of a committed app directory that may record its catalog ID
/// (`keel.io/catalog-id`) and remote cluster (`spec.kubeConfig.secretRef.name`).
const CLUSTER_SINGLETON_OBJECT_FILES: [&str; 3] = [
    "flux-kustomization.yaml",
    "helmrelease.yaml",
    "helmrelease-canary.yaml",
];

/// Rejects a second instance of the cluster singleton app `catalog_id` (see
/// [`is_cluster_singleton_app`]) on the workload cluster behind the tenant's kubeconfig Secret
/// `kube_config_secret`: another committed Flux app `Kustomization` or `HelmRelease` of `tenant`
/// (outside `own_rel_dir`, the instance being created/edited) with the same `keel.io/catalog-id`
/// label and `spec.kubeConfig.secretRef.name` gives `409`. Such apps ship fixed-name cluster-wide
/// objects (`ClusterRole`s, CRDs, admission webhooks and policies, a fixed-name `Secret`), so two
/// instances would overwrite each other (or fight over the same CRDs) and deleting either would
/// prune the other's objects. The kubeconfig Secret name identifies the cluster only within one
/// tenant (another tenant's Secret of the same name may point elsewhere), so other tenants are
/// not checked.
pub(super) async fn ensure_cluster_singleton_not_installed(
    repo_workdir: &Path,
    tenant: &str,
    own_rel_dir: &str,
    catalog_id: &str,
    kube_config_secret: &str,
) -> Result<(), ServerError> {
    let own_dir = repo_workdir.join(own_rel_dir);
    let mut stack = vec![repo_workdir.join("tenants").join(tenant)];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                if path != own_dir {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file()
                || !CLUSTER_SINGLETON_OBJECT_FILES
                    .iter()
                    .any(|name| entry.file_name() == *name)
            {
                continue;
            }
            let Ok(raw) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let Ok(doc) = noyalib::from_str::<Value>(&raw) else {
                continue;
            };
            let same_app = doc
                .pointer("/metadata/labels/keel.io~1catalog-id")
                .and_then(Value::as_str)
                == Some(catalog_id);
            let same_cluster = doc
                .pointer("/spec/kubeConfig/secretRef/name")
                .and_then(Value::as_str)
                == Some(kube_config_secret);
            if same_app && same_cluster {
                let owner = path
                    .parent()
                    .and_then(|p| p.strip_prefix(repo_workdir).ok())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                return Err(ServerError::Conflict(format!(
                    "Catalog application '{catalog_id}' installs cluster-wide objects and is \
                     already managed on target cluster '{kube_config_secret}' by '{owner}'; \
                     install it once per cluster: change that instance with Edit on its row \
                     instead"
                )));
            }
        }
    }
    Ok(())
}

/// Tenant routes attach to the shared `keel-ingress` listeners: rejects (before anything is
/// committed) hostnames that claim a platform host (IdP issuer, keel-server, Forgejo) or a
/// hostname already routed by another committed app: in the tenant's own working tree, the
/// platform repository and (tenants with their own repositories) the freshly fetched committed
/// `main` of every other tenant's repository.
pub(super) async fn ensure_tenant_route_hostnames_allowed(
    state: &AppState,
    repo: &ScopeRepo,
    rel_dir: &str,
    route_hostnames: &[String],
) -> Result<(), ServerError> {
    if route_hostnames.is_empty() {
        return Ok(());
    }
    ensure_route_hostnames_not_reserved(state, route_hostnames).await?;
    ensure_route_hostnames_unclaimed(&repo.workdir, rel_dir, route_hostnames).await?;
    if repo.is_dedicated() {
        ensure_route_hostnames_unclaimed(&state.repo_workdir, rel_dir, route_hostnames).await?;
        ensure_route_hostnames_unclaimed_by_other_tenants(state, repo.tenant(), route_hostnames)
            .await?;
    }
    Ok(())
}

/// Rejects route hostnames that are unresolved, empty, or claim a platform host (IdP issuer,
/// keel-server, Forgejo, exactly or through a wildcard), as the Kyverno
/// `restrict-tenant-route-hostnames` policy would at admission.
async fn ensure_route_hostnames_not_reserved(
    state: &AppState,
    route_hostnames: &[String],
) -> Result<(), ServerError> {
    let reserved = {
        let oidc = state.oidc_config.read().await;
        platform_reserved_hostnames(
            oidc.iter()
                .flat_map(|c| [c.issuer_url.as_str(), c.redirect_url.as_str()]),
        )
    };
    validate_route_hostnames(route_hostnames, &reserved)?;
    Ok(())
}

/// Bound of one tenant repository mirror fetch in the cross-tenant hostname check.
const TENANT_MIRROR_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// [`ensure_route_hostnames_unclaimed`] over the committed `main` of every other tenant's
/// repository (tenants listed by the platform `tenants/` directories). Each repository is
/// fetched into its read-only mirror ([`crate::TenantRepos::sync_mirror`]) first, with bounded
/// concurrency, so the check never depends on which clones this replica happens to cache.
/// Fails closed (`503`) when any tenant repository cannot be read. Callers hold `git_lock` with
/// the platform checkout synced (see `create_proposal`).
async fn ensure_route_hostnames_unclaimed_by_other_tenants(
    state: &AppState,
    own_tenant: Option<&str>,
    route_hostnames: &[String],
) -> Result<(), ServerError> {
    use futures::StreamExt as _;
    let reader = match RepoReader::committed(state).await {
        Some(reader) => reader,
        None => RepoReader::Worktree {
            root: &state.repo_workdir,
        },
    };
    let others: Vec<String> = reader
        .list_dir("tenants")
        .await
        .into_iter()
        .filter(|id| {
            Some(id.as_str()) != own_tenant
                && id != crate::proposal_repo::PLATFORM_SCOPE
                && validate_dns_label(id, "tenant").is_ok()
        })
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    let platform_remote = state
        .git_runner
        .get_remote_url(&state.repo_workdir, "origin")
        .await?;
    let mirrors: Vec<(String, Result<std::path::PathBuf, ServerError>)> =
        futures::stream::iter(others)
            .map(|tenant| {
                let platform_remote = platform_remote.as_str();
                async move {
                    let dir = state
                        .tenant_repos
                        .sync_mirror(
                            &state.git_runner,
                            platform_remote,
                            &tenant,
                            TENANT_MIRROR_FETCH_TIMEOUT,
                        )
                        .await;
                    (tenant, dir)
                }
            })
            .buffer_unordered(crate::proposal_repo::PROPOSAL_LIST_CONCURRENCY)
            .collect()
            .await;
    let rev = "refs/remotes/origin/main";
    for (tenant, dir) in mirrors {
        let dir = dir.map_err(|err| {
            tracing::warn!(tenant = %tenant, error = %err, "tenant repository unavailable for the route hostname check");
            ServerError::ServiceUnavailable(
                "route hostnames cannot be checked against every tenant right now (a tenant repository is unavailable); retry later".to_string(),
            )
        })?;
        let files = state
            .git_runner
            .list_files_at_ref(&dir, rev, "tenants")
            .await
            .unwrap_or_default();
        for path in &files {
            let file_name = path.rsplit('/').next().unwrap_or_default();
            let remote_canary = || {
                path.rsplitn(3, '/').nth(2).is_some_and(|app_dir| {
                    files.contains(&format!("{app_dir}/{REMOTE_CANARY_KUSTOMIZATION}"))
                })
            };
            if !claims_route_hostnames(file_name, remote_canary) {
                continue;
            }
            let Ok(Some(contents)) = state.git_runner.read_file_at_ref(&dir, rev, path).await
            else {
                continue;
            };
            let claimed = claimed_route_hostnames(file_name, &contents);
            if let Some(host) = conflicting_hostname(route_hostnames, &claimed) {
                return Err(ServerError::Conflict(format!(
                    "Route hostname '{host}' is already claimed by another tenant; choose another hostname"
                )));
            }
        }
    }
    Ok(())
}

/// Flux `Kustomization` file of a remote canary (next to its `canary/` directory, see
/// `build_remote_canary_files`).
const REMOTE_CANARY_KUSTOMIZATION: &str = "canary-kustomization.yaml";

/// `true` for committed app files whose content claims route hostnames on the management
/// cluster's shared Gateway: `manifests.yaml`, `helmrelease.yaml`, `helmrelease-canary.yaml` and a
/// management-cluster Flagger `canary.yaml` (Flagger copies its hosts into the routes it writes).
/// `remote_canary` tells whether a `canary.yaml` belongs to a remote canary (`<app>/canary/`,
/// with `<app>/canary-kustomization.yaml`), whose routes live on the workload cluster's Gateway.
fn claims_route_hostnames(file_name: &str, remote_canary: impl FnOnce() -> bool) -> bool {
    match file_name {
        "manifests.yaml" | "helmrelease.yaml" | "helmrelease-canary.yaml" => true,
        "canary.yaml" => !remote_canary(),
        _ => false,
    }
}

/// Hostnames a committed app file claims: a Helm app's `keel.io/route-hostnames` annotation, a
/// Flagger `Canary`'s `spec.service.hosts` or a manifest app's rendered Gateway API routes.
fn claimed_route_hostnames(file_name: &str, contents: &str) -> Vec<String> {
    if file_name == "canary.yaml" {
        return validate_k8s_manifests_yaml(contents)
            .map(|docs| {
                docs.iter()
                    .filter(|d| {
                        d.get("kind").and_then(Value::as_str) == Some("Canary")
                            && d.get("apiVersion")
                                .and_then(Value::as_str)
                                .is_some_and(|v| v.starts_with("flagger.app/"))
                    })
                    .filter_map(|d| d.pointer("/spec/service/hosts").and_then(Value::as_array))
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    }
    if file_name == "helmrelease.yaml" || file_name == "helmrelease-canary.yaml" {
        // Helm apps record the hostnames their chart routes (`spec.gatewayRoutes`).
        return noyalib::from_str::<Value>(contents)
            .ok()
            .and_then(|doc| {
                doc.pointer("/metadata/annotations")
                    .and_then(|a| a.get(keel_kube::ROUTE_HOSTNAMES_ANNOTATION))
                    .and_then(Value::as_str)
                    .map(|hosts| {
                        hosts
                            .split(',')
                            .map(|h| h.trim().to_ascii_lowercase())
                            .filter(|h| !h.is_empty())
                            .collect()
                    })
            })
            .unwrap_or_default();
    }
    validate_k8s_manifests_yaml(contents)
        .map(|docs| gateway_route_hostnames(&docs))
        .unwrap_or_default()
}

fn conflicting_hostname<'a>(wanted: &'a [String], claimed: &[String]) -> Option<&'a String> {
    wanted.iter().find(|h| {
        claimed.iter().any(|c| {
            keel_core::route_hostname_matches(h, c) || keel_core::route_hostname_matches(c, h)
        })
    })
}

/// A tenant repository is usable for proposals once its skeleton PR (opened by
/// `POST /api/v1/tenants`, adding `tenants/<id>/kustomization.yaml`) is merged. Proposing
/// earlier would create the same index on the proposal branch, so the two PRs would conflict
/// (add/add) and whichever merged second could never merge. `409` with the next step instead.
/// Callers hold the repository lock with `main` synced. No-op for the Local Git provider.
pub(crate) async fn ensure_tenant_repo_initialised(repo: &ScopeRepo) -> Result<(), ServerError> {
    let Some(tenant) = repo.tenant().filter(|_| repo.is_dedicated()) else {
        return Ok(());
    };
    let index = TenantIndex::new(repo, tenant);
    if tokio::fs::try_exists(repo.workdir.join(index.rel_path()))
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    let label = repo
        .repository()
        .full_name
        .unwrap_or_else(|| format!("tenant-{tenant}"));
    Err(ServerError::Conflict(format!(
        "tenant `{tenant}` repository `{label}` is not initialised yet (`{}` is missing on `main`): merge its skeleton pull request first (listed by GET /api/v1/tenants/{tenant}/proposals), then retry",
        index.rel_path()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn derived_kustomization_names_and_sibling_guard() {
        assert_eq!(
            reserved_flux_kustomization_names("web", true),
            ["web", "web-config", "web-canary"]
        );
        // Helm apps never create a `<name>-config` Kustomization.
        assert_eq!(
            reserved_flux_kustomization_names("web", false),
            ["web", "web-canary"]
        );
        let repo = tempfile::tempdir().unwrap();
        for dir in ["tenants/acme/apps/web", "tenants/acme/apps/api-canary"] {
            std::fs::create_dir_all(repo.path().join(dir)).unwrap();
        }
        let check = |name: &'static str| {
            let root = repo.path().to_path_buf();
            async move {
                ensure_no_derived_name_sibling(
                    &root,
                    "acme",
                    &format!("tenants/acme/apps/{name}"),
                    name,
                    true,
                )
                .await
            }
        };
        // `web` exists: `web-config` / `web-canary` would share its derived names.
        assert!(check("web-config").await.is_err());
        assert!(check("web-canary").await.is_err());
        // `api-canary` exists: a new `api` would derive `api-canary`.
        assert!(check("api").await.is_err());
        // Unrelated names (including catalog ids ending in `-config`) are fine.
        assert!(check("canary-gateway-config").await.is_ok());
        assert!(check("webapp").await.is_ok());
        // A Helm app does not reserve `<name>-config`: `db-config` existing is fine for `db`.
        std::fs::create_dir_all(repo.path().join("tenants/acme/apps/db-config")).unwrap();
        let root = repo.path().to_path_buf();
        assert!(
            ensure_no_derived_name_sibling(&root, "acme", "tenants/acme/apps/db", "db", false)
                .await
                .is_ok()
        );
        assert!(
            ensure_no_derived_name_sibling(&root, "acme", "tenants/acme/apps/db", "db", true)
                .await
                .is_err()
        );
        // Other tenants never conflict.
        let root = repo.path().to_path_buf();
        assert!(
            ensure_no_derived_name_sibling(
                &root,
                "other",
                "tenants/other/apps/web-config",
                "web-config",
                true
            )
            .await
            .is_ok()
        );
    }

    /// Management-cluster Flagger Canaries claim their hosts (Flagger copies them into routes
    /// on the shared Gateway); remote canaries (`<app>/canary/canary.yaml`) do not, and the app
    /// being edited is skipped.
    #[tokio::test]
    async fn canary_hosts_are_claimed_route_hostnames() {
        let mut input = keel_kube::FlaggerCanarySpecInput::new("acme", "shop");
        input.hosts.push("shop.example.com".to_string());
        let canary = keel_kube::synthesize_flagger_canary_yaml(&input).unwrap();
        assert_eq!(
            claimed_route_hostnames("canary.yaml", &canary),
            ["shop.example.com"]
        );
        assert!(claims_route_hostnames("canary.yaml", || false));
        assert!(!claims_route_hostnames("canary.yaml", || true));
        assert!(!claims_route_hostnames("networkpolicy-canary.yaml", || {
            false
        }));

        let repo = tempfile::tempdir().unwrap();
        let write = |rel: &str, contents: &str| {
            let path = repo.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write("tenants/acme/apps/shop/canary.yaml", &canary);
        // A remote canary of another app on the same host routes on the workload cluster.
        let mut remote = keel_kube::FlaggerCanarySpecInput::new("other", "remote");
        remote.hosts.push("remote.example.com".to_string());
        write(
            "tenants/other/apps/remote/canary/canary.yaml",
            &keel_kube::synthesize_flagger_canary_yaml(&remote).unwrap(),
        );
        write(
            "tenants/other/apps/remote/canary-kustomization.yaml",
            "{}\n",
        );

        let root = repo.path().to_path_buf();
        let hosts = |h: &str| vec![h.to_string()];
        let err = ensure_route_hostnames_unclaimed(
            &root,
            "tenants/other/apps/web",
            &hosts("shop.example.com"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("tenants/acme/apps/shop"), "{err}");
        // A wildcard covering the canary host conflicts too.
        assert!(
            ensure_route_hostnames_unclaimed(
                &root,
                "tenants/other/apps/web",
                &hosts("*.example.com")
            )
            .await
            .is_err()
        );
        // Editing the canary app itself, or a host only a remote canary uses, is fine.
        assert!(
            ensure_route_hostnames_unclaimed(
                &root,
                "tenants/acme/apps/shop",
                &hosts("shop.example.com")
            )
            .await
            .is_ok()
        );
        assert!(
            ensure_route_hostnames_unclaimed(
                &root,
                "tenants/acme/apps/web",
                &hosts("remote.example.com")
            )
            .await
            .is_ok()
        );
    }
}
