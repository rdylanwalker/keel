//! `POST /api/v1/tenants` with dedicated per-tenant repositories (`SDD.md` §3.2, §6.3) against
//! a mock forge: Forgejo (bot token), GitLab (deploy token), GitHub (SSH deploy key) and the
//! development-only Local Git provider (monorepo layout, D8).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, COOKIE};
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use k8s_openapi::api::core::v1::Secret;
use keel_core::AuthPhase;
use keel_git::{
    BranchProtectionOutcome, CommitAuthor, CreatePrRequest, DeployKeyInfo, DeployKeyOutcome,
    EnsuredDeployKey, ForgeError, ForgeProvider, ForkPolicyOutcome, GitRunner, LocalGitProvider,
    PullRequestInfo, PullRequestState, RepoReadToken, RepoReadTokenInfo, RepositoryEnsureOutcome,
    RepositoryInfo, RepositoryOptions, RepositoryTarget, ssh_public_key_fingerprint,
};
use keel_kube::SecretBackend;
use keel_kube::{InMemorySecretStore, KubeError, SecretStateStore, TelemetryMux};
use keel_server::{
    AppState, SESSION_COOKIE_NAME, SessionClaims, TenantRepoCloneBase, TenantRepoForge,
    TenantRepoForgeKind, TenantRepoNaming, TenantRepos, build_router, encrypt_session_token,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const HOST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
const STRAY_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl stray";
const FORGE_ORIGIN: &str = "https://forgejo.keel.local:8888";
const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

/// Records every per-repository forge call; PR operations go to the real local provider
/// (platform repository and, through `for_repository`, the sibling tenant repositories).
#[derive(Default)]
struct ForgeLog {
    calls: Mutex<Vec<String>>,
    minted: AtomicUsize,
    key_seq: AtomicUsize,
    revoked_tokens: Mutex<Vec<String>>,
    /// Live (minted, not revoked) read tokens: `(id, name)`.
    live_tokens: Mutex<Vec<(String, String)>>,
    deploy_keys: Mutex<Vec<DeployKeyInfo>>,
    fail_mint: AtomicBool,
    protected: AtomicBool,
}

impl ForgeLog {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
}

struct MockForge {
    inner: LocalGitProvider,
    kind: TenantRepoForgeKind,
    log: Arc<ForgeLog>,
}

#[async_trait]
impl ForgeProvider for MockForge {
    async fn create_pull_request(
        &self,
        req: CreatePrRequest,
    ) -> Result<PullRequestInfo, ForgeError> {
        self.inner.create_pull_request(req).await
    }
    async fn get_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        self.inner.get_pull_request(pr_id).await
    }
    async fn post_preview_comment(&self, pr_id: &str, report: &str) -> Result<(), ForgeError> {
        self.inner.post_preview_comment(pr_id, report).await
    }
    async fn list_pull_requests(
        &self,
        tenant: Option<&str>,
        state: Option<PullRequestState>,
    ) -> Result<Vec<PullRequestInfo>, ForgeError> {
        self.inner.list_pull_requests(tenant, state).await
    }
    async fn merge_pull_request(
        &self,
        pr_id: &str,
        expected_head_sha: Option<&str>,
    ) -> Result<PullRequestInfo, ForgeError> {
        self.inner
            .merge_pull_request(pr_id, expected_head_sha)
            .await
    }
    async fn close_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        ForgeProvider::close_pull_request(&self.inner, pr_id).await
    }
    async fn reopen_pull_request(&self, pr_id: &str) -> Result<PullRequestInfo, ForgeError> {
        ForgeProvider::reopen_pull_request(&self.inner, pr_id).await
    }
    async fn ensure_repository(
        &self,
        private: bool,
    ) -> Result<RepositoryEnsureOutcome, ForgeError> {
        ForgeProvider::ensure_repository(&self.inner, private).await
    }
    fn for_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Arc<dyn ForgeProvider>, ForgeError> {
        self.inner.for_repository(owner, name)
    }
    async fn ensure_named_repository(
        &self,
        owner: &str,
        name: &str,
        opts: &RepositoryOptions,
    ) -> Result<RepositoryInfo, ForgeError> {
        assert!(
            opts.private && opts.auto_init,
            "tenant repositories are private and seeded"
        );
        let description = opts.description.clone().unwrap_or_default();
        assert!(
            !description.contains("acme"),
            "generic description: {description}"
        );
        self.log.record(format!("ensure_repo {owner}/{name}"));
        let mut info = self
            .inner
            .ensure_named_repository(owner, name, opts)
            .await?;
        info.html_url = Some(format!("{FORGE_ORIGIN}/{owner}/{name}"));
        Ok(info)
    }
    async fn create_repo_read_token(
        &self,
        repo: &RepositoryTarget,
        name: &str,
    ) -> Result<RepoReadToken, ForgeError> {
        assert!(name.starts_with("keel-tenant-repo-"), "{name}");
        if self.log.fail_mint.load(Ordering::SeqCst) {
            return Err(ForgeError::Api {
                status: 503,
                message: "forge unavailable".to_string(),
            });
        }
        let n = self.log.minted.fetch_add(1, Ordering::SeqCst) + 1;
        self.log.record(format!("mint {repo} {name}"));
        self.log
            .live_tokens
            .lock()
            .unwrap()
            .push((n.to_string(), name.to_string()));
        Ok(RepoReadToken {
            id: format!("{n}"),
            name: name.to_string(),
            username: match self.kind {
                TenantRepoForgeKind::GitLab => format!("gitlab+deploy-token-{n}"),
                _ => "keel-tenant-reader".to_string(),
            },
            token: format!("s3cr3t-token-{n}"),
        })
    }
    async fn revoke_repo_read_token(
        &self,
        repo: &RepositoryTarget,
        token_id: &str,
    ) -> Result<(), ForgeError> {
        self.log.record(format!("revoke_token {repo} {token_id}"));
        self.log
            .revoked_tokens
            .lock()
            .unwrap()
            .push(token_id.to_string());
        self.log
            .live_tokens
            .lock()
            .unwrap()
            .retain(|(id, _)| id != token_id);
        Ok(())
    }
    async fn list_repo_read_tokens(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<Vec<RepoReadTokenInfo>, ForgeError> {
        Ok(self
            .log
            .live_tokens
            .lock()
            .unwrap()
            .iter()
            .map(|(id, name)| RepoReadTokenInfo {
                id: id.clone(),
                name: name.clone(),
            })
            .collect())
    }
    async fn ensure_deploy_key(
        &self,
        repo: &RepositoryTarget,
        title: &str,
        public_key: &str,
        read_only: bool,
    ) -> Result<EnsuredDeployKey, ForgeError> {
        assert!(read_only, "tenant deploy keys are read-only");
        self.log.record(format!("deploy_key {repo} {title}"));
        let mut keys = self.log.deploy_keys.lock().unwrap();
        let key = DeployKeyInfo {
            id: format!("k{}", self.log.key_seq.fetch_add(1, Ordering::SeqCst) + 100),
            title: title.to_string(),
            fingerprint: Some(ssh_public_key_fingerprint(public_key)?),
            read_only,
        };
        keys.push(key.clone());
        Ok(EnsuredDeployKey {
            key,
            outcome: DeployKeyOutcome::Created,
        })
    }
    async fn list_deploy_keys(
        &self,
        _repo: &RepositoryTarget,
    ) -> Result<Vec<DeployKeyInfo>, ForgeError> {
        Ok(self.log.deploy_keys.lock().unwrap().clone())
    }
    async fn revoke_deploy_key(
        &self,
        repo: &RepositoryTarget,
        key_id: &str,
    ) -> Result<(), ForgeError> {
        self.log.record(format!("revoke_key {repo} {key_id}"));
        self.log
            .deploy_keys
            .lock()
            .unwrap()
            .retain(|k| k.id != key_id);
        Ok(())
    }
    async fn ensure_main_protection(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<BranchProtectionOutcome, ForgeError> {
        self.log.record(format!("protect {repo}"));
        Ok(if self.log.protected.swap(true, Ordering::SeqCst) {
            BranchProtectionOutcome::Unchanged
        } else {
            BranchProtectionOutcome::Created
        })
    }
    async fn disable_forks(
        &self,
        repo: &RepositoryTarget,
    ) -> Result<ForkPolicyOutcome, ForgeError> {
        self.log.record(format!("forks {repo}"));
        Ok(ForkPolicyOutcome::AlreadyDisabled)
    }
    async fn ssh_host_keys(&self) -> Result<Vec<String>, ForgeError> {
        Ok(vec![HOST_KEY.to_string()])
    }
}

/// Secret backend whose writes of `keel-gitops-credentials` fail (Secret-write rollback).
#[derive(Clone, Default)]
struct FailingCredentialStore {
    inner: InMemorySecretStore,
}

#[async_trait]
impl SecretBackend for FailingCredentialStore {
    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>, KubeError> {
        self.inner.get_secret(namespace, name).await
    }
    async fn upsert_secret(&self, namespace: &str, secret: Secret) -> Result<Secret, KubeError> {
        if secret.metadata.name.as_deref() == Some("keel-gitops-credentials") {
            return Err(KubeError::InvalidInput("admission denied".to_string()));
        }
        self.inner.upsert_secret(namespace, secret).await
    }
    async fn create_secret_if_absent(
        &self,
        namespace: &str,
        secret: Secret,
    ) -> Result<Secret, KubeError> {
        if secret.metadata.name.as_deref() == Some("keel-gitops-credentials") {
            return Err(KubeError::InvalidInput("admission denied".to_string()));
        }
        self.inner.create_secret_if_absent(namespace, secret).await
    }
    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError> {
        SecretBackend::namespace_exists(&self.inner, namespace).await
    }
}

struct Fixture {
    _tmp: TempDir,
    state: AppState,
    store: Arc<InMemorySecretStore>,
    log: Arc<ForgeLog>,
    root: std::path::PathBuf,
}

async fn seed_platform_repo(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let bare = tmp.path().join("infra-gitops.git");
    let workdir = tmp.path().join("work");
    let git = GitRunner::new();
    git.init_bare(&bare).await.unwrap();
    git.clone(bare.to_str().unwrap(), &workdir).await.unwrap();
    tokio::fs::create_dir_all(workdir.join("tenants/default"))
        .await
        .unwrap();
    tokio::fs::write(
        workdir.join("tenants/default/rbac.yaml"),
        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: tenant-default\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        workdir.join("tenants/kustomization.yaml"),
        "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n- default/rbac.yaml\n",
    )
    .await
    .unwrap();
    git.add(&workdir, ["."]).await.unwrap();
    git.commit(
        &workdir,
        "chore: seed",
        &CommitAuthor::new("Seed", "seed@keel.local"),
    )
    .await
    .unwrap();
    git.push(&workdir, "origin", "HEAD:refs/heads/main")
        .await
        .unwrap();
    (bare, workdir)
}

fn clone_base(kind: TenantRepoForgeKind) -> TenantRepoCloneBase {
    match kind {
        TenantRepoForgeKind::GitHub => TenantRepoCloneBase::Ssh {
            host: "github.com".to_string(),
            port: 22,
        },
        _ => TenantRepoCloneBase::Https {
            origin: FORGE_ORIGIN.to_string(),
        },
    }
}

async fn fixture_with_store(
    kind: TenantRepoForgeKind,
    store: Arc<dyn SecretStateStore>,
    mem: Arc<InMemorySecretStore>,
) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, workdir) = seed_platform_repo(&tmp).await;
    let log = Arc::new(ForgeLog::default());
    // A deploy key someone else added to a (pre-created) tenant repository.
    log.deploy_keys.lock().unwrap().push(DeployKeyInfo {
        id: "stray-1".to_string(),
        title: "someone".to_string(),
        fingerprint: Some(ssh_public_key_fingerprint(STRAY_KEY).unwrap()),
        read_only: false,
    });
    let forge: Arc<dyn ForgeProvider> = Arc::new(MockForge {
        inner: LocalGitProvider::new(&bare),
        kind,
        log: log.clone(),
    });
    let repos = TenantRepos::dedicated(
        TenantRepoForge {
            naming: TenantRepoNaming {
                kind,
                owner: "keel-tenants".to_string(),
                monorepo_name: "infra-gitops".to_string(),
                clone_base: clone_base(kind),
            },
            ca_crt: (kind == TenantRepoForgeKind::Forgejo).then(|| CA.to_string()),
            admin: forge.clone(),
            token_minter: (kind == TenantRepoForgeKind::Forgejo).then(|| forge.clone()),
        },
        tmp.path().join("work-tenant-repos"),
    );
    let state = AppState::new(
        forge,
        store,
        TelemetryMux::default(),
        GitRunner::new(),
        &workdir,
        bare.to_string_lossy().to_string(),
        AuthPhase::OidcActive,
    )
    .with_tenant_repos(repos);
    let root = tmp.path().to_path_buf();
    Fixture {
        _tmp: tmp,
        state,
        store: mem,
        log,
        root,
    }
}

async fn fixture(kind: TenantRepoForgeKind) -> Fixture {
    let mem = Arc::new(InMemorySecretStore::new());
    fixture_with_store(kind, mem.clone(), mem).await
}

async fn admin_cookie(state: &AppState) -> String {
    let key = state
        .secret_store
        .get_or_create_session_key()
        .await
        .unwrap();
    let claims = SessionClaims {
        sub: "admin@keel.local".to_string(),
        email: Some("admin@keel.local".to_string()),
        name: Some("Keel Admin".to_string()),
        groups: vec!["keel-admins".to_string()],
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
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let cookie = admin_cookie(state).await;
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        raw,
    )
}

async fn create(state: &AppState, id: &str) -> (StatusCode, Value, String) {
    send(
        state,
        Method::POST,
        "/api/v1/tenants",
        Some(json!({ "id": id })),
    )
    .await
}

/// Creates a tenant and merges its platform stub proposal (the tenant then exists on `main`).
async fn create_and_merge(state: &AppState, id: &str) -> Value {
    let (status, body, _) = create(state, id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pr = body["proposal"]["id"].as_str().unwrap();
    let (status, merged, _) = send(
        state,
        Method::POST,
        &format!("/api/v1/tenants/management/proposals/{pr}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    body
}

fn git_show(repo: &std::path::Path, rev_path: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["show", rev_path])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn credentials(store: &InMemorySecretStore, tenant: &str) -> Option<Secret> {
    store
        .inspect_raw_secret(&format!("tenant-{tenant}"), "keel-gitops-credentials")
        .await
}

async fn wait_for_credentials(store: &InMemorySecretStore, tenant: &str) -> Secret {
    for _ in 0..200 {
        if let Some(s) = credentials(store, tenant).await {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("credentials Secret was not written after the merge");
}

fn data(secret: &Secret, key: &str) -> String {
    String::from_utf8(secret.data.as_ref().unwrap()[key].0.clone()).unwrap()
}

fn annotation(secret: &Secret, key: &str) -> String {
    secret.metadata.annotations.as_ref().unwrap()[key].clone()
}

#[tokio::test]
async fn forgejo_tenant_creation_provisions_repository_stub_and_credentials() {
    let fx = fixture(TenantRepoForgeKind::Forgejo).await;
    fx.store.set_namespace_present("tenant-acme", false).await;

    let (status, body, raw) = create(&fx.state, "acme").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo = &body["repository"];
    assert_eq!(repo["layout"], "dedicated");
    assert_eq!(repo["owner"], "keel-tenants");
    assert_eq!(repo["name"], "infra-gitops-tenant-acme");
    assert_eq!(
        repo["url"],
        "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-acme.git"
    );
    assert_eq!(repo["transport"], "https");
    assert_eq!(repo["created"], true);
    assert_eq!(repo["mainProtection"], "created");
    assert_eq!(repo["cluster"]["namespaceReady"], false);
    assert_eq!(repo["cluster"]["sopsAge"], "pendingNamespace");
    assert_eq!(repo["cluster"]["credential"], "pendingNamespace");
    assert!(raw.contains("removed") || repo["notes"].to_string().contains("deploy key"));
    // Nothing minted before the namespace exists; the foreign deploy key was removed.
    assert_eq!(fx.log.minted.load(Ordering::SeqCst), 0);
    assert!(fx.log.calls().iter().any(|c| c.ends_with("stray-1")));
    assert!(fx.log.deploy_keys.lock().unwrap().is_empty());

    // Platform PR: stub only, never a Secret.
    let branch = body["branch"].as_str().unwrap().to_string();
    assert!(branch.starts_with("keel/acme/create-tenant-"), "{branch}");
    let bare = fx.root.join("infra-gitops.git");
    let git_repo = git_show(&bare, &format!("{branch}:tenants/acme/git-repository.yaml")).unwrap();
    assert!(git_repo.contains("keel-tenants/infra-gitops-tenant-acme.git"));
    assert!(git_repo.contains("keel-gitops-credentials"));
    let sync = git_show(&bare, &format!("{branch}:tenants/acme/sync.yaml")).unwrap();
    assert!(sync.contains("keel-tenant-root"));
    assert!(sync.contains("tenant-acme-sa"));
    assert!(git_show(&bare, &format!("{branch}:tenants/acme/rbac.yaml")).is_some());
    let kustomization = git_show(&bare, &format!("{branch}:tenants/kustomization.yaml")).unwrap();
    for entry in [
        "acme/rbac.yaml",
        "acme/git-repository.yaml",
        "acme/sync.yaml",
        "default/rbac.yaml",
    ] {
        assert!(kustomization.contains(entry), "{kustomization}");
    }

    // Tenant repository skeleton PR (one PR per repository).
    let tenant_bare = fx.root.join("keel-tenants/infra-gitops-tenant-acme.git");
    let skeleton = &repo["skeletonProposal"];
    let skeleton_branch = skeleton["sourceBranch"].as_str().unwrap().to_string();
    assert!(skeleton_branch.starts_with("keel/acme/init-tenant-repo-"));
    let k = git_show(
        &tenant_bare,
        &format!("{skeleton_branch}:tenants/acme/kustomization.yaml"),
    )
    .unwrap();
    assert!(k.contains("resources: []"), "{k}");
    assert!(git_show(&tenant_bare, "main:tenants/acme/kustomization.yaml").is_none());
    // Each PR names the scope its routes live under (PR ids are per repository).
    assert_eq!(body["proposal"]["scope"], "management");
    assert_eq!(body["proposal"]["repository"]["label"], "platform");
    assert_eq!(skeleton["scope"], "acme");
    assert_eq!(skeleton["repository"]["label"], "tenant-acme");

    // Re-run before merge: resumes both PRs, reuses the repository.
    let (status, again, _) = create(&fx.state, "acme").await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["proposal"]["id"], body["proposal"]["id"]);
    assert_eq!(again["repository"]["created"], false);
    assert_eq!(again["repository"]["mainProtection"], "unchanged");
    assert_eq!(
        again["repository"]["skeletonProposal"]["id"],
        skeleton["id"]
    );

    // Merge the skeleton in the tenant repository through its tenant scope.
    let (status, merged, _) = send(
        &fx.state,
        Method::POST,
        &format!(
            "/api/v1/tenants/acme/proposals/{}/merge",
            skeleton["id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["proposal"]["repository"]["label"], "tenant-acme");
    assert!(git_show(&tenant_bare, "main:tenants/acme/kustomization.yaml").is_some());

    // Merge the stub; the namespace appears; the post-merge task writes the runtime Secrets.
    fx.store.set_namespace_present("tenant-acme", true).await;
    let pr_id = body["proposal"]["id"].as_str().unwrap();
    let (status, merged, _) = send(
        &fx.state,
        Method::POST,
        &format!("/api/v1/tenants/management/proposals/{pr_id}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let secret = wait_for_credentials(&fx.store, "acme").await;
    assert_eq!(data(&secret, "username"), "keel-tenant-reader");
    assert_eq!(data(&secret, "password"), "s3cr3t-token-1");
    assert_eq!(data(&secret, "ca.crt"), CA);
    assert_eq!(annotation(&secret, "keel.io/credential-id"), "1");
    let labels = secret.metadata.labels.as_ref().unwrap();
    assert_eq!(labels["keel.io/platform-owned"], "true");
    assert_eq!(labels["keel.io/secret-kind"], "tenant-gitops-credentials");
    assert!(
        fx.store.get_tenant_age_key("acme").await.unwrap().is_some(),
        "the sops-age key is mandatory"
    );

    // Re-run after merge: nothing new to propose, credential kept, nothing minted.
    let (status, done, raw) = create(&fx.state, "acme").await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert!(done["proposal"].is_null());
    assert_eq!(done["repository"]["cluster"]["credential"], "existed");
    assert_eq!(done["repository"]["cluster"]["sopsAge"], "existed");
    assert_eq!(fx.log.minted.load(Ordering::SeqCst), 1);
    assert!(
        !raw.contains("s3cr3t"),
        "credentials never leave the cluster: {raw}"
    );

    // Secret lost: re-run mints a fresh credential and revokes the one nobody holds any more.
    fx.store
        .remove_secret("tenant-acme", "keel-gitops-credentials")
        .await;
    let (_, again, _) = create(&fx.state, "acme").await;
    assert_eq!(again["repository"]["cluster"]["credential"], "created");
    assert_eq!(fx.log.minted.load(Ordering::SeqCst), 2);
    assert_eq!(
        *fx.log.revoked_tokens.lock().unwrap(),
        vec!["1".to_string()]
    );

    // Secret for another URL (tampered): rotated and the previous token revoked.
    let mut tampered = credentials(&fx.store, "acme").await.unwrap();
    tampered.metadata.annotations.as_mut().unwrap().insert(
        "keel.io/repo-url".to_string(),
        "https://evil.example/x.git".to_string(),
    );
    fx.store
        .upsert_secret("tenant-acme", tampered)
        .await
        .unwrap();
    let (_, rotated, _) = create(&fx.state, "acme").await;
    assert_eq!(rotated["repository"]["cluster"]["credential"], "rotated");
    assert_eq!(
        *fx.log.revoked_tokens.lock().unwrap(),
        vec!["1".to_string(), "2".to_string()]
    );
    assert_eq!(
        *fx.log.live_tokens.lock().unwrap(),
        vec![(
            "3".to_string(),
            "keel-tenant-repo-infra-gitops-tenant-acme".to_string()
        )]
    );
    let secret = credentials(&fx.store, "acme").await.unwrap();
    assert_eq!(annotation(&secret, "keel.io/credential-id"), "3");
}

#[tokio::test]
async fn credential_failures_fail_closed_and_roll_back() {
    // Minting fails: the request fails, no Secret is written; the repository is reused later.
    let fx = fixture(TenantRepoForgeKind::Forgejo).await;
    fx.log.fail_mint.store(true, Ordering::SeqCst);
    let (status, body, _) = create(&fx.state, "beta").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(credentials(&fx.store, "beta").await.is_none());
    fx.log.fail_mint.store(false, Ordering::SeqCst);
    let (status, body, _) = create(&fx.state, "beta").await;
    assert_eq!(status, StatusCode::OK, "resumes the open stub PR: {body}");
    assert_eq!(body["repository"]["created"], false);
    assert_eq!(body["repository"]["cluster"]["credential"], "created");

    // The Secret write fails: the freshly minted token is revoked again.
    let mem = Arc::new(InMemorySecretStore::new());
    let failing = Arc::new(FailingCredentialStore {
        inner: (*mem).clone(),
    });
    let fx = fixture_with_store(TenantRepoForgeKind::Forgejo, failing, mem).await;
    let (status, body, _) = create(&fx.state, "gamma").await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "{body}"
    );
    assert_eq!(fx.log.minted.load(Ordering::SeqCst), 1);
    assert_eq!(
        *fx.log.revoked_tokens.lock().unwrap(),
        vec!["1".to_string()]
    );
}

#[tokio::test]
async fn gitlab_tenant_uses_a_project_deploy_token() {
    let fx = fixture(TenantRepoForgeKind::GitLab).await;
    let (status, body, raw) = create(&fx.state, "web").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["repository"]["transport"], "https");
    assert_eq!(body["repository"]["cluster"]["credential"], "created");
    assert!(!raw.contains("s3cr3t"));
    let secret = credentials(&fx.store, "web").await.unwrap();
    assert_eq!(data(&secret, "username"), "gitlab+deploy-token-1");
    assert!(!secret.data.as_ref().unwrap().contains_key("ca.crt"));
}

#[tokio::test]
async fn github_tenant_uses_a_read_only_deploy_key_with_pinned_host_keys() {
    let fx = fixture(TenantRepoForgeKind::GitHub).await;
    let (status, body, raw) = create(&fx.state, "web").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["repository"]["transport"], "ssh");
    assert_eq!(
        body["repository"]["url"],
        "ssh://git@github.com/keel-tenants/infra-gitops-tenant-web.git"
    );
    assert!(!raw.contains("PRIVATE KEY"));
    let secret = credentials(&fx.store, "web").await.unwrap();
    assert!(data(&secret, "identity").contains("OPENSSH PRIVATE KEY"));
    assert_eq!(data(&secret, "identity"), data(&secret, "sshPrivateKey"));
    assert_eq!(
        data(&secret, "known_hosts"),
        format!("github.com {HOST_KEY}\n")
    );
    // Only the new read-only key remains on the repository (the stray key was retired).
    let keys = fx.log.deploy_keys.lock().unwrap().clone();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].read_only);
    assert_eq!(
        keys[0].fingerprint.as_deref(),
        Some(
            ssh_public_key_fingerprint(&data(&secret, "identity.pub"))
                .unwrap()
                .as_str()
        )
    );
    assert_eq!(annotation(&secret, "keel.io/credential-id"), keys[0].id);
}

#[tokio::test]
async fn existing_monorepo_tenant_directory_is_a_conflict() {
    let fx = fixture(TenantRepoForgeKind::Forgejo).await;
    let (status, body, _) = create(&fx.state, "default").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        fx.log.calls().is_empty(),
        "no forge mutation: {:?}",
        fx.log.calls()
    );
}

#[tokio::test]
async fn local_provider_keeps_tenants_in_the_platform_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, workdir) = seed_platform_repo(&tmp).await;
    let store = Arc::new(InMemorySecretStore::new());
    let state = AppState::new(
        Arc::new(LocalGitProvider::new(&bare)),
        store.clone(),
        TelemetryMux::default(),
        GitRunner::new(),
        &workdir,
        bare.to_string_lossy().to_string(),
        AuthPhase::OidcActive,
    );
    let (status, body, _) = create(&state, "acme").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["repository"]["layout"], "monorepo");
    assert_eq!(body["repository"]["cluster"]["credential"], "notApplicable");
    assert_eq!(body["repository"]["cluster"]["sopsAge"], "created");
    assert!(
        body["repository"]["notes"][0]
            .as_str()
            .unwrap()
            .contains("development only")
    );
    let branch = body["branch"].as_str().unwrap();
    assert!(git_show(&bare, &format!("{branch}:tenants/acme/rbac.yaml")).is_some());
    assert!(git_show(&bare, &format!("{branch}:tenants/acme/sync.yaml")).is_none());
    let (status, _, _) = create(&state, "default").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=Mallory", "-c", "user.email=m@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Clones `bare`, runs `edit` on `branch` (created from `main` when missing) and pushes it.
fn push_change(
    bare: &std::path::Path,
    branch: &str,
    path: &str,
    content: &str,
    tmp: &std::path::Path,
) {
    let work = tmp.join(format!("push-{}", uuid_like()));
    git(
        tmp,
        &["clone", bare.to_str().unwrap(), work.to_str().unwrap()],
    );
    let exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", &format!("origin/{branch}")])
        .current_dir(&work)
        .output()
        .unwrap()
        .status
        .success();
    if exists {
        git(&work, &["checkout", branch]);
    } else {
        git(&work, &["checkout", "-b", branch]);
    }
    let file = work.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, content).unwrap();
    git(&work, &["add", path]);
    git(&work, &["commit", "-m", "change"]);
    git(
        &work,
        &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
    );
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[tokio::test]
async fn concurrent_cluster_provisioning_keeps_exactly_one_live_credential() {
    for kind in [TenantRepoForgeKind::GitLab, TenantRepoForgeKind::Forgejo] {
        let fx = fixture(kind).await;
        let (status, body, _) = create(&fx.state, "web").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        fx.store
            .remove_secret("tenant-web", "keel-gitops-credentials")
            .await;
        // The post-merge task and a re-run of POST race on a lost Secret.
        let run = || {
            keel_server::complete_cluster_provisioning(
                fx.state.secret_store.as_ref(),
                &fx.state.tenant_repos,
                "web",
            )
        };
        let (a, b) = tokio::join!(run(), run());
        a.unwrap();
        b.unwrap();
        let secret = credentials(&fx.store, "web").await.unwrap();
        let live = fx.log.live_tokens.lock().unwrap().clone();
        assert_eq!(live.len(), 1, "{kind:?}: {live:?}");
        assert_eq!(live[0].0, annotation(&secret, "keel.io/credential-id"));
    }
}

#[tokio::test]
async fn deleted_tenant_credentials_are_revoked_and_its_repository_is_not_silently_adopted() {
    let fx = fixture(TenantRepoForgeKind::GitLab).await;
    fx.store.set_namespace_present("tenant-acme", false).await;
    let (status, body, _) = create(&fx.state, "acme").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    fx.store.set_namespace_present("tenant-acme", true).await;
    let pr = body["proposal"]["id"].as_str().unwrap();
    let (status, merged, _) = send(
        &fx.state,
        Method::POST,
        &format!("/api/v1/tenants/management/proposals/{pr}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    wait_for_credentials(&fx.store, "acme").await;
    // Wait until the post-merge task has finished (one live token, and it is in the Secret).
    for _ in 0..200 {
        if fx.log.live_tokens.lock().unwrap().len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // The tenant ships apps in its own repository.
    let tenant_bare = fx.root.join("keel-tenants/infra-gitops-tenant-acme.git");
    push_change(
        &tenant_bare,
        "main",
        "tenants/acme/apps/redis/values.yaml",
        "replicas: 1\n",
        &fx.root,
    );

    // Delete: the platform stub PR; merging it through Keel revokes every read credential.
    let (status, deleted, _) = send(&fx.state, Method::DELETE, "/api/v1/tenants/acme", None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert!(
        deleted["notes"].to_string().contains("archive"),
        "{deleted}"
    );
    let pr = deleted["proposal"]["id"].as_str().unwrap();
    let (status, merged, _) = send(
        &fx.state,
        Method::POST,
        &format!("/api/v1/tenants/management/proposals/{pr}/merge"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert!(
        fx.log.live_tokens.lock().unwrap().is_empty(),
        "{:?}",
        fx.log.live_tokens.lock().unwrap()
    );

    // `tenant-acme` still exists until `keel-tenants` prunes it: a credential repair in that
    // window must not mint a credential for the deleted tenant.
    let (status, refused, _) = send(
        &fx.state,
        Method::POST,
        "/api/v1/tenants/acme/credentials/repair",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.to_string().contains("being deleted"), "{refused}");
    assert!(
        fx.log.live_tokens.lock().unwrap().is_empty(),
        "{:?}",
        fx.log.live_tokens.lock().unwrap()
    );

    // `keel-tenants` prunes the namespace. Re-creating the id refuses the old content...
    fx.store.set_namespace_present("tenant-acme", false).await;
    // Proposal branches carry a seconds timestamp.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, refused, _) = create(&fx.state, "acme").await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.to_string().contains("apps/redis"), "{refused}");
    assert!(refused.to_string().contains("adoptExisting"), "{refused}");

    // ...unless the administrator adopts it explicitly.
    let (status, adopted, _) = send(
        &fx.state,
        Method::POST,
        "/api/v1/tenants",
        Some(json!({ "id": "acme", "adoptExisting": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{adopted}");
    assert_eq!(adopted["repository"]["created"], false);
    assert!(
        adopted["repository"]["notes"]
            .to_string()
            .contains("adoptExisting"),
        "{adopted}"
    );
}

#[tokio::test]
async fn a_tampered_open_creation_proposal_is_never_resumed() {
    let fx = fixture(TenantRepoForgeKind::Forgejo).await;
    fx.store.set_namespace_present("tenant-beta", false).await;
    let (status, body, _) = create(&fx.state, "beta").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let branch = body["branch"].as_str().unwrap().to_string();
    let first = body["proposal"]["id"].clone();

    // Someone pushes a different sync.yaml onto the open proposal branch.
    push_change(
        &fx.root.join("infra-gitops.git"),
        &branch,
        "tenants/beta/sync.yaml",
        "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: keel-tenant-root\n  namespace: tenant-beta\nspec:\n  path: ./\n",
        &fx.root,
    );
    // Proposal branches carry a seconds timestamp.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, again, _) = create(&fx.state, "beta").await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_ne!(again["proposal"]["id"], first);
    assert!(
        again["repository"]["notes"]
            .to_string()
            .contains("Ignored open pull request"),
        "{again}"
    );
    // The fresh proposal is resumed on the next run.
    let (status, resumed, _) = create(&fx.state, "beta").await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["proposal"]["id"], again["proposal"]["id"]);
}

#[tokio::test]
async fn a_remote_forge_without_credentials_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, workdir) = seed_platform_repo(&tmp).await;
    let state = AppState::new(
        Arc::new(LocalGitProvider::new(&bare)),
        Arc::new(InMemorySecretStore::new()),
        TelemetryMux::default(),
        GitRunner::new(),
        &workdir,
        bare.to_string_lossy().to_string(),
        AuthPhase::OidcActive,
    )
    .with_tenant_repos(TenantRepos::unavailable(
        "no forge token was found",
        tmp.path().join("cache"),
    ));
    let (status, body, _) = create(&state, "acme").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.to_string().contains("no forge token"), "{body}");
    let bare_has_branch = git_show(&bare, "main:tenants/acme/rbac.yaml").is_some();
    assert!(
        !bare_has_branch,
        "never falls back to the platform repository"
    );
}

#[tokio::test]
async fn ssh_credentials_without_a_public_key_are_rotated_not_kept() {
    let fx = fixture(TenantRepoForgeKind::GitHub).await;
    let (status, body, _) = create(&fx.state, "web").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut secret = credentials(&fx.store, "web").await.unwrap();
    let old_id = annotation(&secret, "keel.io/credential-id");
    secret.data.as_mut().unwrap().remove("identity.pub");
    fx.store.upsert_secret("tenant-web", secret).await.unwrap();
    let (_, again, _) = create(&fx.state, "web").await;
    assert_eq!(again["repository"]["cluster"]["credential"], "rotated");
    let secret = credentials(&fx.store, "web").await.unwrap();
    let keys = fx.log.deploy_keys.lock().unwrap().clone();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_ne!(keys[0].id, old_id);
    assert_eq!(annotation(&secret, "keel.io/credential-id"), keys[0].id);
}

#[tokio::test]
async fn repair_credentials_rotates_a_valid_credential_and_revokes_the_old_one() {
    for kind in [
        TenantRepoForgeKind::Forgejo,
        TenantRepoForgeKind::GitLab,
        TenantRepoForgeKind::GitHub,
    ] {
        let fx = fixture(kind).await;
        create_and_merge(&fx.state, "web").await;
        let before = credentials(&fx.store, "web").await.unwrap();
        let old_id = annotation(&before, "keel.io/credential-id");

        let (status, repaired, raw) = send(
            &fx.state,
            Method::POST,
            "/api/v1/tenants/web/credentials/repair",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{kind:?}: {repaired}");
        assert_eq!(repaired["id"], "web");
        assert_eq!(repaired["credential"], "rotated", "{kind:?}");
        assert_eq!(repaired["sopsAge"], "existed", "{kind:?}");
        assert_eq!(repaired["namespaceReady"], true);
        assert_eq!(
            repaired["repository"], "keel-tenants/infra-gitops-tenant-web",
            "{kind:?}"
        );
        // Never a secret value in the response.
        for key in ["password", "identity", "sshPrivateKey"] {
            if let Some(v) = before.data.as_ref().unwrap().get(key) {
                assert!(!raw.contains(&*String::from_utf8_lossy(&v.0)), "{kind:?}");
            }
        }
        assert!(!raw.contains("PRIVATE KEY"), "{kind:?}");

        let after = credentials(&fx.store, "web").await.unwrap();
        let new_id = annotation(&after, "keel.io/credential-id");
        assert_ne!(new_id, old_id, "{kind:?}");
        // Exactly the new credential is live on the repository.
        match kind {
            TenantRepoForgeKind::GitHub => {
                let keys = fx.log.deploy_keys.lock().unwrap().clone();
                assert_eq!(keys.len(), 1, "{keys:?}");
                assert_eq!(keys[0].id, new_id);
            }
            _ => {
                let live = fx.log.live_tokens.lock().unwrap().clone();
                assert_eq!(live.len(), 1, "{kind:?}: {live:?}");
                assert_eq!(live[0].0, new_id);
                assert!(
                    fx.log.revoked_tokens.lock().unwrap().contains(&old_id),
                    "{kind:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn repair_credentials_recreates_lost_secrets_and_refuses_pending_tenants() {
    let fx = fixture(TenantRepoForgeKind::Forgejo).await;
    create_and_merge(&fx.state, "web").await;
    fx.store
        .remove_secret("tenant-web", "keel-gitops-credentials")
        .await;
    fx.store.remove_secret("tenant-web", "sops-age").await;
    let (status, repaired, _) = send(
        &fx.state,
        Method::POST,
        "/api/v1/tenants/web/credentials/repair",
        None,
    )
    .await;
    if status == StatusCode::SERVICE_UNAVAILABLE
        && repaired.to_string().contains("age-keygen")
        && std::env::var_os("KEEL_REQUIRE_SOPS").is_none()
    {
        eprintln!("SKIPPED: age-keygen unavailable");
        return;
    }
    assert_eq!(status, StatusCode::OK, "{repaired}");
    assert_eq!(repaired["credential"], "created");
    assert_eq!(repaired["sopsAge"], "created");
    assert!(credentials(&fx.store, "web").await.is_some());
    assert_eq!(fx.log.live_tokens.lock().unwrap().len(), 1);

    // A tenant whose namespace does not exist yet (stub PR not merged) is a conflict.
    fx.store.set_namespace_present("tenant-new", false).await;
    let (status, body, _) = send(
        &fx.state,
        Method::POST,
        "/api/v1/tenants/new/credentials/repair",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(credentials(&fx.store, "new").await.is_none());

    // Invalid ids are rejected before any forge call.
    let calls = fx.log.calls().len();
    let (status, _, _) = send(
        &fx.state,
        Method::POST,
        "/api/v1/tenants/Bad_Id/credentials/repair",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fx.log.calls().len(), calls);
}

#[tokio::test]
async fn repair_credentials_is_admin_only_and_not_applicable_to_the_local_provider() {
    let fx = fixture(TenantRepoForgeKind::Forgejo).await;
    let key = fx
        .state
        .secret_store
        .get_or_create_session_key()
        .await
        .unwrap();
    let member = SessionClaims {
        sub: "dev@keel.local".to_string(),
        email: Some("dev@keel.local".to_string()),
        name: None,
        groups: vec!["tenant-web".to_string()],
        auth_phase: AuthPhase::OidcActive,
        exp: chrono::Utc::now().timestamp() + 3600,
    };
    let cookie = format!(
        "{SESSION_COOKIE_NAME}={}",
        encrypt_session_token(&key, &member).unwrap()
    );
    let res = build_router(fx.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tenants/web/credentials/repair")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(fx.log.calls().is_empty(), "{:?}", fx.log.calls());

    let tmp = tempfile::tempdir().unwrap();
    let (bare, workdir) = seed_platform_repo(&tmp).await;
    let local = AppState::new(
        Arc::new(LocalGitProvider::new(&bare)),
        Arc::new(InMemorySecretStore::new()),
        TelemetryMux::default(),
        GitRunner::new(),
        &workdir,
        bare.to_string_lossy().to_string(),
        AuthPhase::OidcActive,
    );
    let (status, body, _) = send(
        &local,
        Method::POST,
        "/api/v1/tenants/default/credentials/repair",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Local Git provider"), "{body}");
}
