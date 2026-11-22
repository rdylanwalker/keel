use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use keel_core::{AuthPhase, OidcConfig};
use keel_git::{CommitAuthor, GitRunner, LocalGitProvider};
use keel_kube::{
    CrdReconcileStatus, CrdWatchEvent, InMemorySecretStore, SecretStateStore, TelemetryMux,
    WatchedCrdKind, user_git_creds_secret_name,
};
use keel_server::{
    AppState, SESSION_COOKIE_NAME, SessionClaims, build_router, encrypt_session_token,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

struct TestFixture {
    _temp_dir: TempDir,
    state: AppState,
    secret_store: Arc<InMemorySecretStore>,
}

/// Fixture for a provisioned tenant `acme` (its Pulumi state passphrase Secret exists).
/// Cluster-reachable GitOps URL used by the platform-written tenant source stubs.
const TENANT_GIT_URL: &str =
    "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git";
/// Forgejo HTTPS origin (`forge.publicUrl`) the fixture server is configured with.
const FORGE_PUBLIC_URL: &str = "https://forgejo.keel.local:8888";
/// URL tenant/preview PKO Stacks clone: PKO rejects the http Service URL Flux uses, so the
/// in-cluster Forgejo is cloned through its HTTPS ingress origin.
const TENANT_PKO_GIT_URL: &str = "https://forgejo.keel.local:8888/keel/infra-gitops.git";

fn tenant_git_source_stub(tenant: &str) -> String {
    format!(
        "apiVersion: source.toolkit.fluxcd.io/v1\nkind: GitRepository\nmetadata:\n  name: keel-gitops\n  namespace: tenant-{tenant}\nspec:\n  interval: 1m\n  url: {TENANT_GIT_URL}\n  ref:\n    branch: main\n  secretRef:\n    name: git-credentials\n"
    )
}

async fn setup_fixture() -> TestFixture {
    let fixture = setup_fixture_without_tenant_passphrase().await;
    SecretStateStore::ensure_pulumi_passphrase(fixture.secret_store.as_ref(), "tenant-acme")
        .await
        .unwrap();
    fixture
}

async fn setup_fixture_without_tenant_passphrase() -> TestFixture {
    setup_fixture_at("remote.git", "workdir").await
}

/// Fixture whose bare remote / working tree live at `bare_name` / `workdir_name` inside the
/// temporary directory (e.g. paths containing spaces, as on `keel serve --repo-dir`).
async fn setup_fixture_at(bare_name: &str, workdir_name: &str) -> TestFixture {
    let temp_dir = tempfile::tempdir().expect("create tempdir");
    let bare_repo = temp_dir.path().join(bare_name);
    let workdir = temp_dir.path().join(workdir_name);
    if let Some(parent) = bare_repo.parent() {
        tokio::fs::create_dir_all(parent).await.unwrap();
    }

    let git = GitRunner::new();
    git.init_bare(&bare_repo).await.expect("init bare repo");
    git.clone(bare_repo.to_str().unwrap(), &workdir)
        .await
        .expect("clone bare repo");

    // Seed providers/registry.yaml, catalog/apps/grafana/catalog.yaml, catalog/infra/*
    tokio::fs::create_dir_all(workdir.join("providers"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("providers/registry.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: ProviderRegistry
providers:
  - id: proxmox
    name: Proxmox VE
    enabled: true
    version: "0.71.0"
    plugin: bpg/proxmox
    terraformBridge: true
  - id: aws
    name: AWS
    enabled: false
    version: "6.68.0"
    plugin: pulumi/aws
    terraformBridge: false
"#,
    )
    .await
    .unwrap();

    tokio::fs::create_dir_all(workdir.join("catalog/apps/grafana"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/apps/grafana/catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: grafana
  name: Grafana
  description: Observability dashboards
  category: observability
  runtime: helm
spec:
  upstream:
    repoURL: https://grafana.github.io/helm-charts
    chart: grafana
    version: "8.8.2"
  progressiveDelivery:
    flaggerSupported: true
    defaultCanary: false
  schema:
    type: object
    required:
      - replicas
    properties:
      replicas:
        type: integer
        minimum: 1
      adminPassword:
        type: string
        format: password
        writeOnly: true
"#,
    )
    .await
    .unwrap();

    tokio::fs::create_dir_all(workdir.join("catalog/infra/proxmox-vm"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/infra/proxmox-vm/catalog.yaml"),
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
    .await
    .unwrap();

    tokio::fs::create_dir_all(workdir.join("catalog/infra/aws-eks"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/infra/aws-eks/catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: aws-eks
  name: AWS EKS
  category: kubernetes
  provider: aws
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  schema:
    type: object
    properties:
      nodeCount:
        type: integer
"#,
    )
    .await
    .unwrap();

    // Seed one tenant stack and release for GET /api/v1/stacks & /releases
    tokio::fs::create_dir_all(workdir.join("tenants/acme/infra/net"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("tenants/acme/infra/net/stack.yaml"),
        r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: acme-net
  namespace: tenant-acme
  labels:
    keel.io/tenant: acme
    keel.io/provider: proxmox
spec:
  stack: keel/acme/net
  protect: false
"#,
    )
    .await
    .unwrap();

    tokio::fs::create_dir_all(workdir.join("tenants/acme/apps/redis"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("tenants/acme/apps/redis/helmrelease.yaml"),
        r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: redis
  namespace: tenant-acme
  labels:
    keel.io/tenant: acme
spec:
  chart:
    spec:
      chart: redis
      version: "20.0.0"
"#,
    )
    .await
    .unwrap();

    // Platform-written tenant Git source stubs (`tenants/<id>/git-repository.yaml`): tenant PKO
    // Stacks clone only their tenant's own source (fail closed without one).
    for tenant in ["acme", "default"] {
        tokio::fs::create_dir_all(workdir.join(format!("tenants/{tenant}")))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join(format!("tenants/{tenant}/git-repository.yaml")),
            tenant_git_source_stub(tenant),
        )
        .await
        .unwrap();
    }

    git.add(&workdir, ["."]).await.unwrap();
    git.commit(
        &workdir,
        "chore: initial repo seed",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();

    let forge = Arc::new(LocalGitProvider::new(&bare_repo));
    let secret_store = Arc::new(InMemorySecretStore::new());

    let state = AppState::new(
        forge,
        secret_store.clone(),
        TelemetryMux::default(),
        git,
        &workdir,
        bare_repo.to_string_lossy().to_string(),
        AuthPhase::OidcActive,
    )
    .with_forge_public_url(Some(FORGE_PUBLIC_URL.to_string()));

    TestFixture {
        _temp_dir: temp_dir,
        state,
        secret_store,
    }
}

async fn login_with_groups_and_name(
    fixture: &TestFixture,
    sub: &str,
    name: &str,
    groups: Vec<&str>,
) -> String {
    let session_key = fixture
        .secret_store
        .get_or_create_session_key()
        .await
        .unwrap();
    let claims = SessionClaims {
        sub: sub.to_string(),
        email: Some(sub.to_string()),
        name: Some(name.to_string()),
        groups: groups.into_iter().map(str::to_string).collect(),
        auth_phase: AuthPhase::OidcActive,
        exp: chrono::Utc::now().timestamp() + 3600,
    };
    let token = encrypt_session_token(&session_key, &claims).unwrap();
    format!("{SESSION_COOKIE_NAME}={token}")
}

async fn login_with_groups(fixture: &TestFixture, sub: &str, groups: Vec<&str>) -> String {
    login_with_groups_and_name(fixture, sub, sub, groups).await
}

async fn login_and_get_cookie(fixture: &TestFixture) -> String {
    login_with_groups_and_name(
        fixture,
        "admin@keel.local",
        "Keel Admin",
        vec!["keel-admins"],
    )
    .await
}

#[tokio::test]
async fn test_healthz_metrics_and_spa_fallback() {
    let fixture = setup_fixture().await;
    let app = build_router(fixture.state.clone());

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"OK");

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let metrics_text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(metrics_text.contains("keel_server_up 1"));

    let res = app
        .oneshot(
            Request::builder()
                .uri("/proposals/42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_security_headers_on_spa_api_and_rejected_responses() {
    let fixture = setup_fixture().await;
    *fixture.state.oidc_config.write().await = Some(OidcConfig {
        enabled: true,
        issuer_url: "https://idp.keel.local:8888/auth/v1".to_string(),
        client_id: "keel-ui".to_string(),
        redirect_url: "https://keel.local/api/v1/auth/callback".to_string(),
        scopes: vec!["openid".to_string()],
    });
    let app = build_router(fixture.state.clone());
    let expected_csp = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self' data:; frame-ancestors 'none'; base-uri 'self'; form-action 'self' https://idp.keel.local:8888";
    for (method, uri) in [
        (Method::GET, "/"),
        (Method::GET, "/proposals/42"),
        (Method::GET, "/api/v1/catalog"), // 401 without session
        (Method::GET, "/healthz"),
        (Method::POST, "/api/v1/tenants"), // CSRF / auth rejection
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(uri)
                    .header("origin", "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let h = res.headers();
        assert_eq!(h["content-security-policy"], expected_csp, "{method} {uri}");
        assert_eq!(h["x-content-type-options"], "nosniff", "{uri}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{uri}");
        assert_eq!(h["x-frame-options"], "DENY", "{uri}");
    }
}

#[tokio::test]
async fn test_session_cookie_auth_middleware_and_status() {
    let fixture = setup_fixture().await;
    let app = build_router(fixture.state.clone());

    // Unauthenticated access to protected route is rejected
    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Tampered session cookie is rejected
    let bad_auth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(
                    COOKIE,
                    format!("{SESSION_COOKIE_NAME}=invalid-tampered-cookie"),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_auth.status(), StatusCode::UNAUTHORIZED);

    // Valid encrypted OIDC session cookie grants access
    let cookie = login_and_get_cookie(&fixture).await;

    let status_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/status")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status_res.status(), StatusCode::OK);
    let status_json: Value =
        serde_json::from_slice(&status_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(status_json["authenticated"], true);
    assert_eq!(status_json["sub"], "admin@keel.local");
    assert_eq!(status_json["name"], "Keel Admin");
    assert_eq!(status_json["email"], "admin@keel.local");
    assert_eq!(status_json["groups"], serde_json::json!(["keel-admins"]));
    assert_eq!(status_json["isAdmin"], true);

    // POST /api/v1/auth/logout clears keel_session and keel_pkce cookies
    let logout_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout_res.status(), StatusCode::OK);
    let set_cookies: Vec<String> = logout_res
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    assert!(
        set_cookies
            .iter()
            .any(|c| c.starts_with("keel_session=;") && c.contains("Max-Age=0")),
        "expected Set-Cookie clearing keel_session, got {set_cookies:?}"
    );
    assert!(
        set_cookies
            .iter()
            .any(|c| c.starts_with("keel_pkce=;") && c.contains("Max-Age=0")),
        "expected Set-Cookie clearing keel_pkce, got {set_cookies:?}"
    );
    assert!(
        set_cookies
            .iter()
            .any(|c| c.starts_with("keel_logged_out=1;")),
        "expected Set-Cookie setting keel_logged_out=1, got {set_cookies:?}"
    );
    let logout_json: Value =
        serde_json::from_slice(&logout_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(logout_json["authenticated"], false);

    // Subsequent /api/v1/auth/login with keel_logged_out=1 appends prompt=login
    *fixture.state.oidc_config.write().await = Some(OidcConfig {
        enabled: true,
        issuer_url: "https://idp.keel.local/realms/keel".to_string(),
        client_id: "keel-ui".to_string(),
        redirect_url: "https://keel.local/api/v1/auth/callback".to_string(),
        scopes: vec!["openid".to_string()],
    });
    let reauth_login = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/login")
                .header(COOKIE, "keel_logged_out=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reauth_login.status(), StatusCode::FOUND);
    let reauth_loc = reauth_login
        .headers()
        .get(axum::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        reauth_loc.contains("prompt=login"),
        "expected prompt=login in redirect URL, got {reauth_loc}"
    );
}

#[tokio::test]
async fn test_oidc_pkce_login_scaffolding() {
    let fixture = setup_fixture().await;

    // Disabled by default -> 503
    let app = build_router(fixture.state.clone());
    let disabled_res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(disabled_res.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Enable OIDC -> 302 redirect with PKCE S256 & encrypted keel_pkce cookie
    *fixture.state.oidc_config.write().await = Some(OidcConfig {
        enabled: true,
        issuer_url: "https://idp.keel.local/realms/keel".to_string(),
        client_id: "keel-ui".to_string(),
        redirect_url: "https://keel.local/api/v1/auth/callback".to_string(),
        scopes: vec!["openid".to_string(), "profile".to_string()],
    });

    let app = build_router(fixture.state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FOUND);
    let location = res.headers().get(LOCATION).unwrap().to_str().unwrap();
    assert!(location.contains("code_challenge_method=S256"));
    assert!(location.contains("client_id=keel-ui"));
    let pkce_cookie = res.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
    assert!(pkce_cookie.contains("keel_pkce="));
    assert!(pkce_cookie.contains("HttpOnly"));
}

#[tokio::test]
async fn test_catalog_listing_filters_by_provider_registry() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(body["applications"].as_array().unwrap().len(), 1);
    assert_eq!(body["applications"][0]["metadata"]["id"], "grafana");

    // proxmox is enabled, aws is disabled -> only proxmox-vm is returned
    let blueprints = body["blueprints"].as_array().unwrap();
    assert_eq!(blueprints.len(), 1);
    assert_eq!(blueprints[0]["metadata"]["id"], "proxmox-vm");
    assert_eq!(body["enabledProviders"], json!(["proxmox"]));

    // Built-in `kubernetes` blueprints are returned even when omitted from registry.yaml
    let k8s_dir = fixture
        .state
        .repo_workdir
        .join("catalog/infra/canary-gateway-config");
    tokio::fs::create_dir_all(&k8s_dir).await.unwrap();
    tokio::fs::write(
        k8s_dir.join("catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: canary-gateway-config
  name: Canary Gateway & MetricTemplates
  category: progressive-delivery
  provider: kubernetes
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  schema:
    type: object
    properties: {}
"#,
    )
    .await
    .unwrap();
    let res2 = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&res2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let blueprints2 = body2["blueprints"].as_array().unwrap();
    assert!(
        blueprints2
            .iter()
            .any(|bp| bp["metadata"]["id"] == "canary-gateway-config")
    );
}

#[tokio::test]
async fn test_proposal_create_preview_merge_and_stacks_releases() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Invalid schema payload -> 422
    let invalid_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "values": { "replicas": 0 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // 2. Secret field without tenant age key: either auto-generates SOPS age key & encrypts (201)
    //    when sops/age-keygen are installed, or rejects with 400 when unavailable (never commits plaintext!).
    let no_age_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "name": "grafana-secret-test",
                        "values": { "replicas": 2, "adminPassword": "super-secret-password" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let no_age_status = no_age_res.status();
    assert!(
        no_age_status == StatusCode::CREATED || no_age_status == StatusCode::BAD_REQUEST,
        "unexpected status for secret proposal without pre-existing age key: {no_age_status}"
    );
    if no_age_status == StatusCode::CREATED {
        let secret_body: Value =
            serde_json::from_slice(&no_age_res.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(
            fixture
                .state
                .secret_store
                .get_tenant_age_key("acme")
                .await
                .unwrap()
                .is_some()
        );
        let secret_branch = secret_body["branch"].as_str().unwrap();
        fixture
            .state
            .git_runner
            .checkout_branch(&fixture.state.repo_workdir, secret_branch)
            .await
            .unwrap();
        // Helm app secret fields: a SOPS-encrypted `<instance>-secrets` Secret (one values
        // fragment per field) applied by the root tenants Kustomization and merged via
        // `valuesFrom`.
        let app_dir = fixture
            .state
            .repo_workdir
            .join("tenants/acme/apps/grafana-secret-test");
        let enc_yaml = tokio::fs::read_to_string(app_dir.join("secrets.enc.yaml"))
            .await
            .unwrap();
        assert!(enc_yaml.contains("ENC[AES256_GCM,"));
        assert!(enc_yaml.contains("kind: Secret"));
        assert!(enc_yaml.contains("adminPassword:"));
        assert!(!enc_yaml.contains("super-secret-password"));
        assert!(!app_dir.join("config").exists());
        assert!(!app_dir.join("config-kustomization.yaml").exists());
        let hr: Value = noyalib::from_str(
            &tokio::fs::read_to_string(app_dir.join("helmrelease.yaml"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            hr["spec"]["valuesFrom"],
            json!([{
                "kind": "Secret",
                "name": "grafana-secret-test-secrets",
                "valuesKey": "adminPassword"
            }])
        );
        assert!(!hr.to_string().contains("super-secret-password"));
        let tenants_k = tokio::fs::read_to_string(
            fixture
                .state
                .repo_workdir
                .join("tenants/kustomization.yaml"),
        )
        .await
        .unwrap();
        assert!(tenants_k.contains("acme/apps/grafana-secret-test/secrets.enc.yaml"));
        assert!(!tenants_k.contains("acme/apps/grafana-secret-test/config-kustomization.yaml"));
        fixture
            .state
            .git_runner
            .checkout_branch(&fixture.state.repo_workdir, "main")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    // 3. Valid proposal -> 201 Created, PR opened, preview synthesized
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "name": "grafana-prod",
                        "values": { "replicas": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_slice(&create_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let pr_id = created["proposal"]["id"].as_str().unwrap().to_string();
    assert!(
        created["branch"]
            .as_str()
            .unwrap()
            .starts_with("keel/acme/")
    );
    assert_eq!(created["previewStack"]["spec"]["preview"], true);
    assert_eq!(created["report"]["summary"]["create"], 1);

    // 4. List open proposals
    let list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let list_json: Value =
        serde_json::from_slice(&list_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let expected_open_prs = if no_age_status == StatusCode::CREATED {
        2
    } else {
        1
    };
    assert_eq!(
        list_json["proposals"].as_array().unwrap().len(),
        expected_open_prs
    );

    // 5. Get proposal preview
    let preview_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr_id}/preview"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preview_res.status(), StatusCode::OK);
    let preview_json: Value =
        serde_json::from_slice(&preview_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(preview_json["prId"], pr_id);
    assert_eq!(preview_json["report"]["summary"]["create"], 1);
    assert!(!preview_json["comments"].as_array().unwrap().is_empty());

    // 6. Merge proposal -> merges PR & plans ephemeral preview GC
    let merge_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr_id}/merge"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(merge_res.status(), StatusCode::OK);
    let merge_json: Value =
        serde_json::from_slice(&merge_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(merge_json["proposal"]["state"], "merged");
    // Only the preview Stack: previews clone through PKO `gitSource` (no Flux GitRepository).
    assert_eq!(merge_json["gcPlan"]["targets"].as_array().unwrap().len(), 1);

    // 7. Stacks & Releases endpoints
    let stacks_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/stacks?tenant=acme")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stacks_res.status(), StatusCode::OK);
    let stacks_json: Value =
        serde_json::from_slice(&stacks_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(stacks_json["stacks"].as_array().unwrap().len(), 1);
    assert_eq!(stacks_json["stacks"][0]["name"], "acme-net");

    let releases_res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/releases?tenant=acme")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(releases_res.status(), StatusCode::OK);
    let releases_json: Value =
        serde_json::from_slice(&releases_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let releases = releases_json["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 2);
    let release_names: Vec<&str> = releases.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(release_names.contains(&"redis"));
    assert!(release_names.contains(&"grafana-prod"));
}

#[tokio::test]
async fn test_sse_events_stream() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/events/stream")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Publish an event after subscribing
    fixture.state.telemetry.publish(CrdWatchEvent::new(
        WatchedCrdKind::HelmRelease,
        "tenant-acme",
        "grafana",
        CrdReconcileStatus::Ready,
        "HelmRelease reconciled",
        Some("main@sha1:abc123".to_string()),
    ));

    let mut body = resp.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
        .await
        .expect("SSE frame timeout")
        .expect("SSE stream ended")
        .expect("SSE frame error");
    let data = frame.into_data().expect("data frame");
    let text = String::from_utf8(data.to_vec()).unwrap();
    assert!(text.contains("event: crd-watch"));
    assert!(text.contains("\"name\":\"grafana\""));
    assert!(text.contains("\"status\":\"Ready\""));
}

#[tokio::test]
async fn test_put_git_credentials_encrypts_and_never_leaks_secret() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let pat = "glpat-super-secret-token-value-12345";
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/profile/git-credentials")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "username": "alice",
                        "personalAccessToken": pat
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body_text = String::from_utf8(body_bytes.to_vec()).unwrap();

    // Never echo secret back in HTTP response
    assert!(!body_text.contains(pat));
    let body_json: Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(body_json["stored"], true);
    assert_eq!(body_json["hasPersonalAccessToken"], true);
    assert_eq!(body_json["hasSshKey"], false);

    // Inspect raw Secret in InMemorySecretStore to confirm AES-256-GCM encryption
    let secret_name = user_git_creds_secret_name("admin@keel.local").unwrap();
    let raw_secret = fixture
        .secret_store
        .inspect_raw_secret("keel-system", &secret_name)
        .await
        .expect("secret stored");
    let data = raw_secret.data.expect("secret data");
    assert_eq!(data.get("algorithm").unwrap().0, b"AES-256-GCM");
    let ciphertext = &data.get("ciphertext").unwrap().0;
    assert!(
        !String::from_utf8_lossy(ciphertext).contains(pat),
        "ciphertext must not contain plaintext PAT"
    );

    // Verify decryption round-trip with session key
    let key = fixture
        .secret_store
        .get_or_create_session_key()
        .await
        .unwrap();
    let decrypted = fixture
        .secret_store
        .get_user_git_creds("admin@keel.local", &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decrypted.personal_access_token.as_deref(), Some(pat));
    assert_eq!(decrypted.username.as_deref(), Some("alice"));
}

#[tokio::test]
async fn test_tenants_list_create_delete_and_protect_default() {
    let fixture = setup_fixture().await;
    // Seed a `default` tenant directory alongside `acme`
    tokio::fs::create_dir_all(fixture.state.repo_workdir.join("tenants/default"))
        .await
        .unwrap();
    tokio::fs::write(
        fixture.state.repo_workdir.join("tenants/default/rbac.yaml"),
        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: tenant-default\n",
    )
    .await
    .unwrap();
    fixture
        .state
        .git_runner
        .add(&fixture.state.repo_workdir, ["tenants/default/rbac.yaml"])
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .commit(
            &fixture.state.repo_workdir,
            "chore: add default tenant",
            &CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(
            &fixture.state.repo_workdir,
            "origin",
            "HEAD:refs/heads/main",
        )
        .await
        .unwrap();

    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. GET /api/v1/tenants returns deterministic sorted list
    let list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/tenants")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let list_json: Value =
        serde_json::from_slice(&list_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        list_json["tenants"],
        json!([
            { "id": "acme", "namespace": "tenant-acme" },
            { "id": "default", "namespace": "tenant-default" }
        ])
    );

    // 2. POST /api/v1/tenants rejects invalid DNS-1123 id
    let bad_id_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "Invalid_Tenant" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_id_res.status(), StatusCode::BAD_REQUEST);

    // 3. POST /api/v1/tenants rejects duplicate tenant
    let dup_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "acme" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dup_res.status(), StatusCode::CONFLICT);

    // 4. POST /api/v1/tenants creates proposal branch, PR, rbac.yaml, and telemetry event
    let mut rx = fixture.state.telemetry.subscribe();
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "payments" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let create_json: Value =
        serde_json::from_slice(&create_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(create_json["tenant"]["id"], "payments");
    assert_eq!(create_json["tenant"]["namespace"], "tenant-payments");
    assert!(
        create_json["branch"]
            .as_str()
            .unwrap()
            .starts_with("keel/payments/create-tenant-")
    );
    assert_eq!(create_json["proposal"]["state"], "open");

    // The tenant bundle grants keel-server its per-tenant access (keel-server-tenant RoleBinding).
    let branch = create_json["branch"].as_str().unwrap();
    let shown = std::process::Command::new("git")
        .arg("-C")
        .arg(&fixture.state.repo_workdir)
        .args(["show", &format!("{branch}:tenants/payments/rbac.yaml")])
        .output()
        .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let rbac = String::from_utf8(shown.stdout).unwrap();
    assert!(rbac.contains("name: keel-server-tenant"));
    assert!(rbac.contains("namespace: keel-system"));

    let ev = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("telemetry timeout")
        .expect("telemetry event");
    assert_eq!(ev.namespace, "tenant-payments");

    // 5. DELETE /api/v1/tenants/default is rejected with 400 Bad Request
    let del_default_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/v1/tenants/default")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_default_res.status(), StatusCode::BAD_REQUEST);

    // 6. DELETE /api/v1/tenants/missing returns 404 Not Found
    let del_missing_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/v1/tenants/missing")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_missing_res.status(), StatusCode::NOT_FOUND);

    // 7. DELETE /api/v1/tenants/acme opens a deletion proposal PR
    let del_res = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/v1/tenants/acme")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_res.status(), StatusCode::OK);
    let del_json: Value =
        serde_json::from_slice(&del_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(del_json["id"], "acme");
    assert!(
        del_json["branch"]
            .as_str()
            .unwrap()
            .starts_with("keel/acme/delete-tenant-")
    );
    assert_eq!(del_json["proposal"]["state"], "open");
}

#[tokio::test]
async fn test_register_catalog_app_endpoint() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Invalid DNS-1123 id -> 400 Bad Request
    let bad_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "id": "Bad_Id",
                        "name": "Bad App",
                        "category": "tools",
                        "runtime": "helm",
                        "repoUrl": "https://charts.example.com",
                        "chart": "bad"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_res.status(), StatusCode::BAD_REQUEST);

    // 2. Valid registration without schema uses default replicaCount schema
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "id": "nginx",
                        "name": "NGINX Ingress",
                        "category": "networking",
                        "runtime": "helm",
                        "repoUrl": "https://kubernetes.github.io/ingress-nginx",
                        "chart": "ingress-nginx",
                        "version": "4.12.0",
                        "flaggerSupported": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_slice(&create_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(created["application"]["apiVersion"], "keel.io/v1alpha1");
    assert_eq!(created["application"]["kind"], "CatalogApplication");
    assert_eq!(created["application"]["metadata"]["id"], "nginx");
    assert_eq!(
        created["application"]["spec"]["progressiveDelivery"]["flaggerSupported"],
        true
    );
    let props = created["application"]["spec"]["schema"]["properties"]
        .as_object()
        .expect("schema properties object");
    assert!(
        props.contains_key("controller")
            || props.get("replicaCount").and_then(|r| r.get("default")) == Some(&json!(1)),
        "expected upstream ingress-nginx schema or fallback replicaCount schema, got: {props:?}"
    );
    assert!(
        created["branch"]
            .as_str()
            .unwrap()
            .starts_with("keel/catalog/add-nginx-")
    );
    assert_eq!(created["proposal"]["state"], "open");
}

#[tokio::test]
async fn test_register_catalog_infra_blueprint_endpoint() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Unknown kind -> 400 Bad Request
    let unknown_kind_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "unknown",
                        "id": "rds-postgres",
                        "name": "RDS Postgres",
                        "category": "database"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_kind_res.status(), StatusCode::BAD_REQUEST);

    // 2. kind: "infra" without provider -> 400 Bad Request
    let missing_provider_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "infra",
                        "id": "rds-postgres",
                        "name": "RDS Postgres",
                        "category": "database"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_provider_res.status(), StatusCode::BAD_REQUEST);

    // 3. Valid kind: "infra" registration creates catalog.yaml and Pulumi.yaml on a PR branch
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "infra",
                        "id": "rds-postgres",
                        "name": "RDS Postgres",
                        "category": "database",
                        "provider": "aws",
                        "protectByDefault": true,
                        "terraformProvider": "hashicorp/aws"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_slice(&create_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert!(created.get("application").is_none());
    assert_eq!(created["blueprint"]["apiVersion"], "keel.io/v1alpha1");
    assert_eq!(created["blueprint"]["kind"], "CatalogInfraBlueprint");
    assert_eq!(created["blueprint"]["metadata"]["id"], "rds-postgres");
    assert_eq!(created["blueprint"]["metadata"]["provider"], "aws");
    assert_eq!(created["blueprint"]["spec"]["pulumiRuntime"], "yaml");
    assert_eq!(created["blueprint"]["spec"]["entrypoint"], "Pulumi.yaml");
    assert_eq!(created["blueprint"]["spec"]["protectByDefault"], true);
    assert_eq!(
        created["blueprint"]["spec"]["schema"]["properties"]["name"]["default"],
        "workload-1"
    );
    let branch = created["branch"].as_str().unwrap();
    assert!(branch.starts_with("keel/catalog/add-rds-postgres-"));
    assert_eq!(created["proposal"]["state"], "open");

    // Verify catalog.yaml and Pulumi.yaml were committed to the proposal branch
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();
    let catalog_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("catalog/infra/rds-postgres/catalog.yaml"),
    )
    .await
    .unwrap();
    assert!(catalog_yaml.contains("CatalogInfraBlueprint"));
    assert!(catalog_yaml.contains("provider: aws"));

    let pulumi_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("catalog/infra/rds-postgres/Pulumi.yaml"),
    )
    .await
    .unwrap();
    assert!(pulumi_yaml.contains("name: rds-postgres"));
    assert!(pulumi_yaml.contains("runtime: yaml"));
    assert!(pulumi_yaml.contains("terraform-provider"));
    assert!(pulumi_yaml.contains("hashicorp/aws"));
}

#[tokio::test]
async fn test_register_catalog_bundle_endpoint() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "bundle",
                        "id": "vm-plus-grafana",
                        "name": "VM + Grafana Bundle",
                        "category": "Platform",
                        "description": "Provisions VM then deploys Grafana",
                        "items": [
                            {
                                "id": "vm",
                                "kind": "infra",
                                "catalogRef": "proxmox-vm"
                            },
                            {
                                "id": "monitoring",
                                "kind": "app",
                                "catalogRef": "grafana",
                                "dependsOn": ["vm"],
                                "targetClusterFrom": "vm",
                                "targetNamespace": "monitoring"
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["bundle"]["metadata"]["id"], "vm-plus-grafana");
    assert!(body["bundle"]["spec"]["schema"]["properties"]["vm"].is_object());
    assert!(body["bundle"]["spec"]["schema"]["properties"]["monitoring"].is_object());
    let branch = body["branch"].as_str().unwrap();
    assert!(branch.starts_with("keel/catalog/add-vm-plus-grafana-"));

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();
    let catalog_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("catalog/bundles/vm-plus-grafana/catalog.yaml"),
    )
    .await
    .unwrap();
    assert!(catalog_yaml.contains("CatalogBundle"));
    assert!(catalog_yaml.contains("targetClusterFrom: vm"));
}

#[tokio::test]
async fn test_register_and_propose_bundle_with_flagger_canary_app() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;

    // Seed a non-Flagger app (`redis`) in the repo
    let redis_dir = fixture.state.repo_workdir.join("catalog/apps/redis");
    tokio::fs::create_dir_all(&redis_dir).await.unwrap();
    tokio::fs::write(
        redis_dir.join("catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: redis
  name: Redis
  category: databases
  runtime: helm
spec:
  upstream:
    repoURL: https://charts.bitnami.com/bitnami
    chart: redis
    version: "20.1.0"
  progressiveDelivery:
    flaggerSupported: false
  schema:
    type: object
    properties: {}
"#,
    )
    .await
    .unwrap();
    fixture
        .state
        .git_runner
        .add(
            &fixture.state.repo_workdir,
            ["catalog/apps/redis/catalog.yaml"],
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .commit(
            &fixture.state.repo_workdir,
            "feat: seed redis app",
            &CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(&fixture.state.repo_workdir, "origin", "main")
        .await
        .unwrap();

    let app = build_router(fixture.state.clone());

    // 1. Rejects canary: true when referenced app has flaggerSupported=false (redis)
    let bad_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "bundle",
                        "id": "bad-canary-bundle",
                        "name": "Bad Canary Bundle",
                        "category": "Platform",
                        "items": [
                            {
                                "id": "cache",
                                "kind": "app",
                                "catalogRef": "redis",
                                "canary": true
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_res.status(), StatusCode::BAD_REQUEST);

    // 2. Accepts canary: true when referenced app has flaggerSupported=true (grafana)
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "bundle",
                        "id": "canary-bundle",
                        "name": "Canary Bundle",
                        "category": "Platform",
                        "items": [
                            {
                                "id": "vm",
                                "kind": "infra",
                                "catalogRef": "proxmox-vm"
                            },
                            {
                                "id": "web",
                                "kind": "app",
                                "catalogRef": "grafana",
                                "dependsOn": ["vm"],
                                "canary": true
                            }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let create_body: Value =
        serde_json::from_slice(&create_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(create_body["bundle"]["spec"]["items"][1]["canary"], true);

    // Merge catalog PR to main so POST /api/v1/proposals can load the bundle
    let catalog_pr_id = create_body["proposal"]["id"].as_str().unwrap();
    let merge_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{catalog_pr_id}/merge"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(merge_res.status(), StatusCode::OK);

    // 3. Propose the bundle and verify Flagger Canary + NetworkPolicy + helmrelease-canary.yaml
    let prop_res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "catalogId": "canary-bundle",
                        "kind": "bundle",
                        "tenant": "acme",
                        "name": "canary-stack",
                        "values": {
                            "vm": { "cores": 2 },
                            "web": { "replicas": 2 }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prop_res.status(), StatusCode::CREATED);
    let prop_body: Value =
        serde_json::from_slice(&prop_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let prop_branch = prop_body["branch"].as_str().unwrap();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, prop_branch)
        .await
        .unwrap();

    let item_dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/bundles/canary-stack/web");
    assert!(
        tokio::fs::try_exists(item_dir.join("helmrelease-canary.yaml"))
            .await
            .unwrap()
    );
    let canary_yaml = tokio::fs::read_to_string(item_dir.join("canary.yaml"))
        .await
        .unwrap();
    assert!(canary_yaml.contains("kind: Canary"));
    assert!(canary_yaml.contains("name: canary-stack-web"));
    assert!(canary_yaml.contains("keel.io/bundle: canary-stack"));
    assert!(
        tokio::fs::try_exists(item_dir.join("networkpolicy-canary.yaml"))
            .await
            .unwrap()
    );
    let kustomization_yaml = tokio::fs::read_to_string(item_dir.join("kustomization.yaml"))
        .await
        .unwrap();
    assert!(kustomization_yaml.contains("helmrelease-canary.yaml"));
    assert!(kustomization_yaml.contains("canary.yaml"));
    assert!(kustomization_yaml.contains("networkpolicy-canary.yaml"));
}

#[tokio::test]
async fn test_providers_list_and_toggle_endpoint() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Unauthenticated GET /api/v1/providers -> 401 Unauthorized
    let unauth_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_res.status(), StatusCode::UNAUTHORIZED);

    // 2. Authenticated GET /api/v1/providers reads providers/registry.yaml
    let list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/providers")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let list_json: Value =
        serde_json::from_slice(&list_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let providers = list_json["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0]["id"], "proxmox");
    assert_eq!(providers[0]["enabled"], true);
    assert_eq!(providers[0]["package"], "bpg/proxmox");
    assert_eq!(providers[1]["id"], "aws");
    assert_eq!(providers[1]["enabled"], false);
    assert_eq!(providers[1]["package"], "pulumi/aws");

    // 3. PUT /api/v1/providers/{id} with invalid DNS-1123 id -> 400 Bad Request
    let bad_id_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/providers/Invalid_Id")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "enabled": true }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_id_res.status(), StatusCode::BAD_REQUEST);

    // 4. PUT /api/v1/providers/{id} for unknown provider -> 404 Not Found
    let missing_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/providers/nonexistent")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "enabled": true }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_res.status(), StatusCode::NOT_FOUND);

    // 5. PUT /api/v1/providers/aws with { enabled: true } -> commits on branch & opens PR
    let toggle_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/providers/aws")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "enabled": true }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_res.status(), StatusCode::OK);
    let toggle_json: Value =
        serde_json::from_slice(&toggle_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(toggle_json["provider"]["id"], "aws");
    assert_eq!(toggle_json["provider"]["enabled"], true);
    assert_eq!(toggle_json["provider"]["package"], "pulumi/aws");
    let branch = toggle_json["branch"].as_str().unwrap();
    assert!(branch.starts_with("keel/providers/toggle-aws-"));
    assert_eq!(toggle_json["proposal"]["state"], "open");

    // Verify providers/registry.yaml was updated on the proposal branch
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();
    let updated_yaml =
        tokio::fs::read_to_string(fixture.state.repo_workdir.join("providers/registry.yaml"))
            .await
            .unwrap();
    let parsed = keel_core::ProviderRegistry::from_yaml(&updated_yaml).unwrap();
    assert!(parsed.is_enabled("aws"));
    assert!(parsed.is_enabled("proxmox"));

    // 6. Fallback to default_day1 when providers/registry.yaml is absent
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, "main")
        .await
        .unwrap();
    tokio::fs::remove_file(fixture.state.repo_workdir.join("providers/registry.yaml"))
        .await
        .unwrap();
    let fallback_res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/providers")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fallback_res.status(), StatusCode::OK);
    let fallback_json: Value =
        serde_json::from_slice(&fallback_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let fallback_providers = fallback_json["providers"].as_array().unwrap();
    assert_eq!(fallback_providers.len(), 7);
    assert!(
        fallback_providers
            .iter()
            .any(|p| p["id"] == "kind" && p["enabled"] == true)
    );
    assert!(fallback_providers[0]["description"].is_string());
}

#[tokio::test]
async fn test_stacks_includes_management_cluster_core_by_default() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Even without clusters/management/*.yaml, GET /api/v1/stacks includes management-cluster-core
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/stacks")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let stacks = body["stacks"].as_array().unwrap();
    assert_eq!(stacks.len(), 2);
    assert_eq!(stacks[0]["name"], "management-cluster-core");
    assert_eq!(stacks[0]["namespace"], "flux-system");
    assert_eq!(stacks[0]["protect"], true);
    assert_eq!(stacks[1]["name"], "acme-net");

    // 2. When clusters/management/management-stack.yaml exists, it is read without duplication
    tokio::fs::create_dir_all(fixture.state.repo_workdir.join("clusters/management"))
        .await
        .unwrap();
    tokio::fs::write(
        fixture
            .state
            .repo_workdir
            .join("clusters/management/management-stack.yaml"),
        r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: management-cluster-core
  namespace: flux-system
  labels:
    keel.io/protect: "true"
spec:
  stack: keel/management/core
"#,
    )
    .await
    .unwrap();

    let res2 = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/stacks")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res2.status(), StatusCode::OK);
    let body2: Value =
        serde_json::from_slice(&res2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let stacks2 = body2["stacks"].as_array().unwrap();
    assert_eq!(stacks2.len(), 2);
    assert_eq!(stacks2[0]["name"], "management-cluster-core");
    assert_eq!(stacks2[0]["protect"], true);
}

#[tokio::test]
async fn test_force_delete_stack_endpoint_guardrails() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Unauthenticated POST /api/v1/stacks/tenant-acme/azure-vm/force-delete -> 401
    let unauth_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/stacks/tenant-acme/azure-vm/force-delete")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_res.status(), StatusCode::UNAUTHORIZED);

    // 2. Protected management-cluster-core stack -> 409 Conflict
    let protected_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/stacks/flux-system/management-cluster-core/force-delete")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protected_res.status(), StatusCode::CONFLICT);

    // 3. Cross-tenant access -> 403 Forbidden
    let tenant_cookie =
        login_with_groups_and_name(&fixture, "dev-user", "Dev User", vec!["tenant:acme"]).await;
    let cross_tenant_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/stacks/tenant-beta/azure-vm/force-delete")
                .header(COOKIE, &tenant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_tenant_res.status(), StatusCode::FORBIDDEN);

    // 4. Unknown stack (neither in Git nor live) -> 404 before any live-cluster check.
    let missing_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/stacks/tenant-acme/azure-vm/force-delete")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_res.status(), StatusCode::NOT_FOUND);

    // 5. Known stack on the repo-only provider (no live cluster) -> 503 Service Unavailable.
    for action in ["force-delete", "unlock", "restart"] {
        assert_eq!(
            post_status(
                &app,
                &format!("/api/v1/stacks/tenant-acme/acme-net/{action}"),
                &cookie
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE,
            "{action}"
        );
        assert_eq!(
            post_status(
                &app,
                &format!("/api/v1/stacks/tenant-acme/does-not-exist/{action}"),
                &cookie
            )
            .await,
            StatusCode::NOT_FOUND,
            "{action}"
        );
    }
}

#[tokio::test]
async fn test_blueprint_merge_visibility_and_proposal_status_history() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Register an infra blueprint using an enabled provider (`proxmox` in setup_fixture)
    let create_bp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "infra",
                        "id": "pve-vm",
                        "name": "Proxmox VM",
                        "category": "compute",
                        "provider": "proxmox",
                        "pulumiRuntime": "yaml"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_bp.status(), StatusCode::CREATED);
    let bp_json: Value =
        serde_json::from_slice(&create_bp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let pr1_id = bp_json["proposal"]["id"].as_str().unwrap().to_string();

    // Merge PR #1 -> repo_workdir syncs and GET /api/v1/catalog immediately includes pve-vm
    let merge_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr1_id}/merge"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(merge_res.status(), StatusCode::OK);

    let cat_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cat_res.status(), StatusCode::OK);
    let cat_json: Value =
        serde_json::from_slice(&cat_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        cat_json["blueprints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["metadata"]["id"] == "pve-vm")
    );

    // 2. Create a second PR and reject it -> status becomes closed and is retained under ?status=all
    let create_app = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "app",
                        "id": "echo-test",
                        "name": "Echo Test",
                        "category": "apps",
                        "runtime": "helm",
                        "repoUrl": "https://ealenn.github.io/charts",
                        "chart": "echo-server"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_app.status(), StatusCode::CREATED);
    let app_json: Value =
        serde_json::from_slice(&create_app.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let pr2_id = app_json["proposal"]["id"].as_str().unwrap().to_string();

    let reject_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr2_id}/reject"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reject_res.status(), StatusCode::OK);
    let reject_json: Value =
        serde_json::from_slice(&reject_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(reject_json["proposal"]["state"], "closed");

    // GET /api/v1/proposals?status=all retains both merged and closed PRs
    let all_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/proposals?status=all")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(all_res.status(), StatusCode::OK);
    let all_json: Value =
        serde_json::from_slice(&all_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let all_prs = all_json["proposals"].as_array().unwrap();
    assert_eq!(all_prs.len(), 2);

    // POST /api/v1/proposals/{pr2_id}/reopen transitions closed PR back to open
    let reopen_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr2_id}/reopen"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reopen_res.status(), StatusCode::OK);
    let reopen_json: Value =
        serde_json::from_slice(&reopen_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(reopen_json["proposal"]["state"], "open");

    // Reopening an already-open PR returns 409 CONFLICT
    let reopen_open_res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr2_id}/reopen"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reopen_open_res.status(), StatusCode::CONFLICT);
}

/// The built-in self-service Dex catalog app installs from its own tenant-scoped HelmRepository
/// (Flux `--no-cross-namespace-refs`), so it never depends on a `keel-system` HelmRepository from
/// the management core add-ons.
#[tokio::test]
async fn test_builtin_dex_catalog_app_installs_from_tenant_scoped_helm_repository() {
    let fixture = setup_fixture().await;
    let workdir = fixture.state.repo_workdir.clone();
    tokio::fs::create_dir_all(workdir.join("catalog/apps/dex"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/apps/dex/catalog.yaml"),
        include_str!("../../../deploy/catalog/apps/dex/catalog.yaml"),
    )
    .await
    .unwrap();
    let git = &fixture.state.git_runner;
    git.add(&workdir, ["catalog/apps/dex/catalog.yaml"])
        .await
        .unwrap();
    git.commit(
        &workdir,
        "feat: add dex starter app",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&workdir, "origin", "main").await.unwrap();

    let cookie = login_and_get_cookie(&fixture).await;
    let post = |body: Value| {
        build_router(fixture.state.clone()).oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(proposals_uri(&body))
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
    };
    // Dex renders a ClusterRole/Binding (`spec.placement.scope: cluster`): management cluster → 400.
    let res = post(json!({
        "tenant": "acme",
        "catalogId": "dex",
        "name": "dex",
        "values": { "replicaCount": 1 }
    }))
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = post(json!({
        "tenant": "acme",
        "catalogId": "dex",
        "name": "dex",
        "targetCluster": "workload-kubeconfig",
        "targetNamespace": "dex",
        "values": { "replicaCount": 1 }
    }))
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let branch = created["branch"].as_str().unwrap();
    git.checkout_branch(&workdir, branch).await.unwrap();

    let read = |name: &str| {
        let path = workdir.join(format!("tenants/acme/apps/dex/{name}"));
        async move { tokio::fs::read_to_string(path).await.unwrap() }
    };
    let repo: Value = noyalib::from_str(&read("helm-repository.yaml").await).unwrap();
    assert_eq!(repo["kind"], "HelmRepository");
    assert_eq!(repo["metadata"]["name"], "dex-repo");
    assert_eq!(repo["metadata"]["namespace"], "tenant-acme");
    assert_eq!(repo["spec"]["url"], "https://charts.dexidp.io");

    let hr: Value = noyalib::from_str(&read("helmrelease.yaml").await).unwrap();
    let chart = &hr["spec"]["chart"]["spec"];
    assert_eq!(chart["chart"], "dex");
    assert_eq!(chart["version"], "0.21.0");
    assert_eq!(chart["sourceRef"]["kind"], "HelmRepository");
    assert_eq!(chart["sourceRef"]["name"], "dex-repo");
    assert_eq!(chart["sourceRef"]["namespace"], "tenant-acme");
    assert_eq!(
        hr["spec"]["kubeConfig"]["secretRef"]["name"],
        "workload-kubeconfig"
    );
}

#[tokio::test]
async fn test_proposal_supports_multiple_instances_version_and_target_namespace() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // Local targetNamespace != tenant-acme is rejected (400)
    let bad_ns_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "name": "grafana-alt",
                        "targetNamespace": "other-ns",
                        "values": { "replicas": 1 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_ns_res.status(), StatusCode::BAD_REQUEST);

    // Remote targetCluster + targetNamespace + version override succeeds and uses instance-scoped HelmRepository
    let ok_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "name": "grafana-staging",
                        "version": "8.5.0",
                        "targetCluster": "workload-kubeconfig",
                        "targetNamespace": "staging-obs",
                        "values": { "replicas": 1 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok_res.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_slice(&ok_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        created["proposal"]["title"]
            .as_str()
            .unwrap()
            .starts_with("feat(") // Conventional Commit title (UI-15)
    );
    let branch = created["branch"].as_str().unwrap();

    // Verify synthesized files on the proposal branch
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();
    let hr_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/apps/grafana-staging/helmrelease.yaml"),
    )
    .await
    .unwrap();
    assert!(hr_yaml.contains("name: grafana-staging"));
    assert!(hr_yaml.contains("8.5.0"));
    assert!(hr_yaml.contains("targetNamespace: staging-obs"));
    assert!(hr_yaml.contains("createNamespace: true"));
    assert!(hr_yaml.contains("name: grafana-staging-repo"));

    let repo_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/apps/grafana-staging/helm-repository.yaml"),
    )
    .await
    .unwrap();
    assert!(repo_yaml.contains("name: grafana-staging-repo"));

    // Updating an existing instance (tenants/acme/apps/redis) yields a `feat(<tenant>): update <name>` PR title
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let update_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "name": "redis",
                        "values": { "replicas": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_res.status(), StatusCode::CREATED);
    let updated: Value =
        serde_json::from_slice(&update_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert!(
        updated["proposal"]["title"]
            .as_str()
            .unwrap()
            .contains("): update ") // Conventional Commit update title (UI-15)
    );

    // GET /api/v1/releases/{namespace}/{name}/config returns persisted values & metadata
    tokio::fs::write(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/apps/redis/values.yaml"),
        "replicaCount: 3\nimage:\n  tag: \"7.2\"\n",
    )
    .await
    .unwrap();
    let cfg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/releases/tenant-acme/redis/config")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cfg_res.status(), StatusCode::OK);
    let cfg_json: Value =
        serde_json::from_slice(&cfg_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(cfg_json["tenant"], "acme");
    assert_eq!(cfg_json["name"], "redis");
    assert_eq!(cfg_json["values"]["replicaCount"], 3);
    assert_eq!(cfg_json["values"]["image"]["tag"], "7.2");

    let missing_cfg = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/releases/tenant-acme/nonexistent/config")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_cfg.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_catalog_bundle_proposal_and_dependency_chaining() {
    let fixture = setup_fixture().await;

    // Seed a CatalogBundle referencing existing `proxmox-vm` (infra) and `podinfo` (app)
    let bundle_dir = fixture
        .state
        .repo_workdir
        .join("catalog/bundles/cluster-with-podinfo");
    tokio::fs::create_dir_all(&bundle_dir).await.unwrap();
    tokio::fs::write(
        bundle_dir.join("catalog.yaml"),
        r#"
apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: cluster-with-podinfo
  name: Cluster + Podinfo Bundle
  category: Platform
spec:
  items:
    - id: cluster
      kind: infra
      catalogRef: proxmox-vm
    - id: app
      kind: app
      catalogRef: grafana
      dependsOn: [cluster]
      targetClusterFrom: cluster
      targetNamespace: apps-prod
  schema:
    type: object
    properties:
      cluster:
        type: object
        properties:
          nodeCount:
            type: integer
            default: 3
      app:
        type: object
        properties:
          replicaCount:
            type: integer
            default: 2
"#,
    )
    .await
    .unwrap();
    fixture
        .state
        .git_runner
        .add(
            &fixture.state.repo_workdir,
            ["catalog/bundles/cluster-with-podinfo/catalog.yaml"],
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .commit(
            &fixture.state.repo_workdir,
            "feat: seed cluster-with-podinfo bundle",
            &CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(
            &fixture.state.repo_workdir,
            "origin",
            "HEAD:refs/heads/main",
        )
        .await
        .unwrap();

    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. GET /api/v1/catalog returns the bundle
    let cat_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cat_res.status(), StatusCode::OK);
    let cat_json: Value =
        serde_json::from_slice(&cat_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let bundles = cat_json["bundles"].as_array().unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0]["metadata"]["id"], "cluster-with-podinfo");

    // 2. POST /api/v1/proposals synthesizes atomic PR with Flux Kustomization healthChecks + dependsOn
    let prop_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "catalogId": "cluster-with-podinfo",
                        "kind": "bundle",
                        "tenant": "acme",
                        "name": "prod-stack",
                        "values": {
                            "cluster": { "nodeCount": 5 },
                            "app": { "replicaCount": 3 }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prop_res.status(), StatusCode::CREATED);
    let prop_json: Value =
        serde_json::from_slice(&prop_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let branch = prop_json["branch"].as_str().unwrap();
    assert!(branch.starts_with("keel/acme/bundle-cluster-with-podinfo-"));
    assert_eq!(prop_json["report"]["summary"]["create"], 2);
    let resources = prop_json["report"]["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);
    assert!(
        resources[1]["changedFields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v
                .as_str()
                .unwrap()
                .contains("deferred-preview(dependsOn: cluster)"))
    );

    // Inspect branch files
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();
    let app_hr = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/bundles/prod-stack/app/helmrelease.yaml"),
    )
    .await
    .unwrap();
    assert!(app_hr.contains("prod-stack-cluster-kubeconfig"));
    assert!(app_hr.contains("targetNamespace: apps-prod"));
    assert!(app_hr.contains("keel.io/bundle: prod-stack"));

    let cluster_stack = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/bundles/prod-stack/cluster/stack.yaml"),
    )
    .await
    .unwrap();
    assert!(cluster_stack.contains("kubeconfigSecretName: prod-stack-cluster-kubeconfig"));
    assert!(cluster_stack.contains("kubeconfigSecretNamespace: tenant-acme"));
    // Bundle infra items clone the tenant's own source through PKO `gitSource`.
    let cluster_stack_doc: Value = noyalib::from_str(&cluster_stack).unwrap();
    assert!(cluster_stack_doc["spec"].get("fluxSource").is_none());
    assert_eq!(cluster_stack_doc["spec"]["projectRepo"], TENANT_PKO_GIT_URL);
    assert_eq!(
        cluster_stack_doc["spec"]["repoDir"],
        "tenants/acme/bundles/prod-stack/cluster"
    );
    assert_eq!(
        cluster_stack_doc["spec"]["gitAuth"]["basicAuth"]["password"]["secret"]["name"],
        "git-credentials"
    );

    let app_fk = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/bundles/prod-stack/app/flux-kustomization.yaml"),
    )
    .await
    .unwrap();
    assert!(app_fk.contains("name: prod-stack-cluster"));

    let cluster_fk = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/bundles/prod-stack/cluster/flux-kustomization.yaml"),
    )
    .await
    .unwrap();
    assert!(cluster_fk.contains("kind: Stack"));
    assert!(cluster_fk.contains("name: prod-stack-cluster"));

    // 3. GET /api/v1/bundles/{namespace}/{name}/config returns combined per-item values
    let cfg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/bundles/tenant-acme/prod-stack/config")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cfg_res.status(), StatusCode::OK);
    let cfg_json: Value =
        serde_json::from_slice(&cfg_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(cfg_json["tenant"], "acme");
    assert_eq!(cfg_json["name"], "prod-stack");
    assert_eq!(cfg_json["values"]["cluster"]["nodeCount"], 5);
    assert_eq!(cfg_json["values"]["app"]["replicaCount"], 3);

    // 4. GET /api/v1/stacks/{namespace}/{bundle}-{item}/config and /api/v1/releases/{namespace}/{bundle}-{item}/config
    //    must resolve bundle-created items under `tenants/<tenant>/bundles/<bundle>/<item>/`
    let stack_cfg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/stacks/tenant-acme/prod-stack-cluster/config")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stack_cfg_res.status(), StatusCode::OK);
    let stack_cfg_json: Value = serde_json::from_slice(
        &stack_cfg_res
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(stack_cfg_json["catalogId"], "proxmox-vm");
    assert_eq!(stack_cfg_json["tenant"], "acme");
    assert_eq!(stack_cfg_json["name"], "prod-stack-cluster");
    assert_eq!(stack_cfg_json["values"]["nodeCount"], 5);

    let rel_cfg_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/releases/tenant-acme/prod-stack-app/config")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rel_cfg_res.status(), StatusCode::OK);
    let rel_cfg_json: Value =
        serde_json::from_slice(&rel_cfg_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(rel_cfg_json["catalogId"], "grafana");
    assert_eq!(rel_cfg_json["tenant"], "acme");
    assert_eq!(rel_cfg_json["name"], "prod-stack-app");
    assert_eq!(
        rel_cfg_json["targetCluster"],
        "prod-stack-cluster-kubeconfig"
    );
    assert_eq!(rel_cfg_json["targetNamespace"], "apps-prod");
    assert_eq!(rel_cfg_json["values"]["replicaCount"], 3);

    // 5. Editing a bundle-created stack or app via POST /api/v1/proposals updates the bundle item directory
    //    rather than creating a duplicate `tenants/<tenant>/infra|apps/<name>` directory.
    //    First merge the bundle branch into `main` so `sync_branch_from_remote` starts from it.
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, "main")
        .await
        .unwrap();
    // Copy the bundle files to main via a merge from the proposal branch
    let merge_out = tokio::process::Command::new("git")
        .args(["merge", "--ff-only", branch])
        .current_dir(&fixture.state.repo_workdir)
        .output()
        .await
        .unwrap();
    assert!(merge_out.status.success());
    fixture
        .state
        .git_runner
        .push(&fixture.state.repo_workdir, "origin", "main")
        .await
        .unwrap();

    let edit_stack_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "catalogId": "proxmox-vm",
                        "kind": "infra",
                        "tenant": "acme",
                        "name": "prod-stack-cluster",
                        "values": { "cores": 4, "nodeCount": 7 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(edit_stack_res.status(), StatusCode::CREATED);
    let edit_stack_json: Value = serde_json::from_slice(
        &edit_stack_res
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    let edit_stack_branch = edit_stack_json["branch"].as_str().unwrap().to_string();
    assert!(
        edit_stack_json["proposal"]["title"]
            .as_str()
            .unwrap()
            .contains("): update ") // Conventional Commit update title (UI-15)
    );
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, &edit_stack_branch)
        .await
        .unwrap();
    assert!(
        !tokio::fs::try_exists(
            fixture
                .state
                .repo_workdir
                .join("tenants/acme/infra/prod-stack-cluster")
        )
        .await
        .unwrap()
    );
    let updated_cluster_values = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/bundles/prod-stack/cluster/values.yaml"),
    )
    .await
    .unwrap();
    assert!(updated_cluster_values.contains("cores: 4"));
    let tenants_k = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/kustomization.yaml"),
    )
    .await
    .unwrap();
    assert!(!tenants_k.contains("acme/infra/prod-stack-cluster/stack.yaml"));
    assert!(tenants_k.contains("acme/bundles/prod-stack/cluster/flux-kustomization.yaml"));

    let edit_app_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "catalogId": "grafana",
                        "kind": "app",
                        "tenant": "acme",
                        "name": "prod-stack-app",
                        "targetCluster": "prod-stack-cluster-kubeconfig",
                        "targetNamespace": "apps-prod",
                        "values": { "replicas": 6, "replicaCount": 6 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(edit_app_res.status(), StatusCode::CREATED);
    let edit_app_json: Value =
        serde_json::from_slice(&edit_app_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let edit_app_branch = edit_app_json["branch"].as_str().unwrap().to_string();
    assert!(
        edit_app_json["proposal"]["title"]
            .as_str()
            .unwrap()
            .contains("): update ") // Conventional Commit update title (UI-15)
    );
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, &edit_app_branch)
        .await
        .unwrap();
    assert!(
        !tokio::fs::try_exists(
            fixture
                .state
                .repo_workdir
                .join("tenants/acme/apps/prod-stack-app")
        )
        .await
        .unwrap()
    );
    let updated_app_hr = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/bundles/prod-stack/app/helmrelease.yaml"),
    )
    .await
    .unwrap();
    assert!(updated_app_hr.contains("replicas: 6"));
    assert!(updated_app_hr.contains("keel.io/bundle: prod-stack"));
    assert!(updated_app_hr.contains("keel.io/bundle-item: app"));
    assert!(updated_app_hr.contains("prod-stack-cluster-kubeconfig"));

    let missing_cfg = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/bundles/tenant-acme/missing-bundle/config")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_cfg.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_tenant_admin_scoped_merge_reject_reopen_and_escalation_guards() {
    let fixture = setup_fixture().await;
    let app = build_router(fixture.state.clone());

    let acme_dev_cookie = login_with_groups(&fixture, "dev@acme.local", vec!["tenant-acme"]).await;
    let acme_admin_cookie =
        login_with_groups(&fixture, "admin@acme.local", vec!["tenant-acme-admin"]).await;
    let cluster_admin_cookie = login_and_get_cookie(&fixture).await;

    // 1. tenant-acme-admin cannot create or delete tenants (cluster-admin only)
    let create_tenant_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &acme_admin_cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "beta" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_tenant_res.status(), StatusCode::FORBIDDEN);

    // 2. tenant-acme developer opens a scoped app PR in `acme`
    let prop_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &acme_dev_cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "catalogId": "grafana",
                        "kind": "app",
                        "tenant": "acme",
                        "name": "acme-grafana",
                        "values": { "replicas": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prop_res.status(), StatusCode::CREATED);
    let prop_json: Value =
        serde_json::from_slice(&prop_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let acme_pr_id = prop_json["proposal"]["id"].as_str().unwrap().to_string();

    // 3. Plain `tenant-acme` developer cannot merge the PR
    let dev_merge_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/tenants/acme/proposals/{acme_pr_id}/merge"))
                .header(COOKIE, &acme_dev_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dev_merge_res.status(), StatusCode::FORBIDDEN);

    // 4. `tenant-acme-admin` CAN reject, reopen, and merge the scoped `acme` PR
    let reject_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/acme/proposals/{acme_pr_id}/reject"
                ))
                .header(COOKIE, &acme_admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reject_res.status(), StatusCode::OK);

    let reopen_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/acme/proposals/{acme_pr_id}/reopen"
                ))
                .header(COOKIE, &acme_admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reopen_res.status(), StatusCode::OK);

    let admin_merge_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/tenants/acme/proposals/{acme_pr_id}/merge"))
                .header(COOKIE, &acme_admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin_merge_res.status(), StatusCode::OK);

    // 5. `tenant-acme-admin` CANNOT merge a PR that modifies `tenants/acme/rbac.yaml`
    let rbac_branch = "keel/acme/escalate-rbac-1700000001";
    {
        let _guard = fixture.state.git_lock.lock().await;
        fixture
            .state
            .git_runner
            .sync_branch_from_remote(&fixture.state.repo_workdir, "origin", "main")
            .await
            .unwrap();
        fixture
            .state
            .git_runner
            .checkout_new_branch(&fixture.state.repo_workdir, rbac_branch)
            .await
            .unwrap();
        tokio::fs::write(
            fixture.state.repo_workdir.join("tenants/acme/rbac.yaml"),
            "kind: Namespace\nmetadata:\n  name: tenant-acme\n",
        )
        .await
        .unwrap();
        fixture
            .state
            .git_runner
            .add(&fixture.state.repo_workdir, ["tenants/acme/rbac.yaml"])
            .await
            .unwrap();
        fixture
            .state
            .git_runner
            .commit(
                &fixture.state.repo_workdir,
                "chore(acme): attempt rbac change",
                &keel_git::CommitAuthor::new("Acme Admin", "admin@acme.local"),
            )
            .await
            .unwrap();
        fixture
            .state
            .git_runner
            .push(&fixture.state.repo_workdir, "origin", rbac_branch)
            .await
            .unwrap();
        fixture
            .state
            .git_runner
            .checkout_branch(&fixture.state.repo_workdir, "main")
            .await
            .unwrap();
    }
    let rbac_pr = fixture
        .state
        .forge
        .create_pull_request(
            keel_git::CreatePrRequest::new("Attempt RBAC edit", "test", rbac_branch)
                .with_tenant("acme"),
        )
        .await
        .unwrap();

    let blocked_rbac_merge = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/acme/proposals/{}/merge",
                    rbac_pr.id
                ))
                .header(COOKIE, &acme_admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked_rbac_merge.status(), StatusCode::FORBIDDEN);

    // 6. Cluster admin (`keel-admins`) CAN merge the `rbac.yaml` PR
    let cluster_merge = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/acme/proposals/{}/merge",
                    rbac_pr.id
                ))
                .header(COOKIE, &cluster_admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cluster_merge.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_update_catalog_item_endpoint() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Update existing app ("grafana" from setup_fixture)
    let update_app_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/catalog/app/grafana")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "Grafana Updated",
                        "category": "Monitoring",
                        "description": "Updated observability stack",
                        "version": "8.9.0",
                        "flaggerSupported": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_app_res.status(), StatusCode::OK);
    let app_json: Value = serde_json::from_slice(
        &update_app_res
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(
        app_json["application"]["metadata"]["name"],
        "Grafana Updated"
    );
    assert_eq!(
        app_json["application"]["metadata"]["description"],
        "Updated observability stack"
    );
    assert_eq!(
        app_json["application"]["spec"]["upstream"]["version"],
        "8.9.0"
    );
    assert_eq!(
        app_json["application"]["spec"]["progressiveDelivery"]["flaggerSupported"],
        true
    );
    assert!(
        app_json["branch"]
            .as_str()
            .unwrap()
            .starts_with("keel/catalog/update-grafana-")
    );

    // 2. Update existing infra blueprint ("proxmox-vm" from setup_fixture) and verify Pulumi.yaml is preserved
    let pulumi_path = fixture
        .state
        .repo_workdir
        .join("catalog/infra/proxmox-vm/Pulumi.yaml");
    let custom_pulumi = "name: proxmox-vm\nruntime: yaml\nresources:\n  vm:\n    type: custom:Vm\n";
    tokio::fs::write(&pulumi_path, custom_pulumi).await.unwrap();
    fixture
        .state
        .git_runner
        .add(
            &fixture.state.repo_workdir,
            ["catalog/infra/proxmox-vm/Pulumi.yaml"],
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .commit(
            &fixture.state.repo_workdir,
            "chore: add custom Pulumi.yaml",
            &keel_git::CommitAuthor::new("Test", "test@keel.local"),
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(&fixture.state.repo_workdir, "origin", "main")
        .await
        .unwrap();

    let update_infra_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/catalog/infra/proxmox-vm")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "Proxmox VM v2",
                        "category": "Compute",
                        "description": "Updated PVE blueprint",
                        "protectByDefault": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_infra_res.status(), StatusCode::OK);
    let infra_json: Value = serde_json::from_slice(
        &update_infra_res
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(infra_json["blueprint"]["metadata"]["name"], "Proxmox VM v2");
    assert_eq!(infra_json["blueprint"]["spec"]["protectByDefault"], true);

    let infra_branch = infra_json["branch"].as_str().unwrap();
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, infra_branch)
        .await
        .unwrap();
    let preserved_pulumi = tokio::fs::read_to_string(&pulumi_path).await.unwrap();
    assert_eq!(preserved_pulumi, custom_pulumi);
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, "main")
        .await
        .unwrap();

    // 3. Updating a non-existent catalog item returns 404
    let missing_res = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/catalog/app/does-not-exist")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "Missing",
                        "category": "Applications"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_and_update_infra_blueprint_pulumi_yaml() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let custom_pulumi_v1 = "name: custom-vm\nruntime: yaml\nresources:\n  vm:\n    type: proxmoxve:vm:VirtualMachine\n";

    // 1. Invalid YAML in pulumiYaml returns 400 Bad Request
    let invalid_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "infra",
                        "id": "bad-yaml-bp",
                        "name": "Bad YAML",
                        "category": "Compute",
                        "provider": "proxmox",
                        "pulumiYaml": "name: [unclosed"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_res.status(), StatusCode::BAD_REQUEST);

    // 2. Create blueprint with custom Pulumi.yaml
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind": "infra",
                        "id": "custom-vm",
                        "name": "Custom VM",
                        "category": "Compute",
                        "provider": "proxmox",
                        "pulumiYaml": custom_pulumi_v1
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created: Value =
        serde_json::from_slice(&create_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let create_branch = created["branch"].as_str().unwrap();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, create_branch)
        .await
        .unwrap();
    let stored_v1 = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("catalog/infra/custom-vm/Pulumi.yaml"),
    )
    .await
    .unwrap();
    assert_eq!(stored_v1, custom_pulumi_v1);
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, "main")
        .await
        .unwrap();

    // 3. Seed Pulumi.yaml on main and verify GET /api/v1/catalog returns pulumiYaml map
    tokio::fs::write(
        fixture
            .state
            .repo_workdir
            .join("catalog/infra/proxmox-vm/Pulumi.yaml"),
        "name: proxmox-vm\nruntime: yaml\nresources: {}\n",
    )
    .await
    .unwrap();
    let cat_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cat_res.status(), StatusCode::OK);
    let cat_json: Value =
        serde_json::from_slice(&cat_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(cat_json["pulumiYaml"]["proxmox-vm"].is_string());

    // 4. PUT /api/v1/catalog/infra/proxmox-vm with pulumiYaml updates Pulumi.yaml
    let updated_pulumi =
        "name: proxmox-vm\nruntime: yaml\ndescription: Updated via GUI\nresources: {}\n";
    let update_res = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/catalog/infra/proxmox-vm")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "Proxmox VM",
                        "category": "Compute",
                        "pulumiYaml": updated_pulumi
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_res.status(), StatusCode::OK);
    let updated: Value =
        serde_json::from_slice(&update_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    let update_branch = updated["branch"].as_str().unwrap();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, update_branch)
        .await
        .unwrap();
    let stored_v2 = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("catalog/infra/proxmox-vm/Pulumi.yaml"),
    )
    .await
    .unwrap();
    assert_eq!(stored_v2, updated_pulumi);
}

#[tokio::test]
async fn test_infra_proposal_forwards_config_and_encrypts_secret_env_refs() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // Seed an infra blueprint with normal config fields and a writeOnly credential field
    let bp_dir = fixture.state.repo_workdir.join("catalog/infra/gcp-free-vm");
    tokio::fs::create_dir_all(&bp_dir).await.unwrap();
    tokio::fs::write(
        bp_dir.join("catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: gcp-free-vm
  name: GCP Free VM
  category: Compute
  provider: proxmox
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  schema:
    type: object
    required:
      - vmName
      - project
      - GOOGLE_CREDENTIALS
    properties:
      vmName:
        type: string
        default: keel-free-vm
      project:
        type: string
      GOOGLE_CREDENTIALS:
        type: string
        writeOnly: true
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        bp_dir.join("Pulumi.yaml"),
        "name: gcp-free-vm\nruntime: yaml\nresources: {}\n",
    )
    .await
    .unwrap();
    fixture
        .state
        .git_runner
        .add(
            &fixture.state.repo_workdir,
            [
                "catalog/infra/gcp-free-vm/catalog.yaml",
                "catalog/infra/gcp-free-vm/Pulumi.yaml",
            ],
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .commit(
            &fixture.state.repo_workdir,
            "feat(catalog): add gcp-free-vm test blueprint",
            &CommitAuthor::new("Keel Seed", "seed@keel.local"),
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(
            &fixture.state.repo_workdir,
            "origin",
            "HEAD:refs/heads/main",
        )
        .await
        .unwrap();

    let secret_json = r#"{"type":"service_account","project_id":"demo-free-123"}"#;
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "gcp-free-vm",
                        "name": "my-gcp-vm",
                        "values": {
                            "vmName": "keel-free-vm",
                            "project": "demo-free-123",
                            "GOOGLE_CREDENTIALS": secret_json
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);
    // Without `age-keygen` on the host the server reports a misconfiguration (503) that names it.
    if status == StatusCode::SERVICE_UNAVAILABLE {
        assert!(
            body_str.contains("has no SOPS age key configured") && body_str.contains("age-keygen"),
            "unexpected 503 body: {body_str}"
        );
        return;
    }
    assert_eq!(
        status,
        StatusCode::CREATED,
        "unexpected status {status}: {body_str}"
    );
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    let branch = body["branch"].as_str().unwrap();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();

    let stack_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/infra/my-gcp-vm/stack.yaml"),
    )
    .await
    .unwrap();
    assert!(stack_yaml.contains("vmName: keel-free-vm"));
    assert!(stack_yaml.contains("project: demo-free-123"));
    assert!(stack_yaml.contains("GOOGLE_CREDENTIALS:"));
    assert!(stack_yaml.contains("name: my-gcp-vm-secrets"));
    assert!(stack_yaml.contains("key: GOOGLE_CREDENTIALS"));
    assert!(!stack_yaml.contains("service_account"));

    let secret_enc_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/infra/my-gcp-vm/secret.enc.yaml"),
    )
    .await
    .unwrap();
    assert!(secret_enc_yaml.contains("ENC[AES256_GCM,"));
    assert!(!secret_enc_yaml.contains("service_account"));

    let tenants_kustomization = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/kustomization.yaml"),
    )
    .await
    .unwrap();
    assert!(tenants_kustomization.contains("acme/infra/my-gcp-vm/secret.enc.yaml"));
}

#[tokio::test]
async fn test_remote_canary_kustomization_creates_target_namespace_first() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "kind": "app",
                        "name": "grafana-edge",
                        "canary": true,
                        "targetCluster": "workload-kubeconfig",
                        "targetNamespace": "staging-obs",
                        "values": { "replicas": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let branch = body["branch"].as_str().unwrap();
    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();

    let app_dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/apps/grafana-edge");
    let ns: Value = noyalib::from_str(
        &tokio::fs::read_to_string(app_dir.join("canary/namespace.yaml"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ns["kind"], "Namespace");
    assert_eq!(ns["metadata"]["name"], "staging-obs");
    assert_restricted_pss(&ns);
    assert_eq!(
        ns["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/prune"],
        "disabled"
    );
    assert_eq!(
        ns["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/ssa"],
        "IfNotPresent"
    );

    let canary_k: Value = noyalib::from_str(
        &tokio::fs::read_to_string(app_dir.join("canary/kustomization.yaml"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        canary_k["resources"],
        json!(["namespace.yaml", "canary.yaml", "networkpolicy-canary.yaml"])
    );

    let fk: Value = noyalib::from_str(
        &tokio::fs::read_to_string(app_dir.join("canary-kustomization.yaml"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(fk["spec"]["targetNamespace"], "staging-obs");
    assert_eq!(
        fk["spec"]["kubeConfig"]["secretRef"]["name"],
        "workload-kubeconfig"
    );
    assert_eq!(
        fk["spec"]["path"],
        "./tenants/acme/apps/grafana-edge/canary"
    );

    // The Namespace lives only in the remote canary path, never in the local tenant tree.
    let tenants_kustomization = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/kustomization.yaml"),
    )
    .await
    .unwrap();
    assert!(tenants_kustomization.contains("acme/apps/grafana-edge/canary-kustomization.yaml"));
    assert!(!tenants_kustomization.contains("namespace.yaml"));
    assert!(!tenants_kustomization.contains("acme/apps/grafana-edge/canary.yaml"));
}

#[tokio::test]
async fn test_app_proposal_with_flagger_canary_stages_canary_yaml() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // `grafana` in setup_fixture has flaggerSupported: true
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "kind": "app",
                        "canary": true,
                        "values": {
                            "replicas": 2,
                            "ingressHost": "grafana.keel.local"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let branch = body["branch"].as_str().unwrap();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, branch)
        .await
        .unwrap();

    let canary_yaml = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/apps/grafana/canary.yaml"),
    )
    .await
    .unwrap();
    assert!(canary_yaml.contains("kind: Canary"));
    assert!(
        canary_yaml.contains("provider: gatewayapi:v1")
            || canary_yaml.contains("provider: \"gatewayapi:v1\"")
    );
    assert!(canary_yaml.contains("name: keel-ingress"));
    assert!(canary_yaml.contains("grafana.keel.local"));

    let tenants_kustomization = tokio::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/kustomization.yaml"),
    )
    .await
    .unwrap();
    assert!(tenants_kustomization.contains("acme/apps/grafana/helmrelease-canary.yaml"));
    assert!(tenants_kustomization.contains("acme/apps/grafana/canary.yaml"));
    assert!(tenants_kustomization.contains("acme/apps/grafana/networkpolicy-canary.yaml"));
    // Local (management-cluster) canaries never ship a Namespace object.
    assert!(
        !fixture
            .state
            .repo_workdir
            .join("tenants/acme/apps/grafana/canary")
            .exists()
    );
    assert!(!tenants_kustomization.contains("namespace.yaml"));

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, "main")
        .await
        .unwrap();

    // App with flaggerSupported: false must reject canary=true (merged to `main`: proposals
    // read the catalog from freshly synced `main`).
    commit_to_main(
        &fixture,
        &[(
            "catalog/apps/redis/catalog.yaml",
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: redis
  name: Redis
  category: datastore
  runtime: helm
spec:
  upstream:
    chart: redis
    repoURL: https://charts.bitnami.com/bitnami
    version: "20.1.0"
  progressiveDelivery:
    flaggerSupported: false
    defaultCanary: false
  schema:
    type: object
    properties:
      replicas:
        type: integer
"#,
        )],
        "chore: add redis catalog entry",
    )
    .await;

    let bad_res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "redis",
                        "kind": "app",
                        "canary": true,
                        "values": { "replicas": 1 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_toggling_canary_on_bundle_item_removes_stale_manifests_and_avoids_duplicates() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // 1. Seed an existing bundle app item on `main` with canary=false (`helmrelease.yaml`)
    let item_rel = "tenants/acme/bundles/test/step-2";
    let item_dir = fixture.state.repo_workdir.join(item_rel);
    tokio::fs::create_dir_all(&item_dir).await.unwrap();
    tokio::fs::write(
        item_dir.join("values.yaml"),
        "replicas: 1\ningressHost: step2.keel.local\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        item_dir.join("helmrelease.yaml"),
        "apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata:\n  name: test-step-2\n  namespace: tenant-acme\n  labels:\n    keel.io/tenant: acme\n    keel.io/bundle: test\n    keel.io/bundle-item: step-2\nspec:\n  chart:\n    spec:\n      chart: grafana\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        item_dir.join("kustomization.yaml"),
        "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - helmrelease.yaml\n",
    )
    .await
    .unwrap();
    fixture
        .state
        .git_runner
        .add(&fixture.state.repo_workdir, [item_rel])
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .commit(
            &fixture.state.repo_workdir,
            "seed bundle item",
            &keel_git::CommitAuthor::new("tester", "tester@keel.local"),
        )
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(&fixture.state.repo_workdir, "origin", "main")
        .await
        .unwrap();

    // 2. Toggle canary=true via POST /api/v1/proposals
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "kind": "app",
                        "name": "test-step-2",
                        "canary": true,
                        "values": {
                            "replicas": 1,
                            "ingressHost": "step2.keel.local"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let canary_branch = body["branch"].as_str().unwrap().to_string();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, &canary_branch)
        .await
        .unwrap();

    assert!(
        tokio::fs::try_exists(item_dir.join("helmrelease-canary.yaml"))
            .await
            .unwrap()
    );
    assert!(
        tokio::fs::try_exists(item_dir.join("canary.yaml"))
            .await
            .unwrap()
    );
    assert!(
        tokio::fs::try_exists(item_dir.join("networkpolicy-canary.yaml"))
            .await
            .unwrap()
    );
    assert!(
        !tokio::fs::try_exists(item_dir.join("helmrelease.yaml"))
            .await
            .unwrap(),
        "stale helmrelease.yaml must be deleted when toggling canary=true"
    );

    // Verify GET /api/v1/releases returns exactly 1 release for test-step-2 with canary=true
    let rel_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/releases?tenant=acme")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rel_res.status(), StatusCode::OK);
    let rel_body: Value =
        serde_json::from_slice(&rel_res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let step2_releases: Vec<&Value> = rel_body["releases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["name"] == "test-step-2")
        .collect();
    assert_eq!(step2_releases.len(), 1);
    assert_eq!(step2_releases[0]["canary"], true);

    // Fast-forward main to canary_branch so the next edit starts from canary=true
    fixture
        .state
        .git_runner
        .fast_forward_merge(&fixture.state.repo_workdir, "main", &canary_branch)
        .await
        .unwrap();
    fixture
        .state
        .git_runner
        .push(&fixture.state.repo_workdir, "origin", "main")
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // 3. Toggle canary=false via POST /api/v1/proposals
    let res2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "kind": "app",
                        "name": "test-step-2",
                        "canary": false,
                        "values": {
                            "replicas": 1,
                            "ingressHost": "step2.keel.local"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let res2_status = res2.status();
    let res2_bytes = res2.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        res2_status,
        StatusCode::CREATED,
        "unexpected status: {}",
        String::from_utf8_lossy(&res2_bytes)
    );
    let body2: Value = serde_json::from_slice(&res2_bytes).unwrap();
    let non_canary_branch = body2["branch"].as_str().unwrap().to_string();

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, &non_canary_branch)
        .await
        .unwrap();

    assert!(
        tokio::fs::try_exists(item_dir.join("helmrelease.yaml"))
            .await
            .unwrap()
    );
    assert!(
        !tokio::fs::try_exists(item_dir.join("helmrelease-canary.yaml"))
            .await
            .unwrap(),
        "stale helmrelease-canary.yaml must be deleted when toggling canary=false"
    );
    assert!(
        !tokio::fs::try_exists(item_dir.join("canary.yaml"))
            .await
            .unwrap(),
        "stale canary.yaml must be deleted when toggling canary=false"
    );
    assert!(
        !tokio::fs::try_exists(item_dir.join("networkpolicy-canary.yaml"))
            .await
            .unwrap(),
        "stale networkpolicy-canary.yaml must be deleted when toggling canary=false"
    );
}

#[tokio::test]
async fn test_security_ssrf_rbac_csrf_and_open_redirect_guards() {
    let fixture = setup_fixture().await;
    let admin_cookie = login_and_get_cookie(&fixture).await;

    // Mint a non-admin tenant-scoped session cookie (only member of `tenant-alpha`)
    let session_key = fixture
        .state
        .secret_store
        .get_or_create_session_key()
        .await
        .unwrap();
    let tenant_claims = SessionClaims {
        sub: "alice@keel.local".to_string(),
        email: Some("alice@keel.local".to_string()),
        name: Some("Alice".to_string()),
        groups: vec!["tenant-alpha".to_string()],
        auth_phase: AuthPhase::OidcActive,
        exp: chrono::Utc::now().timestamp() + 3600,
    };
    let tenant_token = keel_server::encrypt_session_token(&session_key, &tenant_claims).unwrap();
    let tenant_cookie = format!("{SESSION_COOKIE_NAME}={tenant_token}");

    let app = build_router(fixture.state.clone());

    // 1. SSRF and path-traversal rejection in fetch-schema
    for (repo_url, chart) in [
        ("http://127.0.0.1:8080/charts", "podinfo"),
        ("http://169.254.169.254/latest/meta-data", "podinfo"),
        ("http://dex.keel-system.svc.cluster.local:5556", "podinfo"),
        ("https://charts.example.com", "foo/bar"),
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/catalog/fetch-schema")
                    .header(COOKIE, &admin_cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "repoUrl": repo_url, "chart": chart }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for ({repo_url}, {chart})"
        );
    }

    // 2. RBAC: non-admin cannot create tenants or toggle providers
    let create_tenant_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &tenant_cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "beta" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_tenant_res.status(), StatusCode::FORBIDDEN);

    let toggle_provider_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/providers/proxmox")
                .header(COOKIE, &tenant_cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "enabled": false }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(toggle_provider_res.status(), StatusCode::FORBIDDEN);

    // 3. Tenant isolation: tenant-alpha user cannot propose into tenant-acme
    let cross_tenant_prop = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &tenant_cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "grafana",
                        "kind": "app",
                        "values": { "replicas": 1 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_tenant_prop.status(), StatusCode::FORBIDDEN);

    // 4. CSRF guard: cross-origin Origin header rejected when cookie-authenticated
    let csrf_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &admin_cookie)
                .header("host", "127.0.0.1:8080")
                .header("origin", "https://evil.example.com")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "gamma" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(csrf_res.status(), StatusCode::FORBIDDEN);

    // 5. Open redirect rejection in OIDC login
    *fixture.state.oidc_config.write().await = Some(OidcConfig {
        enabled: true,
        issuer_url: "https://idp.keel.local/dex".to_string(),
        client_id: "keel-server".to_string(),
        redirect_url: "https://keel.local/api/v1/auth/callback".to_string(),
        scopes: vec!["openid".to_string()],
    });
    let open_redirect_res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/login?redirect_uri=https://evil.example.com/steal")
                .header("host", "keel.local")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(open_redirect_res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_manifest_template_injection_is_neutralised_or_rejected() {
    let fixture = setup_fixture().await;
    tokio::fs::create_dir_all(
        fixture
            .state
            .repo_workdir
            .join("catalog/apps/echo-manifest"),
    )
    .await
    .unwrap();
    tokio::fs::write(
        fixture
            .state
            .repo_workdir
            .join("catalog/apps/echo-manifest/catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: echo-manifest
  name: Echo Manifest
  category: utilities
  runtime: manifest
spec:
  defaultEngine: pulumi
  upstream:
    repoURL: https://example.invalid/echo
    path: ./manifests.yaml
    version: 1.0.0
  schema:
    type: object
    properties:
      message:
        type: string
      image:
        type: string
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        fixture
            .state
            .repo_workdir
            .join("catalog/apps/echo-manifest/manifests.yaml"),
        r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: echo-config
data:
  message: ${message}
  image: repo/${image:=echo}:1.0
"#,
    )
    .await
    .unwrap();

    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    git.add(workdir, ["."]).await.unwrap();
    git.commit(
        workdir,
        "chore: seed echo-manifest catalog app",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();

    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    let propose = |values: Value, name: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/tenants/acme/proposals")
            .header(COOKIE, &cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "tenant": "acme",
                    "catalogId": "echo-manifest",
                    "name": name,
                    "values": values
                })
                .to_string(),
            ))
            .unwrap()
    };

    // Embedded placeholder with a newline/document separator is rejected before any commit.
    let res = app
        .clone()
        .oneshot(propose(
            json!({ "image": "x\n---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: pwned" }),
            "echo-bad",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Whole-scalar injection attempt is quoted: still exactly one ConfigMap, no Secret.
    let res = app
        .clone()
        .oneshot(propose(
            json!({ "message": "hi\n---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: pwned" }),
            "echo-ok",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["report"]["source"], "synthesized");
    let resources = body["report"]["resources"].as_array().unwrap();
    assert!(
        resources
            .iter()
            .all(|r| !r["resourceType"].as_str().unwrap_or("").contains("Secret")),
        "{resources:?}"
    );
    assert!(
        resources
            .iter()
            .any(|r| r["resourceType"] == "kubernetes:v1:ConfigMap" && r["name"] == "echo-config")
    );
}

/// Tenant PKO Stacks must use the tenant-scoped state prefix (`tenants/<id>`) and the tenant's
/// own scoped S3 identity; without one the server fails closed (409) instead of committing.
#[tokio::test]
async fn test_tenant_stack_uses_scoped_state_backend_and_fails_closed_without_identity() {
    use k8s_openapi::ByteString;
    use k8s_openapi::api::core::v1::Secret;
    use keel_kube::secret_store::SecretBackend;
    use std::collections::BTreeMap;

    let fixture = setup_fixture().await;
    let mut state = fixture
        .state
        .clone()
        .with_s3_credentials_secret(Some("keel-s3-credentials".to_string()));
    state.backend_url =
        "s3://pulumi-state?endpoint=seaweedfs:8333&region=us-east-1&s3ForcePathStyle=true"
            .to_string();
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(state.clone());

    let propose = |name: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/tenants/acme/proposals")
            .header(COOKIE, &cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "tenant": "acme",
                    "catalogId": "proxmox-vm",
                    "kind": "infra",
                    "name": name,
                    "values": { "cores": 2 }
                })
                .to_string(),
            ))
            .unwrap()
    };

    // 1. No scoped tenant identity -> 409, nothing committed, admin keys never mirrored.
    let res = app.clone().oneshot(propose("vm-noid")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no scoped state credentials"),
        "{body}"
    );
    assert!(
        fixture
            .secret_store
            .inspect_raw_secret("tenant-acme", "keel-s3-credentials")
            .await
            .is_none()
    );
    let open = fixture
        .state
        .forge
        .list_open_pull_requests(None)
        .await
        .unwrap();
    assert!(open.is_empty(), "no PR may be opened: {open:?}");

    // 2. Seed the scoped tenant identity -> 201 with tenant-prefixed backend + mirrored creds.
    let mut data = BTreeMap::new();
    data.insert(
        "accessKey".to_string(),
        ByteString(b"TENANTACCESSKEY0001".to_vec()),
    );
    data.insert(
        "secretKey".to_string(),
        ByteString(b"tenantsecretkey00000000000000001".to_vec()),
    );
    fixture
        .secret_store
        .upsert_secret(
            "tenant-acme",
            Secret {
                metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                    name: Some("keel-tenant-s3-credentials".to_string()),
                    namespace: Some("tenant-acme".to_string()),
                    ..Default::default()
                },
                data: Some(data),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let res = app.clone().oneshot(propose("vm-scoped")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let branch = body["branch"].as_str().unwrap().to_string();
    // Ephemeral previews diff against the tenant's own scoped state prefix (OPEN-9) and never
    // reference the control-plane admin S3 keys.
    assert_eq!(
        body["previewStack"]["spec"]["backend"],
        "s3://pulumi-state/tenants/acme?endpoint=seaweedfs:8333&region=us-east-1&s3ForcePathStyle=true"
    );
    assert!(body["previewStack"]["spec"]["envRefs"]["AWS_ACCESS_KEY_ID"].is_null());
    assert_eq!(body["report"]["run"]["phase"], "notExecuted");

    state
        .git_runner
        .checkout_branch(&state.repo_workdir, &branch)
        .await
        .unwrap();
    let stack_yaml = tokio::fs::read_to_string(
        state
            .repo_workdir
            .join("tenants/acme/infra/vm-scoped/stack.yaml"),
    )
    .await
    .unwrap();
    state
        .git_runner
        .checkout_branch(&state.repo_workdir, "main")
        .await
        .unwrap();
    let stack: Value = noyalib::from_str(&stack_yaml).unwrap();
    assert_eq!(
        stack["spec"]["backend"],
        "s3://pulumi-state/tenants/acme?endpoint=seaweedfs:8333&region=us-east-1&s3ForcePathStyle=true"
    );
    assert_eq!(
        stack["spec"]["envRefs"]["AWS_ACCESS_KEY_ID"]["secret"]["name"],
        "keel-s3-credentials"
    );
    let mirror = fixture
        .secret_store
        .inspect_raw_secret("tenant-acme", "keel-s3-credentials")
        .await
        .expect("tenant mirror of scoped identity");
    assert_eq!(
        mirror.data.unwrap()["accessKey"].0,
        b"TENANTACCESSKEY0001".to_vec()
    );
}

/// When a tenant has no scoped identity yet, the proposal path lazily provisions it through the
/// configured `TenantS3IdentityProvisioner` (kube-rs exec in production) once the namespace exists.
#[tokio::test]
async fn test_tenant_stack_lazily_provisions_scoped_identity() {
    use keel_kube::{
        SeaweedFsIdentityOps, TenantS3IdentityProvisioner, TenantS3ProvisionOutcome,
        provision_tenant_s3_identity_with,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockProvisioner {
        store: Arc<InMemorySecretStore>,
        namespace_present: bool,
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl SeaweedFsIdentityOps for MockProvisioner {
        async fn namespace_exists(&self, _ns: &str) -> Result<bool, keel_kube::KubeError> {
            Ok(self.namespace_present)
        }
        async fn run_weed_shell(&self, script: &str) -> Result<(), keel_kube::KubeError> {
            assert!(script.contains("-user=keel-tenant-acme"));
            assert!(script.contains("pulumi-state/tenants/acme/*"));
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl TenantS3IdentityProvisioner for MockProvisioner {
        async fn provision_tenant_s3_identity(
            &self,
            tenant_id: &str,
        ) -> Result<TenantS3ProvisionOutcome, keel_kube::KubeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            provision_tenant_s3_identity_with(
                self.store.as_ref(),
                self,
                "s3://pulumi-state?endpoint=seaweedfs:8333",
                tenant_id,
            )
            .await
        }
    }

    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let propose = |name: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/tenants/acme/proposals")
            .header(COOKIE, &cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "tenant": "acme",
                    "catalogId": "proxmox-vm",
                    "kind": "infra",
                    "name": name,
                    "values": { "cores": 2 }
                })
                .to_string(),
            ))
            .unwrap()
    };

    // Namespace not reconciled yet -> provisioner is a no-op and the server fails closed.
    let missing = Arc::new(MockProvisioner {
        store: fixture.secret_store.clone(),
        namespace_present: false,
        calls: AtomicUsize::new(0),
    });
    let app = build_router(
        fixture
            .state
            .clone()
            .with_s3_credentials_secret(Some("keel-s3-credentials".to_string()))
            .with_tenant_s3_provisioner(Some(missing.clone())),
    );
    let res = app.oneshot(propose("vm-a")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(missing.calls.load(Ordering::SeqCst), 1);

    // Namespace present -> identity provisioned on demand, Stack proposal succeeds.
    let present = Arc::new(MockProvisioner {
        store: fixture.secret_store.clone(),
        namespace_present: true,
        calls: AtomicUsize::new(0),
    });
    let app = build_router(
        fixture
            .state
            .clone()
            .with_s3_credentials_secret(Some("keel-s3-credentials".to_string()))
            .with_tenant_s3_provisioner(Some(present.clone())),
    );
    let res = app.clone().oneshot(propose("vm-b")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(present.calls.load(Ordering::SeqCst), 1);
    let mirror = fixture
        .secret_store
        .inspect_raw_secret("tenant-acme", "keel-s3-credentials")
        .await
        .expect("scoped identity mirrored for PKO envRefs");
    assert!(
        String::from_utf8(mirror.data.unwrap()["accessKey"].0.clone())
            .unwrap()
            .starts_with("KEEL")
    );

    // Identity now exists -> no further provisioning round-trips.
    let res = app.oneshot(propose("vm-c")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(present.calls.load(Ordering::SeqCst), 1);
}

async fn post_status(app: &axum::Router, uri: &str, cookie: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn get_json_status(app: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn test_out_of_band_stack_operations_require_tenant_admin_and_refuse_protected() {
    let fixture = setup_fixture().await;
    let app = build_router(fixture.state.clone());
    let member = login_with_groups(&fixture, "member@acme", vec!["tenant-acme"]).await;
    let tenant_admin = login_with_groups(&fixture, "admin@acme", vec!["tenant-acme-admin"]).await;

    // OPEN-11: tenant members may not run force-delete / unlock / restart.
    for action in ["force-delete", "unlock", "restart"] {
        let uri = format!("/api/v1/stacks/tenant-acme/acme-net/{action}");
        assert_eq!(
            post_status(&app, &uri, &member).await,
            StatusCode::FORBIDDEN,
            "{action}"
        );
        // Tenant admins pass authorization; the repo-only provider then reports no live cluster.
        assert_eq!(
            post_status(&app, &uri, &tenant_admin).await,
            StatusCode::SERVICE_UNAVAILABLE,
            "{action}"
        );
    }
    // Non-tenant namespaces require cluster admin.
    assert_eq!(
        post_status(
            &app,
            "/api/v1/stacks/flux-system/core/unlock",
            &tenant_admin
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // OPEN-1: a stack protected via the canonical `keel.io/protect` label (or spec.config.protect)
    // is refused with 409 before any live mutation.
    let dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/infra/locked-vm");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("stack.yaml"),
        "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: locked-vm\n  namespace: tenant-acme\n  labels:\n    keel.io/tenant: acme\n    keel.io/protect: \"true\"\nspec:\n  stack: acme/locked-vm\n",
    )
    .await
    .unwrap();
    assert_eq!(
        post_status(
            &app,
            "/api/v1/stacks/tenant-acme/locked-vm/force-delete",
            &tenant_admin
        )
        .await,
        StatusCode::CONFLICT
    );

    // OPEN-16: the PROTECT column reflects the canonical label.
    let (status, body) = get_json_status(&app, "/api/v1/stacks?tenant=acme", &tenant_admin).await;
    assert_eq!(status, StatusCode::OK);
    let locked = body["stacks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "locked-vm")
        .unwrap();
    assert_eq!(locked["protect"], true);
}

#[tokio::test]
async fn test_ssh_info_rejects_unknown_and_non_vm_stacks() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    // OPEN-17: unknown stack -> 404 (no fabricated `<stack>.<tenant>.local` target).
    let (status, _) =
        get_json_status(&app, "/api/v1/stacks/tenant-acme/ghost/ssh-info", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Non-VM stack (`acme-net` is a network service) -> 422.
    let (status, body) = get_json_status(
        &app,
        "/api/v1/stacks/tenant-acme/acme-net/ssh-info",
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("SSH not supported for service"),
        "{body}"
    );

    // Existing VM stack still resolves.
    let dir = fixture.state.repo_workdir.join("tenants/acme/infra/web-vm");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("stack.yaml"),
        "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: web-vm\n  namespace: tenant-acme\n  labels:\n    keel.io/tenant: acme\n    keel.io/provider: proxmox\n    keel.io/resource-kind: vm\nspec:\n  stack: acme/web-vm\n",
    )
    .await
    .unwrap();
    let (status, body) =
        get_json_status(&app, "/api/v1/stacks/tenant-acme/web-vm/ssh-info", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stackName"], "web-vm");
    assert_eq!(body["provider"], "proxmox");
}

// ============================================================================
// Git/forge correctness, proposal flow & auth hardening
// (OPEN-2, OPEN-11, OPEN-12, OPEN-14, OPEN-29, CLI-15)
// ============================================================================

/// Records `delete_tenant_stack` calls so merge-time Stack deletions can be asserted.
#[derive(Default)]
struct RecordingStateProvider {
    deleted: std::sync::Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl keel_server::ClusterStateProvider for RecordingStateProvider {
    async fn list_stacks(
        &self,
        _tenant: Option<&str>,
        _provider: Option<&str>,
    ) -> Result<Vec<keel_server::StackSummary>, keel_server::ServerError> {
        Ok(Vec::new())
    }

    async fn list_releases(
        &self,
        _tenant: Option<&str>,
    ) -> Result<Vec<keel_server::ReleaseSummary>, keel_server::ServerError> {
        Ok(Vec::new())
    }

    async fn delete_tenant_stack(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<bool, keel_server::ServerError> {
        self.deleted
            .lock()
            .unwrap()
            .push((namespace.to_string(), name.to_string()));
        Ok(true)
    }
}

async fn send_json(
    app: &axum::Router,
    method: Method,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(COOKIE, cookie);
    let req_body = match body {
        Some(b) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app
        .clone()
        .oneshot(builder.body(req_body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn test_pr_diff_is_merge_base_and_merge_deletes_stacks_from_diff() {
    let fixture = setup_fixture().await;
    let recorder = Arc::new(RecordingStateProvider::default());
    let state = fixture.state.clone().with_state_provider(recorder.clone());
    let app = build_router(state.clone());
    let cookie = login_and_get_cookie(&fixture).await;

    // PR1 deletes the seeded `tenants/acme/infra/net` stack (metadata.name: acme-net);
    // PR2 deletes the `redis` release. Both branch from the same `main`.
    let (status, pr1) = send_json(
        &app,
        Method::DELETE,
        "/api/v1/stacks/tenant-acme/net",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pr1}");
    let pr1_id = pr1["proposal"]["id"].as_str().unwrap().to_string();
    // PullRequestInfo is camelCase on the wire (CLI-15).
    assert!(pr1["proposal"]["sourceBranch"].is_string());
    assert!(pr1["proposal"].get("source_branch").is_none());
    let (status, pr2) = send_json(
        &app,
        Method::DELETE,
        "/api/v1/releases/tenant-acme/redis",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pr2}");
    let pr2_id = pr2["proposal"]["id"].as_str().unwrap().to_string();

    // Merge PR1: the live Stack deletion is derived from the removed manifest (name `acme-net`),
    // not from the PR title text.
    let (status, merged) = send_json(
        &app,
        Method::POST,
        &format!("/api/v1/tenants/acme/proposals/{pr1_id}/merge"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["proposal"]["state"], "merged");
    assert_eq!(
        recorder.deleted.lock().unwrap().clone(),
        vec![("tenant-acme".to_string(), "acme-net".to_string())]
    );

    // `main` moves on further with an unrelated commit pushed from another clone.
    let other = fixture._temp_dir.path().join("other-clone");
    let git = GitRunner::new();
    git.clone(&state.remote_url, &other).await.unwrap();
    tokio::fs::write(other.join("unrelated-change.yaml"), "x: 1\n")
        .await
        .unwrap();
    git.add(&other, ["unrelated-change.yaml"]).await.unwrap();
    git.commit(
        &other,
        "chore: unrelated",
        &CommitAuthor::new("Other", "other@keel.local"),
    )
    .await
    .unwrap();
    git.push(&other, "origin", "main").await.unwrap();

    // Open PR2 diff only shows its own change: neither PR1's merged deletion nor the unrelated
    // commit (two-dot vs current main would show both, reversed).
    let (status, preview) = send_json(
        &app,
        Method::GET,
        &format!("/api/v1/tenants/acme/proposals/{pr2_id}/preview"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let diff = preview["gitDiff"].as_str().unwrap_or_default();
    assert!(diff.contains("tenants/acme/apps/redis"), "{diff}");
    assert!(!diff.contains("unrelated-change.yaml"), "{diff}");
    assert!(!diff.contains("tenants/acme/infra/net"), "{diff}");

    // Merged PR1 diff uses the recorded merge-base, excluding later main commits.
    let (status, preview) = send_json(
        &app,
        Method::GET,
        &format!("/api/v1/tenants/acme/proposals/{pr1_id}/preview"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let diff = preview["gitDiff"].as_str().unwrap_or_default();
    assert!(diff.contains("tenants/acme/infra/net/stack.yaml"), "{diff}");
    assert!(!diff.contains("unrelated-change.yaml"), "{diff}");
    assert!(!diff.contains("tenants/acme/apps/redis"), "{diff}");
}

#[tokio::test]
async fn test_reject_response_is_camel_case_and_titles_are_conventional() {
    let fixture = setup_fixture().await;
    let app = build_router(fixture.state.clone());
    let cookie = login_and_get_cookie(&fixture).await;

    let (status, deleted) = send_json(
        &app,
        Method::DELETE,
        "/api/v1/releases/tenant-acme/redis",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let pr_id = deleted["proposal"]["id"].as_str().unwrap().to_string();

    let (status, rejected) = send_json(
        &app,
        Method::POST,
        &format!("/api/v1/tenants/acme/proposals/{pr_id}/reject"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");
    assert_eq!(rejected["proposal"]["state"], "closed");
    assert!(rejected["proposal"]["sourceBranch"].is_string());
    assert!(rejected["proposal"]["headSha"].is_string());
    assert!(rejected["proposal"].get("source_branch").is_none());
    assert!(rejected["gcPlan"].is_object());

    // A UI-style free-form title is replaced by a Conventional Commit title (kept in the body).
    let (status, created) = send_json(
        &app,
        Method::POST,
        "/api/v1/tenants/acme/proposals",
        &cookie,
        Some(json!({
            "tenant": "acme",
            "catalogId": "grafana",
            "name": "dash",
            "title": "Deploy Grafana for acme",
            "values": { "replicas": 1 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["proposal"]["title"], "feat(acme): deploy dash");
    assert!(
        created["proposal"]["body"]
            .as_str()
            .unwrap()
            .contains("Deploy Grafana for acme")
    );
}

#[tokio::test]
async fn test_auth_hardening_csrf_group_mapping_and_fetch_schema_rbac() {
    let fixture = setup_fixture().await;
    let app = build_router(fixture.state.clone());
    let admin_cookie = login_and_get_cookie(&fixture).await;

    // CSRF: cookie-authenticated mutation with a Host but no Origin/Referer/Sec-Fetch-Site -> 403.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants")
                .header(COOKIE, &admin_cookie)
                .header("host", "keel.local")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "id": "gamma" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Tenant ids ending in `-admin` are reserved (group-name collision guard).
    let (status, body) = send_json(
        &app,
        Method::POST,
        "/api/v1/tenants",
        &admin_cookie,
        Some(json!({ "id": "acme-admin" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A bare IdP group equal to the tenant id no longer grants tenant access.
    let bare = login_with_groups(&fixture, "eve@keel.local", vec!["acme"]).await;
    let (status, _) = send_json(
        &app,
        Method::GET,
        "/api/v1/tenants/acme/proposals",
        &bare,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let member = login_with_groups(&fixture, "carol@keel.local", vec!["tenant-acme"]).await;
    let (status, _) = send_json(
        &app,
        Method::GET,
        "/api/v1/tenants/acme/proposals",
        &member,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // fetch-schema is restricted to admins / tenant admins (SSRF surface).
    let (status, _) = send_json(
        &app,
        Method::POST,
        "/api/v1/catalog/fetch-schema",
        &member,
        Some(json!({ "repoUrl": "https://charts.example.com", "chart": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Tenant admin passes RBAC but private targets are still rejected before any fetch.
    let tenant_admin =
        login_with_groups(&fixture, "bob@keel.local", vec!["tenant-acme-admin"]).await;
    for repo in [
        "http://127.0.0.1:8080/charts",
        "http://[::ffff:10.0.0.1]/charts",
        "https://svc.cluster.local/charts",
        "http://169.254.169.254/latest",
    ] {
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/v1/catalog/fetch-schema",
            &tenant_admin,
            Some(json!({ "repoUrl": repo, "chart": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{repo}: {body}");
    }

    // CLI loopback login endpoint validates the loopback redirect before touching OIDC.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/cli/exchange")
                .method(Method::POST)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "code": "bogus",
                        "codeVerifier": "a".repeat(43),
                        "redirectUri": "http://127.0.0.1:5000/callback"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// OPEN-5/OPEN-9/OPEN-10: Pulumi-backed proposals apply a real preview `Stack` into
/// `keel-previews`; PKO results are collected from cluster state (not process memory), published
/// to the PR, and the preview is garbage-collected once the PR is closed — even after a restart.
#[tokio::test]
async fn test_pko_preview_lifecycle_is_applied_published_and_gced_from_cluster_state() {
    use keel_kube::{InMemoryPreviewCluster, PreviewCluster, PreviewPrerequisites};

    let fixture = setup_fixture().await;
    let cluster = InMemoryPreviewCluster::new().with_prerequisites(PreviewPrerequisites {
        s3_secret: Some("keel-preview-s3-acme".to_string()),
        git_url: Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git".to_string()),
        git_secret: Some("git-credentials".to_string()),
        git_secret_keys: ["password".to_string(), "username".to_string()].into(),
    });
    cluster.put_tenant_secret(
        "acme",
        "git-credentials",
        &[
            ("username", "git"),
            ("password", "ro-token"),
            ("other", "never"),
        ],
    );
    cluster.put_tenant_secret(
        "acme",
        "keel-pulumi-passphrase",
        &[("passphrase", "tenant-pass")],
    );
    let state = fixture
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(cluster.clone())));
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(state.clone());

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "proxmox-vm",
                        "kind": "infra",
                        "name": "vm-preview",
                        "values": { "cores": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let pr_id = body["proposal"]["id"].as_str().unwrap().to_string();
    let stack_name = format!("preview-stack-pr-{pr_id}");
    assert_eq!(body["report"]["source"], "synthesized");
    assert_eq!(body["report"]["run"]["phase"], "pending");
    assert_eq!(
        body["report"]["run"]["stack"],
        format!("keel-previews/{stack_name}")
    );

    // The applied preview Stack is a real `spec.preview: true` Stack with persisted identity,
    // the tenant-scoped backend/credentials, and the tenant's cluster Git source.
    let applied = cluster.stack(&stack_name).expect("preview Stack applied");
    assert_eq!(applied["spec"]["preview"], true);
    assert_eq!(applied["spec"]["destroyOnFinalize"], false);
    assert_eq!(
        applied["metadata"]["labels"]["keel.io/pr-id"],
        pr_id.as_str()
    );
    assert_eq!(applied["metadata"]["labels"]["keel.io/tenant"], "acme");
    assert_eq!(applied["metadata"]["labels"]["keel.io/provider"], "proxmox");
    assert!(applied["metadata"]["annotations"]["keel.io/head-sha"].is_string());
    assert!(
        applied["spec"]["backend"]
            .as_str()
            .unwrap()
            .starts_with("s3://pulumi-state/tenants/acme")
    );
    assert_eq!(
        applied["spec"]["envRefs"]["AWS_ACCESS_KEY_ID"]["secret"]["name"],
        "keel-preview-s3-acme"
    );
    assert!(
        applied["spec"]["stack"]
            .as_str()
            .unwrap()
            .ends_with("/acme-vm-preview")
    );
    // PKO `gitSource`: the preview clones the PR branch of the platform-written tenant source
    // (the tenant Stack's `projectRepo`, not the live `keel-gitops` URL) with the mirrored
    // read-only credentials; no Flux GitRepository / source-controller artifact is involved.
    assert!(applied["spec"].get("fluxSource").is_none());
    assert_eq!(applied["spec"]["projectRepo"], TENANT_PKO_GIT_URL);
    assert!(
        applied["spec"]["branch"]
            .as_str()
            .unwrap()
            .starts_with("keel/acme/")
    );
    assert_eq!(applied["spec"]["repoDir"], "tenants/acme/infra/vm-preview");
    let git_copy = format!("keel-preview-{pr_id}-git-credentials");
    assert_eq!(
        applied["spec"]["gitAuth"],
        json!({ "basicAuth": {
            "userName": { "type": "Secret", "secret": { "name": git_copy, "key": "username" } },
            "password": { "type": "Secret", "secret": { "name": git_copy, "key": "password" } }
        }})
    );
    assert_eq!(applied["spec"]["shallow"], true);
    // The read-only Git credential is copied per preview (only the auth keys), owned by the
    // preview Stack and garbage-collected with it; no long-lived per-tenant mirror.
    let copy = cluster
        .preview_secret(&git_copy)
        .expect("per-preview Git credential copy");
    assert_eq!(copy.owner_stack.as_deref(), Some(stack_name.as_str()));
    // PKO resolves `gitAuth` as soon as it sees the Stack and stalls (`SpecInvalid`, never
    // retried) when the Secret is missing: every copy is written before the Stack is applied,
    // and only then adopted by it.
    let ops = cluster.operations();
    let apply_at = ops
        .iter()
        .position(|o| o == &format!("apply:{stack_name}"))
        .expect("Stack applied");
    let copies: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.starts_with("copy:"))
        .map(|(i, _)| i)
        .collect();
    let adoptions: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.starts_with("adopt:"))
        .map(|(i, _)| i)
        .collect();
    assert!(ops.contains(&format!("copy:{git_copy}")), "{ops:?}");
    assert!(copies.iter().all(|&i| i < apply_at), "{ops:?}");
    assert_eq!(copies.len(), adoptions.len(), "{ops:?}");
    assert!(adoptions.iter().all(|&i| i > apply_at), "{ops:?}");
    assert_eq!(
        copy.data.keys().cloned().collect::<Vec<_>>(),
        vec!["password".to_string(), "username".to_string()]
    );
    assert!(cluster.preview_secret("keel-preview-git-acme").is_none());

    // PKO finishes the preview; a *fresh* server state (restart: empty in-memory cache) collects
    // and publishes the real plan from cluster state only.
    cluster.set_stack_status(
        &stack_name,
        json!({ "lastUpdate": {
            "name": "u1", "generation": 1, "state": "succeeded",
            "lastAttemptedCommit": "keel/acme/x@sha1:feedface"
        } }),
    );
    cluster.put_workspace_log(
        &stack_name,
        "Previewing update (acme-vm-preview):\n  + proxmoxve:VM/virtualMachine:VirtualMachine: (create)\n    [urn=urn:pulumi:acme-vm-preview::vm::proxmoxve:VM/virtualMachine:VirtualMachine::vm-preview]\nResources:\n    + 1 to create\n    1 unchanged\n",
    );
    let restarted = fixture
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(cluster.clone())));
    restarted.preview_cache.write().await.clear();
    let outcome = keel_server::preview_lifecycle::reconcile_previews_once(&restarted).await;
    assert_eq!(outcome.published, 1, "{outcome:?}");
    // Second pass is idempotent (reported commit recorded on the Stack).
    let again = keel_server::preview_lifecycle::reconcile_previews_once(&restarted).await;
    assert_eq!(again.published, 0);
    assert_eq!(
        cluster.stack(&stack_name).unwrap()["metadata"]["annotations"]["keel.io/preview-reported-commit"],
        "feedface"
    );

    let res = build_router(restarted.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr_id}/preview"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let preview: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(preview["report"]["source"], "pko");
    assert_eq!(preview["report"]["run"]["phase"], "succeeded");
    assert_eq!(preview["report"]["summary"]["create"], 1);
    assert_eq!(preview["report"]["summary"]["same"], 1);
    assert_eq!(preview["report"]["resources"][0]["name"], "vm-preview");
    let comments = preview["comments"].as_array().unwrap();
    assert!(
        comments
            .iter()
            .any(|c| c.to_string().contains("PKO preview:** Succeeded")),
        "{comments:?}"
    );

    // A copy never adopted by a Stack (e.g. keel-server stopped between copying and applying)
    // is found by its labels; an open PR's copies are never swept (a launch may be in progress).
    cluster.put_tenant_secret("acme", "orphan-source", &[("k", "v")]);
    let orphan = format!("keel-preview-{pr_id}-orphan-source");
    cluster
        .copy_preview_secrets(
            "acme",
            &pr_id,
            &[keel_pko::PreviewSecretCopy {
                source_name: "orphan-source".to_string(),
                target_name: orphan.clone(),
                keys: ["k".to_string()].into(),
                proposed_data: None,
            }],
        )
        .await
        .unwrap();
    let outcome = keel_server::preview_lifecycle::reconcile_previews_once(&restarted).await;
    assert_eq!(outcome.deleted, 0, "{outcome:?}");
    assert!(cluster.preview_secret(&orphan).is_some());

    // Closing the PR out-of-band (not via the API) is still GC'd by the reconciler: the Stack,
    // the adopted copies (with their owner) and the orphan copy.
    restarted.forge.close_pull_request(&pr_id).await.unwrap();
    let outcome = keel_server::preview_lifecycle::reconcile_previews_once(&restarted).await;
    assert_eq!(outcome.deleted, 2, "{outcome:?}");
    assert!(cluster.stack_names().is_empty());
    assert!(cluster.preview_secret_names().is_empty());
}

/// Launch failures never leave a Stack pointing at missing Secrets nor stray credential copies:
/// a failed copy applies nothing, and a failed adoption tears the preview down.
#[tokio::test]
async fn test_pko_preview_launch_failures_clean_up_secret_copies() {
    use keel_kube::{InMemoryPreviewCluster, PreviewPrerequisites};

    let prereqs = PreviewPrerequisites {
        git_url: Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git".to_string()),
        git_secret: Some("git-credentials".to_string()),
        git_secret_keys: ["password".to_string(), "username".to_string()].into(),
        ..PreviewPrerequisites::default()
    };

    // 1. The passphrase source is missing: the Git copy may already be written, but no Stack is
    //    applied and the partial copies are removed.
    let fixture = setup_fixture().await;
    let cluster = InMemoryPreviewCluster::new().with_prerequisites(prereqs.clone());
    cluster.put_tenant_secret(
        "acme",
        "git-credentials",
        &[("username", "git"), ("password", "ro-token")],
    );
    let body = post_infra_proposal_with_cluster(&fixture, cluster.clone(), "vm-no-pass").await;
    assert_eq!(body["report"]["run"]["phase"], "notExecuted", "{body}");
    assert!(cluster.stack_names().is_empty());
    assert!(cluster.preview_secret_names().is_empty());
    assert!(
        !cluster.operations().iter().any(|o| o.starts_with("apply:")),
        "{:?}",
        cluster.operations()
    );

    // 2. Adoption fails after the apply: Stack and copies are deleted, nothing keeps running.
    let fixture = setup_fixture().await;
    let cluster = InMemoryPreviewCluster::new().with_prerequisites(prereqs);
    cluster.put_tenant_secret(
        "acme",
        "git-credentials",
        &[("username", "git"), ("password", "ro-token")],
    );
    cluster.put_tenant_secret(
        "acme",
        "keel-pulumi-passphrase",
        &[("passphrase", "tenant-pass")],
    );
    cluster.fail_adoption_with("conflict");
    let body = post_infra_proposal_with_cluster(&fixture, cluster.clone(), "vm-no-adopt").await;
    assert_eq!(body["report"]["run"]["phase"], "notExecuted", "{body}");
    assert!(
        body["report"]["run"]["message"]
            .as_str()
            .unwrap()
            .contains("could not be attached"),
        "{body}"
    );
    assert!(cluster.stack_names().is_empty());
    assert!(cluster.preview_secret_names().is_empty());
}

/// A rejected preview apply (e.g. Kyverno admission denial) is reported honestly as `failed`, and
/// Flux-only proposals are labelled `notExecuted` instead of claiming a preview Stack.
#[tokio::test]
async fn test_pko_preview_apply_failure_is_reported_honestly() {
    use keel_kube::{InMemoryPreviewCluster, PreviewPrerequisites};

    let fixture = setup_fixture().await;
    let cluster = InMemoryPreviewCluster::new().with_prerequisites(PreviewPrerequisites {
        git_url: Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git".to_string()),
        ..PreviewPrerequisites::default()
    });
    cluster.put_tenant_secret(
        "acme",
        "keel-pulumi-passphrase",
        &[("passphrase", "tenant-pass")],
    );
    cluster.fail_applies_with("admission webhook denied: restrict-pulumi-providers");
    let state = fixture
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(cluster.clone())));
    let cookie = login_and_get_cookie(&fixture).await;
    let res = build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "proxmox-vm",
                        "kind": "infra",
                        "name": "vm-denied",
                        "values": { "cores": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["report"]["run"]["phase"], "failed");
    assert!(
        body["report"]["run"]["message"]
            .as_str()
            .unwrap()
            .contains("restrict-pulumi-providers")
    );
    assert!(body["report"]["run"]["stack"].is_null());
    assert!(cluster.stack_names().is_empty());
    // The copies written before the rejected apply are removed again.
    assert!(cluster.preview_secret_names().is_empty());
    let ops = cluster.operations();
    let apply_at = ops.iter().position(|o| o.starts_with("apply:")).unwrap();
    assert!(
        ops[..apply_at].iter().any(|o| o.starts_with("copy:")),
        "{ops:?}"
    );
    assert!(
        ops[apply_at..]
            .iter()
            .any(|o| o.starts_with("delete-secrets:")),
        "{ops:?}"
    );
}

/// Posts the `proxmox-vm` infra proposal used by the fail-closed preview source tests.
async fn post_infra_proposal_with_cluster(
    fixture: &TestFixture,
    cluster: keel_kube::InMemoryPreviewCluster,
    name: &str,
) -> Value {
    let state = fixture
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(cluster)));
    let cookie = login_and_get_cookie(fixture).await;
    let res = build_router(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "proxmox-vm",
                        "kind": "infra",
                        "name": name,
                        "values": { "cores": 2 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

/// A tenant without its own `tenant-<id>/keel-gitops` source must fail closed: no preview Stack
/// or GitRepository is applied (so nothing can read the platform `flux-system` whole-repository
/// source), and the report/PR comment carry an actionable `notExecuted` reason.
#[tokio::test]
async fn test_pko_preview_fails_closed_without_tenant_git_source() {
    use keel_kube::InMemoryPreviewCluster;

    let fixture = setup_fixture().await;
    let cluster = InMemoryPreviewCluster::new().without_tenant_source();
    let body = post_infra_proposal_with_cluster(&fixture, cluster.clone(), "vm-no-source").await;
    assert_eq!(body["report"]["run"]["phase"], "notExecuted");
    let message = body["report"]["run"]["message"].as_str().unwrap();
    assert!(message.contains("tenant-acme/keel-gitops"), "{message}");
    assert!(message.contains("PKO preview not executed"), "{message}");
    assert!(body["report"]["run"]["stack"].is_null());
    assert!(cluster.stack_names().is_empty());

    // A cluster that reports no Git URL (defense in depth) is refused too, never falling back to
    // the control-plane remote.
    let no_url = InMemoryPreviewCluster::new();
    let body = post_infra_proposal_with_cluster(&fixture, no_url.clone(), "vm-no-url").await;
    assert_eq!(body["report"]["run"]["phase"], "notExecuted");
    assert!(
        body["report"]["run"]["message"]
            .as_str()
            .unwrap()
            .contains("never fall back to the platform repository")
    );
    assert!(no_url.stack_names().is_empty());
}

/// Seeds a provider-credential infra blueprint (`writeOnly` GOOGLE_CREDENTIALS) on `main`.
async fn seed_credential_blueprint(fixture: &TestFixture) {
    let bp_dir = fixture.state.repo_workdir.join("catalog/infra/gcp-free-vm");
    tokio::fs::create_dir_all(&bp_dir).await.unwrap();
    tokio::fs::write(
        bp_dir.join("catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: gcp-free-vm
  name: GCP Free VM
  category: Compute
  provider: proxmox
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  schema:
    type: object
    required:
      - vmName
    properties:
      vmName:
        type: string
        default: keel-free-vm
      GOOGLE_CREDENTIALS:
        type: string
        writeOnly: true
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        bp_dir.join("Pulumi.yaml"),
        "name: gcp-free-vm\nruntime: yaml\nresources: {}\n",
    )
    .await
    .unwrap();
    // An instance whose committed Stack points at the unscoped (bucket-root) backend.
    let legacy_dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/infra/legacy-vm");
    tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
    tokio::fs::write(
        legacy_dir.join("stack.yaml"),
        r#"apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: legacy-vm
  namespace: tenant-acme
spec:
  stack: organization/gcp-free-vm/acme-legacy-vm
  backend: s3://pulumi-state
"#,
    )
    .await
    .unwrap();
    let git = &fixture.state.git_runner;
    let wd = &fixture.state.repo_workdir;
    git.add(
        wd,
        [
            "catalog/infra/gcp-free-vm/catalog.yaml",
            "catalog/infra/gcp-free-vm/Pulumi.yaml",
            "tenants/acme/infra/legacy-vm/stack.yaml",
        ],
    )
    .await
    .unwrap();
    git.commit(
        wd,
        "feat(catalog): add credential blueprint",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(wd, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();
}

async fn propose_infra(
    app: &axum::Router,
    cookie: &str,
    values: Value,
    name: &str,
) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "gcp-free-vm",
                        "kind": "infra",
                        "name": name,
                        "values": values
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

/// OPEN-9b + preview provider credentials: previews of brand-new *and* existing tenant stacks run
/// on the tenant state prefix, and exactly the Secrets the tenant Stack references are copied into
/// `keel-previews` under per-preview names (labelled, owned by the preview Stack, GC'd with it).
#[tokio::test]
async fn test_preview_uses_tenant_backend_and_copies_only_referenced_provider_secrets() {
    use keel_kube::{InMemoryPreviewCluster, PREVIEW_PROVIDER_SECRET_KIND, PreviewPrerequisites};

    let fixture = setup_fixture().await;
    seed_credential_blueprint(&fixture).await;
    let cluster = InMemoryPreviewCluster::new().with_prerequisites(PreviewPrerequisites {
        s3_secret: Some("keel-preview-s3-acme".to_string()),
        git_url: Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git".to_string()),
        git_secret: Some("git-credentials".to_string()),
        git_secret_keys: ["password".to_string(), "username".to_string()].into(),
    });
    cluster.put_tenant_secret(
        "acme",
        "git-credentials",
        &[
            ("username", "git"),
            ("password", "ro-token"),
            ("other", "never"),
        ],
    );
    cluster.put_tenant_secret("acme", "unrelated-secret", &[("k", "v")]);
    cluster.put_tenant_secret(
        "acme",
        "keel-pulumi-passphrase",
        &[("passphrase", "tenant-pass")],
    );
    let state = fixture
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(cluster.clone())));
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(state.clone());

    // 1. Existing (unscoped-backend) stack, no credentials in this change.
    let (status, body) =
        propose_infra(&app, &cookie, json!({ "vmName": "legacy" }), "legacy-vm").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pr = body["proposal"]["id"].as_str().unwrap().to_string();
    let applied = cluster.stack(&format!("preview-stack-pr-{pr}")).unwrap();
    assert_eq!(applied["spec"]["backend"], "s3://pulumi-state/tenants/acme");
    assert_eq!(
        body["previewStack"]["spec"]["backend"],
        "s3://pulumi-state/tenants/acme"
    );

    // 2. Brand-new stack carrying a provider credential.
    let (status, body) = propose_infra(
        &app,
        &cookie,
        json!({ "vmName": "fresh", "GOOGLE_CREDENTIALS": "{\"type\":\"service_account\"}" }),
        "fresh-vm",
    )
    .await;
    if status == StatusCode::BAD_REQUEST {
        // sops/age unavailable: secret fields are rejected rather than committed in plaintext.
        assert!(body.to_string().contains("SOPS age key"), "{body}");
        eprintln!("sops/age unavailable: skipping provider credential copy assertions");
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pr = body["proposal"]["id"].as_str().unwrap().to_string();
    let stack_name = format!("preview-stack-pr-{pr}");
    let applied = cluster.stack(&stack_name).unwrap();
    assert_eq!(applied["spec"]["backend"], "s3://pulumi-state/tenants/acme");
    let copy_name = format!("keel-preview-{pr}-fresh-vm-secrets");
    assert_eq!(
        applied["spec"]["envRefs"]["GOOGLE_CREDENTIALS"]["secret"]["name"],
        copy_name.as_str()
    );
    assert_eq!(
        applied["spec"]["envRefs"]["AWS_ACCESS_KEY_ID"]["secret"]["name"],
        "keel-preview-s3-acme"
    );
    let this_pr_copies: Vec<String> = cluster
        .preview_secret_names()
        .into_iter()
        .filter(|n| n.starts_with(&format!("keel-preview-{pr}-")))
        .collect();
    assert_eq!(
        this_pr_copies,
        vec![
            copy_name.clone(),
            format!("keel-preview-{pr}-git-credentials"),
            format!("keel-preview-{pr}-keel-pulumi-passphrase")
        ]
    );
    let copied = cluster.preview_secret(&copy_name).unwrap();
    assert_eq!(copied.owner_stack.as_deref(), Some(stack_name.as_str()));
    assert_eq!(copied.labels["keel.io/pr-id"], pr.as_str());
    assert_eq!(copied.labels["keel.io/tenant"], "acme");
    assert_eq!(
        copied.labels["keel.io/secret-kind"],
        PREVIEW_PROVIDER_SECRET_KIND
    );
    assert_eq!(
        copied.data["GOOGLE_CREDENTIALS"],
        b"{\"type\":\"service_account\"}".to_vec()
    );
    // The API response never echoes credential values.
    assert!(!body.to_string().contains("service_account"));

    // GC of the preview Stack removes its credential copies (ownerReference).
    keel_server::preview_lifecycle::gc_preview_for_pr(&state, &pr, "test").await;
    assert!(
        !cluster
            .preview_secret_names()
            .iter()
            .any(|n| n.starts_with(&format!("keel-preview-{pr}-")))
    );
}

/// Per-tenant Pulumi state encryption: tenant Stacks and their previews source
/// `PULUMI_CONFIG_PASSPHRASE` from the tenant's `keel-pulumi-passphrase` Secret (never an empty
/// literal), previews receive a per-preview owned copy of it, and proposals fail closed (409)
/// when the tenant has no passphrase Secret.
#[tokio::test]
async fn test_tenant_stacks_use_tenant_passphrase_secret_and_fail_closed_without_it() {
    use keel_kube::{InMemoryPreviewCluster, PreviewPrerequisites};

    let propose = |cookie: &str, name: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/tenants/acme/proposals")
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "tenant": "acme",
                    "catalogId": "proxmox-vm",
                    "kind": "infra",
                    "name": name,
                    "values": { "cores": 2 }
                })
                .to_string(),
            ))
            .unwrap()
    };
    let passphrase_env = |stack: &Value| -> Value {
        stack["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "PULUMI_CONFIG_PASSPHRASE")
            .cloned()
            .expect("PULUMI_CONFIG_PASSPHRASE env present")
    };

    // 1. No tenant passphrase Secret -> 409 with a clear message; nothing committed.
    let fixture = setup_fixture_without_tenant_passphrase().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());
    let res = app
        .clone()
        .oneshot(propose(&cookie, "vm-nopass"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("no Pulumi state passphrase"), "{body}");
    assert!(msg.contains("tenant-acme/keel-pulumi-passphrase"), "{body}");
    assert!(
        !fixture
            .state
            .repo_workdir
            .join("tenants/acme/infra/vm-nopass")
            .exists()
    );
    // The server never invents a passphrase on the proposal path.
    assert!(
        fixture
            .secret_store
            .inspect_raw_secret("tenant-acme", "keel-pulumi-passphrase")
            .await
            .is_none()
    );

    // 2. Provisioned tenant -> Stack + preview reference the passphrase via secretKeyRef only.
    let fixture = setup_fixture().await;
    let tenant_pass = String::from_utf8(
        fixture
            .secret_store
            .inspect_raw_secret("tenant-acme", "keel-pulumi-passphrase")
            .await
            .unwrap()
            .data
            .unwrap()["passphrase"]
            .0
            .clone(),
    )
    .unwrap();
    assert!(tenant_pass.len() >= 43);
    let cluster = InMemoryPreviewCluster::new().with_prerequisites(PreviewPrerequisites {
        s3_secret: Some("keel-preview-s3-acme".to_string()),
        git_url: Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git".to_string()),
        git_secret: Some("git-credentials".to_string()),
        git_secret_keys: ["password".to_string(), "username".to_string()].into(),
    });
    cluster.put_tenant_secret(
        "acme",
        "git-credentials",
        &[
            ("username", "git"),
            ("password", "ro-token"),
            ("other", "never"),
        ],
    );
    cluster.put_tenant_secret(
        "acme",
        "keel-pulumi-passphrase",
        &[("passphrase", tenant_pass.as_str())],
    );
    let state = fixture
        .state
        .clone()
        .with_preview_cluster(Some(Arc::new(cluster.clone())));
    let cookie = login_and_get_cookie(&fixture).await;
    let res = build_router(state)
        .oneshot(propose(&cookie, "vm-pass"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let raw = res.into_body().collect().await.unwrap().to_bytes();
    let body_text = String::from_utf8(raw.to_vec()).unwrap();
    assert!(
        !body_text.contains(&tenant_pass),
        "passphrase leaked in API response"
    );
    let body: Value = serde_json::from_str(&body_text).unwrap();
    let pr = body["proposal"]["id"].as_str().unwrap().to_string();

    // Tenant Stack committed on the PR branch.
    let branch = body["branch"].as_str().unwrap();
    let stack_yaml = fixture
        .state
        .git_runner
        .read_file_at_ref(
            &fixture.state.repo_workdir,
            branch,
            "tenants/acme/infra/vm-pass/stack.yaml",
        )
        .await
        .unwrap()
        .expect("tenant Stack committed on the PR branch");
    assert!(!stack_yaml.contains(&tenant_pass));
    let stack: Value = noyalib::from_str(&stack_yaml).unwrap();
    let env = passphrase_env(&stack);
    // Never format `env` into the failure message: on regression it would hold the passphrase.
    assert!(
        env.get("value").is_none(),
        "PULUMI_CONFIG_PASSPHRASE must be a secretKeyRef, not an inline value"
    );
    assert_eq!(
        env["valueFrom"]["secretKeyRef"],
        json!({ "name": "keel-pulumi-passphrase", "key": "passphrase" })
    );

    // Preview Stack references its per-preview copy; the copy is owned by the preview Stack.
    let preview_stack = cluster
        .stack(&format!("preview-stack-pr-{pr}"))
        .expect("preview Stack applied");
    let copy_name = format!("keel-preview-{pr}-keel-pulumi-passphrase");
    let env = passphrase_env(&preview_stack);
    // Never format `env` into the failure message: on regression it would hold the passphrase.
    assert!(
        env.get("value").is_none(),
        "PULUMI_CONFIG_PASSPHRASE must be a secretKeyRef, not an inline value"
    );
    assert_eq!(
        env["valueFrom"]["secretKeyRef"],
        json!({ "name": copy_name, "key": "passphrase" })
    );
    let copy = cluster
        .preview_secret(&copy_name)
        .expect("tenant passphrase copied into keel-previews");
    assert_eq!(copy.owner_stack, Some(format!("preview-stack-pr-{pr}")));
    assert_eq!(copy.labels["keel.io/tenant"], "acme");
    assert!(
        copy.data["passphrase"] == tenant_pass.as_bytes(),
        "preview passphrase copy does not match the tenant passphrase"
    );
    assert!(!preview_stack.to_string().contains(&tenant_pass));
}

/// Committed symlinks in the catalog must never leak files from outside the worktree into API
/// responses or tenant PR branches, nor let an update write through them (CodeQL path-injection).
#[cfg(unix)]
#[tokio::test]
async fn test_catalog_symlink_escapes_are_refused_by_listing_update_and_proposals() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());
    let workdir = fixture.state.repo_workdir.clone();

    let outside = TempDir::new().unwrap();
    let secret_manifest = outside.path().join("token.yaml");
    let secret_body = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: outside-secret-marker\n";
    tokio::fs::write(&secret_manifest, secret_body)
        .await
        .unwrap();

    let app_dir = workdir.join("catalog/apps/leaky");
    tokio::fs::create_dir_all(&app_dir).await.unwrap();
    tokio::fs::write(
        app_dir.join("catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: leaky
  name: Leaky
  category: utilities
  runtime: manifest
spec:
  defaultEngine: flux
  upstream:
    repoURL: https://example.invalid/leaky
    path: ./manifests.yaml
    version: 1.0.0
  schema:
    type: object
"#,
    )
    .await
    .unwrap();
    std::os::unix::fs::symlink(&secret_manifest, app_dir.join("manifests.yaml")).unwrap();

    let bp_dir = workdir.join("catalog/infra/leaky-bp");
    tokio::fs::create_dir_all(&bp_dir).await.unwrap();
    tokio::fs::write(
        bp_dir.join("catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: leaky-bp
  name: Leaky Blueprint
  category: compute
  provider: proxmox
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  schema:
    type: object
"#,
    )
    .await
    .unwrap();
    std::os::unix::fs::symlink(&secret_manifest, bp_dir.join("Pulumi.yaml")).unwrap();

    let git = &fixture.state.git_runner;
    git.add(&workdir, ["catalog"]).await.unwrap();
    git.commit(
        &workdir,
        "test: add escaping catalog symlinks",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();

    // 1. GET /api/v1/catalog lists the items but never returns the outside file's contents.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("outside-secret-marker"),
        "catalog response leaked the outside-repo secret marker"
    );
    let catalog: Value = serde_json::from_slice(&body).unwrap();
    assert!(catalog["manifestsYaml"].get("leaky").is_none());
    assert!(catalog["pulumiYaml"].get("leaky-bp").is_none());

    // 2. PUT with manifestsYaml refuses to write through the symlink.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/catalog/app/leaky")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "Leaky",
                        "category": "utilities",
                        "manifestsYaml": "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: overwritten\n"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!res.status().is_success(), "status {}", res.status());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("escapes"), "{body_str}");
    assert!(!body_str.contains("outside-secret-marker"));
    assert_eq!(
        tokio::fs::read_to_string(&secret_manifest).await.unwrap(),
        secret_body
    );

    // 3. An infra proposal refuses the escaping blueprint Pulumi.yaml instead of copying it
    //    (or silently substituting a stub) into the tenant PR branch.
    git.checkout_branch(&workdir, "main").await.unwrap();
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "acme",
                        "catalogId": "leaky-bp",
                        "name": "leaky-vm",
                        "values": {}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::FORBIDDEN, "{body_str}");
    assert!(body_str.contains("escapes"), "{body_str}");
    assert!(!body_str.contains("outside-secret-marker"));
}

/// The built-in self-service Rauthy app (`deploy/catalog/apps/rauthy`) proposes through the Flux
/// manifest path: secrets only reach Git SOPS-encrypted, and the tenant Kustomization decrypts them.
#[tokio::test]
async fn test_rauthy_catalog_app_proposal_keeps_secrets_out_of_git() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let app = build_router(fixture.state.clone());

    seed_rauthy_catalog(&fixture).await;
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;

    // Field names and sample values are kept apart so assertion messages name only the field.
    let fields = [
        "encKey",
        "hiqliteSecretRaft",
        "hiqliteSecretApi",
        "adminPassword",
    ];
    let samples = [
        "q2Vz7mU0ZcV1x7pYb7Yk2r9mD0aH3sW5tL8nQ4eR6fA=",
        "raftSecretSampleValue-0123456789",
        "apiSecretSampleValue-9876543210",
        "Sample-Admin#Passw0rd!",
    ];
    let mut values = json!({
        "hostname": "auth.acme.example",
        "publicPortSuffix": ":8888",
        "adminEmail": "ops@acme.example",
        "logLevel": "info",
        "storageSize": "1Gi",
        "encKeyId": "acme1",
    });
    for (i, field) in fields.into_iter().enumerate() {
        values[field] = json!(samples[i]);
    }
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/acme/proposals")
                .header(COOKIE, &cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "tenant": "acme", "catalogId": "rauthy", "values": values })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);
    if status == StatusCode::SERVICE_UNAVAILABLE {
        // No `sops`/`age-keygen` on this host; CI sets KEEL_REQUIRE_SOPS=1 to forbid skipping.
        assert!(
            std::env::var_os("KEEL_REQUIRE_SOPS").is_none(),
            "KEEL_REQUIRE_SOPS is set but the proposal could not encrypt: {body_str}"
        );
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body_str}");
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    git.checkout_branch(workdir, body["branch"].as_str().unwrap())
        .await
        .unwrap();

    let dir = workdir.join("tenants/acme/apps/rauthy");
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
    let committed = read_tree(&dir);
    for (i, field) in fields.into_iter().enumerate() {
        assert!(
            !committed.contains(samples[i]),
            "{field} committed in plaintext"
        );
    }

    let manifests = read("manifests.yaml");
    assert!(manifests.contains("${encKey}"));
    assert!(manifests.contains("${adminPassword}"));
    assert!(manifests.contains("auth.acme.example:8888"));
    assert_rauthy_split_layout(&dir, "rauthy", None);

    let tenants_k = std::fs::read_to_string(workdir.join("tenants/kustomization.yaml")).unwrap();
    assert!(tenants_k.contains("acme/apps/rauthy/config-kustomization.yaml"));
    assert!(tenants_k.contains("acme/apps/rauthy/flux-kustomization.yaml"));

    git.checkout_branch(workdir, "main").await.unwrap();
}

/// Concatenates every file below `dir` (recursively).
fn read_tree(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.push_str(&read_tree(&path));
        } else {
            out.push_str(&std::fs::read_to_string(&path).unwrap());
        }
    }
    out
}

/// Asserts the split Flux manifest-app layout of a Rauthy instance in `dir`: the
/// `<name>-config` Kustomization applies the SOPS Secret + values ConfigMap on the management
/// cluster first, and the app Kustomization `dependsOn` it with non-optional substitution.
/// `remote` = `(kubeconfig secret, target namespace)` of a remote workload cluster.
fn assert_rauthy_split_layout(dir: &std::path::Path, name: &str, remote: Option<(&str, &str)>) {
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).unwrap();
    let yaml = |file: &str| noyalib::from_str::<Value>(&read(file)).unwrap();

    // Old single-Kustomization layout files are gone.
    assert!(!dir.join("secrets.enc.yaml").exists());
    assert!(!dir.join("values-configmap.yaml").exists());

    assert!(read("config/secrets.enc.yaml").contains("ENC[AES256_GCM,"));
    assert_eq!(
        yaml("config/kustomization.yaml")["resources"],
        json!(["values-configmap.yaml", "secrets.enc.yaml"])
    );
    let cm = yaml("config/values-configmap.yaml");
    assert_eq!(cm["metadata"]["name"], format!("{name}-values"));
    assert_eq!(cm["metadata"]["namespace"], "tenant-acme");

    let config_fk = yaml("config-kustomization.yaml");
    assert_eq!(config_fk["metadata"]["name"], format!("{name}-config"));
    assert!(
        config_fk["spec"]["path"]
            .as_str()
            .unwrap()
            .ends_with(&format!(
                "/{}/config",
                dir.file_name().unwrap().to_str().unwrap()
            ))
    );
    assert_eq!(config_fk["spec"]["targetNamespace"], "tenant-acme");
    assert!(config_fk["spec"].get("kubeConfig").is_none());
    assert_eq!(config_fk["spec"]["decryption"]["provider"], "sops");
    assert_eq!(
        config_fk["spec"]["decryption"]["secretRef"]["name"],
        "sops-age"
    );

    let fk = yaml("flux-kustomization.yaml");
    assert_eq!(fk["metadata"]["name"], name);
    assert!(fk["spec"].get("decryption").is_none());
    assert!(
        fk["spec"]["dependsOn"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == format!("{name}-config") && d["namespace"] == "tenant-acme")
    );
    let sub_from = fk["spec"]["postBuild"]["substituteFrom"]
        .as_array()
        .unwrap();
    assert!(
        sub_from
            .iter()
            .any(|s| s["kind"] == "Secret" && s["name"] == format!("{name}-secrets"))
    );
    assert!(
        sub_from
            .iter()
            .any(|s| s["kind"] == "ConfigMap" && s["name"] == format!("{name}-values"))
    );
    assert!(sub_from.iter().all(|s| s["optional"] == false));

    let app_k = yaml("kustomization.yaml");
    match remote {
        None => {
            assert_eq!(app_k["resources"], json!(["manifests.yaml"]));
            assert_eq!(fk["spec"]["targetNamespace"], "tenant-acme");
            assert!(fk["spec"].get("kubeConfig").is_none());
            assert!(!dir.join("namespace.yaml").exists());
        }
        Some((kubeconfig, target_ns)) => {
            // The Secret never ships to the workload cluster; the namespace is created first.
            assert_eq!(
                app_k["resources"],
                json!(["namespace.yaml", "manifests.yaml"])
            );
            assert_eq!(fk["spec"]["kubeConfig"]["secretRef"]["name"], kubeconfig);
            assert_eq!(fk["spec"]["targetNamespace"], target_ns);
            let ns = yaml("namespace.yaml");
            assert_eq!(ns["kind"], "Namespace");
            assert_eq!(ns["metadata"]["name"], target_ns);
            assert_eq!(
                ns["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/prune"],
                "disabled"
            );
            assert_restricted_pss(&ns);
        }
    }
}

/// Keel-created workload namespaces enforce the `restricted` Pod Security Standard.
fn assert_restricted_pss(ns: &Value) {
    for mode in ["enforce", "audit", "warn"] {
        assert_eq!(
            ns["metadata"]["labels"][format!("pod-security.kubernetes.io/{mode}")],
            "restricted",
            "{ns}"
        );
    }
}

/// Commits the built-in self-service Rauthy catalog app to `main` of the fixture repository.
async fn seed_rauthy_catalog(fixture: &TestFixture) {
    let app_dir = fixture.state.repo_workdir.join("catalog/apps/rauthy");
    tokio::fs::create_dir_all(&app_dir).await.unwrap();
    tokio::fs::write(
        app_dir.join("catalog.yaml"),
        include_str!("../../../deploy/catalog/apps/rauthy/catalog.yaml"),
    )
    .await
    .unwrap();
    tokio::fs::write(
        app_dir.join("manifests.yaml"),
        include_str!("../../../deploy/catalog/apps/rauthy/manifests.yaml"),
    )
    .await
    .unwrap();
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    git.add(
        workdir,
        [
            "catalog/apps/rauthy/catalog.yaml",
            "catalog/apps/rauthy/manifests.yaml",
        ],
    )
    .await
    .unwrap();
    git.commit(
        workdir,
        "feat(catalog): add rauthy",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();
}

/// `POST /api/v1/tenants/{tenant}/proposals` for the body's `tenant`.
fn proposals_uri(body: &Value) -> String {
    format!(
        "/api/v1/tenants/{}/proposals",
        body["tenant"]
            .as_str()
            .expect("proposal body names its tenant")
    )
}

/// `POST /api/v1/tenants/{tenant}/proposals` returning `(status, body)`.
async fn post_proposal(fixture: &TestFixture, cookie: &str, body: Value) -> (StatusCode, String) {
    let res = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(proposals_uri(&body))
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn rauthy_values(hostname: &str) -> Value {
    json!({
        "hostname": hostname,
        "publicPortSuffix": ":8888",
        "adminEmail": "ops@acme.example",
        "encKeyId": "acme1",
        "encKey": "q2Vz7mU0ZcV1x7pYb7Yk2r9mD0aH3sW5tL8nQ4eR6fA=",
        "hiqliteSecretRaft": "raftSecretSampleValue-0123456789",
        "hiqliteSecretApi": "apiSecretSampleValue-9876543210",
        "adminPassword": "Sample-Admin#Passw0rd!",
    })
}

/// Secret fields only work through Flux substitution (never the Pulumi engine), and tenant routes
/// may never claim platform hosts or another app's hostname. All are rejected before any commit.
#[tokio::test]
async fn test_rauthy_catalog_app_rejects_unsupported_engine_and_hostnames() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "actionEngine": "pulumi",
                "values": rauthy_values("auth-acme.keel.local") }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Pulumi engine"), "{body}");

    // Platform hosts (default Stage 0 hosts) are reserved.
    for host in ["idp.keel.local", "keel.keel.local", "forgejo.keel.local"] {
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": "rauthy", "values": rauthy_values(host) }),
        )
        .await;
        if status == StatusCode::SERVICE_UNAVAILABLE {
            assert!(
                std::env::var_os("KEEL_REQUIRE_SOPS").is_none(),
                "KEEL_REQUIRE_SOPS is set but the proposal could not encrypt: {body}"
            );
            return;
        }
        assert_eq!(status, StatusCode::BAD_REQUEST, "{host}: {body}");
        assert!(body.contains("reserved platform host"), "{body}");
    }

    // A hostname already routed by another tenant's app is rejected (409).
    let other = workdir.join("tenants/other/apps/web");
    tokio::fs::create_dir_all(&other).await.unwrap();
    tokio::fs::write(
        other.join("manifests.yaml"),
        "apiVersion: gateway.networking.k8s.io/v1\nkind: HTTPRoute\nmetadata:\n  name: web\nspec:\n  hostnames:\n    - auth-acme.keel.local\n",
    )
    .await
    .unwrap();
    git.add(workdir, ["tenants/other/apps/web/manifests.yaml"])
        .await
        .unwrap();
    git.commit(
        workdir,
        "feat(other): web",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    // Proposals validate against the committed `origin/main` they are built on.
    git.push(workdir, "origin", "main").await.unwrap();
    let head_before = git.rev_parse(workdir, "HEAD").await.unwrap();
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy",
                "values": rauthy_values("auth-acme.keel.local") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("tenants/other/apps/web"), "{body}");
    assert_eq!(git.rev_parse(workdir, "HEAD").await.unwrap(), head_before);
}

/// Writes `files` (repo-relative) into the fixture repository and commits them to `main`.
async fn commit_to_main(fixture: &TestFixture, files: &[(&str, &str)], message: &str) {
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    for (path, contents) in files {
        let abs = workdir.join(path);
        tokio::fs::create_dir_all(abs.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&abs, contents).await.unwrap();
    }
    git.add(workdir, files.iter().map(|(p, _)| *p))
        .await
        .unwrap();
    git.commit(
        workdir,
        message,
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();
}

/// Tenant PKO Stacks clone the tenant's own platform-written Git source through PKO `gitSource`
/// (`projectRepo`/`branch`/`repoDir`/`gitAuth` on the tenant's read-only `git-credentials`),
/// never a Flux artifact downloaded from `source-controller`.
#[tokio::test]
async fn test_tenant_infra_stack_uses_pko_git_source_of_the_tenant() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "proxmox-vm", "kind": "infra",
                "name": "gitsrc-vm", "values": { "cores": 2 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let raw = std::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/acme/infra/gitsrc-vm/stack.yaml"),
    )
    .unwrap();
    let stack: Value = noyalib::from_str(&raw).unwrap();
    assert_eq!(stack["metadata"]["namespace"], "tenant-acme");
    assert!(stack["spec"].get("fluxSource").is_none(), "{raw}");
    assert_eq!(stack["spec"]["projectRepo"], TENANT_PKO_GIT_URL);
    assert_eq!(stack["spec"]["branch"], "main");
    assert!(stack["spec"].get("commit").is_none());
    assert_eq!(stack["spec"]["repoDir"], "tenants/acme/infra/gitsrc-vm");
    assert_eq!(
        stack["spec"]["gitAuth"],
        json!({ "basicAuth": {
            "userName": { "type": "Secret", "secret": { "name": "git-credentials", "key": "username" } },
            "password": { "type": "Secret", "secret": { "name": "git-credentials", "key": "password" } }
        }})
    );
}

/// A tenant without a platform-written Git source (e.g. created through `POST /api/v1/tenants`)
/// cannot get a Pulumi Stack: `409` with an actionable message, no branch, commit or PR, and no
/// fallback to the platform repository. Flux-only changes for the same tenant still work.
#[tokio::test]
async fn test_stack_proposal_fails_closed_without_tenant_git_source() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    remove_tenant_git_source_stub(&fixture, "acme").await;
    let before = fixture
        .state
        .forge
        .list_pull_requests(None, None)
        .await
        .unwrap()
        .len();

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "proxmox-vm", "kind": "infra",
                "name": "nosrc-vm", "values": { "cores": 2 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let err: Value = serde_json::from_str(&body).unwrap();
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("tenants/acme/git-repository.yaml"), "{msg}");
    assert!(msg.contains("tenant-acme/keel-gitops"), "{msg}");
    assert!(
        msg.contains("no fallback to the platform repository"),
        "{msg}"
    );
    assert!(!msg.contains("flux-system/flux-system"), "{msg}");
    assert_eq!(
        fixture
            .state
            .forge
            .list_pull_requests(None, None)
            .await
            .unwrap()
            .len(),
        before
    );
    assert!(
        !fixture
            .state
            .repo_workdir
            .join("tenants/acme/infra/nosrc-vm")
            .exists()
    );

    // Helm apps (Flux only, no PKO Stack) are unaffected.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "grafana", "name": "dash",
                "values": { "replicas": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// PKO cannot clone the in-cluster `http://` Forgejo Service Flux uses: without the platform's
/// Forgejo HTTPS origin (`forge.publicUrl`) a Pulumi Stack proposal fails closed with `409` and an
/// actionable message before any branch or PR exists, and never commits an `http://` projectRepo.
#[tokio::test]
async fn test_stack_proposal_fails_closed_without_forge_public_url() {
    let mut fixture = setup_fixture().await;
    fixture.state.forge_public_url = None;
    let cookie = login_and_get_cookie(&fixture).await;
    let before = fixture
        .state
        .forge
        .list_pull_requests(None, None)
        .await
        .unwrap()
        .len();
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "proxmox-vm", "kind": "infra",
                "name": "http-vm", "values": { "cores": 2 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let err: Value = serde_json::from_str(&body).unwrap();
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("forge.publicUrl"), "{msg}");
    assert!(msg.contains("tenant-acme/keel-gitops"), "{msg}");
    assert_eq!(
        fixture
            .state
            .forge
            .list_pull_requests(None, None)
            .await
            .unwrap()
            .len(),
        before
    );
    assert!(
        !fixture
            .state
            .repo_workdir
            .join("tenants/acme/infra/http-vm")
            .exists()
    );
}

/// `keel serve --repo-dir` paths may contain spaces: the control-plane remote (a local bare repo)
/// is never used as a preview clone URL, so Helm and Pulumi proposals still open (201) and the
/// returned preview Stack never exposes the server's filesystem path.
#[tokio::test]
async fn test_proposals_work_when_the_local_repo_path_has_spaces() {
    let fixture = setup_fixture_at("My Repos/remote gitops.git", "work dir").await;
    SecretStateStore::ensure_pulumi_passphrase(fixture.secret_store.as_ref(), "tenant-acme")
        .await
        .unwrap();
    let cookie = login_and_get_cookie(&fixture).await;
    assert!(fixture.state.remote_url.contains(' '));

    for body in [
        json!({ "tenant": "acme", "catalogId": "grafana", "name": "dash",
                "values": { "replicas": 1 } }),
        json!({ "tenant": "acme", "catalogId": "proxmox-vm", "kind": "infra",
                "name": "space-vm", "values": { "cores": 2 } }),
    ] {
        let (status, raw) = post_proposal(&fixture, &cookie, body).await;
        assert_eq!(status, StatusCode::CREATED, "{raw}");
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let preview = parsed["previewStack"].to_string();
        assert!(
            !preview.contains("My Repos"),
            "server path in the preview Stack: {preview}"
        );
        // Synthesized-only preview (no live cluster): no clone URL at all.
        assert!(
            parsed["previewStack"]["spec"].get("projectRepo").is_none(),
            "{raw}"
        );
        assert!(
            parsed["previewStack"]["spec"].get("fluxSource").is_none(),
            "{raw}"
        );
        assert_eq!(parsed["report"]["run"]["phase"], "notExecuted", "{raw}");
    }
}

/// Deletes the platform-written `tenants/<tenant>/git-repository.yaml` stub on `main`
/// (tenants created through `POST /api/v1/tenants` have none yet).
async fn remove_tenant_git_source_stub(fixture: &TestFixture, tenant: &str) {
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    let rel = format!("tenants/{tenant}/git-repository.yaml");
    tokio::fs::remove_file(workdir.join(&rel)).await.unwrap();
    git.add(workdir, [rel.as_str()]).await.unwrap();
    git.commit(
        workdir,
        &format!("chore: drop {tenant} Git source"),
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();
}

/// Returns `true` (and asserts it is allowed) when the host cannot SOPS-encrypt.
fn sops_unavailable(status: StatusCode, body: &str) -> bool {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        assert!(
            std::env::var_os("KEEL_REQUIRE_SOPS").is_none(),
            "KEEL_REQUIRE_SOPS is set but the proposal could not encrypt: {body}"
        );
        return true;
    }
    false
}

/// Checks out the proposal branch from a `POST /api/v1/proposals` response body.
async fn checkout_proposal_branch(fixture: &TestFixture, body: &str) {
    let body: Value = serde_json::from_str(body).unwrap();
    fixture
        .state
        .git_runner
        .checkout_branch(
            &fixture.state.repo_workdir,
            body["branch"].as_str().unwrap(),
        )
        .await
        .unwrap();
}

fn tenants_kustomization_resources(fixture: &TestFixture) -> Vec<String> {
    let raw = std::fs::read_to_string(
        fixture
            .state
            .repo_workdir
            .join("tenants/kustomization.yaml"),
    )
    .unwrap();
    noyalib::from_str::<Value>(&raw).unwrap()["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect()
}

/// Secret substitution happens on the management cluster (`<instance>-config`), so manifest apps
/// with secret fields can target a remote workload cluster; the Secret itself never ships there.
#[tokio::test]
async fn test_manifest_app_with_secrets_targets_remote_cluster() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    let workdir = fixture.state.repo_workdir.clone();

    for (name, target_ns, expected_ns) in [
        ("rauthy-edge", Some("auth"), "auth"),
        ("rauthy-edge-default", None, "tenant-acme"),
    ] {
        let mut req = json!({ "tenant": "acme", "catalogId": "rauthy", "name": name,
                              "targetCluster": "edge-kubeconfig",
                              "values": rauthy_values(&format!("{name}.acme.example")) });
        if let Some(ns) = target_ns {
            req["targetNamespace"] = json!(ns);
        }
        let (status, body) = post_proposal(&fixture, &cookie, req).await;
        if sops_unavailable(status, &body) {
            return;
        }
        assert_eq!(status, StatusCode::CREATED, "{body}");
        checkout_proposal_branch(&fixture, &body).await;

        let dir = workdir.join(format!("tenants/acme/apps/{name}"));
        assert_rauthy_split_layout(&dir, name, Some(("edge-kubeconfig", expected_ns)));
        let resources = tenants_kustomization_resources(&fixture);
        for file in ["config-kustomization.yaml", "flux-kustomization.yaml"] {
            assert!(
                resources.contains(&format!("acme/apps/{name}/{file}")),
                "{resources:?}"
            );
        }
        assert!(!resources.iter().any(|r| r.ends_with("secrets.enc.yaml")));
        fixture
            .state
            .git_runner
            .checkout_branch(&workdir, "main")
            .await
            .unwrap();
    }
}

/// Deleting a manifest app removes its directory and both Kustomizations from the tenant tree.
#[tokio::test]
async fn test_deleting_manifest_app_removes_config_and_app_kustomizations() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    let workdir = fixture.state.repo_workdir.clone();

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy",
                "values": rauthy_values("auth.acme.example") }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pr_id = serde_json::from_str::<Value>(&body).unwrap()["proposal"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr_id}/merge"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    fixture
        .state
        .git_runner
        .sync_branch_from_remote(&workdir, "origin", "main")
        .await
        .unwrap();
    assert!(
        tenants_kustomization_resources(&fixture)
            .contains(&"acme/apps/rauthy/config-kustomization.yaml".to_string())
    );

    let res = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/v1/releases/tenant-acme/rauthy")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body =
        String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
    assert_eq!(status, StatusCode::OK, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    assert!(!workdir.join("tenants/acme/apps/rauthy").exists());
    let resources = tenants_kustomization_resources(&fixture);
    assert!(
        !resources.iter().any(|r| r.starts_with("acme/apps/rauthy/")),
        "{resources:?}"
    );
}

/// A canary bundle item whose HelmRelease targets a remote cluster ships its Flagger Canary and
/// NetworkPolicy through an `<item>-canary` Kustomization to that cluster, never the local tree.
#[tokio::test]
async fn test_bundle_remote_canary_item_applies_canary_on_workload_cluster() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    commit_to_main(
        &fixture,
        &[(
            "catalog/bundles/edge-canary/catalog.yaml",
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: edge-canary
  name: Edge Canary
  category: platform
spec:
  items:
    - id: web
      kind: app
      catalogRef: grafana
      canary: true
      targetNamespace: obs
  schema:
    type: object
"#,
        )],
        "feat(catalog): edge-canary bundle",
    )
    .await;

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "edge-canary", "kind": "bundle",
                "targetCluster": "edge-kubeconfig", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;

    let dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/bundles/edge-canary/web");
    let yaml = |file: &str| {
        noyalib::from_str::<Value>(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap()
    };
    assert!(!dir.join("canary.yaml").exists());
    assert!(!dir.join("networkpolicy-canary.yaml").exists());
    assert_eq!(
        yaml("kustomization.yaml")["resources"],
        json!([
            "helmrelease-canary.yaml",
            "helm-repository.yaml",
            "canary-kustomization.yaml"
        ])
    );
    assert_eq!(
        yaml("canary/kustomization.yaml")["resources"],
        json!(["namespace.yaml", "canary.yaml", "networkpolicy-canary.yaml"])
    );
    assert_eq!(yaml("canary/namespace.yaml")["metadata"]["name"], "obs");
    assert_restricted_pss(&yaml("canary/namespace.yaml"));
    assert_eq!(yaml("canary/canary.yaml")["kind"], "Canary");
    let fk = yaml("canary-kustomization.yaml");
    assert_eq!(fk["metadata"]["name"], "edge-canary-web-canary");
    assert_eq!(fk["metadata"]["labels"]["keel.io/bundle"], "edge-canary");
    assert_eq!(
        fk["spec"]["kubeConfig"]["secretRef"]["name"],
        "edge-kubeconfig"
    );
    assert_eq!(fk["spec"]["targetNamespace"], "obs");
    assert_eq!(
        fk["spec"]["path"],
        "./tenants/acme/bundles/edge-canary/web/canary"
    );
    let hr = yaml("helmrelease-canary.yaml");
    assert_eq!(
        hr["spec"]["kubeConfig"]["secretRef"]["name"],
        "edge-kubeconfig"
    );
}

/// Manifest apps inside bundles use the same split layout (config Kustomization first).
#[tokio::test]
async fn test_bundle_manifest_app_item_uses_split_config_layout() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    commit_to_main(
        &fixture,
        &[(
            "catalog/bundles/idp-stack/catalog.yaml",
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: idp-stack
  name: IdP Stack
  category: security-identity
spec:
  items:
    - id: web
      kind: app
      catalogRef: grafana
    - id: idp
      kind: app
      catalogRef: rauthy
      dependsOn:
        - web
  schema:
    type: object
"#,
        )],
        "feat(catalog): idp-stack bundle",
    )
    .await;

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "idp-stack", "kind": "bundle",
                "values": { "idp": rauthy_values("idp.acme.example") } }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert!(
        parsed["report"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "idp-stack-idp"
                && r["resourceType"] == "flux:kustomize.toolkit.fluxcd.io/v1:Kustomization"),
        "{body}"
    );
    checkout_proposal_branch(&fixture, &body).await;

    let dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/bundles/idp-stack/idp");
    assert_rauthy_split_layout(&dir, "idp-stack-idp", None);
    assert!(!dir.join("helmrelease.yaml").exists());
    let committed = read_tree(&dir);
    for secret in ["raftSecretSampleValue-0123456789", "Sample-Admin#Passw0rd!"] {
        assert!(!committed.contains(secret));
    }
    let fk: Value =
        noyalib::from_str(&std::fs::read_to_string(dir.join("flux-kustomization.yaml")).unwrap())
            .unwrap();
    assert_eq!(fk["metadata"]["labels"]["keel.io/bundle"], "idp-stack");
    // Ready (and so any later item's `dependsOn`) only once the IdP workloads are healthy.
    assert_eq!(fk["spec"]["wait"], true);
    let deps: Vec<&str> = fk["spec"]["dependsOn"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(deps, ["idp-stack-web", "idp-stack-idp-config"]);
    let resources = tenants_kustomization_resources(&fixture);
    for file in ["config-kustomization.yaml", "flux-kustomization.yaml"] {
        assert!(
            resources.contains(&format!("acme/bundles/idp-stack/idp/{file}")),
            "{resources:?}"
        );
    }

    // Editing the merged bundle item through the single-app path keeps the split layout and the
    // bundle ordering (never self-referencing its own Kustomization).
    let workdir = fixture.state.repo_workdir.clone();
    fixture
        .state
        .git_runner
        .checkout_branch(&workdir, "main")
        .await
        .unwrap();
    let pr_id = parsed["proposal"]["id"].as_str().unwrap();
    let res = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr_id}/merge"
                ))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    fixture
        .state
        .git_runner
        .sync_branch_from_remote(&workdir, "origin", "main")
        .await
        .unwrap();
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "kind": "app", "name": "idp-stack-idp",
                "values": rauthy_values("idp.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    assert!(!workdir.join("tenants/acme/apps/idp-stack-idp").exists());
    assert_rauthy_split_layout(&dir, "idp-stack-idp", None);
    let fk: Value =
        noyalib::from_str(&std::fs::read_to_string(dir.join("flux-kustomization.yaml")).unwrap())
            .unwrap();
    let deps: Vec<&str> = fk["spec"]["dependsOn"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(deps, ["idp-stack-web", "idp-stack-idp-config"]);
    assert_eq!(fk["metadata"]["labels"]["keel.io/bundle-item"], "idp");
    let resources = tenants_kustomization_resources(&fixture);
    assert!(
        !resources
            .iter()
            .any(|r| r.starts_with("acme/apps/idp-stack-idp")),
        "{resources:?}"
    );
    for file in ["config-kustomization.yaml", "flux-kustomization.yaml"] {
        let entry = format!("acme/bundles/idp-stack/idp/{file}");
        assert_eq!(resources.iter().filter(|r| **r == entry).count(), 1);
    }
}

/// Merges the proposal in a `POST /api/v1/proposals` response body and syncs `main`.
async fn merge_proposal_and_sync(fixture: &TestFixture, cookie: &str, body: &str) {
    let workdir = &fixture.state.repo_workdir;
    fixture
        .state
        .git_runner
        .checkout_branch(workdir, "main")
        .await
        .unwrap();
    let pr_id = serde_json::from_str::<Value>(body).unwrap()["proposal"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/v1/tenants/management/proposals/{pr_id}/merge"
                ))
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    fixture
        .state
        .git_runner
        .sync_branch_from_remote(workdir, "origin", "main")
        .await
        .unwrap();
}

/// `rauthy_values` without any secret field (an edit leaving every secret blank).
fn rauthy_values_without_secrets(hostname: &str) -> Value {
    let mut values = rauthy_values(hostname);
    let obj = values.as_object_mut().unwrap();
    for key in [
        "encKey",
        "hiqliteSecretRaft",
        "hiqliteSecretApi",
        "adminPassword",
    ] {
        obj.remove(key);
    }
    values
}

/// `stringData` keys of a SOPS-encrypted Secret (key names stay plaintext).
fn sops_secret_keys(ciphertext: &str) -> Vec<String> {
    let doc: Value = noyalib::from_str(ciphertext).unwrap();
    doc["stringData"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// Editing a manifest app without re-entering its secrets (the UI never receives them) keeps the
/// committed SOPS Secret and its `substituteFrom` source; re-entering only some secrets keeps the
/// others. Required secret fields are only relaxed when a stored value exists.
#[tokio::test]
async fn test_manifest_app_edit_keeps_stored_secrets() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    let workdir = fixture.state.repo_workdir.clone();
    let secret_path = workdir.join("tenants/acme/apps/rauthy/config/secrets.enc.yaml");

    // A new instance still needs every required secret.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy",
                "values": rauthy_values_without_secrets("auth.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy",
                "values": rauthy_values("auth.acme.example") }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    merge_proposal_and_sync(&fixture, &cookie, &body).await;
    let stored = std::fs::read_to_string(&secret_path).unwrap();

    // Only a non-secret field changes: the ciphertext is committed unchanged.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy",
                "values": rauthy_values_without_secrets("login.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    assert_eq!(std::fs::read_to_string(&secret_path).unwrap(), stored);
    let dir = workdir.join("tenants/acme/apps/rauthy");
    assert_rauthy_split_layout(&dir, "rauthy", None);
    assert!(
        std::fs::read_to_string(dir.join("manifests.yaml"))
            .unwrap()
            .contains("login.acme.example")
    );

    // One secret re-entered: re-encrypted with every stored key kept.
    fixture
        .state
        .git_runner
        .checkout_branch(&workdir, "main")
        .await
        .unwrap();
    let mut values = rauthy_values_without_secrets("auth.acme.example");
    values["adminPassword"] = json!("Rotated-Admin#Passw0rd!");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "values": values }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let rotated = std::fs::read_to_string(&secret_path).unwrap();
    assert_ne!(rotated, stored);
    assert!(!rotated.contains("Rotated-Admin#Passw0rd!"));
    assert_eq!(sops_secret_keys(&rotated), sops_secret_keys(&stored));
    let age_key = fixture
        .state
        .secret_store
        .get_tenant_age_key("acme")
        .await
        .unwrap()
        .unwrap();
    let plain = keel_core::SopsManager::new()
        .decrypt_yaml(&age_key.secret_key, &rotated)
        .await
        .unwrap();
    let doc: Value = noyalib::from_str(&plain).unwrap();
    assert_eq!(
        doc["stringData"]["adminPassword"],
        "Rotated-Admin#Passw0rd!"
    );
    assert_eq!(
        doc["stringData"]["hiqliteSecretRaft"],
        "raftSecretSampleValue-0123456789"
    );
    assert_eq!(doc["metadata"]["name"], "rauthy-secrets");
}

/// Bundle edits keep a manifest-app item's stored secrets the same way.
#[tokio::test]
async fn test_bundle_manifest_app_item_edit_keeps_stored_secrets() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    commit_to_main(
        &fixture,
        &[(
            "catalog/bundles/idp-only/catalog.yaml",
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: idp-only
  name: IdP Only
  category: security-identity
spec:
  items:
    - id: idp
      kind: app
      catalogRef: rauthy
  schema:
    type: object
    properties:
      idp:
        type: object
        required: [hostname, encKey, hiqliteSecretRaft, hiqliteSecretApi, adminPassword]
        properties:
          hostname:
            type: string
          publicPortSuffix:
            type: string
          adminEmail:
            type: string
          encKeyId:
            type: string
          encKey:
            type: string
            writeOnly: true
          hiqliteSecretRaft:
            type: string
            writeOnly: true
          hiqliteSecretApi:
            type: string
            writeOnly: true
          adminPassword:
            type: string
            writeOnly: true
"#,
        )],
        "feat(catalog): idp-only bundle",
    )
    .await;
    let workdir = fixture.state.repo_workdir.clone();
    let secret_path = workdir.join("tenants/acme/bundles/idp-only/idp/config/secrets.enc.yaml");

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "idp-only", "kind": "bundle",
                "values": { "idp": rauthy_values("idp.acme.example") } }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    merge_proposal_and_sync(&fixture, &cookie, &body).await;
    let stored = std::fs::read_to_string(&secret_path).unwrap();

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "idp-only", "kind": "bundle",
                "values": { "idp": rauthy_values_without_secrets("login.acme.example") } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    assert_eq!(std::fs::read_to_string(&secret_path).unwrap(), stored);
    let fk: Value = noyalib::from_str(
        &std::fs::read_to_string(
            workdir.join("tenants/acme/bundles/idp-only/idp/flux-kustomization.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        fk["spec"]["postBuild"]["substituteFrom"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "Secret" && s["name"] == "idp-only-idp-secrets")
    );
}

/// Flux Kustomizations derived from instance names (`<name>`, `<name>-config`, `<name>-canary`,
/// bundle `<bundle>-<item>`) must stay unique per tenant: one tenant's duplicate id would stop
/// the shared root `tenants/` Kustomization for every tenant.
#[tokio::test]
async fn test_derived_flux_kustomization_name_collisions_are_refused() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_rauthy_catalog(&fixture).await;
    let flux_kustomization = |name: &str| {
        format!(
            "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: {name}\n  namespace: tenant-acme\nspec:\n  path: ./x\n"
        )
    };
    let web = flux_kustomization("web");
    let web_config_of_web = flux_kustomization("web-config");
    let api_config = flux_kustomization("api-config");
    let edge_web = flux_kustomization("edge-web");
    let stack_db = flux_kustomization("stack-db");
    let stack_db_config = flux_kustomization("stack-db-config");
    commit_to_main(
        &fixture,
        &[
            // Manifest app `web` (app + `-config` Kustomizations).
            (
                "tenants/acme/apps/web/flux-kustomization.yaml",
                web.as_str(),
            ),
            (
                "tenants/acme/apps/web/config-kustomization.yaml",
                web_config_of_web.as_str(),
            ),
            // A manifest app named `api-config`.
            (
                "tenants/acme/apps/api-config/flux-kustomization.yaml",
                api_config.as_str(),
            ),
            // A manifest app named like a bundle item resource.
            (
                "tenants/acme/apps/edge-web/flux-kustomization.yaml",
                edge_web.as_str(),
            ),
            // Manifest-app item `db` of bundle `stack`.
            (
                "tenants/acme/bundles/stack/db/flux-kustomization.yaml",
                stack_db.as_str(),
            ),
            (
                "tenants/acme/bundles/stack/db/config-kustomization.yaml",
                stack_db_config.as_str(),
            ),
            (
                "catalog/bundles/edge-canary/catalog.yaml",
                r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: edge-canary
  name: Edge Canary
  category: platform
spec:
  items:
    - id: web
      kind: app
      catalogRef: grafana
  schema:
    type: object
"#,
            ),
        ],
        "feat(acme): existing instances",
    )
    .await;
    let git = &fixture.state.git_runner;
    let head_before = git
        .rev_parse(&fixture.state.repo_workdir, "HEAD")
        .await
        .unwrap();

    // `web` exists: a new `web-config` is refused (would duplicate `web-config`).
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "name": "web-config",
                "values": rauthy_values("w.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("tenants/acme/apps/web"), "{body}");

    // `api-config` exists: a new `api` (deriving `api-config`) is refused.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "name": "api",
                "values": rauthy_values("a.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("api-config"), "{body}");

    // A bundle item resource name may not reuse an app's Kustomization name, and vice versa.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "edge-canary", "kind": "bundle",
                "name": "edge", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("edge-web"), "{body}");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "name": "stack-db",
                "values": rauthy_values("s.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("tenants/acme/bundles/stack/db"), "{body}");

    assert_eq!(
        git.rev_parse(&fixture.state.repo_workdir, "HEAD")
            .await
            .unwrap(),
        head_before
    );
}

/// Manifest apps have no Canary/bindValues wiring: bundle items requesting them are refused.
#[tokio::test]
async fn test_bundle_manifest_app_item_rejects_canary_and_bind_values() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let catalog = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/catalog/apps/http-echo-manifest/catalog.yaml"),
    )
    .unwrap()
    .replace("id: http-echo-manifest", "id: echo-canary")
    .replace("flaggerSupported: false", "flaggerSupported: true");
    let manifests = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/catalog/apps/http-echo-manifest/manifests.yaml"),
    )
    .unwrap();
    let bundle = |id: &str, extra: &str| {
        format!(
            "apiVersion: keel.io/v1alpha1\nkind: CatalogBundle\nmetadata:\n  id: {id}\n  name: {id}\n  category: platform\nspec:\n  items:\n    - id: echo\n      kind: app\n      catalogRef: echo-canary\n{extra}  schema:\n    type: object\n"
        )
    };
    let canary_bundle = bundle("echo-canary-b", "      canary: true\n");
    let bind_bundle = bundle(
        "echo-bind-b",
        "      bindValues:\n        echoText: \"${items.db.outputs.host}\"\n",
    );
    commit_to_main(
        &fixture,
        &[
            ("catalog/apps/echo-canary/catalog.yaml", catalog.as_str()),
            (
                "catalog/apps/echo-canary/manifests.yaml",
                manifests.as_str(),
            ),
            (
                "catalog/bundles/echo-canary-b/catalog.yaml",
                canary_bundle.as_str(),
            ),
            (
                "catalog/bundles/echo-bind-b/catalog.yaml",
                bind_bundle.as_str(),
            ),
        ],
        "feat(catalog): echo bundles",
    )
    .await;

    for (bundle_id, needle) in [("echo-canary-b", "canary"), ("echo-bind-b", "bindValues")] {
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": bundle_id, "kind": "bundle",
                    "values": { "echo": { "echoText": "hi", "replicas": 1 } } }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bundle_id}: {body}");
        assert!(body.contains(needle), "{body}");
        assert!(body.contains("Helm apps"), "{body}");
    }
}

/// `spec.placement.scope: cluster` apps (cluster-scoped CRDs/ClusterRoles/webhooks) can only be
/// proposed onto a remote workload cluster, both directly and as bundle items (`SDD.md` §5.1).
#[tokio::test]
async fn test_cluster_scoped_app_placement_requires_remote_target_cluster() {
    let fixture = setup_fixture().await;
    let workdir = fixture.state.repo_workdir.clone();
    tokio::fs::create_dir_all(workdir.join("catalog/apps/cert-manager"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/apps/cert-manager/catalog.yaml"),
        include_str!("../../../deploy/catalog/apps/cert-manager/catalog.yaml"),
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(workdir.join("catalog/bundles/tls-stack"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/bundles/tls-stack/catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: tls-stack
  name: TLS Stack
  category: platform
spec:
  items:
    - id: certs
      kind: app
      catalogRef: cert-manager
  schema:
    type: object
    properties: {}
"#,
    )
    .await
    .unwrap();
    let git = &fixture.state.git_runner;
    git.add(
        &workdir,
        [
            "catalog/apps/cert-manager/catalog.yaml",
            "catalog/bundles/tls-stack/catalog.yaml",
        ],
    )
    .await
    .unwrap();
    git.commit(
        &workdir,
        "feat: add cert-manager app and tls bundle",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&workdir, "origin", "main").await.unwrap();

    let cookie = login_and_get_cookie(&fixture).await;
    let post = |body: Value| {
        let app = build_router(fixture.state.clone());
        let cookie = cookie.clone();
        async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(proposals_uri(&body))
                        .header(COOKIE, &cookie)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status();
            let body =
                String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
                    .unwrap();
            (status, body)
        }
    };

    // Management cluster (absent, blank, or `local` targetCluster) is rejected.
    for target in [None, Some(""), Some("local")] {
        let mut req = json!({
            "tenant": "acme",
            "catalogId": "cert-manager",
            "values": { "replicaCount": 1 }
        });
        if let Some(t) = target {
            req["targetCluster"] = json!(t);
        }
        let (status, body) = post(req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{target:?}: {body}");
        assert!(body.contains("spec.placement.scope: cluster"), "{body}");
        assert!(body.contains("remote workload cluster"), "{body}");
    }
    // Nothing was written for the rejected proposals.
    assert!(!workdir.join("tenants/acme/apps/cert-manager").exists());

    // Bundle items without targetClusterFrom fall back to the request targetCluster.
    let (status, body) = post(json!({
        "tenant": "acme",
        "catalogId": "tls-stack",
        "kind": "bundle",
        "values": {}
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Bundle item 'certs'"), "{body}");
    assert!(body.contains("spec.placement.scope: cluster"), "{body}");

    // A remote workload cluster is accepted and wires the remote kubeconfig.
    let (status, body) = post(json!({
        "tenant": "acme",
        "catalogId": "cert-manager",
        "targetCluster": "workload-kubeconfig",
        "targetNamespace": "cert-manager",
        "values": { "replicaCount": 1 }
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    let branch = created["branch"].as_str().unwrap();
    git.checkout_branch(&workdir, branch).await.unwrap();
    let hr_yaml =
        tokio::fs::read_to_string(workdir.join("tenants/acme/apps/cert-manager/helmrelease.yaml"))
            .await
            .unwrap();
    assert!(hr_yaml.contains("workload-kubeconfig"), "{hr_yaml}");
    assert!(
        hr_yaml.contains("targetNamespace: cert-manager"),
        "{hr_yaml}"
    );
    git.checkout_branch(&workdir, "main").await.unwrap();

    let (status, body) = post(json!({
        "tenant": "acme",
        "catalogId": "tls-stack",
        "kind": "bundle",
        "name": "tls",
        "targetCluster": "workload-kubeconfig",
        "values": {}
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Authoring a bundle (composed or custom schema) whose cluster-scoped app item has no
    // targetClusterFrom is rejected: GUI bundle deploys never send a request targetCluster.
    for schema in [None, Some(json!({ "type": "object", "properties": {} }))] {
        let mut req = json!({
            "kind": "bundle",
            "id": "tls-only",
            "name": "TLS Only",
            "category": "platform",
            "items": [{ "id": "certs", "kind": "app", "catalogRef": "cert-manager" }]
        });
        if let Some(schema) = schema {
            req["schema"] = schema;
        }
        let (status, body) =
            send_text(&fixture, &cookie, Method::POST, "/api/v1/catalog", req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("targetClusterFrom"), "{body}");
    }
    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::POST,
        "/api/v1/catalog",
        json!({
            "kind": "bundle",
            "id": "vm-tls",
            "name": "VM + TLS",
            "category": "platform",
            "items": [
                { "id": "vm", "kind": "infra", "catalogRef": "proxmox-vm" },
                {
                    "id": "certs",
                    "kind": "app",
                    "catalogRef": "cert-manager",
                    "dependsOn": ["vm"],
                    "targetClusterFrom": "vm"
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Updating the items of a bundle (Git-seeded tls-stack) without targetClusterFrom is rejected.
    git.checkout_branch(&workdir, "main").await.unwrap();
    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::PUT,
        "/api/v1/catalog/bundle/tls-stack",
        json!({
            "name": "TLS Stack",
            "category": "platform",
            "items": [{ "id": "certs", "kind": "app", "catalogRef": "cert-manager" }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

async fn send_text(
    fixture: &TestFixture,
    cookie: &str,
    method: Method,
    uri: &str,
    body: Value,
) -> (StatusCode, String) {
    let res = build_router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, body)
}

/// `scope: namespaced` manifest apps may not declare well-known cluster-scoped kinds: rejected when
/// authored, and rejected at proposal time when (Git-authored) one targets the management cluster.
#[tokio::test]
async fn test_namespaced_manifest_app_rejects_cluster_scoped_kinds() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let cluster_manifests = "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: ops\n---\napiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata:\n  name: ops-reader\nrules: []\n";
    let register = |id: &str, scope: Option<&str>| {
        let mut req = json!({
            "id": id,
            "name": id,
            "category": "platform",
            "runtime": "manifest",
            "manifestsYaml": cluster_manifests,
        });
        if let Some(scope) = scope {
            req["placementScope"] = json!(scope);
        }
        req
    };
    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::POST,
        "/api/v1/catalog",
        register("ops-ns", None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Namespace, ClusterRole"), "{body}");
    assert!(body.contains("placementScope: cluster"), "{body}");

    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::POST,
        "/api/v1/catalog",
        register("ops-cluster", Some("cluster")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A Git-authored, mislabelled namespaced manifest app is caught after rendering.
    let workdir = fixture.state.repo_workdir.clone();
    let git = &fixture.state.git_runner;
    git.checkout_branch(&workdir, "main").await.unwrap();
    tokio::fs::create_dir_all(workdir.join("catalog/apps/sneaky"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("catalog/apps/sneaky/catalog.yaml"),
        r#"apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: sneaky
  name: Sneaky
  category: platform
  runtime: manifest
spec:
  defaultEngine: flux
  schema:
    type: object
    properties: {}
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        workdir.join("catalog/apps/sneaky/manifests.yaml"),
        cluster_manifests,
    )
    .await
    .unwrap();
    git.add(
        &workdir,
        [
            "catalog/apps/sneaky/catalog.yaml",
            "catalog/apps/sneaky/manifests.yaml",
        ],
    )
    .await
    .unwrap();
    git.commit(
        &workdir,
        "feat: add sneaky manifest app",
        &CommitAuthor::new("Keel Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&workdir, "origin", "main").await.unwrap();

    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::POST,
        "/api/v1/tenants/acme/proposals",
        json!({ "tenant": "acme", "catalogId": "sneaky", "values": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("cluster-scoped kinds"), "{body}");
    assert!(!workdir.join("tenants/acme/apps/sneaky").exists());

    // Editing the mislabelled app is rejected until it declares placementScope: cluster.
    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::PUT,
        "/api/v1/catalog/app/sneaky",
        json!({ "name": "Sneaky", "category": "platform" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("placementScope: cluster"), "{body}");
    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::PUT,
        "/api/v1/catalog/app/sneaky",
        json!({ "name": "Sneaky", "category": "platform", "placementScope": "cluster" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Catalog authoring round-trips `placementScope` into `spec.placement.scope`.
#[tokio::test]
async fn test_catalog_authoring_sets_placement_scope() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let send = |method: Method, uri: &'static str, body: Value| {
        let app = build_router(fixture.state.clone());
        let cookie = cookie.clone();
        async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header(COOKIE, &cookie)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };

    let (status, created) = send(
        Method::POST,
        "/api/v1/catalog",
        json!({
            "id": "crd-operator",
            "name": "CRD Operator",
            "category": "platform",
            "runtime": "manifest",
            "manifestsYaml": "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: crd-operator\ndata:\n  k: v\n",
            "placementScope": "cluster"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["application"]["spec"]["placement"]["scope"],
        "cluster"
    );

    let (status, _) = send(
        Method::POST,
        "/api/v1/catalog",
        json!({
            "id": "bad-scope",
            "name": "Bad Scope",
            "category": "platform",
            "runtime": "manifest",
            "manifestsYaml": "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n",
            "placementScope": "global"
        }),
    )
    .await;
    assert!(status.is_client_error(), "{status}");

    let (status, updated) = send(
        Method::PUT,
        "/api/v1/catalog/app/grafana",
        json!({ "name": "Grafana", "category": "observability", "placementScope": "cluster" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(
        updated["application"]["spec"]["placement"]["scope"],
        "cluster"
    );
}

/// Seeds `catalog/apps/<id>/catalog.yaml` from the built-in `deploy/catalog/apps/<id>`.
async fn seed_builtin_helm_catalog(fixture: &TestFixture, id: &str, raw: &str) {
    let path = format!("catalog/apps/{id}/catalog.yaml");
    commit_to_main(
        fixture,
        &[(&path, raw)],
        &format!("feat(catalog): add {id}"),
    )
    .await;
}

const FORGEJO_APP_CATALOG: &str = include_str!("../../../deploy/catalog/apps/forgejo/catalog.yaml");
const SEAWEEDFS_APP_CATALOG: &str =
    include_str!("../../../deploy/catalog/apps/seaweedfs/catalog.yaml");
const FORGEJO_ADMIN_PASSWORD: &str = "Forgejo-Admin#Passw0rd!";
const FORGEJO_OIDC_SECRET: &str = "oidc-client-secret-0123456789";
const SEAWEEDFS_ACCESS_KEY: &str = "0123456789abcdef0123";
const SEAWEEDFS_SECRET_KEY: &str = "c2VjcmV0c2VjcmV0c2VjcmV0c2VjcmV0c2Vj";

fn forgejo_values(hostname: &str) -> Value {
    json!({
        "gitea": {
            "admin": {
                "username": "forgejo-admin",
                "email": "ops@acme.example",
                "password": FORGEJO_ADMIN_PASSWORD,
                "passwordMode": "keepUpdated"
            },
            "config": {
                "server": { "ROOT_URL": "" },
                "service": { "DISABLE_REGISTRATION": false, "ALLOW_ONLY_EXTERNAL_REGISTRATION": true }
            },
            "oauth": [{
                "name": "rauthy",
                "provider": "openidConnect",
                "key": "forgejo",
                "secret": FORGEJO_OIDC_SECRET,
                "autoDiscoverUrl": "https://auth.acme.example/auth/v1/.well-known/openid-configuration"
            }]
        },
        "persistence": { "enabled": true, "size": "20Gi", "claimName": "forgejo-data", "storageClass": "" },
        "httpRoute": { "enabled": true, "hostnames": [hostname] }
    })
}

fn seaweedfs_values() -> Value {
    json!({
        "s3": {
            "credentials": { "admin": {
                "accessKey": SEAWEEDFS_ACCESS_KEY,
                "secretKey": SEAWEEDFS_SECRET_KEY
            } },
            "createBuckets": [{ "name": "artifacts" }],
            "httpRoute": { "enabled": false }
        },
        "master": { "data": { "size": "1Gi" } },
        "filer": { "data": { "size": "2Gi" } },
        "volume": {
            "idx": { "size": "2Gi" },
            "dataDirs": [{ "name": "data1", "type": "persistentVolumeClaim", "size": "15Gi", "maxVolumes": 0 }]
        }
    })
}

fn read_yaml(path: &std::path::Path) -> Value {
    noyalib::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn decrypt_tenant_secret(fixture: &TestFixture, ciphertext: &str) -> Value {
    let age_key = fixture
        .state
        .secret_store
        .get_tenant_age_key("acme")
        .await
        .unwrap()
        .unwrap();
    let plain = keel_core::SopsManager::new()
        .decrypt_yaml(&age_key.secret_key, ciphertext)
        .await
        .unwrap();
    noyalib::from_str(&plain).unwrap()
}

/// Render test for the self-service Forgejo Helm app: the upstream chart from its OCI
/// HelmRepository at the catalog/core version, the locked restricted-PSS values, the route pinned
/// to keel-ingress, the NetworkPolicy, and secret fields only as SOPS ciphertext merged through
/// `valuesFrom`. An Edit leaving the secrets blank keeps the stored ciphertext.
#[tokio::test]
async fn test_forgejo_catalog_app_renders_helm_release_with_encrypted_secrets() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_builtin_helm_catalog(&fixture, "forgejo", FORGEJO_APP_CATALOG).await;
    let workdir = fixture.state.repo_workdir.clone();
    let dir = workdir.join("tenants/acme/apps/git");

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo", "name": "git",
                "values": forgejo_values("git.acme.example") }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;

    let committed = read_tree(&dir);
    for secret in [FORGEJO_ADMIN_PASSWORD, FORGEJO_OIDC_SECRET] {
        assert!(!committed.contains(secret), "secret committed in plaintext");
    }
    // No tenant-namespace `<name>-config` Kustomization (only `tenant-default` has a tenant
    // GitRepository): the root tenants Kustomization applies the Secret.
    assert!(!dir.join("config").exists());
    assert!(!dir.join("config-kustomization.yaml").exists());

    let repo = read_yaml(&dir.join("helm-repository.yaml"));
    assert_eq!(repo["spec"]["type"], "oci");
    assert_eq!(repo["spec"]["url"], "oci://code.forgejo.org/forgejo-helm");

    let hr = read_yaml(&dir.join("helmrelease.yaml"));
    assert_eq!(hr["metadata"]["name"], "git");
    assert_eq!(hr["spec"]["chart"]["spec"]["chart"], "forgejo");
    assert_eq!(hr["spec"]["chart"]["spec"]["version"], "17.3.3");
    assert_eq!(
        hr["metadata"]["annotations"]["keel.io/route-hostnames"],
        "git.acme.example"
    );
    let values = &hr["spec"]["values"];
    // Locked known-good tenant config (restricted PSS) is always applied.
    assert_eq!(values["image"]["rootless"], true);
    assert_eq!(values["replicaCount"], 1);
    assert_eq!(values["test"]["enabled"], false);
    assert_eq!(values["podSecurityContext"]["runAsNonRoot"], true);
    let csc = &values["containerSecurityContext"];
    assert_eq!(csc["allowPrivilegeEscalation"], false);
    assert_eq!(csc["capabilities"]["drop"], json!(["ALL"]));
    assert_eq!(csc["seccompProfile"]["type"], "RuntimeDefault");
    assert_eq!(values["gitea"]["config"]["database"]["DB_TYPE"], "sqlite3");
    assert_eq!(values["gitea"]["admin"]["username"], "forgejo-admin");
    assert!(values["gitea"]["admin"].get("password").is_none());
    assert!(values["gitea"].get("oauth").is_none());
    assert_eq!(values["persistence"]["size"], "20Gi");
    assert_eq!(
        values["httpRoute"]["hostnames"],
        json!(["git.acme.example"])
    );
    assert_eq!(
        values["httpRoute"]["parentRefs"],
        keel_core::keel_ingress_parent_refs()
    );
    assert_eq!(
        hr["spec"]["valuesFrom"],
        json!([
            { "kind": "Secret", "name": "git-secrets", "valuesKey": "gitea.admin.password" },
            { "kind": "Secret", "name": "git-secrets", "valuesKey": "gitea.oauth" }
        ])
    );
    // values.yaml keeps only the submitted (non-secret) form values.
    let form_values = read_yaml(&dir.join("values.yaml"));
    assert!(form_values.get("podSecurityContext").is_none());
    assert!(form_values["httpRoute"].get("parentRefs").is_none());

    let stored = std::fs::read_to_string(dir.join("secrets.enc.yaml")).unwrap();
    assert_eq!(
        sops_secret_keys(&stored),
        vec!["gitea.admin.password", "gitea.oauth"]
    );
    let secret = decrypt_tenant_secret(&fixture, &stored).await;
    assert_eq!(secret["metadata"]["name"], "git-secrets");
    assert_eq!(secret["metadata"]["namespace"], "tenant-acme");
    // helm-controller reconciles as soon as the Secret exists (no interval wait).
    assert_eq!(
        secret["metadata"]["labels"]["reconcile.fluxcd.io/watch"],
        "Enabled"
    );
    let admin: Value = noyalib::from_str(
        secret["stringData"]["gitea.admin.password"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(admin["gitea"]["admin"]["password"], FORGEJO_ADMIN_PASSWORD);
    let oauth: Value =
        noyalib::from_str(secret["stringData"]["gitea.oauth"].as_str().unwrap()).unwrap();
    assert_eq!(oauth["gitea"]["oauth"][0]["secret"], FORGEJO_OIDC_SECRET);

    let np = read_yaml(&dir.join("networkpolicy.yaml"));
    assert_eq!(np["metadata"]["name"], "allow-git-app");
    assert_eq!(
        np["spec"]["podSelector"]["matchLabels"]["app.kubernetes.io/instance"],
        "git"
    );
    assert_eq!(np["spec"]["ingress"][0]["ports"][0]["port"], 3000);
    // Egress to keel-system reaches only the keel-ingress Envoy proxies, on their listener ports.
    let gateway_egress = &np["spec"]["egress"][1];
    assert_eq!(
        gateway_egress["to"][0]["podSelector"]["matchLabels"]["gateway.envoyproxy.io/owning-gateway-name"],
        "keel-ingress"
    );
    assert_eq!(
        gateway_egress["ports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["port"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![10080, 10443, 8888]
    );
    assert_eq!(np["spec"]["egress"][2]["ports"][1]["port"], 443);
    // Only the guarded route may expose the release; no SSRF into private networks.
    assert_eq!(values["extraDeploy"], json!([]));
    assert_eq!(values["ingress"]["enabled"], false);
    assert_eq!(values["tcpRoute"]["enabled"], false);
    assert_eq!(
        values["gitea"]["config"]["webhook"]["ALLOWED_HOST_LIST"],
        "external"
    );
    assert_eq!(
        values["gitea"]["config"]["migrations"]["ALLOW_LOCALNETWORKS"],
        false
    );

    let resources = tenants_kustomization_resources(&fixture);
    for file in [
        "helmrelease.yaml",
        "helm-repository.yaml",
        "networkpolicy.yaml",
        "secrets.enc.yaml",
    ] {
        let entry = format!("acme/apps/git/{file}");
        assert!(resources.contains(&entry), "{entry} missing: {resources:?}");
    }

    // Edit without re-entering the secrets (the UI never receives them): the nested required
    // password is relaxed because it is stored, and the ciphertext is kept verbatim.
    merge_proposal_and_sync(&fixture, &cookie, &body).await;
    let mut edit = forgejo_values("git.acme.example");
    edit["gitea"]["admin"]
        .as_object_mut()
        .unwrap()
        .remove("password");
    edit["gitea"]["oauth"] = json!([]);
    // Volume settings are create-only: Kubernetes cannot apply them to the existing PVC.
    for (field, value) in [
        ("size", json!("30Gi")),
        ("storageClass", json!("fast")),
        ("claimName", json!("other-data")),
    ] {
        let mut resize = edit.clone();
        resize["persistence"][field] = value;
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": "forgejo", "name": "git", "values": resize }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert!(
            body.contains(&format!("persistence.{field}"))
                && body.contains("cannot be changed after install"),
            "{body}"
        );
    }
    edit["gitea"]["admin"]["passwordMode"] = json!("initialOnlyNoReset");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo", "name": "git", "values": edit }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    assert_eq!(
        std::fs::read_to_string(dir.join("secrets.enc.yaml")).unwrap(),
        stored
    );
    let hr = read_yaml(&dir.join("helmrelease.yaml"));
    assert_eq!(
        hr["spec"]["values"]["gitea"]["admin"]["passwordMode"],
        "initialOnlyNoReset"
    );
    assert_eq!(hr["spec"]["values"]["persistence"]["size"], "20Gi");
    assert_eq!(hr["spec"]["valuesFrom"].as_array().unwrap().len(), 2);
    fixture
        .state
        .git_runner
        .checkout_branch(&workdir, "main")
        .await
        .unwrap();

    // A new instance still needs the admin password.
    let mut missing = forgejo_values("git2.acme.example");
    missing["gitea"]["admin"]
        .as_object_mut()
        .unwrap()
        .remove("password");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo", "name": "git2", "values": missing }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

/// Helm app routes get the same guards as manifest app routes, before anything is committed:
/// no catch-all route, no platform host, and no hostname claimed by another app (Helm apps
/// record theirs in the `keel.io/route-hostnames` HelmRelease annotation).
#[tokio::test]
async fn test_helm_app_gateway_routes_are_guarded() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_builtin_helm_catalog(&fixture, "forgejo", FORGEJO_APP_CATALOG).await;
    seed_builtin_helm_catalog(&fixture, "seaweedfs", SEAWEEDFS_APP_CATALOG).await;
    seed_rauthy_catalog(&fixture).await;
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    let head_before = git.rev_parse(workdir, "HEAD").await.unwrap();

    // Enabled without a hostname would match every host on the shared listeners.
    let mut catch_all = forgejo_values("unused.example");
    catch_all["httpRoute"]["hostnames"] = json!([]);
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo", "values": catch_all }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("at least one hostname"), "{body}");

    for host in ["forgejo.keel.local", "idp.keel.local", "keel.keel.local"] {
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": "forgejo", "values": forgejo_values(host) }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{host}: {body}");
        assert!(body.contains("reserved platform host"), "{body}");
    }
    assert_eq!(git.rev_parse(workdir, "HEAD").await.unwrap(), head_before);

    // Forgejo claims git.acme.example on main.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo",
                "values": forgejo_values("git.acme.example") }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    merge_proposal_and_sync(&fixture, &cookie, &body).await;

    // Another tenant's SeaweedFS S3 route and a Rauthy manifest app may not take it over.
    let mut s3 = seaweedfs_values();
    s3["s3"]["httpRoute"] = json!({ "enabled": true, "hostnames": ["git.acme.example"] });
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "other", "catalogId": "seaweedfs", "values": s3 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("tenants/acme/apps/forgejo"), "{body}");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "rauthy", "values": rauthy_values("git.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Editing the owner itself is fine (its own claim is skipped).
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo",
                "values": forgejo_values("git.acme.example") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Apps with locked values / declared routes only accept values their schema declares: chart
/// options outside the form (other routes, `extraDeploy`, sidecars, ServiceAccount overrides)
/// would bypass the locks and route guards. Values naming the tenant reconciler ServiceAccount
/// and shell metacharacters in the OIDC sources (rendered into the chart's init script) are
/// rejected too. Nothing is committed.
#[tokio::test]
async fn test_helm_app_values_stay_inside_catalog_schema() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_builtin_helm_catalog(&fixture, "forgejo", FORGEJO_APP_CATALOG).await;
    seed_builtin_helm_catalog(&fixture, "seaweedfs", SEAWEEDFS_APP_CATALOG).await;
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    let head_before = git.rev_parse(workdir, "HEAD").await.unwrap();

    let mut cases: Vec<(&str, Value, &str, StatusCode)> = Vec::new();
    let mut filer_route = seaweedfs_values();
    filer_route["filer"]["httpRoute"] = json!({
        "enabled": true,
        "parentRefs": [{ "name": "keel-ingress", "namespace": "keel-system" }]
    });
    cases.push((
        "seaweedfs",
        filer_route,
        "filer.httpRoute",
        StatusCode::BAD_REQUEST,
    ));
    let mut cosi = seaweedfs_values();
    cosi["cosi"] = json!({ "enabled": true });
    cases.push(("seaweedfs", cosi, "cosi", StatusCode::BAD_REQUEST));
    let mut bucket_extra = seaweedfs_values();
    bucket_extra["s3"]["createBuckets"] = json!([{ "name": "data", "anonymousRead": true }]);
    cases.push((
        "seaweedfs",
        bucket_extra,
        "s3.createBuckets[0].anonymousRead",
        StatusCode::BAD_REQUEST,
    ));
    let mut reconciler_sa = seaweedfs_values();
    reconciler_sa["global"] = json!({ "seaweedfs": { "serviceAccountName": "tenant-acme-sa" } });
    cases.push((
        "seaweedfs",
        reconciler_sa,
        "tenant-acme-sa",
        StatusCode::BAD_REQUEST,
    ));
    let mut extra_deploy = forgejo_values("git.acme.example");
    extra_deploy["extraDeploy"] = json!([{ "apiVersion": "v1", "kind": "ConfigMap" }]);
    cases.push((
        "forgejo",
        extra_deploy,
        "extraDeploy",
        StatusCode::BAD_REQUEST,
    ));
    let mut service_account = forgejo_values("git.acme.example");
    service_account["serviceAccount"] = json!({ "name": "tenant-acme-sa" });
    cases.push((
        "forgejo",
        service_account,
        "serviceAccount",
        StatusCode::BAD_REQUEST,
    ));
    let mut webhook = forgejo_values("git.acme.example");
    webhook["gitea"]["config"]["webhook"] = json!({ "ALLOWED_HOST_LIST": "*" });
    cases.push((
        "forgejo",
        webhook,
        "gitea.config.webhook",
        StatusCode::BAD_REQUEST,
    ));
    let mut injection = forgejo_values("git.acme.example");
    injection["gitea"]["oauth"][0]["secret"] =
        json!("$(curl http://seaweedfs-filer.keel-system:8888/)");
    cases.push((
        "forgejo",
        injection,
        "/gitea/oauth/0/secret",
        StatusCode::UNPROCESSABLE_ENTITY,
    ));
    let mut discovery = forgejo_values("git.acme.example");
    discovery["gitea"]["oauth"][0]["autoDiscoverUrl"] = json!("https://x.example/`id`");
    cases.push((
        "forgejo",
        discovery,
        "/gitea/oauth/0/autoDiscoverUrl",
        StatusCode::UNPROCESSABLE_ENTITY,
    ));

    for (catalog_id, values, needle, expected) in cases {
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": catalog_id, "values": values }),
        )
        .await;
        assert_eq!(status, expected, "{needle}: {body}");
        assert!(body.contains(needle), "{needle}: {body}");
    }
    assert_eq!(git.rev_parse(workdir, "HEAD").await.unwrap(), head_before);
}

/// Render test for the self-service SeaweedFS Helm app: the upstream chart at the catalog/core
/// version without cluster-scoped RBAC, restricted-PSS contexts on every component, persistent
/// volumes, encrypted S3 credentials, an intra-release NetworkPolicy, and no route unless enabled.
/// On a remote workload cluster no NetworkPolicy is committed (it would land on the management
/// cluster) while the secrets still come from the management cluster through `valuesFrom`.
#[tokio::test]
async fn test_seaweedfs_catalog_app_renders_helm_release() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_builtin_helm_catalog(&fixture, "seaweedfs", SEAWEEDFS_APP_CATALOG).await;
    let workdir = fixture.state.repo_workdir.clone();
    let dir = workdir.join("tenants/acme/apps/seaweedfs");

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "seaweedfs", "values": seaweedfs_values() }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;

    let committed = read_tree(&dir);
    for secret in [SEAWEEDFS_ACCESS_KEY, SEAWEEDFS_SECRET_KEY] {
        assert!(!committed.contains(secret), "secret committed in plaintext");
    }
    let repo = read_yaml(&dir.join("helm-repository.yaml"));
    assert_eq!(
        repo["spec"]["url"],
        "https://seaweedfs.github.io/seaweedfs/helm"
    );
    assert!(repo["spec"].get("type").is_none());

    let hr = read_yaml(&dir.join("helmrelease.yaml"));
    assert_eq!(hr["spec"]["chart"]["spec"]["chart"], "seaweedfs");
    assert_eq!(hr["spec"]["chart"]["spec"]["version"], "4.54.0");
    assert!(hr["metadata"].get("annotations").is_none());
    let values = &hr["spec"]["values"];
    assert_eq!(
        values["global"]["seaweedfs"]["createClusterRole"], false,
        "no ClusterRole/ClusterRoleBinding in a tenant namespace"
    );
    assert_eq!(
        values["global"]["seaweedfs"]["serviceAccountName"],
        Value::Null
    );
    assert_eq!(values["s3"]["enableAuth"], true);
    for component in ["master", "filer", "volume", "s3"] {
        let csc = &values[component]["containerSecurityContext"];
        assert_eq!(csc["enabled"], true, "{component}");
        assert_eq!(csc["runAsNonRoot"], true, "{component}");
        assert_eq!(csc["allowPrivilegeEscalation"], false, "{component}");
        assert_eq!(csc["capabilities"]["drop"], json!(["ALL"]), "{component}");
        assert_eq!(
            csc["seccompProfile"]["type"], "RuntimeDefault",
            "{component}"
        );
        assert_eq!(
            values[component]["podSecurityContext"]["enabled"], true,
            "{component}"
        );
        assert_eq!(values[component]["logs"]["type"], "emptyDir", "{component}");
    }
    assert_eq!(values["master"]["data"]["type"], "persistentVolumeClaim");
    assert_eq!(values["master"]["data"]["size"], "1Gi");
    assert_eq!(values["volume"]["dataDirs"][0]["size"], "15Gi");
    assert_eq!(
        values["s3"]["createBuckets"],
        json!([{ "name": "artifacts" }])
    );
    assert!(values["s3"].get("credentials").is_none());
    assert_eq!(
        hr["spec"]["valuesFrom"],
        json!([
            { "kind": "Secret", "name": "seaweedfs-secrets",
              "valuesKey": "s3.credentials.admin.accessKey" },
            { "kind": "Secret", "name": "seaweedfs-secrets",
              "valuesKey": "s3.credentials.admin.secretKey" }
        ])
    );

    let np = read_yaml(&dir.join("networkpolicy.yaml"));
    assert_eq!(np["spec"]["ingress"][0]["ports"][0]["port"], 8333);
    // Other workloads of the tenant namespace may use the S3 endpoint (and only that port).
    assert_eq!(
        np["spec"]["ingress"][1]["from"],
        json!([{ "podSelector": {} }])
    );
    assert_eq!(
        np["spec"]["ingress"][1]["ports"],
        json!([{ "protocol": "TCP", "port": 8333 }])
    );
    assert_eq!(
        np["spec"]["ingress"][2]["from"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/instance"],
        "seaweedfs"
    );
    // Only the S3 gateway may be routed; optional components and the resize hook stay off.
    for component in ["master", "filer", "volume", "admin"] {
        assert_eq!(
            values[component]["httpRoute"]["enabled"], false,
            "{component}"
        );
    }
    for component in ["admin", "sftp", "worker", "cosi", "vector"] {
        assert_eq!(values[component]["enabled"], false, "{component}");
    }
    assert_eq!(values["filer"]["ingresses"]["http"]["enabled"], false);
    assert_eq!(values["volume"]["resizeHook"]["enabled"], false);

    // hostPath data directories are not expressible (const), so restricted PSS holds.
    fixture
        .state
        .git_runner
        .checkout_branch(&workdir, "main")
        .await
        .unwrap();
    let mut host_path = seaweedfs_values();
    host_path["volume"]["dataDirs"][0]["type"] = json!("hostPath");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "seaweedfs", "name": "s3-bad", "values": host_path }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Routes attach to the management cluster's keel-ingress: not on a remote workload cluster.
    let mut routed = seaweedfs_values();
    routed["s3"]["httpRoute"] = json!({ "enabled": true, "hostnames": ["s3-edge.acme.example"] });
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "seaweedfs", "name": "s3-edge",
                "targetCluster": "workload-kubeconfig", "values": routed }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("s3.httpRoute.enabled"), "{body}");

    // Remote workload cluster: no NetworkPolicy file, secrets still via valuesFrom.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "seaweedfs", "name": "s3-edge",
                "targetCluster": "workload-kubeconfig", "values": seaweedfs_values() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let edge = workdir.join("tenants/acme/apps/s3-edge");
    assert!(!edge.join("networkpolicy.yaml").exists());
    let hr = read_yaml(&edge.join("helmrelease.yaml"));
    assert_eq!(
        hr["spec"]["kubeConfig"]["secretRef"]["name"],
        "workload-kubeconfig"
    );
    assert_eq!(hr["spec"]["valuesFrom"].as_array().unwrap().len(), 2);
    // The Secret stays on the management cluster (helm-controller resolves `valuesFrom` in the
    // HelmRelease namespace), applied by the root tenants Kustomization.
    assert!(edge.join("secrets.enc.yaml").exists());
    assert!(!edge.join("config-kustomization.yaml").exists());
    let resources = tenants_kustomization_resources(&fixture);
    assert!(
        !resources
            .iter()
            .any(|r| r == "acme/apps/s3-edge/networkpolicy.yaml")
    );
    assert!(resources.contains(&"acme/apps/s3-edge/secrets.enc.yaml".to_string()));
    fixture
        .state
        .git_runner
        .checkout_branch(&workdir, "main")
        .await
        .unwrap();
}

/// Bundle Helm items get the same secret layout as single Helm apps: the item's SOPS
/// `secrets.enc.yaml` Secret is registered directly in `tenants/kustomization.yaml` (decrypted by
/// the root `keel-tenants` Kustomization, not the item Kustomization), no `<item>-config`
/// Kustomization is generated, and the NetworkPolicy is applied with the item.
#[tokio::test]
async fn test_bundle_helm_item_registers_secret_in_tenants_kustomization() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_builtin_helm_catalog(&fixture, "seaweedfs", SEAWEEDFS_APP_CATALOG).await;
    commit_to_main(
        &fixture,
        &[(
            "catalog/bundles/storage-stack/catalog.yaml",
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: storage-stack
  name: Storage Stack
  category: storage
spec:
  items:
    - id: s3
      kind: app
      catalogRef: seaweedfs
  schema:
    type: object
"#,
        )],
        "feat(catalog): storage-stack bundle",
    )
    .await;

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "storage-stack", "kind": "bundle",
                "values": { "s3": seaweedfs_values() } }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let dir = fixture
        .state
        .repo_workdir
        .join("tenants/acme/bundles/storage-stack/s3");
    let stored = std::fs::read_to_string(dir.join("secrets.enc.yaml")).unwrap();
    assert!(stored.contains("kind: Secret") && stored.contains("ENC[AES256_GCM,"));
    assert!(!dir.join("config").exists());
    assert!(!dir.join("config-kustomization.yaml").exists());
    assert!(!read_tree(&dir).contains(SEAWEEDFS_SECRET_KEY));
    let hr = read_yaml(&dir.join("helmrelease.yaml"));
    assert_eq!(
        hr["spec"]["valuesFrom"][0]["name"],
        "storage-stack-s3-secrets"
    );
    assert_eq!(
        hr["spec"]["values"]["global"]["seaweedfs"]["createClusterRole"],
        false
    );
    let item_k = read_yaml(&dir.join("kustomization.yaml"));
    let item_resources = item_k["resources"].as_array().unwrap();
    assert!(
        item_resources.contains(&json!("networkpolicy.yaml")),
        "{item_k}"
    );
    assert!(
        !item_resources.contains(&json!("secrets.enc.yaml")),
        "{item_k}"
    );
    let resources = tenants_kustomization_resources(&fixture);
    assert!(resources.contains(&"acme/bundles/storage-stack/s3/secrets.enc.yaml".to_string()));
    assert!(
        !resources
            .iter()
            .any(|r| r.ends_with("config-kustomization.yaml"))
    );
}

/// Regression (live E2E): a Helm app with secret fields in a tenant without a tenant
/// `GitRepository` (only `tenant-default` gets `tenants/default/git-repository.yaml` at
/// bootstrap). A tenant-namespace `<name>-config` Kustomization would never resolve its source, so
/// the HelmRelease's `valuesFrom` Secret would never exist. The Secret is listed directly in
/// `tenants/kustomization.yaml` (root `keel-tenants`, cluster SOPS key) and no tenant Flux
/// Kustomization is generated. A partial secret edit merges into the stored root Secret.
#[tokio::test]
async fn test_helm_app_secrets_in_non_default_tenant_use_root_tenants_kustomization() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_builtin_helm_catalog(&fixture, "forgejo", FORGEJO_APP_CATALOG).await;
    remove_tenant_git_source_stub(&fixture, "acme").await;
    let workdir = fixture.state.repo_workdir.clone();
    assert!(!workdir.join("tenants/acme/git-repository.yaml").exists());
    let dir = workdir.join("tenants/acme/apps/git");

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo", "name": "git",
                "values": forgejo_values("git.acme.example") }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;

    let flux_kustomizations: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with("-kustomization.yaml"))
        .collect();
    assert!(flux_kustomizations.is_empty(), "{flux_kustomizations:?}");
    assert!(!dir.join("config").exists());
    let stored = std::fs::read_to_string(dir.join("secrets.enc.yaml")).unwrap();
    let doc: Value = noyalib::from_str(&stored).unwrap();
    assert_eq!(doc["kind"], "Secret");
    assert_eq!(doc["metadata"]["name"], "git-secrets");
    assert_eq!(doc["metadata"]["namespace"], "tenant-acme");
    let resources = tenants_kustomization_resources(&fixture);
    for file in ["helmrelease.yaml", "secrets.enc.yaml"] {
        let entry = format!("acme/apps/git/{file}");
        assert!(resources.contains(&entry), "{entry} missing: {resources:?}");
    }
    assert!(
        !resources.iter().any(|r| r.ends_with("-kustomization.yaml")),
        "{resources:?}"
    );

    // Edit re-entering only the admin password: the stored OIDC secret is kept (decrypted from
    // the root Secret, merged and re-encrypted).
    merge_proposal_and_sync(&fixture, &cookie, &body).await;
    let mut edit = forgejo_values("git.acme.example");
    edit["gitea"]["admin"]["password"] = json!("Rotated-Admin#Passw0rd!");
    edit["gitea"]["oauth"] = json!([]);
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "forgejo", "name": "git", "values": edit }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let rotated = std::fs::read_to_string(dir.join("secrets.enc.yaml")).unwrap();
    assert_ne!(rotated, stored);
    assert_eq!(
        sops_secret_keys(&rotated),
        vec!["gitea.admin.password", "gitea.oauth"]
    );
    let secret = decrypt_tenant_secret(&fixture, &rotated).await;
    let admin: Value = noyalib::from_str(
        secret["stringData"]["gitea.admin.password"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        admin["gitea"]["admin"]["password"],
        "Rotated-Admin#Passw0rd!"
    );
    let oauth: Value =
        noyalib::from_str(secret["stringData"]["gitea.oauth"].as_str().unwrap()).unwrap();
    assert_eq!(oauth["gitea"]["oauth"][0]["secret"], FORGEJO_OIDC_SECRET);
    assert_eq!(
        read_yaml(&dir.join("helmrelease.yaml"))["spec"]["valuesFrom"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

const KYVERNO_APP_CATALOG: &str = include_str!("../../../deploy/catalog/apps/kyverno/catalog.yaml");
const PKO_APP_CATALOG: &str =
    include_str!("../../../deploy/catalog/apps/pulumi-kubernetes-operator/catalog.yaml");
const FLUX2_APP_CATALOG: &str = include_str!("../../../deploy/catalog/apps/flux2/catalog.yaml");

/// Render test for the workload-cluster twins of the management operators (Kyverno, PKO, Flux):
/// `scope: cluster` (400 on the management cluster), the upstream chart at the catalog/core
/// version from a tenant-scoped (OCI where needed) HelmRepository, the remote `kubeConfig` and
/// dedicated target namespace (`spec.placement.namespace`: the default when omitted, any other
/// namespace rejected), the locked seed values merged, the chart's CRD policy, no NetworkPolicy
/// (remote), and strict values (undeclared chart values rejected, HEAD unchanged).
#[tokio::test]
async fn test_workload_operator_catalog_apps_render_remote_helm_releases() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let workdir = fixture.state.repo_workdir.clone();
    let git = &fixture.state.git_runner;

    struct Case {
        id: &'static str,
        raw: &'static str,
        core: &'static str,
        target_ns: &'static str,
        values: Value,
        undeclared: Value,
        repo_url: &'static str,
        oci: bool,
        crds: Option<&'static str>,
    }
    let cases = [
        Case {
            id: "kyverno",
            raw: KYVERNO_APP_CATALOG,
            core: include_str!("../../../deploy/catalog/core/kyverno/catalog.yaml"),
            target_ns: "kyverno",
            values: json!({
                "admissionController": { "replicas": 3 },
                "features": { "policyExceptions": { "enabled": true } }
            }),
            // A webhook bypass knob outside the form.
            undeclared: json!({ "config": { "webhooks": { "namespaceSelector": {} } } }),
            repo_url: "https://kyverno.github.io/kyverno/",
            oci: false,
            crds: Some("CreateReplace"),
        },
        Case {
            id: "pulumi-kubernetes-operator",
            raw: PKO_APP_CATALOG,
            core: include_str!(
                "../../../deploy/catalog/core/pulumi-kubernetes-operator/catalog.yaml"
            ),
            target_ns: "pulumi-kubernetes-operator",
            values: json!({ "replicaCount": 1, "controller": { "logLevel": "debug" } }),
            undeclared: json!({ "extraSidecars": [{ "name": "x", "image": "busybox" }] }),
            repo_url: "oci://ghcr.io/pulumi/helm-charts",
            oci: true,
            crds: Some("CreateReplace"),
        },
        Case {
            id: "flux2",
            raw: FLUX2_APP_CATALOG,
            core: include_str!("../../../deploy/catalog/core/flux2/catalog.yaml"),
            target_ns: "flux-system",
            values: json!({ "multitenancy": { "enabled": true }, "logLevel": "info" }),
            undeclared: json!({
                "multitenancy": { "enabled": true },
                "extraObjects": [{ "kind": "ClusterRoleBinding" }]
            }),
            repo_url: "oci://ghcr.io/fluxcd-community/charts",
            oci: true,
            crds: None,
        },
    ];

    for case in cases {
        let id = case.id;
        seed_builtin_helm_catalog(&fixture, id, case.raw).await;
        let core = keel_core::parse_catalog_application(case.core).unwrap();

        // Cluster-wide CRDs/RBAC/webhooks: never into a management-cluster tenant namespace.
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": id, "name": id, "values": case.values }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {body}");

        // Values outside the schema are rejected before anything is committed.
        let head_before = git.rev_parse(&workdir, "HEAD").await.unwrap();
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": id, "name": id,
                    "targetCluster": "workload-kubeconfig", "targetNamespace": case.target_ns,
                    "values": case.undeclared }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {body}");
        assert!(body.contains("not declared"), "{id}: {body}");
        assert_eq!(
            git.rev_parse(&workdir, "HEAD").await.unwrap(),
            head_before,
            "{id}"
        );

        // The operator only installs into its dedicated namespace: never the tenant's shared
        // namespace (chart NetworkPolicies, Kyverno's own-namespace exclusion) or another one.
        for other in ["tenant-acme", "apps"] {
            let (status, body) = post_proposal(
                &fixture,
                &cookie,
                json!({ "tenant": "acme", "catalogId": id, "name": id,
                        "targetCluster": "workload-kubeconfig", "targetNamespace": other,
                        "values": case.values }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{id}/{other}: {body}");
            assert!(body.contains("spec.placement.namespace"), "{id}: {body}");
        }
        assert_eq!(
            git.rev_parse(&workdir, "HEAD").await.unwrap(),
            head_before,
            "{id}"
        );

        // No targetNamespace: the dedicated namespace is the default.
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": id, "name": id,
                    "targetCluster": "workload-kubeconfig", "values": case.values }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{id}: {body}");
        checkout_proposal_branch(&fixture, &body).await;
        let dir = workdir.join(format!("tenants/acme/apps/{id}"));

        let repo = read_yaml(&dir.join("helm-repository.yaml"));
        assert_eq!(repo["metadata"]["namespace"], "tenant-acme", "{id}");
        assert_eq!(repo["spec"]["url"], case.repo_url, "{id}");
        assert_eq!(
            repo["spec"].get("type").and_then(Value::as_str),
            case.oci.then_some("oci"),
            "{id}"
        );

        let hr = read_yaml(&dir.join("helmrelease.yaml"));
        let chart = &hr["spec"]["chart"]["spec"];
        assert_eq!(
            chart["chart"],
            json!(
                core.spec
                    .upstream
                    .chart
                    .as_deref()
                    .unwrap()
                    .rsplit('/')
                    .next()
            ),
            "{id}"
        );
        assert_eq!(
            chart["version"],
            json!(core.spec.upstream.version.as_deref().unwrap()),
            "{id}: catalog/core version"
        );
        assert_eq!(chart["sourceRef"]["namespace"], "tenant-acme", "{id}");
        assert_eq!(
            hr["spec"]["kubeConfig"]["secretRef"]["name"], "workload-kubeconfig",
            "{id}"
        );
        assert_eq!(hr["spec"]["targetNamespace"], case.target_ns, "{id}");
        assert_eq!(hr["spec"]["install"]["createNamespace"], true, "{id}");
        assert_eq!(
            hr["spec"]["install"].get("crds").and_then(Value::as_str),
            case.crds,
            "{id}"
        );
        assert_eq!(
            hr["spec"]
                .get("upgrade")
                .and_then(|u| u.get("crds"))
                .and_then(Value::as_str),
            case.crds,
            "{id}"
        );
        // Remote installs get no Keel NetworkPolicy and no secrets.
        assert!(!dir.join("networkpolicy.yaml").exists(), "{id}");
        assert!(hr["spec"].get("valuesFrom").is_none(), "{id}");

        let values = &hr["spec"]["values"];
        let locked: Value =
            noyalib::from_str::<Value>(case.raw).unwrap()["spec"]["lockedValues"].clone();
        fn contains(haystack: &Value, needle: &Value) -> bool {
            match (haystack, needle) {
                (Value::Object(h), Value::Object(n)) => n
                    .iter()
                    .all(|(k, v)| h.get(k).is_some_and(|hv| contains(hv, v))),
                _ => haystack == needle,
            }
        }
        assert!(contains(values, &locked), "{id}: locked values not merged");
        assert!(contains(values, &case.values), "{id}: form values lost");
        // values.yaml keeps only the form values.
        let form = read_yaml(&dir.join("values.yaml"));
        assert_eq!(form, case.values, "{id}");

        match id {
            "kyverno" => {
                assert_eq!(
                    values["crds"]["groups"]["kyverno"]["clusterpolicies"],
                    false
                );
                assert_eq!(values["admissionController"]["replicas"], 3);
                // Exceptions only from the dedicated install namespace (never empty or `*`).
                assert_eq!(
                    values["features"]["policyExceptions"],
                    json!({ "enabled": true, "namespace": "kyverno" })
                );
            }
            "pulumi-kubernetes-operator" => {
                assert_eq!(
                    values["securityContext"]["capabilities"]["drop"],
                    json!(["ALL"])
                );
                assert_eq!(values["podSecurityContext"]["runAsNonRoot"], true);
            }
            "flux2" => {
                assert_eq!(values["multitenancy"]["enabled"], true);
                assert_eq!(values["multitenancy"]["defaultServiceAccount"], "default");
                assert_eq!(
                    values["crds"]["annotations"]["helm.sh/resource-policy"],
                    "keep"
                );
            }
            _ => unreachable!(),
        }

        git.checkout_branch(&workdir, "main").await.unwrap();
    }

    let remote = |values: Value| {
        json!({ "tenant": "acme", "catalogId": "flux2", "name": "flux-gitops",
                "targetCluster": "workload-kubeconfig", "values": values })
    };
    let head_before = git.rev_parse(&workdir, "HEAD").await.unwrap();
    // Keel applies no schema defaults server-side, so an API/CLI request without the Flux
    // lockdown switch fails schema validation (422) instead of getting the chart default (lockdown off) next to
    // the locked cluster-admin `privileged: true`.
    for values in [
        json!({}),
        json!({ "logLevel": "info" }),
        json!({ "multitenancy": {} }),
    ] {
        let (status, body) = post_proposal(&fixture, &cookie, remote(values.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{values}: {body}");
        assert!(body.contains("is a required property"), "{body}");
    }
    // Kyverno's PolicyException namespace is locked: neither `*`, nor empty, nor another one.
    for namespace in ["*", "", "kyverno"] {
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": "kyverno", "name": "kyverno-x",
                    "targetCluster": "workload-kubeconfig",
                    "values": { "features": { "policyExceptions": {
                        "enabled": true, "namespace": namespace } } } }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{namespace:?}: {body}");
        assert!(body.contains("not declared"), "{body}");
    }
    assert_eq!(git.rev_parse(&workdir, "HEAD").await.unwrap(), head_before);

    // An explicit opt-out is honoured (single-team clusters).
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        remote(json!({ "multitenancy": { "enabled": false } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let hr = read_yaml(&workdir.join("tenants/acme/apps/flux-gitops/helmrelease.yaml"));
    assert_eq!(hr["spec"]["values"]["multitenancy"]["enabled"], false);
    assert_eq!(hr["spec"]["targetNamespace"], "flux-system");
    git.checkout_branch(&workdir, "main").await.unwrap();

    // Bundle items get the same dedicated namespace: register/update and proposals reject
    // another one, and an item without targetNamespace installs into it.
    let (status, body) = send_text(
        &fixture,
        &cookie,
        Method::POST,
        "/api/v1/catalog",
        json!({
            "kind": "bundle",
            "id": "vm-gitops",
            "name": "VM + GitOps",
            "category": "platform",
            "items": [
                { "id": "vm", "kind": "infra", "catalogRef": "proxmox-vm" },
                {
                    "id": "flux",
                    "kind": "app",
                    "catalogRef": "flux2",
                    "dependsOn": ["vm"],
                    "targetClusterFrom": "vm",
                    "targetNamespace": "tenant-acme"
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("spec.placement.namespace"), "{body}");
    let bundle = |item_ns: &str| {
        format!(
            r#"apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: gitops-stack
  name: GitOps Stack
  category: platform
spec:
  items:
    - id: flux
      kind: app
      catalogRef: flux2
{item_ns}  schema:
    type: object
"#
        )
    };
    commit_to_main(
        &fixture,
        &[(
            "catalog/bundles/gitops-stack/catalog.yaml",
            &bundle("      targetNamespace: tenant-acme\n"),
        )],
        "feat(catalog): gitops-stack bundle (wrong namespace)",
    )
    .await;
    let flux_values = json!({ "flux": { "multitenancy": { "enabled": true } } });
    let bundle_req = json!({ "tenant": "acme", "catalogId": "gitops-stack", "kind": "bundle",
                             "targetCluster": "workload-kubeconfig", "values": flux_values });
    let (status, body) = post_proposal(&fixture, &cookie, bundle_req.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Bundle item 'flux'"), "{body}");
    assert!(body.contains("spec.placement.namespace"), "{body}");

    commit_to_main(
        &fixture,
        &[("catalog/bundles/gitops-stack/catalog.yaml", &bundle(""))],
        "feat(catalog): gitops-stack bundle",
    )
    .await;
    let (status, body) = post_proposal(&fixture, &cookie, bundle_req).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let hr = read_yaml(&workdir.join("tenants/acme/bundles/gitops-stack/flux/helmrelease.yaml"));
    assert_eq!(hr["spec"]["targetNamespace"], "flux-system");
    assert_eq!(hr["spec"]["values"]["multitenancy"]["enabled"], true);
    git.checkout_branch(&workdir, "main").await.unwrap();
}

const KEEL_SERVER_APP_CATALOG: &str =
    include_str!("../../../deploy/catalog/apps/keel-server/catalog.yaml");
const KEEL_SERVER_APP_MANIFESTS: &str =
    include_str!("../../../deploy/catalog/apps/keel-server/manifests.yaml");

async fn seed_keel_server_catalog(fixture: &TestFixture) {
    commit_to_main(
        fixture,
        &[
            (
                "catalog/apps/keel-server/catalog.yaml",
                KEEL_SERVER_APP_CATALOG,
            ),
            (
                "catalog/apps/keel-server/manifests.yaml",
                KEEL_SERVER_APP_MANIFESTS,
            ),
        ],
        "feat(catalog): add keel-server",
    )
    .await;
}

fn keel_server_values(hostname: &str) -> Value {
    json!({
        "hostname": hostname,
        "oidcIssuerUrl": "https://auth.team-a.example.com/auth/v1",
        "oidcClientId": "team-a-keel",
        "oidcClientSecret": "oidc-Client-Secret-0123456789",
        "adminGroup": "team-a-platform",
        "gitUrl": "https://git.example.com/team-a/infra-gitops.git",
        "gitToken": "glpat-SampleGitToken0123456789",
    })
}

/// The built-in workload-cluster keel-server (`deploy/catalog/apps/keel-server`) renders a
/// multi-namespace Flux manifest app on the remote cluster: its three namespaces are created
/// first, no `targetNamespace` collapses the per-namespace Roles, and the Git token / OIDC client
/// secret only reach Git SOPS-encrypted on the management cluster.
#[tokio::test]
async fn test_keel_server_catalog_app_renders_multi_namespace_remote_layout() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_keel_server_catalog(&fixture).await;
    let workdir = fixture.state.repo_workdir.clone();

    let values = keel_server_values("keel.team-a.example.com");
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "keel-server",
                "targetCluster": "team-a-kubeconfig", "values": values }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;

    let dir = workdir.join("tenants/acme/apps/keel-server");
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).unwrap();
    let yaml = |file: &str| noyalib::from_str::<Value>(&read(file)).unwrap();
    let committed = read_tree(&dir);
    for field in ["oidcClientSecret", "gitToken"] {
        let secret = values[field].as_str().unwrap();
        assert!(
            !committed.contains(secret),
            "{field} committed in plaintext"
        );
    }
    assert!(read("config/secrets.enc.yaml").contains("ENC[AES256_GCM,"));

    let fk = yaml("flux-kustomization.yaml");
    assert_eq!(fk["metadata"]["name"], "keel-server");
    assert_eq!(
        fk["spec"]["kubeConfig"]["secretRef"]["name"],
        "team-a-kubeconfig"
    );
    assert!(
        fk["spec"].get("targetNamespace").is_none(),
        "a targetNamespace would collapse the keel-previews / flux-system Roles: {fk}"
    );
    assert!(
        fk["spec"]["postBuild"]["substituteFrom"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "Secret" && s["name"] == "keel-server-secrets")
    );
    let config_fk = yaml("config-kustomization.yaml");
    assert!(config_fk["spec"].get("kubeConfig").is_none());
    assert_eq!(config_fk["spec"]["targetNamespace"], "tenant-acme");

    assert_eq!(
        yaml("kustomization.yaml")["resources"],
        json!(["namespace.yaml", "manifests.yaml"])
    );
    let namespaces: Vec<Value> = keel_core::split_yaml_documents(&read("namespace.yaml"))
        .iter()
        .map(|d| noyalib::from_str(d).unwrap())
        .collect();
    assert_eq!(
        namespaces
            .iter()
            .map(|n| n["metadata"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["keel-system", "keel-previews", "flux-system"]
    );
    for ns in &namespaces {
        assert_eq!(ns["kind"], "Namespace");
        assert_eq!(
            ns["metadata"]["annotations"]["kustomize.toolkit.fluxcd.io/ssa"],
            "IfNotPresent"
        );
        assert_restricted_pss(ns);
    }

    let manifests = read("manifests.yaml");
    assert!(manifests.contains("${gitToken}"));
    assert!(manifests.contains("${oidcClientSecret}"));
    assert!(manifests.contains("https://keel.team-a.example.com/api/v1/auth/callback"));
    assert!(manifests.contains("team-a-platform"));
    let docs: Vec<Value> = keel_core::split_yaml_documents(&manifests)
        .iter()
        .filter_map(|d| noyalib::from_str::<Value>(d).ok())
        .filter(Value::is_object)
        .collect();
    let role_namespaces: Vec<&str> = docs
        .iter()
        .filter(|d| d["kind"] == "Role")
        .map(|d| d["metadata"]["namespace"].as_str().unwrap())
        .collect();
    assert_eq!(
        role_namespaces,
        vec!["keel-system", "keel-previews", "flux-system"]
    );
    // The flux-system reconcile-request grant ships with its native admission guard (the
    // workload cluster has no Keel Kyverno `protect-control-plane` policy).
    for kind in [
        "ValidatingAdmissionPolicy",
        "ValidatingAdmissionPolicyBinding",
    ] {
        let doc = docs
            .iter()
            .find(|d| d["kind"] == kind && d["metadata"]["name"] == "keel-server-reconcile-only")
            .unwrap_or_else(|| panic!("{kind} missing"));
        assert_eq!(doc["apiVersion"], "admissionregistration.k8s.io/v1");
        assert!(doc["metadata"].get("namespace").is_none());
    }

    let resources = tenants_kustomization_resources(&fixture);
    for file in ["config-kustomization.yaml", "flux-kustomization.yaml"] {
        assert!(
            resources.contains(&format!("acme/apps/keel-server/{file}")),
            "{resources:?}"
        );
    }
    fixture
        .state
        .git_runner
        .checkout_branch(&workdir, "main")
        .await
        .unwrap();
}

/// The workload keel-server never installs on the management cluster, never leaves its
/// namespaces, only uses the Flux engine and keeps the platform hostname guards.
#[tokio::test]
async fn test_keel_server_catalog_app_rejects_management_cluster_and_bad_requests() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_keel_server_catalog(&fixture).await;
    let git = &fixture.state.git_runner;
    let workdir = &fixture.state.repo_workdir;
    let head_before = git.rev_parse(workdir, "HEAD").await.unwrap();

    let remote = |extra: Value| {
        let mut req = json!({ "tenant": "acme", "catalogId": "keel-server",
                              "targetCluster": "team-a-kubeconfig",
                              "values": keel_server_values("keel.team-a.example.com") });
        for (k, v) in extra.as_object().unwrap() {
            req[k] = v.clone();
        }
        req
    };
    let cases = [
        // scope: cluster (ClusterRoles) => management cluster rejected.
        (
            json!({ "tenant": "acme", "catalogId": "keel-server",
                    "values": keel_server_values("keel.team-a.example.com") }),
            "scope: cluster",
        ),
        (
            remote(json!({ "targetNamespace": "tenant-acme" })),
            "keel-system",
        ),
        (remote(json!({ "actionEngine": "pulumi" })), "flux engine"),
        // Platform host of the management plane.
        (
            remote(json!({ "values": keel_server_values("keel.keel.local") })),
            "keel.keel.local",
        ),
    ];
    for (req, needle) in cases {
        let (status, body) = post_proposal(&fixture, &cookie, req.clone()).await;
        // Checked first (and never interpolated) so later messages only print a secret-free body;
        // the request itself carries sample secrets, so messages name the case by its needle.
        for secret in [
            "oidc-Client-Secret-0123456789",
            "glpat-SampleGitToken0123456789",
        ] {
            assert!(!body.contains(secret), "{needle}: secret echoed");
        }
        assert_eq!(status, StatusCode::BAD_REQUEST, "{needle}: {body}");
        assert!(body.contains(needle), "{needle}: {body}");
    }
    assert_eq!(git.rev_parse(workdir, "HEAD").await.unwrap(), head_before);
}

/// A multi-namespace manifest app (`spec.placement.additionalNamespaces`) must name one of its
/// namespaces on every namespaced object: Keel sets no Flux `targetNamespace` for it.
#[tokio::test]
async fn test_multi_namespace_manifest_app_requires_explicit_namespaces() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    let catalog = "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: multi-ns\n  name: Multi\n  description: d\n  category: platform\n  runtime: manifest\nspec:\n  defaultEngine: flux\n  upstream:\n    repoURL: https://example.com/multi\n    path: ./manifests.yaml\n    version: \"1.0.0\"\n  placement:\n    scope: cluster\n    namespace: ops\n    additionalNamespaces: [ops-extra]\n  schema:\n    type: object\n    properties:\n      ns:\n        type: string\n        default: ops\n";
    let manifests = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n  namespace: ${ns:=ops}\ndata: {}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: b\n  namespace: ops-extra\ndata: {}\n";
    commit_to_main(
        &fixture,
        &[
            ("catalog/apps/multi-ns/catalog.yaml", catalog),
            ("catalog/apps/multi-ns/manifests.yaml", manifests),
        ],
        "feat(catalog): add multi-ns",
    )
    .await;
    let git = &fixture.state.git_runner;
    let workdir = fixture.state.repo_workdir.clone();
    let head_before = git.rev_parse(&workdir, "HEAD").await.unwrap();

    for ns in ["kube-system", "tenant-acme"] {
        let (status, body) = post_proposal(
            &fixture,
            &cookie,
            json!({ "tenant": "acme", "catalogId": "multi-ns", "targetCluster": "ops-kubeconfig",
                    "values": { "ns": ns } }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{ns}: {body}");
        assert!(body.contains("ConfigMap/a"), "{body}");
        assert!(body.contains("ops, ops-extra"), "{body}");
    }
    // Only the Flux layout creates the namespaces and omits `targetNamespace`.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "multi-ns", "targetCluster": "ops-kubeconfig",
                "actionEngine": "pulumi", "values": { "ns": "ops" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("additionalNamespaces"), "{body}");
    assert_eq!(git.rev_parse(&workdir, "HEAD").await.unwrap(), head_before);

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "multi-ns", "targetCluster": "ops-kubeconfig",
                "values": { "ns": "ops-extra" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    checkout_proposal_branch(&fixture, &body).await;
    let dir = workdir.join("tenants/acme/apps/multi-ns");
    let fk: Value =
        noyalib::from_str(&std::fs::read_to_string(dir.join("flux-kustomization.yaml")).unwrap())
            .unwrap();
    assert!(fk["spec"].get("targetNamespace").is_none(), "{fk}");
    let ns_yaml = std::fs::read_to_string(dir.join("namespace.yaml")).unwrap();
    assert_eq!(keel_core::split_yaml_documents(&ns_yaml).len(), 2);
    git.checkout_branch(&workdir, "main").await.unwrap();
}

/// `scope: cluster` manifest apps ship fixed-name cluster-wide objects, so a tenant can install
/// the workload keel-server only once per target cluster; edits of that instance and installs on
/// other clusters still work.
#[tokio::test]
async fn test_keel_server_catalog_app_installs_once_per_cluster() {
    let fixture = setup_fixture().await;
    let cookie = login_and_get_cookie(&fixture).await;
    seed_keel_server_catalog(&fixture).await;
    let git = &fixture.state.git_runner;
    let workdir = fixture.state.repo_workdir.clone();

    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "keel-server",
                "targetCluster": "team-a-kubeconfig",
                "values": keel_server_values("keel.team-a.example.com") }),
    )
    .await;
    if sops_unavailable(status, &body) {
        return;
    }
    assert_eq!(status, StatusCode::CREATED, "{body}");
    merge_proposal_and_sync(&fixture, &cookie, &body).await;
    let head_before = git.rev_parse(&workdir, "HEAD").await.unwrap();

    // A second instance (another name and hostname) on the same cluster.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "keel-server", "name": "keel-server-2",
                "targetCluster": "team-a-kubeconfig",
                "values": keel_server_values("keel2.team-a.example.com") }),
    )
    .await;
    // Checked first (and never interpolated) so later messages only print a secret-free body.
    for secret in [
        "oidc-Client-Secret-0123456789",
        "glpat-SampleGitToken0123456789",
    ] {
        assert!(
            !body.contains(secret),
            "secret echoed in the conflict response"
        );
    }
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("once per cluster"), "{body}");
    assert!(body.contains("tenants/acme/apps/keel-server"), "{body}");
    assert_eq!(git.rev_parse(&workdir, "HEAD").await.unwrap(), head_before);

    // Editing the installed instance is not a second install.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "keel-server",
                "targetCluster": "team-a-kubeconfig",
                "values": keel_server_values("keel.team-a.example.com") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    git.checkout_branch(&workdir, "main").await.unwrap();

    // Another workload cluster of the same tenant.
    let (status, body) = post_proposal(
        &fixture,
        &cookie,
        json!({ "tenant": "acme", "catalogId": "keel-server", "name": "keel-server-b",
                "targetCluster": "team-b-kubeconfig",
                "values": keel_server_values("keel.team-b.example.com") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    git.checkout_branch(&workdir, "main").await.unwrap();
}
