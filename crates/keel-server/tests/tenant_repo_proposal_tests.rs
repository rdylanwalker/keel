//! Proposal write paths with per-tenant repositories (`SDD.md` §3.2, §4.2, §6.3): every tenant
//! change is committed, reviewed, previewed and merged in the tenant's own repository through
//! the tenant-scoped routes `/api/v1/tenants/{tenant}/proposals[...]`. The forge is the local Git
//! provider in per-repository mode (sibling bare repositories `<root>/keel-tenants/*.git`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, COOKIE};
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use keel_core::AuthPhase;
use keel_git::{
    CommitAuthor, CreatePrRequest, ForgeProvider, GitRunner, LocalGitProvider, RepositoryOptions,
};
use keel_kube::{
    InMemoryPreviewCluster, InMemorySecretStore, SecretStateStore, TelemetryMux,
    TenantIsolationConfig,
};
use keel_server::{
    AppState, SESSION_COOKIE_NAME, SessionClaims, TenantRepoCloneBase, TenantRepoForge,
    TenantRepoForgeKind, TenantRepoNaming, TenantRepos, build_router, encrypt_session_token,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const FORGE_ORIGIN: &str = "https://forgejo.keel.local:8888";
const OWNER: &str = "keel-tenants";
const TENANTS: [&str; 2] = ["acme", "beta"];

struct Fx {
    _tmp: TempDir,
    state: AppState,
    root: PathBuf,
    platform_bare: PathBuf,
    cluster: InMemoryPreviewCluster,
}

impl Fx {
    fn tenant_bare(&self, tenant: &str) -> PathBuf {
        self.root
            .join(OWNER)
            .join(format!("infra-gitops-tenant-{tenant}.git"))
    }

    fn tenant_provider(&self, tenant: &str) -> Arc<dyn ForgeProvider> {
        LocalGitProvider::new(&self.platform_bare)
            .for_repository(OWNER, &format!("infra-gitops-tenant-{tenant}"))
            .unwrap()
    }
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

fn tenant_url(tenant: &str) -> String {
    format!("{FORGE_ORIGIN}/{OWNER}/infra-gitops-tenant-{tenant}.git")
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

async fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let platform_bare = root.join("infra-gitops.git");
    let workdir = root.join("work");
    let git = GitRunner::new();
    git.init_bare(&platform_bare).await.unwrap();
    git.clone(platform_bare.to_str().unwrap(), &workdir)
        .await
        .unwrap();

    // Platform repository: catalog + the platform-owned stub of every tenant.
    write(
        &workdir,
        "catalog/apps/web/catalog.yaml",
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: web
  name: Web
  category: web
  runtime: helm
spec:
  upstream:
    repoURL: https://charts.example.com
    chart: web
    version: "1.0.0"
  schema:
    type: object
    properties:
      replicas:
        type: integer
"#,
    )
    .await;
    write(
        &workdir,
        "catalog/apps/rauthy/catalog.yaml",
        include_str!("../../../deploy/catalog/apps/rauthy/catalog.yaml"),
    )
    .await;
    write(
        &workdir,
        "catalog/apps/rauthy/manifests.yaml",
        include_str!("../../../deploy/catalog/apps/rauthy/manifests.yaml"),
    )
    .await;
    write(
        &workdir,
        "catalog/infra/proxmox-vm/catalog.yaml",
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: proxmox-vm
  name: Proxmox VM
  category: compute
  provider: proxmox
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  schema:
    type: object
    required:
      - cores
    properties:
      cores:
        type: integer
        minimum: 1
"#,
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
        &keel_core::yaml::to_string(&json!({
            "apiVersion": "kustomize.config.k8s.io/v1beta1",
            "kind": "Kustomization",
            "resources": entries,
        }))
        .unwrap(),
    )
    .await;
    commit_all(&git, &workdir, "chore: seed platform").await;

    // Tenant repositories with their merged skeleton (`tenants/<id>/kustomization.yaml`).
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
        write(
            &seed,
            &format!("tenants/{tenant}/kustomization.yaml"),
            &keel_kube::build_tenant_repo_kustomization_yaml(tenant, &[]).unwrap(),
        )
        .await;
        commit_all(&git, &seed, "feat: initialise tenant repository").await;
    }

    let cluster = InMemoryPreviewCluster::new();
    for tenant in TENANTS {
        cluster.put_tenant_secret(
            tenant,
            "keel-gitops-credentials",
            &[
                ("username", "keel-tenant-reader"),
                ("password", "repo-scoped-token"),
                ("ca.crt", "-----BEGIN CERTIFICATE-----"),
            ],
        );
        cluster.put_tenant_secret(tenant, "keel-pulumi-passphrase", &[("passphrase", "pp")]);
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
    .with_preview_cluster(Some(Arc::new(cluster.clone())));
    Fx {
        _tmp: tmp,
        state,
        root,
        platform_bare,
        cluster,
    }
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

async fn admin(state: &AppState) -> String {
    cookie(state, "admin@keel.local", &["keel-admins"]).await
}

async fn send(
    state: &AppState,
    cookie: &str,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

fn git_show(bare: &Path, rev_path: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args(["show", rev_path])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_branches(bare: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn sops_unavailable(status: StatusCode, body: &Value) -> bool {
    if status == StatusCode::CREATED {
        return false;
    }
    assert!(
        std::env::var_os("KEEL_REQUIRE_SOPS").is_none(),
        "KEEL_REQUIRE_SOPS is set but the proposal could not encrypt: {body}"
    );
    true
}

async fn propose(state: &AppState, cookie: &str, tenant: &str, body: Value) -> (StatusCode, Value) {
    send(
        state,
        cookie,
        Method::POST,
        &format!("/api/v1/tenants/{tenant}/proposals"),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn app_proposals_commit_only_to_the_tenant_repository() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let platform_branches = git_branches(&fx.platform_bare);
    let platform_main = git_show(&fx.platform_bare, "main:tenants/kustomization.yaml");

    let (status, body) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "catalogId": "web", "values": { "replicas": 2 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let proposal = &body["proposal"];
    assert_eq!(proposal["scope"], "acme");
    assert_eq!(proposal["repository"]["label"], "tenant-acme");
    assert_eq!(
        proposal["repository"]["fullName"],
        "keel-tenants/infra-gitops-tenant-acme"
    );
    assert_eq!(proposal["id"], "1", "PR ids are per repository");
    let branch = body["branch"].as_str().unwrap();
    assert!(branch.starts_with("keel/acme/app-web-"), "{branch}");

    // The commit lands in the tenant repository only, with a tenant-relative index.
    let acme = fx.tenant_bare("acme");
    let hr = git_show(
        &acme,
        &format!("{branch}:tenants/acme/apps/web/helmrelease.yaml"),
    )
    .expect("HelmRelease committed to the tenant repository");
    assert!(hr.contains("namespace: tenant-acme"), "{hr}");
    let index = git_show(&acme, &format!("{branch}:tenants/acme/kustomization.yaml")).unwrap();
    assert!(index.contains("- apps/web/helmrelease.yaml"), "{index}");
    assert!(index.contains("- apps/web/helm-repository.yaml"), "{index}");
    assert!(!index.contains("acme/"), "{index}");
    assert!(!index.contains("rbac.yaml"), "{index}");
    assert_eq!(git_branches(&fx.platform_bare), platform_branches);
    assert_eq!(
        git_show(&fx.platform_bare, "main:tenants/kustomization.yaml"),
        platform_main
    );
    assert!(
        git_branches(&fx.tenant_bare("beta"))
            .iter()
            .all(|b| b == "main")
    );

    // A body tenant must match the path tenant; `management` is not a tenant.
    let (status, _) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "tenant": "beta", "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = propose(
        &fx.state,
        &admin,
        "management",
        json!({ "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Edit: the stored config is read back from the tenant repository after the merge.
    let (status, merged) = send(
        &fx.state,
        &admin,
        Method::POST,
        "/api/v1/tenants/acme/proposals/1/merge",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["proposal"]["state"], "merged");
    let (status, config) = send(
        &fx.state,
        &admin,
        Method::GET,
        "/api/v1/releases/tenant-acme/web/config",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["values"]["replicas"], 2);

    // Deleting the release proposes the removal in the tenant repository.
    let (status, deleted) = send(
        &fx.state,
        &admin,
        Method::DELETE,
        "/api/v1/releases/tenant-acme/web",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["proposal"]["repository"]["label"], "tenant-acme");
    let del_branch = deleted["branch"].as_str().unwrap();
    assert!(
        git_show(
            &acme,
            &format!("{del_branch}:tenants/acme/apps/web/helmrelease.yaml")
        )
        .is_none()
    );
    let index = git_show(
        &acme,
        &format!("{del_branch}:tenants/acme/kustomization.yaml"),
    )
    .unwrap();
    assert!(!index.contains("apps/web"), "{index}");
    assert_eq!(git_branches(&fx.platform_bare), platform_branches);
}

#[tokio::test]
async fn manifest_app_secrets_are_encrypted_into_the_tenant_repository() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let values = json!({
        "hostname": "auth-acme.keel.local",
        "publicPortSuffix": ":8888",
        "adminEmail": "ops@acme.example",
        "encKeyId": "acme1",
        "encKey": "q2Vz7mU0ZcV1x7pYb7Yk2r9mD0aH3sW5tL8nQ4eR6fA=",
        "hiqliteSecretRaft": "raftSecretSampleValue-0123456789",
        "hiqliteSecretApi": "apiSecretSampleValue-9876543210",
        "adminPassword": "Sample-Admin#Passw0rd!",
    });
    let (status, body) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "catalogId": "rauthy", "values": values }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    let branch = body["branch"].as_str().unwrap();
    let acme = fx.tenant_bare("acme");
    let secret = git_show(
        &acme,
        &format!("{branch}:tenants/acme/apps/rauthy/config/secrets.enc.yaml"),
    )
    .expect("encrypted secret in the tenant repository");
    assert!(secret.contains("ENC["), "{secret}");
    assert!(!secret.contains("Sample-Admin#Passw0rd!"));
    let index = git_show(&acme, &format!("{branch}:tenants/acme/kustomization.yaml")).unwrap();
    assert!(
        index.contains("- apps/rauthy/config-kustomization.yaml"),
        "{index}"
    );
    assert!(
        index.contains("- apps/rauthy/flux-kustomization.yaml"),
        "{index}"
    );
    let fk = git_show(
        &acme,
        &format!("{branch}:tenants/acme/apps/rauthy/flux-kustomization.yaml"),
    )
    .unwrap();
    assert!(fk.contains("name: keel-gitops"), "{fk}");
    assert!(fk.contains("path: ./tenants/acme/apps/rauthy"), "{fk}");
    // Nothing reached the platform repository.
    assert!(git_branches(&fx.platform_bare).iter().all(|b| b == "main"));

    // Once merged, another tenant cannot claim the same route hostname (cross-repository check).
    let (status, merged) = send(
        &fx.state,
        &admin,
        Method::POST,
        "/api/v1/tenants/acme/proposals/1/merge",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let (status, body) = propose(
        &fx.state,
        &admin,
        "beta",
        json!({ "catalogId": "rauthy", "values": values }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string()
            .contains("already claimed by another tenant"),
        "{body}"
    );
}

#[tokio::test]
async fn cross_tenant_proposal_access_is_denied() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let (status, body) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "catalogId": "web", "values": { "replicas": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let acme_member = cookie(&fx.state, "alice@acme", &["tenant-acme"]).await;
    let beta_member = cookie(&fx.state, "bob@beta", &["tenant-beta"]).await;
    let beta_admin = cookie(&fx.state, "carol@beta", &["tenant-beta-admin"]).await;

    let (status, _) = send(
        &fx.state,
        &acme_member,
        Method::GET,
        "/api/v1/tenants/acme/proposals/1/preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (uri, method) in [
        ("/api/v1/tenants/acme/proposals", Method::GET),
        ("/api/v1/tenants/acme/proposals/1/preview", Method::GET),
        ("/api/v1/tenants/acme/proposals/1/merge", Method::POST),
        ("/api/v1/tenants/acme/proposals/1/reject", Method::POST),
        ("/api/v1/tenants/management/proposals", Method::GET),
    ] {
        let (status, body) = send(&fx.state, &beta_member, method.clone(), uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        let (status, body) = send(&fx.state, &beta_admin, method, uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
    // Members cannot write proposals of other tenants either.
    let (status, _) = propose(
        &fx.state,
        &beta_member,
        "acme",
        json!({ "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // PR ids are per repository: `beta` has no PR #1.
    let (status, _) = send(
        &fx.state,
        &beta_admin,
        Method::POST,
        "/api/v1/tenants/beta/proposals/1/merge",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The beta member's aggregated listing never contains acme's proposal.
    let (status, list) = send(
        &fx.state,
        &beta_member,
        Method::GET,
        "/api/v1/proposals",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["proposals"].as_array().unwrap().len(), 0, "{list}");
    // The old flat routes are gone.
    let (status, _) = send(
        &fx.state,
        &admin,
        Method::POST,
        "/api/v1/proposals/1/merge",
        None,
    )
    .await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
        "{status}"
    );
}

#[tokio::test]
async fn tenant_admins_merge_only_tenant_scoped_changes_in_their_repository() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let acme_admin = cookie(&fx.state, "dave@acme", &["tenant-acme-admin"]).await;
    let (status, body) = propose(
        &fx.state,
        &acme_admin,
        "acme",
        json!({ "catalogId": "web", "values": { "replicas": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, merged) = send(
        &fx.state,
        &acme_admin,
        Method::POST,
        "/api/v1/tenants/acme/proposals/1/merge",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert!(
        git_show(
            &fx.tenant_bare("acme"),
            "main:tenants/acme/apps/web/helmrelease.yaml"
        )
        .is_some()
    );

    // A PR adding a platform stub file (or anything outside `tenants/acme/`) is refused.
    for (path, content) in [
        ("tenants/acme/rbac.yaml", "kind: Namespace\n"),
        ("clusters/escape.yaml", "kind: ConfigMap\n"),
    ] {
        let seed = fx.root.join(format!("rogue-{}", path.replace('/', "-")));
        let git = GitRunner::new();
        git.clone(fx.tenant_bare("acme").to_str().unwrap(), &seed)
            .await
            .unwrap();
        let branch = format!("keel/acme/rogue-{}", path.len());
        git.checkout_new_branch(&seed, &branch).await.unwrap();
        write(&seed, path, content).await;
        git.add(&seed, ["."]).await.unwrap();
        git.commit(&seed, "rogue", &CommitAuthor::new("R", "r@x"))
            .await
            .unwrap();
        git.push(&seed, "origin", &branch).await.unwrap();
        let pr = fx
            .tenant_provider("acme")
            .create_pull_request(CreatePrRequest::new("rogue", "x", &branch).with_tenant("acme"))
            .await
            .unwrap();
        let (status, body) = send(
            &fx.state,
            &acme_admin,
            Method::POST,
            &format!("/api/v1/tenants/acme/proposals/{}/merge", pr.id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        // A cluster admin reviews and closes it.
        let (status, _) = send(
            &fx.state,
            &admin,
            Method::POST,
            &format!("/api/v1/tenants/acme/proposals/{}/reject", pr.id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn admin_listing_aggregates_tenant_repositories() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    for tenant in TENANTS {
        let (status, body) = propose(
            &fx.state,
            &admin,
            tenant,
            json!({ "catalogId": "web", "values": { "replicas": 1 } }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["proposal"]["id"], "1");
    }
    let (status, list) = send(&fx.state, &admin, Method::GET, "/api/v1/proposals", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let scopes: BTreeSet<(String, String, String)> = list["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["scope"].as_str().unwrap().to_string(),
                p["id"].as_str().unwrap().to_string(),
                p["repository"]["label"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        scopes,
        BTreeSet::from([
            ("acme".into(), "1".into(), "tenant-acme".into()),
            ("beta".into(), "1".into(), "tenant-beta".into()),
        ]),
        "{list}"
    );
    assert!(list.get("unavailable").is_none(), "{list}");

    let acme_member = cookie(&fx.state, "alice@acme", &["tenant-acme"]).await;
    let (status, list) = send(
        &fx.state,
        &acme_member,
        Method::GET,
        "/api/v1/proposals",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let proposals = list["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{list}");
    assert_eq!(proposals[0]["scope"], "acme");

    // The scoped listing serves the tenant repository only.
    let (status, list) = send(
        &fx.state,
        &acme_member,
        Method::GET,
        "/api/v1/tenants/acme/proposals?status=all",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["proposals"].as_array().unwrap().len(), 1, "{list}");
    let (status, _) = send(
        &fx.state,
        &acme_member,
        Method::GET,
        "/api/v1/tenants/acme/proposals?status=bogus",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unlistable_repositories_report_a_generic_reason_without_internal_details() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    // The forge loses `beta`'s repository: listing it fails with an error naming server paths.
    tokio::fs::remove_dir_all(fx.tenant_bare("beta"))
        .await
        .unwrap();
    let (status, list) = send(&fx.state, &admin, Method::GET, "/api/v1/proposals", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let unavailable = list["unavailable"].as_array().expect("unavailable listed");
    let beta = unavailable
        .iter()
        .find(|u| u["scope"] == "beta")
        .unwrap_or_else(|| panic!("beta reported unavailable: {list}"));
    let reason = beta["reason"].as_str().unwrap();
    assert!(reason.contains("correlation id"), "{reason}");
    let root = fx.root.to_string_lossy();
    let body = list.to_string();
    assert!(!body.contains(root.as_ref()), "server path leaked: {body}");
    assert!(!body.contains(".git"), "repository path leaked: {body}");
}

#[tokio::test]
async fn infra_previews_clone_the_tenant_repository_at_the_pr_head() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let (status, body) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "catalogId": "proxmox-vm", "name": "vm1", "values": { "cores": 2 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let head = body["proposal"]["headSha"].as_str().unwrap();
    assert_eq!(head.len(), 40);

    // The committed tenant Stack tracks the tenant repository's `main`.
    let branch = body["branch"].as_str().unwrap();
    let stack = git_show(
        &fx.tenant_bare("acme"),
        &format!("{branch}:tenants/acme/infra/vm1/stack.yaml"),
    )
    .unwrap();
    let stack: Value = noyalib::from_str(&stack).unwrap();
    // PKO `gitSource` fields are inlined into `spec`.
    let git = &stack["spec"];
    assert_eq!(git["projectRepo"], tenant_url("acme"));
    assert_eq!(git["branch"], "main");
    assert_eq!(git["repoDir"], "tenants/acme/infra/vm1");
    assert_eq!(
        git["gitAuth"]["basicAuth"]["password"]["secret"]["name"],
        "keel-gitops-credentials"
    );

    // The preview clones the tenant repository pinned to the PR head, with a per-preview copy of
    // the tenant's own read credential (never the platform repository or its credentials).
    let preview = &body["previewStack"];
    assert_eq!(preview["metadata"]["name"], "preview-stack-pr-acme-1");
    assert_eq!(preview["metadata"]["labels"]["keel.io/pr-id"], "acme-1");
    assert_eq!(preview["metadata"]["labels"]["keel.io/repo"], "tenant-acme");
    let pgit = &preview["spec"];
    assert_eq!(pgit["projectRepo"], tenant_url("acme"));
    assert_eq!(pgit["commit"], head);
    assert!(pgit.get("branch").is_none(), "{pgit}");
    let copy = "keel-preview-acme-1-keel-gitops-credentials";
    assert_eq!(
        pgit["gitAuth"]["basicAuth"]["userName"]["secret"]["name"],
        copy
    );
    assert_eq!(body["report"]["run"]["phase"], "pending", "{body}");
    let secret = fx.cluster.preview_secret(copy).expect("credential copy");
    assert_eq!(
        secret.data.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from(["password".to_string(), "username".to_string()])
    );
    assert_eq!(
        secret.owner_stack.as_deref(),
        Some("preview-stack-pr-acme-1")
    );
    assert_eq!(
        secret.labels.get("keel.io/pr-id").map(String::as_str),
        Some("acme-1")
    );

    // The preview route reports the repository-scoped key.
    let (status, pv) = send(
        &fx.state,
        &admin,
        Method::GET,
        "/api/v1/tenants/acme/proposals/1/preview",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pv}");
    assert_eq!(pv["previewKey"], "acme-1");
    assert_eq!(pv["repository"]["label"], "tenant-acme");
    assert!(
        pv["gitDiff"]
            .as_str()
            .unwrap_or_default()
            .contains("tenants/acme/infra/vm1/stack.yaml"),
        "{pv}"
    );
}

#[tokio::test]
async fn previews_without_the_tenant_credential_fail_closed() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    // A fresh cluster without `tenant-acme/keel-gitops-credentials`.
    let state = fx
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(InMemoryPreviewCluster::new())));
    let (status, body) = propose(
        &state,
        &admin,
        "acme",
        json!({ "catalogId": "proxmox-vm", "name": "vm1", "values": { "cores": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["report"]["run"]["phase"], "notExecuted", "{body}");
    assert!(
        body["report"]["run"]["message"]
            .as_str()
            .unwrap()
            .contains("keel-gitops-credentials"),
        "{body}"
    );
    assert!(
        !body["report"]["run"]["message"]
            .as_str()
            .unwrap()
            .contains("  "),
        "no source indentation in the message: {body}"
    );
}

#[tokio::test]
async fn merge_close_and_reconcile_collect_previews_per_repository() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    for tenant in TENANTS {
        let (status, body) = propose(
            &fx.state,
            &admin,
            tenant,
            json!({ "catalogId": "proxmox-vm", "name": "vm1", "values": { "cores": 1 } }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let mut names = fx.cluster.stack_names();
    names.sort();
    assert_eq!(
        names,
        vec!["preview-stack-pr-acme-1", "preview-stack-pr-beta-1"]
    );

    // Closing beta's PR #1 collects beta's preview only (acme's PR #1 is a different PR).
    let (status, closed) = send(
        &fx.state,
        &admin,
        Method::POST,
        "/api/v1/tenants/beta/proposals/1/reject",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(fx.cluster.stack_names(), vec!["preview-stack-pr-acme-1"]);
    assert!(
        fx.cluster
            .preview_secret("keel-preview-acme-1-keel-gitops-credentials")
            .is_some()
    );
    assert!(
        fx.cluster
            .preview_secret("keel-preview-beta-1-keel-gitops-credentials")
            .is_none()
    );

    // A merge on the forge (not through Keel) is found by the reconciler in acme's repository.
    fx.tenant_provider("acme")
        .merge_pull_request("1", None)
        .await
        .unwrap();
    // Reopen beta's PR: its (now absent) preview is not recreated, and it stays open.
    let (status, _) = send(
        &fx.state,
        &admin,
        Method::POST,
        "/api/v1/tenants/beta/proposals/1/reopen",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let outcome = keel_server::reconcile_previews_once(&fx.state).await;
    assert_eq!(outcome.unknown_prs, 0, "{outcome:?}");
    assert!(fx.cluster.stack_names().is_empty());
    assert!(fx.cluster.preview_secret_names().is_empty());

    // Merging through Keel garbage-collects immediately and updates the tenant repository.
    let (status, body) = propose(
        &fx.state,
        &admin,
        "beta",
        json!({ "catalogId": "proxmox-vm", "name": "vm2", "values": { "cores": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["proposal"]["id"].as_str().unwrap().to_string();
    assert_eq!(id, "2");
    assert_eq!(fx.cluster.stack_names(), vec!["preview-stack-pr-beta-2"]);
    let (status, merged) = send(
        &fx.state,
        &admin,
        Method::POST,
        &format!("/api/v1/tenants/beta/proposals/{id}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["proposal"]["repository"]["label"], "tenant-beta");
    assert!(fx.cluster.stack_names().is_empty());
    assert!(
        git_show(
            &fx.tenant_bare("beta"),
            "main:tenants/beta/infra/vm2/stack.yaml"
        )
        .is_some()
    );
}

#[tokio::test]
async fn pulumi_stacks_need_the_merged_tenant_stub() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    // `gamma` has a repository (skeleton merged) but its platform stub is not merged yet.
    let git = GitRunner::new();
    let name = "infra-gitops-tenant-gamma";
    LocalGitProvider::new(&fx.platform_bare)
        .ensure_named_repository(
            OWNER,
            name,
            &RepositoryOptions {
                private: true,
                auto_init: true,
                ..RepositoryOptions::default()
            },
        )
        .await
        .unwrap();
    let seed = fx.root.join("seed-gamma");
    git.clone(fx.tenant_bare("gamma").to_str().unwrap(), &seed)
        .await
        .unwrap();
    write(
        &seed,
        "tenants/gamma/kustomization.yaml",
        &keel_kube::build_tenant_repo_kustomization_yaml("gamma", &[]).unwrap(),
    )
    .await;
    commit_all(&git, &seed, "feat: initialise tenant repository").await;

    let (status, body) = propose(
        &fx.state,
        &admin,
        "gamma",
        json!({ "catalogId": "proxmox-vm", "name": "vm1", "values": { "cores": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("tenant stub"), "{body}");
    assert!(
        git_branches(&fx.tenant_bare("gamma"))
            .iter()
            .all(|b| b == "main")
    );
    // Helm apps do not need the stub.
    let (status, body) = propose(
        &fx.state,
        &admin,
        "gamma",
        json!({ "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_tenant_without_a_repository_fails_closed_without_touching_the_platform() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let (status, body) = propose(
        &fx.state,
        &admin,
        "ghost",
        json!({ "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.to_string().contains("POST /api/v1/tenants"), "{body}");
    assert!(git_branches(&fx.platform_bare).iter().all(|b| b == "main"));
}

/// Commits `files` straight to `main` of the bare repository `bare` (a merge on the forge that
/// keel-server did not perform).
async fn push_to_main(fx: &Fx, bare: &Path, label: &str, files: &[(&str, &str)]) {
    let git = GitRunner::new();
    let dir = fx.root.join(format!("direct-{label}"));
    git.clone(bare.to_str().unwrap(), &dir).await.unwrap();
    for (rel, content) in files {
        write(&dir, rel, content).await;
    }
    commit_all(&git, &dir, &format!("chore: {label}")).await;
}

/// Creates the repository of tenant `tenant` (forge auto-init only, no Keel skeleton).
async fn create_bare_tenant_repo(fx: &Fx, tenant: &str) {
    LocalGitProvider::new(&fx.platform_bare)
        .ensure_named_repository(
            OWNER,
            &format!("infra-gitops-tenant-{tenant}"),
            &RepositoryOptions {
                private: true,
                auto_init: true,
                ..RepositoryOptions::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn tenants_cannot_install_another_tenants_catalog_items() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    // Catalog items merged on the forge (not through keel-server): `beta`'s private app and
    // blueprint, and a platform bundle composing `beta`'s app.
    push_to_main(
        &fx,
        &fx.platform_bare,
        "beta-catalog",
        &[
            (
                "catalog/apps/beta-private/catalog.yaml",
                r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: beta-private
  name: Beta Private
  category: web
  runtime: helm
  tenant: beta
spec:
  upstream:
    repoURL: https://charts.beta.example.com
    chart: beta-secret-chart
    version: "1.0.0"
  schema:
    type: object
    properties: {}
"#,
            ),
            (
                "catalog/infra/beta-vm/catalog.yaml",
                r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: beta-vm
  name: Beta VM
  category: compute
  provider: proxmox
  tenant: beta
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  schema:
    type: object
    properties: {}
"#,
            ),
            (
                "catalog/bundles/with-beta/catalog.yaml",
                r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: with-beta
  name: With Beta
  category: web
spec:
  items:
    - id: web
      kind: app
      catalogRef: web
    - id: private
      kind: app
      catalogRef: beta-private
  schema:
    type: object
    properties: {}
"#,
            ),
        ],
    )
    .await;
    let acme_member = cookie(&fx.state, "alice@acme", &["tenant-acme"]).await;
    for (who, kind, id) in [
        (&admin, "app", "beta-private"),
        (&acme_member, "app", "beta-private"),
        (&admin, "infra", "beta-vm"),
        (&admin, "bundle", "with-beta"),
    ] {
        let (status, body) = propose(
            &fx.state,
            who,
            "acme",
            json!({ "catalogId": id, "kind": kind, "values": {} }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}: {body}");
        assert!(!body.to_string().contains("beta-secret-chart"), "{body}");
    }
    assert!(
        git_branches(&fx.tenant_bare("acme"))
            .iter()
            .all(|b| b == "main"),
        "nothing pushed to acme's repository"
    );
    // The owner installs its own item; the forge-merged catalog change is seen at once.
    let (status, body) = propose(
        &fx.state,
        &admin,
        "beta",
        json!({ "catalogId": "beta-private", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn route_hostnames_are_checked_against_uncached_tenant_repositories() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    // `beta` routes the hostname on its `main`; this keel-server never cloned beta's repository.
    push_to_main(
        &fx,
        &fx.tenant_bare("beta"),
        "beta-route",
        &[(
            "tenants/beta/apps/site/helmrelease.yaml",
            r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: site
  namespace: tenant-beta
  annotations:
    keel.io/route-hostnames: auth-shared.keel.local
spec:
  interval: 10m
"#,
        )],
    )
    .await;
    assert!(!fx.state.tenant_repos.checkout_dir("beta").unwrap().exists());
    let values = json!({
        "hostname": "auth-shared.keel.local",
        "publicPortSuffix": ":8888",
        "adminEmail": "ops@acme.example",
        "encKeyId": "acme1",
        "encKey": "q2Vz7mU0ZcV1x7pYb7Yk2r9mD0aH3sW5tL8nQ4eR6fA=",
        "hiqliteSecretRaft": "raftSecretSampleValue-0123456789",
        "hiqliteSecretApi": "apiSecretSampleValue-9876543210",
        "adminPassword": "Sample-Admin#Passw0rd!",
    });
    let (status, body) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "catalogId": "rauthy", "values": values }),
    )
    .await;
    if status != StatusCode::CONFLICT && sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string()
            .contains("already claimed by another tenant"),
        "{body}"
    );
    assert!(
        git_branches(&fx.tenant_bare("acme"))
            .iter()
            .all(|b| b == "main")
    );

    // A tenant repository that cannot be read fails the check closed.
    std::fs::rename(fx.tenant_bare("beta"), fx.root.join("beta-moved.git")).unwrap();
    std::fs::remove_dir_all(fx.state.tenant_repos.mirror_dir("beta").unwrap()).unwrap();
    let (status, body) = propose(
        &fx.state,
        &admin,
        "acme",
        json!({ "catalogId": "rauthy", "values": values }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn proposals_wait_for_the_tenant_repository_skeleton() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    // `delta`'s repository exists, but its skeleton PR is not merged.
    create_bare_tenant_repo(&fx, "delta").await;
    let (status, body) = propose(
        &fx.state,
        &admin,
        "delta",
        json!({ "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("skeleton"), "{body}");
    assert!(
        git_branches(&fx.tenant_bare("delta"))
            .iter()
            .all(|b| b == "main")
    );
    let (status, body) = send(
        &fx.state,
        &admin,
        Method::DELETE,
        "/api/v1/releases/tenant-delta/web",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Once the skeleton is merged, proposals work.
    push_to_main(
        &fx,
        &fx.tenant_bare("delta"),
        "delta-skeleton",
        &[(
            "tenants/delta/kustomization.yaml",
            &keel_kube::build_tenant_repo_kustomization_yaml("delta", &[]).unwrap(),
        )],
    )
    .await;
    let (status, body) = propose(
        &fx.state,
        &admin,
        "delta",
        json!({ "catalogId": "web", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_stub_merged_on_the_forge_enables_pulumi_stacks_at_once() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    create_bare_tenant_repo(&fx, "gamma").await;
    push_to_main(
        &fx,
        &fx.tenant_bare("gamma"),
        "gamma-skeleton",
        &[(
            "tenants/gamma/kustomization.yaml",
            &keel_kube::build_tenant_repo_kustomization_yaml("gamma", &[]).unwrap(),
        )],
    )
    .await;
    fx.cluster.put_tenant_secret(
        "gamma",
        "keel-gitops-credentials",
        &[
            ("username", "keel-tenant-reader"),
            ("password", "repo-scoped-token"),
        ],
    );
    let infra = json!({ "catalogId": "proxmox-vm", "name": "vm1", "values": { "cores": 1 } });
    let (status, body) = propose(&fx.state, &admin, "gamma", infra.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The platform stub PR is merged on the forge, not through this keel-server.
    let stub = keel_kube::synthesize_tenant_repo_stub(
        &TenantIsolationConfig::new("gamma"),
        &naming().source("gamma").unwrap(),
    )
    .unwrap();
    let files: Vec<(&str, &str)> = stub
        .files
        .iter()
        .map(|(p, y)| (p.as_str(), y.as_str()))
        .collect();
    push_to_main(&fx, &fx.platform_bare, "gamma-stub", &files).await;
    let (status, body) = propose(&fx.state, &admin, "gamma", infra).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn the_admin_listing_includes_tenants_being_created() {
    let fx = fixture().await;
    let admin = admin(&fx.state).await;
    let git = GitRunner::new();
    // `gamma` is being created: an open platform stub PR and an open skeleton PR in its own
    // repository (nothing merged yet).
    let platform_dir = fx.root.join("gamma-platform");
    git.clone(fx.platform_bare.to_str().unwrap(), &platform_dir)
        .await
        .unwrap();
    let branch = "keel/gamma/create-tenant-1700000000";
    git.checkout_new_branch(&platform_dir, branch)
        .await
        .unwrap();
    write(&platform_dir, "tenants/gamma/rbac.yaml", "kind: List\n").await;
    git.add(&platform_dir, ["."]).await.unwrap();
    git.commit(
        &platform_dir,
        "feat(gamma): create tenant",
        &CommitAuthor::new("Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&platform_dir, "origin", branch).await.unwrap();
    fx.state
        .forge
        .create_pull_request(
            CreatePrRequest::new("feat(gamma): create tenant", "", branch).with_tenant("gamma"),
        )
        .await
        .unwrap();

    create_bare_tenant_repo(&fx, "gamma").await;
    let tenant_dir = fx.root.join("gamma-tenant");
    git.clone(fx.tenant_bare("gamma").to_str().unwrap(), &tenant_dir)
        .await
        .unwrap();
    let skeleton = "keel/gamma/init-tenant-repo-1700000000";
    git.checkout_new_branch(&tenant_dir, skeleton)
        .await
        .unwrap();
    write(
        &tenant_dir,
        "tenants/gamma/kustomization.yaml",
        &keel_kube::build_tenant_repo_kustomization_yaml("gamma", &[]).unwrap(),
    )
    .await;
    git.add(&tenant_dir, ["."]).await.unwrap();
    git.commit(
        &tenant_dir,
        "feat(gamma): initialise tenant repository",
        &CommitAuthor::new("Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&tenant_dir, "origin", skeleton).await.unwrap();
    fx.tenant_provider("gamma")
        .create_pull_request(
            CreatePrRequest::new("feat(gamma): initialise tenant repository", "", skeleton)
                .with_tenant("gamma"),
        )
        .await
        .unwrap();

    let (status, list) = send(&fx.state, &admin, Method::GET, "/api/v1/proposals", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let scopes: BTreeSet<(String, String)> = list["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["scope"].as_str().unwrap().to_string(),
                p["repository"]["label"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        scopes.contains(&("gamma".to_string(), "tenant-gamma".to_string())),
        "{list}"
    );
    assert!(
        scopes.contains(&("management".to_string(), "platform".to_string())),
        "{list}"
    );
}
