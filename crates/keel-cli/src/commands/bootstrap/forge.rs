//! Stage 0 Git forge provisioning: in-cluster Forgejo (repo, tokens, Rauthy SSO) and external
//! repository creation. All forge REST calls (repository creation, Forgejo token minting,
//! access verification) go through `keel-git`'s `ForgeProvider` implementations
//! (`AGENTS.md` §3.5).

use std::time::Duration;

use keel_core::{ForgeKind, KEEL_SYSTEM_NAMESPACE, generate_admin_password};
use keel_git::{
    ForgeProvider, ForgejoGiteaProvider, GitHubProvider, GitLabProvider, RepositoryEnsureOutcome,
    parse_remote_repo_url,
};
use reqwest::Client;
use serde_json::json;
use zeroize::Zeroizing;

use super::kube::KubeTarget;
use super::{DEFAULT_TENANT_ID, apply_git_credentials_secret, apply_tenant_git_credentials_secret};
use crate::commands::CliError;

pub(crate) const FORGE_TOKEN_SECRET_NAME: &str = "forge-token";
/// Secret in `keel-system` storing the Stage 0 Forgejo admin `username`/`password`.
///
/// Deliberately not `forgejo-admin`: the Forgejo chart owns a Secret named `<release>-admin`,
/// and Helm refuses to adopt a pre-existing object with that name. The chart consumes this one
/// through `gitea.admin.existingSecret`, so the password never appears in Helm/Pulumi values.
pub(crate) const FORGEJO_ADMIN_SECRET_NAME: &str = "keel-forgejo-admin";
/// Stage 0 Forgejo admin username.
pub(crate) const FORGEJO_ADMIN_USERNAME: &str = "keel";

/// `keel-system/forge-token` Secret (key `token`) read by `keel serve` (laptop fallback and the
/// in-cluster `keel-server` Deployment's `KEEL_GIT_TOKEN`). Applied to the cluster only, never
/// written to Git.
pub(crate) fn render_forge_token_secret_yaml(token: &str) -> Result<String, CliError> {
    let secret_doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": FORGE_TOKEN_SECRET_NAME,
            "namespace": KEEL_SYSTEM_NAMESPACE,
            "labels": { "app.kubernetes.io/managed-by": "keel" },
        },
        "type": "Opaque",
        "stringData": { "token": token },
    });
    Ok(serde_yml::to_string(&secret_doc)?)
}

/// Applies [`render_forge_token_secret_yaml`].
pub(crate) async fn apply_forge_token_secret(
    target: KubeTarget<'_>,
    token: &str,
) -> Result<(), CliError> {
    target.apply(&render_forge_token_secret_yaml(token)?).await
}

/// Resolves the Stage 0 Forgejo admin password: `KEEL_FORGEJO_ADMIN_PASSWORD`, else the existing
/// `keel-system/keel-forgejo-admin` Secret (stable across re-runs), else a freshly generated one.
pub(crate) async fn resolve_forgejo_admin_password(target: KubeTarget<'_>) -> Zeroizing<String> {
    if let Some(pw) = std::env::var("KEEL_FORGEJO_ADMIN_PASSWORD")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Zeroizing::new(pw);
    }
    if let Some(pw) = target
        .read_secret_key(KEEL_SYSTEM_NAMESPACE, FORGEJO_ADMIN_SECRET_NAME, "password")
        .await
    {
        return Zeroizing::new(pw);
    }
    Zeroizing::new(format!(
        "{}{}",
        generate_admin_password(),
        generate_admin_password()
    ))
}

pub(crate) fn render_forgejo_admin_secret_yaml(password: &str) -> Result<String, CliError> {
    let doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": FORGEJO_ADMIN_SECRET_NAME,
            "namespace": KEEL_SYSTEM_NAMESPACE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/secret-kind": "forgejo-admin",
            },
        },
        "type": "Opaque",
        "stringData": {
            "username": FORGEJO_ADMIN_USERNAME,
            "password": password,
        },
    });
    Ok(serde_yml::to_string(&doc)?)
}

/// Registers the Rauthy OIDC auth source in Forgejo.
///
/// The client secret is streamed over `kubectl exec` **stdin** so it never appears in the host
/// process table or in Kubernetes API audit logs (which record exec argv). The in-container
/// script is a constant; the discovery URL is passed as a positional argument, never
/// interpolated.
async fn register_forgejo_oidc_source(
    target: KubeTarget<'_>,
    oidc_secret: &str,
    discovery_url: &str,
) -> Result<(), CliError> {
    const SCRIPT: &str = r#"IFS= read -r KEEL_OIDC_SECRET; exec env SSL_CERT_DIR=/etc/ssl/certs:/etc/ssl/keel-ca forgejo admin auth add-oauth --name rauthy --provider openidConnect --key forgejo --secret "$KEEL_OIDC_SECRET" --auto-discover-url "$1" --scopes "openid email profile groups""#;
    let mut last_err = String::new();
    for attempt in 1..=15 {
        let out = target
            .output_with_stdin(
                &[
                    "-n",
                    KEEL_SYSTEM_NAMESPACE,
                    "exec",
                    "-i",
                    "deploy/forgejo",
                    "--",
                    "sh",
                    "-c",
                    SCRIPT,
                    "keel-register-oidc",
                    discovery_url,
                ],
                format!("{oidc_secret}\n").as_bytes(),
            )
            .await?;
        if out.status.success() {
            tracing::info!(attempt, "Registered Rauthy OIDC SSO provider in Forgejo");
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.contains("already exists") {
            return Ok(());
        }
        last_err = stderr;
        tracing::debug!(attempt, error = %last_err, "Waiting for Rauthy OIDC discovery before registering in Forgejo");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(CliError::Execution(format!(
        "failed to register Rauthy OIDC SSO provider in Forgejo after retries: {last_err}"
    )))
}

/// Provisions `keel/infra-gitops`, API tokens, credential Secrets and (optionally) Rauthy SSO in
/// the in-cluster Forgejo. Returns the write-capable API token used for the initial push.
pub(crate) async fn provision_in_cluster_forgejo(
    target: KubeTarget<'_>,
    local_port: u16,
    forgejo_oidc_secret: Option<&str>,
    oidc_discovery_url: &str,
) -> Result<Zeroizing<String>, CliError> {
    let base = format!("http://127.0.0.1:{local_port}");
    let user = FORGEJO_ADMIN_USERNAME;
    let pass = resolve_forgejo_admin_password(target).await;
    let admin =
        ForgejoGiteaProvider::with_basic_auth(&base, user, "infra-gitops", user, pass.as_str())?;

    // 1. Ensure `keel/infra-gitops` exists.
    admin.ensure_repository(true).await?;

    // 2. Write-capable token for `keel serve` (stays in keel-system).
    let token = Zeroizing::new(
        admin
            .create_access_token(
                &format!("keel-stage0-{}", chrono::Utc::now().timestamp()),
                &["write:repository", "write:issue", "read:user"],
            )
            .await?,
    );
    apply_forge_token_secret(target, token.as_str()).await?;
    // API access is verified by the caller *after* the seed push: Forgejo answers 404 for
    // pull-request listings on a repository that has no commits yet.

    // 3. Read-only token for Flux pulls (flux-system and tenant-default).
    let ro_token = Zeroizing::new(
        admin
            .create_access_token(
                &format!("keel-flux-readonly-{}", chrono::Utc::now().timestamp()),
                &["read:repository"],
            )
            .await?,
    );
    apply_git_credentials_secret(target, user, &ro_token).await?;
    apply_tenant_git_credentials_secret(target, DEFAULT_TENANT_ID, user, &ro_token).await?;

    // 4. Rauthy SSO auth source.
    if let Some(oidc_secret) = forgejo_oidc_secret {
        let listing = target
            .output(&[
                "-n",
                KEEL_SYSTEM_NAMESPACE,
                "exec",
                "deploy/forgejo",
                "--",
                "forgejo",
                "admin",
                "auth",
                "list",
            ])
            .await;
        let already = listing.as_ref().is_ok_and(|o| o.contains("rauthy"));
        if !already {
            if let Err(err) = target
                .output(&[
                    "-n",
                    KEEL_SYSTEM_NAMESPACE,
                    "rollout",
                    "status",
                    "statefulset/rauthy",
                    "--timeout=120s",
                ])
                .await
            {
                tracing::warn!(error = %err, "Rauthy rollout not confirmed before Forgejo SSO registration");
            }
            if let Err(err) =
                register_forgejo_oidc_source(target, oidc_secret, oidc_discovery_url).await
            {
                tracing::warn!(
                    error = %err,
                    "Forgejo SSO via Rauthy is NOT configured; password login still works. Re-run `keel bootstrap init` once Rauthy is healthy"
                );
            }
        }
        warm_up_forgejo_oidc(&base).await;
    }
    Ok(token)
}

/// Warm up Forgejo's in-memory OIDC provider so the first browser login includes PKCE.
async fn warm_up_forgejo_oidc(base: &str) {
    let Ok(client) = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return;
    };
    let url = format!("{base}/user/oauth2/rauthy");
    for attempt in 1..=3 {
        match client.get(&url).send().await {
            Ok(resp) => {
                let has_pkce = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|loc| loc.contains("code_challenge="));
                if has_pkce {
                    return;
                }
            }
            Err(err) => tracing::debug!(attempt, error = %err, "Forgejo OIDC warm-up failed"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ============================================================================
// External repository provisioning
// ============================================================================

fn forgejo_base(api_url: Option<&str>, origin: &str) -> String {
    api_url
        .map(|u| {
            let t = u.trim_end_matches('/');
            t.strip_suffix("/api/v1").unwrap_or(t).to_string()
        })
        .unwrap_or_else(|| origin.to_string())
}

/// Builds the `keel-git` [`ForgeProvider`] for a remote repository.
pub(crate) fn build_remote_forge_provider(
    forge_kind: ForgeKind,
    repo_url: &str,
    api_url: Option<&str>,
    token: &str,
) -> Result<Option<Box<dyn ForgeProvider>>, CliError> {
    let slug = parse_remote_repo_url(repo_url)?;
    Ok(match forge_kind {
        ForgeKind::GitHub => {
            let base = api_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| {
                    if slug.host.eq_ignore_ascii_case("github.com") {
                        "https://api.github.com".to_string()
                    } else {
                        format!("{}/api/v3", slug.origin_url)
                    }
                });
            Some(Box::new(GitHubProvider::new(
                base,
                &slug.owner,
                &slug.repo,
                token,
            )?))
        }
        ForgeKind::GitLab => {
            let base = api_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| slug.origin_url.clone());
            Some(Box::new(GitLabProvider::new(
                base,
                &slug.project_path,
                token,
            )?))
        }
        ForgeKind::Forgejo | ForgeKind::Gitea => Some(Box::new(ForgejoGiteaProvider::new(
            forgejo_base(api_url, &slug.origin_url),
            &slug.owner,
            &slug.repo,
            token,
        )?)),
        ForgeKind::Local => None,
    })
}

/// Lists open PRs, retrying briefly on `NotFound`: Forgejo/Gitea flip a freshly pushed repository
/// from "empty" asynchronously and answer 404 for pull-request listings until they have.
async fn list_prs_with_settle_retry(
    forge: &dyn ForgeProvider,
) -> Result<Vec<keel_git::PullRequestInfo>, keel_git::ForgeError> {
    let mut delay = std::time::Duration::from_millis(500);
    for attempt in 1..=8 {
        match forge.list_open_pull_requests(None).await {
            Err(keel_git::ForgeError::NotFound(_)) if attempt < 8 => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(5));
            }
            other => return other,
        }
    }
    unreachable!("loop always returns on the final attempt")
}

/// Verifies forge API access (token scope / reachability) through the `ForgeProvider`; warns
/// with context instead of failing so a later manual fix is possible.
pub(crate) async fn verify_forge_access(
    forge_kind: ForgeKind,
    repo_url: &str,
    api_url: Option<&str>,
    token: &str,
) -> bool {
    match build_remote_forge_provider(forge_kind, repo_url, api_url, token) {
        Ok(Some(forge)) => match list_prs_with_settle_retry(forge.as_ref()).await {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!(
                    forge = ?forge_kind,
                    repo = repo_url,
                    error = %err,
                    "Forge API access check failed; `keel serve` will be unable to open PRs until the token/API URL is fixed"
                );
                false
            }
        },
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(forge = ?forge_kind, repo = repo_url, error = %err, "Could not build ForgeProvider for access check");
            false
        }
    }
}

/// Ensures an external repository exists (creating it when missing) and verifies API access.
/// Failures are reported with context; the caller decides whether they are fatal.
pub(crate) async fn ensure_external_repo_exists(
    forge_kind: ForgeKind,
    repo_url: &str,
    api_url: Option<&str>,
    token: &str,
) -> Result<(), CliError> {
    let Some(forge) = build_remote_forge_provider(forge_kind, repo_url, api_url, token)? else {
        return Ok(());
    };
    match forge.ensure_repository(true).await? {
        RepositoryEnsureOutcome::Created => {
            tracing::info!(repo = repo_url, forge = ?forge_kind, "Created remote GitOps repository")
        }
        RepositoryEnsureOutcome::Existed => {
            tracing::debug!(repo = repo_url, "Remote GitOps repository already exists")
        }
    }
    verify_forge_access(forge_kind, repo_url, api_url, token).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn forgejo_admin_secret_is_stored_in_keel_system() {
        let yaml = render_forgejo_admin_secret_yaml("p4ssw0rd-generated").unwrap();
        let doc: Value = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(doc["metadata"]["namespace"], KEEL_SYSTEM_NAMESPACE);
        assert_eq!(doc["metadata"]["name"], FORGEJO_ADMIN_SECRET_NAME);
        assert_eq!(doc["stringData"]["username"], FORGEJO_ADMIN_USERNAME);
        assert_eq!(doc["stringData"]["password"], "p4ssw0rd-generated");
    }

    #[test]
    fn forge_providers_cover_remote_forges_only() {
        for (kind, url) in [
            (ForgeKind::GitHub, "https://github.com/acme/infra.git"),
            (
                ForgeKind::GitLab,
                "https://gitlab.example.com/team/infra.git",
            ),
            (ForgeKind::Forgejo, "https://git.example.com/acme/infra.git"),
        ] {
            assert!(
                build_remote_forge_provider(kind, url, None, "t")
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            build_remote_forge_provider(ForgeKind::Local, "https://x.example/a/b.git", None, "t")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            forgejo_base(Some("https://g.example/api/v1/"), "x"),
            "https://g.example"
        );
    }

    async fn spawn(app: axum::Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, handle)
    }

    #[tokio::test]
    async fn ensure_external_repo_reports_forge_http_failures_with_context() {
        use axum::{http::StatusCode as S, routing::get};
        let app = axum::Router::new().route(
            "/api/v1/repos/{o}/{r}",
            get(|| async { S::INTERNAL_SERVER_ERROR }),
        );
        let (addr, handle) = spawn(app).await;
        let err = ensure_external_repo_exists(
            ForgeKind::Forgejo,
            &format!("http://{addr}/acme/infra.git"),
            Some(&format!("http://{addr}")),
            "tok",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("HTTP 500"), "{err}");
        handle.abort();
    }

    #[tokio::test]
    async fn ensure_external_repo_creates_missing_repo_through_forge_provider() {
        use axum::{
            http::StatusCode as S,
            routing::{get, post},
        };
        let created = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = created.clone();
        let app = axum::Router::new()
            .route("/api/v1/repos/{o}/{r}", get(|| async { S::NOT_FOUND }))
            .route(
                "/api/v1/orgs/{o}/repos",
                post(move || {
                    let flag = flag.clone();
                    async move {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        (S::CREATED, "{}")
                    }
                }),
            )
            .route(
                "/api/v1/repos/{o}/{r}/pulls",
                get(|| async { (S::OK, "[]") }),
            );
        let (addr, handle) = spawn(app).await;
        ensure_external_repo_exists(
            ForgeKind::Gitea,
            &format!("http://{addr}/acme/infra.git"),
            Some(&format!("http://{addr}/api/v1")),
            "tok",
        )
        .await
        .unwrap();
        assert!(created.load(std::sync::atomic::Ordering::SeqCst));
        handle.abort();

        // `local` forges have nothing to provision remotely.
        ensure_external_repo_exists(ForgeKind::Local, "https://x.example/a/b.git", None, "t")
            .await
            .unwrap();
    }
}
