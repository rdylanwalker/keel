//! Pre-commit guards of a proposal: placement, derived Flux names, route hostnames and
//! tenant repository initialisation.

use std::path::Path;

use keel_core::{
    gateway_route_hostnames, platform_reserved_hostnames, validate_dns_label,
    validate_k8s_manifests_yaml, validate_route_hostnames,
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
/// rendered routes or a Helm app's `keel.io/route-hostnames` annotation) under
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
            let is_helm_release =
                file_name == "helmrelease.yaml" || file_name == "helmrelease-canary.yaml";
            if !file_type.is_file() || !(file_name == "manifests.yaml" || is_helm_release) {
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

/// Rejects a second instance of the `scope: cluster` manifest app `catalog_id` on the workload
/// cluster behind the tenant's kubeconfig Secret `kube_config_secret`: another committed Flux app
/// `Kustomization` of `tenant` (outside `own_rel_dir`, the instance being created/edited) with the
/// same `keel.io/catalog-id` label and `spec.kubeConfig.secretRef.name` gives `409`. Such apps
/// ship fixed-name cluster-wide objects (`ClusterRole`s, admission policies, a fixed-name
/// `Secret`), so two instances would overwrite each other and deleting either would prune the
/// other's objects. The kubeconfig Secret name identifies the cluster only within one tenant
/// (another tenant's Secret of the same name may point elsewhere), so other tenants are not
/// checked.
pub(super) async fn ensure_cluster_scoped_manifest_app_not_installed(
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
            if !file_type.is_file() || entry.file_name() != "flux-kustomization.yaml" {
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
                     already installed on target cluster '{kube_config_secret}' by '{owner}'; \
                     install it once per cluster (edit that instance instead)"
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
    let reserved = {
        let oidc = state.oidc_config.read().await;
        platform_reserved_hostnames(
            oidc.iter()
                .flat_map(|c| [c.issuer_url.as_str(), c.redirect_url.as_str()]),
        )
    };
    validate_route_hostnames(route_hostnames, &reserved)?;
    ensure_route_hostnames_unclaimed(&repo.workdir, rel_dir, route_hostnames).await?;
    if repo.is_dedicated() {
        ensure_route_hostnames_unclaimed(&state.repo_workdir, rel_dir, route_hostnames).await?;
        ensure_route_hostnames_unclaimed_by_other_tenants(state, repo.tenant(), route_hostnames)
            .await?;
    }
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
        for path in files {
            let file_name = path.rsplit('/').next().unwrap_or_default();
            if !matches!(
                file_name,
                "manifests.yaml" | "helmrelease.yaml" | "helmrelease-canary.yaml"
            ) {
                continue;
            }
            let Ok(Some(contents)) = state.git_runner.read_file_at_ref(&dir, rev, &path).await
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

/// Hostnames a committed app file claims: a Helm app's `keel.io/route-hostnames` annotation or a
/// manifest app's rendered Gateway API routes.
fn claimed_route_hostnames(file_name: &str, contents: &str) -> Vec<String> {
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
}
