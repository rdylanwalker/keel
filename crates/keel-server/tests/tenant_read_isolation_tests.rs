//! Read-path tenant isolation with per-tenant repositories (`SDD.md` §3.2, §6.3, `NFR-SEC-02`,
//! `NFR-SEC-05`): an endpoint-by-endpoint matrix of tenant `acme` / `beta` callers against each
//! other's stacks, releases, bundles, catalog items, proposals, previews and events. Cluster
//! administrators see every tenant; tenant users only their own tenant; nothing returns another
//! tenant's repository coordinates or any credential.
//!
//! Tenant workloads are committed to each tenant's own repository (local Git provider in
//! per-repository mode) and listed through [`TenantRepoManifests`], the source `keel serve`
//! wires for every remote forge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, COOKIE};
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use keel_core::AuthPhase;
use keel_git::{CommitAuthor, ForgeProvider, GitRunner, LocalGitProvider, RepositoryOptions};
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, InMemorySecretStore, SecretStateStore, TelemetryMux,
    TenantIsolationConfig, WatchedCrdKind,
};
use keel_server::{
    AppState, RepoClusterStateProvider, SESSION_COOKIE_NAME, SessionClaims, TenantRepoCloneBase,
    TenantRepoForge, TenantRepoForgeKind, TenantRepoManifests, TenantRepoNaming, TenantRepos,
    build_router, encrypt_session_token,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const FORGE_ORIGIN: &str = "https://forgejo.keel.local:8888";
const OWNER: &str = "keel-tenants";
const TENANTS: [&str; 2] = ["acme", "beta"];
/// Stand-in for the tenant read credential: must never appear in any response.
const READ_TOKEN: &str = "repo-scoped-token-do-not-leak";

struct Fx {
    _tmp: TempDir,
    state: AppState,
}

fn naming() -> TenantRepoNaming {
    TenantRepoNaming {
        kind: TenantRepoForgeKind::Forgejo,
        owner: OWNER.to_string(),
        monorepo_name: "infra-gitops".to_string(),
        clone_base: TenantRepoCloneBase::Https {
            origin: FORGE_ORIGIN.to_string(),
        },
    }
}

async fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(path, content).await.unwrap();
}

async fn commit_all(git: &GitRunner, dir: &Path, msg: &str) {
    git.add(dir, ["."]).await.unwrap();
    git.commit(dir, msg, &CommitAuthor::new("Seed", "seed@keel.local"))
        .await
        .unwrap();
    git.push(dir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();
}

fn helm_release(name: &str, namespace: &str, extra_labels: &str) -> String {
    format!(
        "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: {name}\n  namespace: {namespace}\n  labels:\n    keel.io/catalog-id: web\n{extra_labels}spec:\n  chart:\n    spec:\n      chart: web\n      version: \"1.0.0\"\n"
    )
}

fn stack(name: &str, namespace: &str, extra_labels: &str) -> String {
    format!(
        "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: {name}\n  namespace: {namespace}\n  labels:\n    keel.io/provider: proxmox\n    keel.io/resource-kind: vm\n{extra_labels}spec:\n  stack: {name}\n"
    )
}

/// Committed `main` content of each tenant repository: own workloads plus hostile documents
/// (another tenant's namespace, a spoofed `keel.io/tenant` label, files outside the tenant's
/// directory) that must never surface in another tenant's view.
fn tenant_repo_files(tenant: &str) -> Vec<(String, String)> {
    let other = if tenant == "acme" { "beta" } else { "acme" };
    let ns = format!("tenant-{tenant}");
    let app = format!("{tenant}-web");
    let vm = format!("{tenant}-vm");
    vec![
        (
            format!("tenants/{tenant}/kustomization.yaml"),
            keel_kube::build_tenant_repo_kustomization_yaml(
                tenant,
                &[
                    format!("apps/{app}"),
                    format!("infra/{vm}"),
                    format!("bundles/{tenant}-bundle"),
                ],
            )
            .unwrap(),
        ),
        (
            format!("tenants/{tenant}/apps/{app}/kustomization.yaml"),
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - helmrelease.yaml\n".to_string(),
        ),
        (
            format!("tenants/{tenant}/apps/{app}/helmrelease.yaml"),
            helm_release(&app, &ns, ""),
        ),
        (
            format!("tenants/{tenant}/apps/{app}/values.yaml"),
            format!("replicas: 2\nowner: {tenant}\n"),
        ),
        (
            format!("tenants/{tenant}/infra/{vm}/stack.yaml"),
            stack(&vm, &ns, ""),
        ),
        (
            format!("tenants/{tenant}/infra/{vm}/values.yaml"),
            "cores: 2\n".to_string(),
        ),
        (
            format!("tenants/{tenant}/bundles/{tenant}-bundle/db/values.yaml"),
            format!("owner: {tenant}\n"),
        ),
        // Hostile: a release declared into the other tenant's namespace.
        (
            format!("tenants/{tenant}/apps/spoof/helmrelease.yaml"),
            helm_release(
                &format!("{tenant}-spoof-ns"),
                &format!("tenant-{other}"),
                "",
            ),
        ),
        // Hostile: a stack in its own namespace labelled as the other tenant's.
        (
            format!("tenants/{tenant}/infra/spoof/stack.yaml"),
            stack(
                &format!("{tenant}-spoof-label"),
                &ns,
                &format!("    keel.io/tenant: {other}\n"),
            ),
        ),
        // Hostile: content under the other tenant's directory of this repository.
        (
            format!("tenants/{other}/apps/planted/helmrelease.yaml"),
            helm_release(&format!("{tenant}-planted"), &format!("tenant-{other}"), ""),
        ),
    ]
}

async fn fixture() -> Fx {
    fixture_with_ttl(None).await
}

/// [`fixture`] with the tenant repository mirror TTL overridden.
async fn fixture_with_ttl(ttl: Option<std::time::Duration>) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let platform_bare = root.join("infra-gitops.git");
    let workdir = root.join("work");
    let git = GitRunner::new();
    git.init_bare(&platform_bare).await.unwrap();
    git.clone(platform_bare.to_str().unwrap(), &workdir)
        .await
        .unwrap();

    // Platform repository: a shared and two tenant-private catalog items, a core add-on, and
    // the platform-owned stub of every tenant (`git-repository.yaml`, `sync.yaml` = the
    // `keel-tenant-root` Kustomization, `rbac.yaml`).
    write(
        &workdir,
        "catalog/apps/web/catalog.yaml",
        "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: web\n  name: Web\n  category: web\n  runtime: helm\nspec:\n  upstream:\n    repoURL: https://charts.example.com\n    chart: web\n    version: \"1.0.0\"\n  schema:\n    type: object\n    properties:\n      replicas:\n        type: integer\n",
    )
    .await;
    write(
        &workdir,
        "catalog/apps/acme-secret-app/catalog.yaml",
        "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: acme-secret-app\n  name: Acme Secret App\n  category: web\n  runtime: manifest\n  tenant: acme\nspec:\n  defaultEngine: flux\n  upstream:\n    path: manifests.yaml\n  schema:\n    type: object\n",
    )
    .await;
    write(
        &workdir,
        "catalog/apps/acme-secret-app/manifests.yaml",
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: acme-internal-marker\n",
    )
    .await;
    write(
        &workdir,
        "catalog/infra/beta-blueprint/catalog.yaml",
        "apiVersion: keel.io/v1alpha1\nkind: CatalogInfraBlueprint\nmetadata:\n  id: beta-blueprint\n  name: Beta Blueprint\n  category: compute\n  provider: kubernetes\n  tenant: beta\nspec:\n  pulumiRuntime: yaml\n  entrypoint: Pulumi.yaml\n  schema:\n    type: object\n",
    )
    .await;
    write(
        &workdir,
        "catalog/infra/beta-blueprint/Pulumi.yaml",
        "name: beta-internal-marker\nruntime: yaml\n",
    )
    .await;
    write(
        &workdir,
        "catalog/core/kyverno/catalog.yaml",
        "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: kyverno\n  name: Kyverno\n  category: security\n  runtime: helm\nspec:\n  upstream:\n    repoURL: https://kyverno.github.io/kyverno\n    chart: kyverno\n    version: \"3.4.0\"\n  schema:\n    type: object\n",
    )
    .await;
    let mut entries = Vec::new();
    for tenant in TENANTS {
        let source = naming().source(tenant).unwrap();
        let stub =
            keel_kube::synthesize_tenant_repo_stub(&TenantIsolationConfig::new(tenant), &source)
                .unwrap();
        for (path, yaml) in &stub.files {
            write(&workdir, path, yaml).await;
        }
        entries.extend(stub.kustomization_entries());
    }
    write(
        &workdir,
        "tenants/kustomization.yaml",
        &serde_yml::to_string(&json!({
            "apiVersion": "kustomize.config.k8s.io/v1beta1",
            "kind": "Kustomization",
            "resources": entries,
        }))
        .unwrap(),
    )
    .await;
    commit_all(&git, &workdir, "chore: seed platform").await;

    let admin = Arc::new(LocalGitProvider::new(&platform_bare));
    for tenant in TENANTS {
        let name = format!("infra-gitops-tenant-{tenant}");
        admin
            .ensure_named_repository(
                OWNER,
                &name,
                &RepositoryOptions {
                    private: true,
                    auto_init: true,
                    ..RepositoryOptions::default()
                },
            )
            .await
            .unwrap();
        let seed = root.join(format!("seed-{tenant}"));
        git.clone(
            root.join(OWNER)
                .join(format!("{name}.git"))
                .to_str()
                .unwrap(),
            &seed,
        )
        .await
        .unwrap();
        for (rel, content) in tenant_repo_files(tenant) {
            write(&seed, &rel, &content).await;
        }
        commit_all(&git, &seed, "feat: tenant workloads").await;
    }

    let store = Arc::new(InMemorySecretStore::new());
    let repos = TenantRepos::dedicated(
        TenantRepoForge {
            naming: naming(),
            ca_crt: None,
            admin: admin.clone(),
            token_minter: None,
        },
        root.join("work-tenant-repos"),
    );
    let mut manifests = TenantRepoManifests::new(repos.clone(), GitRunner::new(), &workdir);
    if let Some(ttl) = ttl {
        manifests = manifests.with_ttl(ttl);
    }
    let provider = RepoClusterStateProvider::new(&workdir).with_tenant_source(Arc::new(manifests));
    let state = AppState::new(
        admin,
        store as Arc<dyn SecretStateStore>,
        TelemetryMux::default(),
        GitRunner::new(),
        &workdir,
        platform_bare.to_string_lossy().to_string(),
        AuthPhase::OidcActive,
    )
    .with_tenant_repos(repos)
    .with_require_pulumi_passphrase(false)
    .with_state_provider(Arc::new(provider));
    Fx { _tmp: tmp, state }
}

async fn cookie(state: &AppState, sub: &str, groups: &[&str]) -> String {
    let key = state
        .secret_store
        .get_or_create_session_key()
        .await
        .unwrap();
    let claims = SessionClaims {
        sub: sub.to_string(),
        email: Some(sub.to_string()),
        name: Some(sub.to_string()),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        auth_phase: AuthPhase::OidcActive,
        exp: chrono::Utc::now().timestamp() + 3600,
    };
    format!(
        "{SESSION_COOKIE_NAME}={}",
        encrypt_session_token(&key, &claims).unwrap()
    )
}

async fn send(
    state: &AppState,
    cookie: &str,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(COOKIE, cookie);
    let body = match body {
        Some(b) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = build_router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let value = serde_json::from_str(&raw).unwrap_or_else(|_| Value::String(raw.clone()));
    (status, value, raw)
}

/// Callers of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caller {
    Admin,
    AcmeMember,
    AcmeAdmin,
    BetaMember,
    /// Member of an IdP group named after the management pseudo-tenant.
    ManagementGroup,
    /// Member of an IdP group named after a non-tenant namespace.
    SystemGroup,
    /// Authenticated, no tenant.
    Outsider,
}

impl Caller {
    const ALL: [Caller; 7] = [
        Caller::Admin,
        Caller::AcmeMember,
        Caller::AcmeAdmin,
        Caller::BetaMember,
        Caller::ManagementGroup,
        Caller::SystemGroup,
        Caller::Outsider,
    ];

    fn groups(self) -> &'static [&'static str] {
        match self {
            Caller::Admin => &["keel-admins"],
            Caller::AcmeMember => &["tenant-acme"],
            Caller::AcmeAdmin => &["tenant-acme-admin"],
            Caller::BetaMember => &["tenant-beta"],
            Caller::ManagementGroup => &["tenant-management", "tenant-management-admin"],
            Caller::SystemGroup => &["tenant-keel-system", "tenant-flux-system"],
            Caller::Outsider => &["developers"],
        }
    }

    /// Tenants whose data the caller may read.
    fn tenants(self) -> &'static [&'static str] {
        match self {
            Caller::Admin => &["acme", "beta"],
            Caller::AcmeMember | Caller::AcmeAdmin => &["acme"],
            Caller::BetaMember => &["beta"],
            Caller::ManagementGroup | Caller::SystemGroup | Caller::Outsider => &[],
        }
    }

    fn reads(self, tenant: &str) -> bool {
        self.tenants().contains(&tenant)
    }

    async fn cookie(self, state: &AppState) -> String {
        cookie(state, &format!("{self:?}@keel.local"), self.groups()).await
    }
}

/// Every string in a non-admin response that would reveal another tenant's repository or a
/// credential.
fn assert_no_foreign_leak(caller: Caller, uri: &str, raw: &str) {
    assert!(
        !raw.contains(READ_TOKEN),
        "{caller:?} {uri}: credential leaked: {raw}"
    );
    // Hostile tenant-repository documents (another tenant's namespace or directory) are never
    // reported, not even to administrators.
    for hostile in ["-spoof-ns", "-planted"] {
        assert!(
            !raw.contains(hostile),
            "{caller:?} {uri}: hostile document `{hostile}` surfaced: {raw}"
        );
    }
    if caller == Caller::Admin {
        return;
    }
    for tenant in TENANTS {
        if caller.reads(tenant) {
            continue;
        }
        for marker in [
            format!("infra-gitops-tenant-{tenant}"),
            format!("tenant-{tenant}\""),
            format!("{tenant}-web"),
            format!("{tenant}-vm"),
            format!("{tenant}-bundle"),
            format!("{tenant}-internal-marker"),
            format!("{tenant}-secret-app"),
            format!("{tenant}-blueprint"),
        ] {
            assert!(
                !raw.contains(&marker),
                "{caller:?} {uri}: leaked `{marker}` of tenant {tenant}: {raw}"
            );
        }
    }
}

fn names(list: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = list[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` missing: {list}"))
        .iter()
        .map(|s| {
            format!(
                "{}/{}",
                s["namespace"].as_str().unwrap(),
                s["name"].as_str().unwrap()
            )
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn workload_listings_come_from_tenant_repositories_and_stay_in_the_callers_tenants() {
    let fx = fixture().await;
    for caller in Caller::ALL {
        let c = caller.cookie(&fx.state).await;

        // GET /api/v1/stacks
        let (status, body, raw) = send(&fx.state, &c, Method::GET, "/api/v1/stacks", None).await;
        assert_eq!(status, StatusCode::OK, "{caller:?}: {body}");
        assert_no_foreign_leak(caller, "/api/v1/stacks", &raw);
        let mut expected: Vec<String> = Vec::new();
        for t in caller.tenants() {
            expected.push(format!("tenant-{t}/{t}-spoof-label"));
            expected.push(format!("tenant-{t}/{t}-vm"));
        }
        if caller == Caller::Admin {
            expected.push("flux-system/management-cluster-core".to_string());
        }
        expected.sort();
        assert_eq!(names(&body, "stacks"), expected, "{caller:?}: {body}");
        for s in body["stacks"].as_array().unwrap() {
            // A tenant repository's stack always belongs to that tenant, whatever its labels.
            if let Some(t) = s["namespace"].as_str().unwrap().strip_prefix("tenant-") {
                assert_eq!(s["tenant"], t, "{caller:?}: {s}");
            }
        }
        // Only administrators get the platform repository URL and the platform's Pulumi
        // state backend.
        assert_eq!(
            body.get("gitRepoUrl").is_some(),
            caller == Caller::Admin,
            "{caller:?}: {body}"
        );
        assert_eq!(
            body.get("stateBackend").is_some(),
            caller == Caller::Admin,
            "{caller:?}: {body}"
        );

        // GET /api/v1/releases
        let (status, body, raw) = send(&fx.state, &c, Method::GET, "/api/v1/releases", None).await;
        assert_eq!(status, StatusCode::OK, "{caller:?}: {body}");
        assert_no_foreign_leak(caller, "/api/v1/releases", &raw);
        let mut expected: Vec<String> = caller
            .tenants()
            .iter()
            .map(|t| format!("tenant-{t}/{t}-web"))
            .collect();
        expected.sort();
        assert_eq!(names(&body, "releases"), expected, "{caller:?}: {body}");
        assert!(!raw.contains("keel-tenant-root"), "{caller:?}: {raw}");

        // Per-tenant filters: own tenant only; `management` is cluster-admin only.
        for t in TENANTS {
            for list in ["stacks", "releases"] {
                let uri = format!("/api/v1/{list}?tenant={t}");
                let (status, body, raw) = send(&fx.state, &c, Method::GET, &uri, None).await;
                if caller.reads(t) {
                    assert_eq!(status, StatusCode::OK, "{caller:?} {uri}: {body}");
                    assert_no_foreign_leak(caller, &uri, &raw);
                    assert!(
                        body[list]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|s| s["namespace"] == format!("tenant-{t}")),
                        "{caller:?} {uri}: {body}"
                    );
                    if list == "stacks" {
                        assert_eq!(
                            body["gitRepoUrl"],
                            format!("{FORGE_ORIGIN}/{OWNER}/infra-gitops-tenant-{t}.git"),
                            "{caller:?} {uri}"
                        );
                    }
                } else {
                    assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?} {uri}: {body}");
                }
            }
        }
        for list in ["stacks", "releases"] {
            let uri = format!("/api/v1/{list}?tenant=management");
            let (status, body, _) = send(&fx.state, &c, Method::GET, &uri, None).await;
            let expected = if caller == Caller::Admin {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            };
            assert_eq!(status, expected, "{caller:?} {uri}: {body}");
        }
    }
}

#[tokio::test]
async fn per_resource_reads_are_denied_across_tenants() {
    let fx = fixture().await;
    for caller in Caller::ALL {
        let c = caller.cookie(&fx.state).await;
        for t in TENANTS {
            // (uri, status when readable)
            let cases = [
                (
                    format!("/api/v1/stacks/tenant-{t}/{t}-vm/config"),
                    StatusCode::OK,
                ),
                (
                    format!("/api/v1/releases/tenant-{t}/{t}-web/config"),
                    StatusCode::OK,
                ),
                (
                    format!("/api/v1/bundles/tenant-{t}/{t}-bundle/config"),
                    StatusCode::OK,
                ),
                (
                    format!("/api/v1/stacks/tenant-{t}/{t}-vm/diagnostics"),
                    StatusCode::OK,
                ),
                (
                    format!("/api/v1/releases/tenant-{t}/{t}-web/diagnostics"),
                    StatusCode::OK,
                ),
                // Resolved from the stack in the tenant repository.
                (
                    format!("/api/v1/stacks/tenant-{t}/{t}-vm/ssh-info"),
                    StatusCode::OK,
                ),
            ];
            for (uri, readable) in cases {
                let (status, body, raw) = send(&fx.state, &c, Method::GET, &uri, None).await;
                if caller.reads(t) {
                    assert_eq!(status, readable, "{caller:?} {uri}: {body}");
                    assert_no_foreign_leak(caller, &uri, &raw);
                } else {
                    assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?} {uri}: {body}");
                    assert!(!raw.contains("owner"), "{caller:?} {uri}: {raw}");
                }
            }
        }
        // Configuration of the tenant's own repository, never another's.
        if let Some(t) = caller.tenants().first() {
            let uri = format!("/api/v1/releases/tenant-{t}/{t}-web/config");
            let (_, body, _) = send(&fx.state, &c, Method::GET, &uri, None).await;
            assert_eq!(body["values"]["owner"], *t, "{caller:?}: {body}");
            let uri = format!("/api/v1/bundles/tenant-{t}/{t}-bundle/config");
            let (_, body, _) = send(&fx.state, &c, Method::GET, &uri, None).await;
            assert_eq!(body["values"]["db"]["owner"], *t, "{caller:?}: {body}");
        }
        // Non-tenant namespaces are never opened by a look-alike IdP group.
        for uri in [
            "/api/v1/stacks/keel-system/x/config",
            "/api/v1/releases/keel-previews/x/config",
            "/api/v1/bundles/keel-system/x/config",
            "/api/v1/stacks/keel-system/x/ssh-info",
            "/api/v1/stacks/flux-system/management-cluster-core/diagnostics",
        ] {
            let (status, body, _) = send(&fx.state, &c, Method::GET, uri, None).await;
            if caller == Caller::Admin {
                assert_ne!(status, StatusCode::FORBIDDEN, "{caller:?} {uri}: {body}");
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?} {uri}: {body}");
            }
        }
        // Management-plane views are cluster-admin only.
        let (status, body, _) = send(
            &fx.state,
            &c,
            Method::GET,
            "/api/v1/management/addons",
            None,
        )
        .await;
        if caller == Caller::Admin {
            assert_eq!(status, StatusCode::OK, "{body}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?}: {body}");
        }
    }
}

#[tokio::test]
async fn catalog_and_tenant_listings_hide_other_tenants_items() {
    let fx = fixture().await;
    for caller in Caller::ALL {
        let c = caller.cookie(&fx.state).await;

        let (status, body, raw) = send(&fx.state, &c, Method::GET, "/api/v1/catalog", None).await;
        assert_eq!(status, StatusCode::OK, "{caller:?}: {body}");
        assert_no_foreign_leak(caller, "/api/v1/catalog", &raw);
        let ids = |key: &str| -> Vec<String> {
            body[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["metadata"]["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(ids("applications").contains(&"web".to_string()));
        assert_eq!(
            ids("applications").contains(&"acme-secret-app".to_string()),
            caller.reads("acme"),
            "{caller:?}: {body}"
        );
        assert_eq!(
            body["manifestsYaml"].get("acme-secret-app").is_some(),
            caller.reads("acme"),
            "{caller:?}: {body}"
        );
        assert_eq!(
            ids("blueprints").contains(&"beta-blueprint".to_string()),
            caller.reads("beta"),
            "{caller:?}: {body}"
        );
        // Core add-ons belong to the management plane.
        assert_eq!(
            body.get("coreAddons").is_some(),
            caller == Caller::Admin,
            "{caller:?}: {body}"
        );
        // A tenant filter never widens visibility.
        let (_, body, raw) = send(
            &fx.state,
            &c,
            Method::GET,
            "/api/v1/catalog?tenant=acme",
            None,
        )
        .await;
        assert_no_foreign_leak(caller, "/api/v1/catalog?tenant=acme", &raw);
        assert_eq!(
            body["applications"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["metadata"]["id"] == "acme-secret-app"),
            caller.reads("acme")
        );

        let (status, body, raw) = send(&fx.state, &c, Method::GET, "/api/v1/tenants", None).await;
        assert_eq!(status, StatusCode::OK, "{caller:?}: {body}");
        assert_no_foreign_leak(caller, "/api/v1/tenants", &raw);
        let tenants: Vec<&str> = body["tenants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(tenants, caller.tenants(), "{caller:?}: {body}");

        // Platform data readable by every authenticated user (no tenant content).
        let (status, _, raw) = send(&fx.state, &c, Method::GET, "/api/v1/providers", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_no_foreign_leak(caller, "/api/v1/providers", &raw);
    }
}

#[tokio::test]
async fn proposals_and_previews_are_scoped_to_the_callers_tenant_repositories() {
    let fx = fixture().await;
    let admin = Caller::Admin.cookie(&fx.state).await;
    for tenant in TENANTS {
        let (status, body, _) = send(
            &fx.state,
            &admin,
            Method::POST,
            &format!("/api/v1/tenants/{tenant}/proposals"),
            Some(json!({
                "catalogId": "web",
                "name": format!("{tenant}-proposed"),
                "values": {},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["proposal"]["id"], "1", "{body}");
    }

    for caller in Caller::ALL {
        let c = caller.cookie(&fx.state).await;
        let (status, body, raw) = send(
            &fx.state,
            &c,
            Method::GET,
            "/api/v1/proposals?status=all",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{caller:?}: {body}");
        assert_no_foreign_leak(caller, "/api/v1/proposals", &raw);
        let mut scopes: Vec<&str> = body["proposals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["scope"].as_str().unwrap())
            .filter(|s| *s != "management")
            .collect();
        scopes.sort();
        assert_eq!(scopes, caller.tenants(), "{caller:?}: {body}");
        if caller != Caller::Admin {
            assert!(
                body["proposals"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|p| p["scope"] != "management"),
                "{caller:?}: {body}"
            );
        }

        for t in TENANTS.iter().copied().chain(["management"]) {
            let readable = caller.reads(t) || caller == Caller::Admin;
            for uri in [
                format!("/api/v1/tenants/{t}/proposals?status=all"),
                format!("/api/v1/tenants/{t}/proposals/1/preview"),
            ] {
                let (status, body, raw) = send(&fx.state, &c, Method::GET, &uri, None).await;
                if !readable {
                    assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?} {uri}: {body}");
                    continue;
                }
                assert_no_foreign_leak(caller, &uri, &raw);
                if t == "management" {
                    continue;
                }
                assert_eq!(status, StatusCode::OK, "{caller:?} {uri}: {body}");
                if uri.ends_with("/preview") {
                    // PR ids are per repository: `1` of this scope is this tenant's PR.
                    assert_eq!(body["repository"]["label"], format!("tenant-{t}"), "{body}");
                    assert!(raw.contains(&format!("{t}-proposed")), "{body}");
                }
            }
        }
    }
}

#[tokio::test]
async fn event_streams_only_carry_the_callers_tenant_namespaces() {
    let fx = fixture().await;
    for caller in Caller::ALL {
        let c = caller.cookie(&fx.state).await;

        // A namespace the caller may not read yields an empty stream.
        for ns in ["tenant-acme", "tenant-beta", "keel-system", "flux-system"] {
            let resp = build_router(fx.state.clone())
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/v1/events/stream?namespace={ns}"))
                        .header(COOKIE, &c)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let readable = caller == Caller::Admin
                || ns.strip_prefix("tenant-").is_some_and(|t| caller.reads(t));
            if !readable {
                let frame = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    resp.into_body().frame(),
                )
                .await
                .expect("an empty stream ends at once");
                assert!(frame.is_none(), "{caller:?} {ns}: stream must be empty");
            }
        }

        // The unfiltered stream drops other tenants' (and platform) events.
        let resp = build_router(fx.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events/stream")
                    .header(COOKIE, &c)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        for ns in ["keel-system", "tenant-beta", "tenant-acme"] {
            fx.state.telemetry.publish(CrdWatchEvent::new(
                WatchedCrdKind::HelmRelease,
                ns,
                format!("{ns}-event"),
                CrdReconcileStatus::Ready,
                "reconciled",
                None,
            ));
        }
        let expected: Vec<String> = if caller == Caller::Admin {
            vec!["keel-system-event".into()]
        } else {
            ["tenant-beta", "tenant-acme"]
                .iter()
                .filter(|ns| caller.reads(ns.strip_prefix("tenant-").unwrap()))
                .map(|ns| format!("{ns}-event"))
                .take(1)
                .collect()
        };
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), body.frame()).await;
        match expected.first() {
            Some(name) => {
                let data = frame
                    .expect("SSE frame timeout")
                    .expect("SSE stream ended")
                    .expect("SSE frame error")
                    .into_data()
                    .expect("data frame");
                let text = String::from_utf8(data.to_vec()).unwrap();
                assert!(
                    text.contains(&format!("\"name\":\"{name}\"")),
                    "{caller:?}: {text}"
                );
            }
            None => assert!(frame.is_err(), "{caller:?}: no event may be delivered"),
        }
    }
}

fn beta_bare(fx: &Fx) -> PathBuf {
    let root: PathBuf = fx.state.tenant_repos.cache_root().to_path_buf();
    root.parent()
        .unwrap()
        .join(OWNER)
        .join("infra-gitops-tenant-beta.git")
}

/// A tenant repository that cannot be read (and has no snapshot yet) is omitted, never another
/// tenant's content in its place; the failed fetch is not retried by the next poll within the
/// backoff window, even once the repository is reachable again.
#[tokio::test]
async fn an_unreadable_tenant_repository_drops_only_its_own_workloads() {
    let fx = fixture().await;
    // Break beta's repository before any snapshot exists.
    let bare = beta_bare(&fx);
    let parked = bare.with_extension("parked");
    std::fs::rename(&bare, &parked).unwrap();
    let admin = Caller::Admin.cookie(&fx.state).await;
    let (status, body, _) = send(&fx.state, &admin, Method::GET, "/api/v1/releases", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body, "releases"), vec!["tenant-acme/acme-web"]);

    // Reachable again, but within the backoff window: not fetched again.
    std::fs::rename(&parked, &bare).unwrap();
    let (status, body, _) = send(&fx.state, &admin, Method::GET, "/api/v1/releases", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body, "releases"), vec!["tenant-acme/acme-web"]);
}

/// A tenant repository that becomes unreadable keeps being listed from its last snapshot, and
/// the failing fetch does not stall the listing.
#[tokio::test]
async fn an_unreadable_tenant_repository_is_served_from_its_last_snapshot() {
    let fx = fixture_with_ttl(Some(std::time::Duration::ZERO)).await;
    let admin = Caller::Admin.cookie(&fx.state).await;
    let expected = vec!["tenant-acme/acme-web", "tenant-beta/beta-web"];
    let (status, body, _) = send(&fx.state, &admin, Method::GET, "/api/v1/releases", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body, "releases"), expected);

    std::fs::remove_dir_all(beta_bare(&fx)).unwrap();
    let started = std::time::Instant::now();
    let (status, body, _) = send(&fx.state, &admin, Method::GET, "/api/v1/releases", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body, "releases"), expected);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "listing stalled: {:?}",
        started.elapsed()
    );
}

/// A member of several tenants sees exactly those tenants' workloads (one listing pass).
#[tokio::test]
async fn a_member_of_several_tenants_lists_each_of_them() {
    let fx = fixture().await;
    let c = cookie(
        &fx.state,
        "both@keel.local",
        &["tenant-acme", "tenant-beta"],
    )
    .await;
    let (status, body, raw) = send(&fx.state, &c, Method::GET, "/api/v1/releases", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        names(&body, "releases"),
        vec!["tenant-acme/acme-web", "tenant-beta/beta-web"]
    );
    assert!(
        !raw.contains("-spoof-ns") && !raw.contains("-planted"),
        "{raw}"
    );
    let (status, body, raw) = send(&fx.state, &c, Method::GET, "/api/v1/stacks", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        names(&body, "stacks"),
        vec![
            "tenant-acme/acme-spoof-label",
            "tenant-acme/acme-vm",
            "tenant-beta/beta-spoof-label",
            "tenant-beta/beta-vm",
        ]
    );
    assert!(body.get("stateBackend").is_none(), "{body}");
    assert!(!raw.contains("management-cluster-core"), "{raw}");
}
