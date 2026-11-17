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

async fn setup_fixture() -> TestFixture {
    let temp_dir = tempfile::tempdir().expect("create tempdir");
    let bare_repo = temp_dir.path().join("remote.git");
    let workdir = temp_dir.path().join("workdir");

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
    );

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
    let logout_json: Value =
        serde_json::from_slice(&logout_res.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(logout_json["authenticated"], false);
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
                .uri("/api/v1/proposals")
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
                .uri("/api/v1/proposals")
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
        let enc_yaml = tokio::fs::read_to_string(
            fixture
                .state
                .repo_workdir
                .join("tenants/acme/apps/grafana-secret-test/secrets.enc.yaml"),
        )
        .await
        .unwrap();
        assert!(enc_yaml.contains("ENC[AES256_GCM,"));
        assert!(!enc_yaml.contains("super-secret-password"));
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
                .uri("/api/v1/proposals")
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
                .uri("/api/v1/proposals?tenant=acme")
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
                .uri(format!("/api/v1/proposals/{pr_id}/preview"))
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
                .uri(format!("/api/v1/proposals/{pr_id}/merge"))
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
    assert_eq!(merge_json["gcPlan"]["targets"].as_array().unwrap().len(), 2);

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
                .uri(format!("/api/v1/proposals/{catalog_pr_id}/merge"))
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
                .uri("/api/v1/proposals")
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
spec:
  stack: keel/management/core
  protect: true
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
                .uri(format!("/api/v1/proposals/{pr1_id}/merge"))
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
                .uri(format!("/api/v1/proposals/{pr2_id}/reject"))
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
                .uri(format!("/api/v1/proposals/{pr2_id}/reopen"))
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
                .uri(format!("/api/v1/proposals/{pr2_id}/reopen"))
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reopen_open_res.status(), StatusCode::CONFLICT);
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
                .uri("/api/v1/proposals")
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
                .uri("/api/v1/proposals")
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
            .starts_with("[New]")
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

    // Updating an existing instance (tenants/acme/apps/redis) tags the PR as [Update]
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let update_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/proposals")
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
            .starts_with("[Update]")
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
                .uri("/api/v1/proposals")
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
        resources[1]["changed_fields"]
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
                .uri("/api/v1/proposals")
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
            .starts_with("[Update]")
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
                .uri("/api/v1/proposals")
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
            .starts_with("[Update]")
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
                .uri("/api/v1/proposals")
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
                .uri(format!("/api/v1/proposals/{acme_pr_id}/merge"))
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
                .uri(format!("/api/v1/proposals/{acme_pr_id}/reject"))
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
                .uri(format!("/api/v1/proposals/{acme_pr_id}/reopen"))
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
                .uri(format!("/api/v1/proposals/{acme_pr_id}/merge"))
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
                .uri(format!("/api/v1/proposals/{}/merge", rbac_pr.id))
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
                .uri(format!("/api/v1/proposals/{}/merge", rbac_pr.id))
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
                .uri("/api/v1/proposals")
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
    if status == StatusCode::BAD_REQUEST {
        assert!(
            body_str.contains("has no SOPS age key configured"),
            "unexpected 400 body: {body_str}"
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
                .uri("/api/v1/proposals")
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
    assert!(canary_yaml.contains("provider: gatewayapi:v1"));
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

    fixture
        .state
        .git_runner
        .checkout_branch(&fixture.state.repo_workdir, "main")
        .await
        .unwrap();

    // App with flaggerSupported: false must reject canary=true
    let no_canary_dir = fixture.state.repo_workdir.join("catalog/apps/redis");
    tokio::fs::create_dir_all(&no_canary_dir).await.unwrap();
    tokio::fs::write(
        no_canary_dir.join("catalog.yaml"),
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
    )
    .await
    .unwrap();

    let bad_res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/proposals")
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
                .uri("/api/v1/proposals")
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
                .uri("/api/v1/proposals")
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
                .uri("/api/v1/proposals")
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
