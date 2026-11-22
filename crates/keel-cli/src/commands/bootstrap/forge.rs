//! Stage 0 Git forge provisioning: in-cluster Forgejo (repo, tokens, Rauthy SSO) and external
//! repository creation. All forge REST calls (repository creation, Forgejo token minting,
//! access verification) go through `keel-git`'s `ForgeProvider` implementations
//! (`AGENTS.md` §3.5).

use std::time::Duration;

use keel_core::{
    ForgeKind, KEEL_SYSTEM_NAMESPACE, TENANT_REPO_READER_SECRET_NAME, TENANT_REPO_READER_USERNAME,
    TENANT_REPO_READERS_TEAM, TENANT_REPO_TOKEN_NAME_PREFIX, TENANT_REPOS_ORG,
    generate_admin_password,
};
use keel_git::{
    AccessTokenInfo, ForgeProvider, ForgejoGiteaProvider, GitHubProvider, GitLabProvider,
    ProvisionOutcome, RepositoryEnsureOutcome, ServiceUserSpec, StrayCredentialReport, TeamSpec,
    parse_remote_repo_url,
};
use reqwest::Client;
use serde_json::json;
use zeroize::Zeroizing;

use super::apply_git_credentials_secret;
use super::kube::KubeTarget;
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

/// Scopes of the `keel-system/forge-token` API token used by `keel serve` / keel-server.
///
/// `write:organization` lets the server create per-tenant repositories in the
/// [`TENANT_REPOS_ORG`] organisation. The token owner (the Stage 0 admin) is a **site admin**,
/// and Forgejo treats a site admin as an owner of every organisation, so in practice this scope
/// lets the token change the teams, members and settings of every organisation on the instance
/// (including `tenant-readers`); Stage 0 re-verifies and reconciles the `keel-tenants` teams on
/// every run. Deliberately **not** `write:user` (it would also manage the admin's own keys and
/// settings) and not `write:admin`: per-repository read tokens are minted by the
/// [`TENANT_REPO_READER_USERNAME`] bot over basic auth instead (`SDD.md` §6.2).
pub(crate) const FORGE_TOKEN_SCOPES: &[&str] = &[
    "write:repository",
    "write:issue",
    "read:user",
    "write:organization",
];

/// Domain of the tenant-reader bot's email. `.invalid` is reserved (RFC 2606), so no identity
/// provider can issue or verify an address there, and Forgejo's OIDC account linking by email
/// (`oauth2_client.ACCOUNT_LINKING: auto`) can never sign anyone in as the bot.
pub(crate) const TENANT_REPO_READER_EMAIL_DOMAIN: &str = "users.noreply.invalid";

/// Generates the bot's email: `keel-tenant-reader-<32 random hex>@users.noreply.invalid`. The
/// random part is stored in the bot Secret so it stays stable across re-runs.
pub(crate) fn generate_tenant_reader_email() -> String {
    format!(
        "{TENANT_REPO_READER_USERNAME}-{}@{TENANT_REPO_READER_EMAIL_DOMAIN}",
        uuid::Uuid::new_v4().simple()
    )
}

/// `true` for an email produced by [`generate_tenant_reader_email`].
fn is_tenant_reader_email(email: &str) -> bool {
    email
        .strip_prefix(&format!("{TENANT_REPO_READER_USERNAME}-"))
        .and_then(|rest| rest.strip_suffix(&format!("@{TENANT_REPO_READER_EMAIL_DOMAIN}")))
        .is_some_and(|nonce| nonce.len() >= 16 && nonce.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The [`TENANT_REPOS_ORG`] team that gives the bot read-only **code** access to every current
/// and future tenant repository, and nothing else (no issues, pulls, wiki, releases, packages,
/// actions; no repository creation).
pub(crate) const TENANT_REPO_READERS_TEAM_SPEC: TeamSpec<'static> = TeamSpec {
    name: TENANT_REPO_READERS_TEAM,
    description: "Keel: read-only code access to every tenant repository for the keel-tenant-reader bot (owner of the per-repository read tokens)",
    permission: "read",
    units: &[("repo.code", "read")],
    includes_all_repositories: true,
    can_create_org_repo: false,
};

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
    Ok(keel_core::yaml::to_string(&secret_doc)?)
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
    Ok(keel_core::yaml::to_string(&doc)?)
}

/// Tenant-reader bot credentials kept in `keel-system/keel-forgejo-tenant-reader`.
#[derive(Clone)]
pub(crate) struct TenantReaderCredentials {
    pub(crate) password: Zeroizing<String>,
    pub(crate) email: String,
    /// Forgejo id of the account Keel created; `None` until Keel has created it. A pre-existing
    /// account is only adopted when its id matches.
    pub(crate) user_id: Option<i64>,
}

impl std::fmt::Debug for TenantReaderCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantReaderCredentials")
            .field("password", &"[REDACTED]")
            .field("email", &self.email)
            .field("user_id", &self.user_id)
            .finish()
    }
}

impl TenantReaderCredentials {
    /// Fresh credentials: random password and email, no recorded account.
    pub(crate) fn generate() -> Self {
        Self {
            password: Zeroizing::new(format!(
                "{}{}",
                generate_admin_password(),
                generate_admin_password()
            )),
            email: generate_tenant_reader_email(),
            user_id: None,
        }
    }

    /// Rebuilds credentials from the Secret's `password`, `email` and `userId` keys. Without a
    /// password everything is regenerated (the recorded id is meaningless without it); a missing
    /// or foreign email is regenerated.
    pub(crate) fn from_secret(
        password: Option<String>,
        email: Option<String>,
        user_id: Option<String>,
    ) -> Self {
        let Some(password) = password.filter(|p| !p.is_empty()) else {
            return Self::generate();
        };
        Self {
            password: Zeroizing::new(password),
            email: email
                .filter(|e| is_tenant_reader_email(e))
                .unwrap_or_else(generate_tenant_reader_email),
            user_id: user_id
                .and_then(|id| id.trim().parse::<i64>().ok())
                .filter(|id| *id > 0),
        }
    }
}

/// `keel-system/keel-forgejo-tenant-reader` Secret with the tenant-reader bot credentials
/// (`username`, `password`, `email`, and `userId` once Keel created the account).
/// Cluster-only; never written to Git, logged or returned by the API.
pub(crate) fn render_tenant_reader_secret_yaml(
    creds: &TenantReaderCredentials,
) -> Result<String, CliError> {
    let mut data = json!({
        "username": TENANT_REPO_READER_USERNAME,
        "password": creds.password.as_str(),
        "email": creds.email,
    });
    if let Some(id) = creds.user_id {
        data["userId"] = json!(id.to_string());
    }
    let doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": TENANT_REPO_READER_SECRET_NAME,
            "namespace": KEEL_SYSTEM_NAMESPACE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/secret-kind": "forgejo-tenant-reader",
            },
        },
        "type": "Opaque",
        "stringData": data,
    });
    Ok(keel_core::yaml::to_string(&doc)?)
}

/// Reads the bot credentials from their Secret (stable across re-runs), else generates them.
async fn resolve_tenant_reader_credentials(target: KubeTarget<'_>) -> TenantReaderCredentials {
    let read = |key: &'static str| {
        target.read_secret_key(KEEL_SYSTEM_NAMESPACE, TENANT_REPO_READER_SECRET_NAME, key)
    };
    TenantReaderCredentials::from_secret(
        read("password").await,
        read("email").await,
        read("userId").await,
    )
}

/// Name of the in-cluster platform repository (`keel/infra-gitops`).
const PLATFORM_REPO_NAME: &str = "infra-gitops";
/// Name prefix of the admin's `flux-system` platform read tokens (`<prefix><unix-millis>`).
const FLUX_READ_TOKEN_NAME_PREFIX: &str = "keel-flux-readonly-";

/// Ids of earlier `flux-system` platform read tokens to revoke once `keep_id` is stored.
pub(crate) fn stale_flux_read_tokens(tokens: &[AccessTokenInfo], keep_id: i64) -> Vec<i64> {
    tokens
        .iter()
        .filter(|t| t.id != keep_id && t.name.starts_with(FLUX_READ_TOKEN_NAME_PREFIX))
        .map(|t| t.id)
        .collect()
}

/// `true` for a token Keel minted as the bot: named `keel-tenant-repo-*`, only
/// `read:repository`, and restricted to repositories in [`TENANT_REPOS_ORG`]. Stage 0 deletes
/// every other token the bot owns.
pub(crate) fn is_keel_minted_tenant_token(token: &AccessTokenInfo) -> bool {
    let org_prefix = format!("{TENANT_REPOS_ORG}/");
    token.name.starts_with(TENANT_REPO_TOKEN_NAME_PREFIX)
        && !token.scopes.is_empty()
        && token.scopes.iter().all(|s| s == "read:repository")
        && token
            .repositories
            .as_ref()
            .is_some_and(|r| !r.is_empty() && r.iter().all(|r| r.starts_with(&org_prefix)))
}

/// What [`ensure_tenant_repo_foundations`] found, created or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TenantRepoFoundations {
    pub(crate) org: ProvisionOutcome,
    pub(crate) reader: ProvisionOutcome,
    pub(crate) team: ProvisionOutcome,
    /// Accounts removed from `tenant-readers` (anyone but the bot).
    pub(crate) removed_members: Vec<String>,
    /// Credentials of the bot Keel did not mint, now deleted.
    pub(crate) stray_credentials: StrayCredentialReport,
}

impl TenantRepoFoundations {
    pub(crate) fn note(&self) -> String {
        let mut note = format!(
            "Forgejo per-tenant repository foundations ready: private organisation `{TENANT_REPOS_ORG}` ({}), restricted bot user `{TENANT_REPO_READER_USERNAME}` ({}; credentials in Secret {KEEL_SYSTEM_NAMESPACE}/{TENANT_REPO_READER_SECRET_NAME}), read-only team `{TENANT_REPO_READERS_TEAM}` ({})",
            self.org.label(),
            self.reader.label(),
            self.team.label(),
        );
        if !self.removed_members.is_empty() {
            note.push_str(&format!(
                "; removed unexpected `{TENANT_REPO_READERS_TEAM}` members: {}",
                self.removed_members.join(", ")
            ));
        }
        let stray = &self.stray_credentials;
        if !stray.is_empty() {
            note.push_str(&format!(
                "; deleted bot credentials Keel did not create (tokens: [{}], SSH keys: {}, GPG keys: {}, OAuth2 apps: {}). Investigate how they were created",
                stray.tokens.join(", "),
                stray.ssh_keys,
                stray.gpg_keys,
                stray.oauth2_apps
            ));
        }
        note
    }
}

/// Lays the Forgejo foundations for per-tenant repositories (`SDD.md` §6.2), idempotently,
/// with the Stage 0 **admin** provider, failing closed on anything Keel does not control:
///
/// 1. private organisation [`TENANT_REPOS_ORG`]; its `Owners` team must be exactly the admin
///    (the `forge-token` owner) and no unmanaged team may reach any repository;
/// 2. restricted, non-admin bot [`TENANT_REPO_READER_USERNAME`]. An existing account is adopted
///    only when its id equals `creds.user_id` (recorded by `record_user_id` right after Keel
///    creates it), so a name registered by someone else is never adopted;
/// 3. team [`TENANT_REPO_READERS_TEAM_SPEC`] with exactly the bot as member;
/// 4. a basic-auth login as the bot (also fails if someone enabled 2FA on it);
/// 5. as the bot, deletion of every token not minted by Keel ([`is_keel_minted_tenant_token`])
///    and of all SSH/GPG keys and OAuth2 applications.
pub(crate) async fn ensure_tenant_repo_foundations(
    admin: &ForgejoGiteaProvider,
    base_url: &str,
    creds: &TenantReaderCredentials,
    mut record_user_id: impl AsyncFnMut(i64) -> Result<(), CliError>,
) -> Result<TenantRepoFoundations, CliError> {
    let org = admin
        .ensure_organization(
            TENANT_REPOS_ORG,
            "private",
            "Keel per-tenant GitOps repositories (managed by Keel)",
        )
        .await?;
    admin
        .verify_organization_control(
            TENANT_REPOS_ORG,
            &[FORGEJO_ADMIN_USERNAME],
            &[TENANT_REPO_READERS_TEAM],
        )
        .await?;
    let reader = admin
        .ensure_service_user(&ServiceUserSpec {
            username: TENANT_REPO_READER_USERNAME,
            email: &creds.email,
            full_name: "Keel tenant repository reader (bot)",
            password: creds.password.as_str(),
            adopt_id: creds.user_id,
        })
        .await?;
    if creds.user_id != Some(reader.id) {
        record_user_id(reader.id).await?;
    }
    let (team_id, team) = admin
        .ensure_team(TENANT_REPOS_ORG, &TENANT_REPO_READERS_TEAM_SPEC)
        .await?;
    let removed_members = admin
        .ensure_team_members_exactly(team_id, &[TENANT_REPO_READER_USERNAME])
        .await?;
    let bot = ForgejoGiteaProvider::with_basic_auth(
        base_url,
        TENANT_REPOS_ORG,
        "probe",
        TENANT_REPO_READER_USERNAME,
        creds.password.as_str(),
    )?;
    let login = bot.authenticated_login().await?;
    if !login.eq_ignore_ascii_case(TENANT_REPO_READER_USERNAME) {
        return Err(CliError::Execution(format!(
            "Forgejo authenticated the tenant-reader credentials as `{login}`, expected `{TENANT_REPO_READER_USERNAME}`"
        )));
    }
    let stray_credentials = bot
        .remove_stray_user_credentials(is_keel_minted_tenant_token)
        .await?;
    Ok(TenantRepoFoundations {
        org,
        reader: reader.outcome,
        team,
        removed_members,
        stray_credentials,
    })
}

/// Stores the bot Secret **before** touching Forgejo (so a crash never loses the password),
/// records the account id as soon as Keel creates it, then runs
/// [`ensure_tenant_repo_foundations`].
async fn provision_tenant_repo_foundations(
    target: KubeTarget<'_>,
    admin: &ForgejoGiteaProvider,
    base_url: &str,
) -> Result<TenantRepoFoundations, CliError> {
    let creds = resolve_tenant_reader_credentials(target).await;
    target
        .apply(&render_tenant_reader_secret_yaml(&creds)?)
        .await?;
    ensure_tenant_repo_foundations(admin, base_url, &creds, async |id| {
        let recorded = TenantReaderCredentials {
            user_id: Some(id),
            ..creds.clone()
        };
        target
            .apply(&render_tenant_reader_secret_yaml(&recorded)?)
            .await
    })
    .await
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

/// Provisions `keel/infra-gitops`, API tokens, credential Secrets, the per-tenant repository
/// foundations and (optionally) Rauthy SSO in the in-cluster Forgejo. Returns the write-capable
/// API token used for the initial push; operator-facing notes are appended to `notes`.
pub(crate) async fn provision_in_cluster_forgejo(
    target: KubeTarget<'_>,
    local_port: u16,
    forgejo_oidc_secret: Option<&str>,
    oidc_discovery_url: &str,
    notes: &mut Vec<String>,
) -> Result<Zeroizing<String>, CliError> {
    let base = format!("http://127.0.0.1:{local_port}");
    let user = FORGEJO_ADMIN_USERNAME;
    let pass = resolve_forgejo_admin_password(target).await;
    let admin = ForgejoGiteaProvider::with_basic_auth(
        &base,
        user,
        PLATFORM_REPO_NAME,
        user,
        pass.as_str(),
    )?;

    // 1. Ensure `keel/infra-gitops` exists.
    admin.ensure_repository(true).await?;

    // 2. Write-capable token for `keel serve` (stays in keel-system).
    let token = Zeroizing::new(
        admin
            .create_access_token(
                &format!("keel-stage0-{}", chrono::Utc::now().timestamp()),
                FORGE_TOKEN_SCOPES,
            )
            .await?,
    );
    apply_forge_token_secret(target, token.as_str()).await?;
    // API access is verified by the caller *after* the seed push: Forgejo answers 404 for
    // pull-request listings on a repository that has no commits yet.

    // 3. Read-only token for the platform `flux-system` source only. Tenants never receive it:
    // every tenant (tenant-default included) reads its own repository with a repository-scoped
    // credential.
    // Restricted to the platform repository (Forgejo 15 repository-specific token), so it cannot
    // read any tenant repository; earlier platform read tokens are revoked once it is stored.
    let platform_repo = keel_git::RepositoryTarget::new(user, PLATFORM_REPO_NAME)?;
    let minted = admin
        .create_repository_scoped_token(
            &format!(
                "{FLUX_READ_TOKEN_NAME_PREFIX}{}",
                chrono::Utc::now().timestamp_millis()
            ),
            &["read:repository"],
            std::slice::from_ref(&platform_repo),
        )
        .await?;
    let ro_token = Zeroizing::new(minted.token.clone());
    apply_git_credentials_secret(target, user, &ro_token).await?;
    let stale = stale_flux_read_tokens(&admin.list_access_tokens().await?, minted.id);
    for id in &stale {
        admin.delete_access_token(*id).await?;
    }
    if !stale.is_empty() {
        tracing::info!(
            revoked = stale.len(),
            "Revoked earlier flux-system platform read tokens"
        );
    }

    // 3b. Per-tenant repository foundations (org, bot user, read-only team). Every tenant,
    // tenant-default included, depends on them, so a failure aborts the bootstrap.
    let report = provision_tenant_repo_foundations(target, &admin, &base)
        .await
        .map_err(|err| {
            CliError::Execution(format!(
                "Forgejo per-tenant repository foundations (organisation `{TENANT_REPOS_ORG}`, bot `{TENANT_REPO_READER_USERNAME}`, team `{TENANT_REPO_READERS_TEAM}`) could not be set up: {err}. Re-run `keel bootstrap init` to retry"
            ))
        })?;
    notes.push(report.note());

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

pub(crate) fn forgejo_base(api_url: Option<&str>, origin: &str) -> String {
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
        let password = generate_admin_password();
        let yaml = render_forgejo_admin_secret_yaml(&password).unwrap();
        let doc: Value = noyalib::from_str(&yaml).unwrap();
        assert_eq!(doc["metadata"]["namespace"], KEEL_SYSTEM_NAMESPACE);
        assert_eq!(doc["metadata"]["name"], FORGEJO_ADMIN_SECRET_NAME);
        assert_eq!(doc["stringData"]["username"], FORGEJO_ADMIN_USERNAME);
        assert_eq!(doc["stringData"]["password"], password.as_str());
    }

    #[test]
    fn tenant_reader_secret_is_cluster_only_in_keel_system() {
        let creds = TenantReaderCredentials::generate();
        assert!(!format!("{creds:?}").contains(creds.password.as_str()));
        let doc: Value =
            noyalib::from_str(&render_tenant_reader_secret_yaml(&creds).unwrap()).unwrap();
        assert_eq!(doc["metadata"]["namespace"], KEEL_SYSTEM_NAMESPACE);
        assert_eq!(doc["metadata"]["name"], TENANT_REPO_READER_SECRET_NAME);
        assert_eq!(
            doc["metadata"]["labels"]["keel.io/secret-kind"],
            "forgejo-tenant-reader"
        );
        assert_eq!(doc["stringData"]["username"], TENANT_REPO_READER_USERNAME);
        assert_eq!(doc["stringData"]["password"], creds.password.as_str());
        assert_eq!(doc["stringData"]["email"], creds.email.as_str());
        assert!(doc["stringData"].get("userId").is_none());
        let recorded = TenantReaderCredentials {
            user_id: Some(7),
            ..creds
        };
        let doc: Value =
            noyalib::from_str(&render_tenant_reader_secret_yaml(&recorded).unwrap()).unwrap();
        assert_eq!(doc["stringData"]["userId"], "7");
    }

    #[test]
    fn tenant_reader_email_is_unguessable_and_unissuable() {
        let email = generate_tenant_reader_email();
        // `.invalid` is reserved: no IdP can issue or verify it, so OIDC account linking by
        // email never reaches the bot.
        assert!(email.ends_with(".invalid"), "{email}");
        assert!(email.ends_with("@users.noreply.invalid"), "{email}");
        assert!(email.starts_with("keel-tenant-reader-"), "{email}");
        assert!(is_tenant_reader_email(&email));
        assert_ne!(email, generate_tenant_reader_email());
        for bad in [
            "keel-tenant-reader@keel.local",
            "keel-tenant-reader-abc@users.noreply.invalid",
            "keel-tenant-reader-0123456789abcdef0123@example.com",
        ] {
            assert!(!is_tenant_reader_email(bad), "{bad}");
        }
    }

    #[test]
    fn tenant_reader_credentials_from_secret() {
        let email = generate_tenant_reader_email();
        let c = TenantReaderCredentials::from_secret(
            Some("pw".into()),
            Some(email.clone()),
            Some("12".into()),
        );
        assert_eq!(
            (c.password.as_str(), c.email.as_str(), c.user_id),
            ("pw", email.as_str(), Some(12))
        );
        // Foreign email regenerated; junk ids ignored.
        let c = TenantReaderCredentials::from_secret(
            Some("pw".into()),
            Some("keel-tenant-reader@keel.local".into()),
            Some("x".into()),
        );
        assert!(is_tenant_reader_email(&c.email));
        assert_eq!(c.user_id, None);
        // No password: nothing is trusted, not even a recorded id.
        let c = TenantReaderCredentials::from_secret(None, Some(email), Some("12".into()));
        assert_eq!(c.user_id, None);
        assert!(c.password.len() >= 32);
    }

    #[test]
    fn only_keel_minted_repository_tokens_survive() {
        let token = |name: &str, scopes: &[&str], repos: Option<&[&str]>| AccessTokenInfo {
            id: 1,
            name: name.to_string(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            repositories: repos.map(|r| r.iter().map(|s| s.to_string()).collect()),
        };
        assert!(is_keel_minted_tenant_token(&token(
            "keel-tenant-repo-acme",
            &["read:repository"],
            Some(&["keel-tenants/tenant-acme"])
        )));
        for stray in [
            token("mallory", &["read:repository"], Some(&["keel-tenants/a"])),
            token("keel-tenant-repo-x", &["read:repository"], None),
            token("keel-tenant-repo-x", &["all"], Some(&["keel-tenants/a"])),
            token("keel-tenant-repo-x", &[], Some(&["keel-tenants/a"])),
            token(
                "keel-tenant-repo-x",
                &["read:repository"],
                Some(&["keel/infra-gitops"]),
            ),
        ] {
            assert!(!is_keel_minted_tenant_token(&stray), "{stray:?}");
        }
        // keel-git refuses to mint tokens without the prefix Stage 0 keeps.
        assert_eq!(
            TENANT_REPO_TOKEN_NAME_PREFIX,
            keel_git::REPO_READ_TOKEN_NAME_PREFIX
        );
    }

    #[test]
    fn forge_token_can_manage_the_tenant_org_but_not_users_or_admin() {
        for scope in ["write:repository", "write:issue", "write:organization"] {
            assert!(FORGE_TOKEN_SCOPES.contains(&scope), "{scope}");
        }
        assert!(
            FORGE_TOKEN_SCOPES
                .iter()
                .all(|s| !s.ends_with(":admin") && *s != "write:user" && *s != "all"),
            "{FORGE_TOKEN_SCOPES:?}"
        );
        let team = &TENANT_REPO_READERS_TEAM_SPEC;
        assert_eq!(team.permission, "read");
        assert_eq!(team.units, &[("repo.code", "read")]);
        assert!(team.includes_all_repositories);
        assert!(!team.can_create_org_repo);
    }

    /// In-memory Forgejo for [`ensure_tenant_repo_foundations`]: records every request and
    /// keeps the org / user / team / membership / bot-credential state so re-runs can be checked.
    #[derive(Default)]
    struct FakeForgejo {
        log: Vec<String>,
        org_visibility: Option<String>,
        org_owners: Vec<String>,
        /// Unmanaged teams: (id, name, includes_all_repositories, repositories).
        other_teams: Vec<(i64, String, bool, Vec<String>)>,
        user_id: Option<i64>,
        user_password: Option<String>,
        user_email: Option<String>,
        user_hardened: bool,
        team_id: Option<i64>,
        team_body: Option<Value>,
        members: Vec<String>,
        bot_tokens: Vec<Value>,
        bot_ssh_keys: Vec<i64>,
    }

    type Shared = std::sync::Arc<std::sync::Mutex<FakeForgejo>>;

    fn fake_forgejo(state: Shared) -> axum::Router {
        use axum::{
            Json,
            extract::{Path, State},
            http::{HeaderMap, StatusCode as S},
            routing::{delete, get, patch, post, put},
        };
        type St = State<Shared>;
        fn basic_user(headers: &HeaderMap) -> Option<(String, String)> {
            use base64::Engine as _;
            let raw = headers.get("authorization")?.to_str().ok()?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(raw.strip_prefix("Basic ")?)
                .ok()?;
            let (u, p) = String::from_utf8(decoded)
                .ok()?
                .split_once(':')
                .map(|(u, p)| (u.to_string(), p.to_string()))?;
            Some((u, p))
        }
        axum::Router::new()
            .route(
                "/api/v1/orgs/{org}",
                get(|State(st): St, Path(org): Path<String>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push(format!("GET org {org}"));
                    match &s.org_visibility {
                        Some(v) => (
                            S::OK,
                            Json(json!({
                                "username": org,
                                "visibility": v,
                                "description": "Keel per-tenant GitOps repositories (managed by Keel)",
                                "repo_admin_change_team_access": false,
                            })),
                        ),
                        None => (S::NOT_FOUND, Json(json!({}))),
                    }
                })
                .patch(|State(st): St, Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("PATCH org".into());
                    s.org_visibility = body["visibility"].as_str().map(str::to_string);
                    (S::OK, Json(json!({})))
                }),
            )
            .route(
                "/api/v1/orgs",
                post(|State(st): St, Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("POST org".into());
                    assert_eq!(body["username"], TENANT_REPOS_ORG);
                    s.org_visibility = body["visibility"].as_str().map(str::to_string);
                    s.org_owners = vec![FORGEJO_ADMIN_USERNAME.to_string()];
                    (S::CREATED, Json(json!({"id": 1})))
                }),
            )
            .route(
                "/api/v1/users/{u}",
                get(|State(st): St, Path(u): Path<String>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push(format!("GET user {u}"));
                    match s.user_id {
                        Some(id) => (S::OK, Json(json!({"id": id, "login": u}))),
                        None => (S::NOT_FOUND, Json(json!({}))),
                    }
                }),
            )
            .route(
                "/api/v1/admin/users",
                post(|State(st): St, Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("POST user".into());
                    assert_eq!(body["username"], TENANT_REPO_READER_USERNAME);
                    assert_eq!(body["restricted"], true);
                    s.user_id = Some(2);
                    s.user_password = body["password"].as_str().map(str::to_string);
                    (S::CREATED, Json(json!({"id": 2, "login": TENANT_REPO_READER_USERNAME})))
                }),
            )
            .route(
                "/api/v1/admin/users/{u}",
                patch(|State(st): St, Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("PATCH user".into());
                    s.user_password = body["password"].as_str().map(str::to_string);
                    s.user_email = body["email"].as_str().map(str::to_string);
                    s.user_hardened = body["admin"] == false
                        && body["restricted"] == true
                        && body["max_repo_creation"] == 0
                        && body["must_change_password"] == false;
                    (S::OK, Json(json!({})))
                }),
            )
            .route(
                "/api/v1/orgs/{org}/teams",
                get(|State(st): St| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("GET teams".into());
                    let mut teams = vec![
                        json!({"id": 1, "name": "Owners", "permission": "owner", "includes_all_repositories": true}),
                    ];
                    if let Some(id) = s.team_id {
                        teams.push(json!({"id": id, "name": TENANT_REPO_READERS_TEAM, "permission": "read", "includes_all_repositories": true}));
                    }
                    for (id, name, all, _) in &s.other_teams {
                        teams.push(json!({"id": id, "name": name, "permission": "read", "includes_all_repositories": all}));
                    }
                    (S::OK, Json(Value::from(teams)))
                })
                .post(|State(st): St, Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("POST team".into());
                    s.team_id = Some(42);
                    s.team_body = Some(body);
                    (
                        S::CREATED,
                        Json(json!({"id": 42, "name": TENANT_REPO_READERS_TEAM})),
                    )
                }),
            )
            .route(
                "/api/v1/teams/{id}",
                patch(|State(st): St, Json(body): Json<Value>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("PATCH team".into());
                    s.team_body = Some(body);
                    (
                        S::OK,
                        Json(json!({"id": 42, "name": TENANT_REPO_READERS_TEAM})),
                    )
                }),
            )
            .route(
                "/api/v1/teams/{id}/members",
                get(|State(st): St, Path(id): Path<i64>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push(format!("GET members {id}"));
                    let logins = if id == 1 {
                        s.org_owners.clone()
                    } else {
                        s.members.clone()
                    };
                    let users: Vec<Value> =
                        logins.iter().map(|l| json!({"id": 9, "login": l})).collect();
                    Json(Value::from(users))
                }),
            )
            .route(
                "/api/v1/teams/{id}/repos",
                get(|State(st): St, Path(id): Path<i64>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push(format!("GET team repos {id}"));
                    let repos: Vec<Value> = s
                        .other_teams
                        .iter()
                        .filter(|t| t.0 == id)
                        .flat_map(|t| t.3.iter().map(|r| json!({"full_name": r})))
                        .collect();
                    Json(Value::from(repos))
                }),
            )
            .route(
                "/api/v1/teams/{id}/members/{u}",
                put(
                    |State(st): St, Path((id, u)): Path<(i64, String)>| async move {
                        let mut s = st.lock().unwrap();
                        s.log.push(format!("PUT member {id} {u}"));
                        if !s.members.contains(&u) {
                            s.members.push(u);
                        }
                        S::NO_CONTENT
                    },
                )
                .delete(
                    |State(st): St, Path((id, u)): Path<(i64, String)>| async move {
                        let mut s = st.lock().unwrap();
                        s.log.push(format!("DELETE member {id} {u}"));
                        s.members.retain(|m| *m != u);
                        S::NO_CONTENT
                    },
                ),
            )
            .route(
                "/api/v1/user",
                get(|State(st): St, headers: HeaderMap| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("GET self".into());
                    match basic_user(&headers) {
                        Some((u, p)) if Some(&p) == s.user_password.as_ref() => {
                            (S::OK, Json(json!({"id": 2, "login": u})))
                        }
                        _ => (S::UNAUTHORIZED, Json(json!({"message": "bad credentials"}))),
                    }
                }),
            )
            .route(
                "/api/v1/users/{u}/tokens",
                get(|State(st): St| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("GET bot tokens".into());
                    Json(Value::from(s.bot_tokens.clone()))
                }),
            )
            .route(
                "/api/v1/users/{u}/tokens/{id}",
                delete(
                    |State(st): St, Path((_u, id)): Path<(String, i64)>| async move {
                        let mut s = st.lock().unwrap();
                        s.log.push(format!("DELETE bot token {id}"));
                        s.bot_tokens.retain(|t| t["id"] != id);
                        S::NO_CONTENT
                    },
                ),
            )
            .route(
                "/api/v1/user/keys",
                get(|State(st): St| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push("GET bot keys".into());
                    let keys: Vec<Value> = s.bot_ssh_keys.iter().map(|id| json!({"id": id})).collect();
                    Json(Value::from(keys))
                }),
            )
            .route(
                "/api/v1/user/keys/{id}",
                delete(|State(st): St, Path(id): Path<i64>| async move {
                    let mut s = st.lock().unwrap();
                    s.log.push(format!("DELETE bot key {id}"));
                    s.bot_ssh_keys.retain(|k| *k != id);
                    S::NO_CONTENT
                }),
            )
            .route(
                "/api/v1/user/gpg_keys",
                get(|State(st): St| async move {
                    st.lock().unwrap().log.push("GET bot gpg keys".into());
                    Json(json!([]))
                }),
            )
            .route(
                "/api/v1/user/applications/oauth2",
                get(|State(st): St| async move {
                    st.lock().unwrap().log.push("GET bot oauth2 apps".into());
                    Json(json!([]))
                }),
            )
            .with_state(state)
    }

    fn admin_provider(base: &str) -> ForgejoGiteaProvider {
        ForgejoGiteaProvider::with_basic_auth(
            base,
            FORGEJO_ADMIN_USERNAME,
            "infra-gitops",
            FORGEJO_ADMIN_USERNAME,
            generate_admin_password(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn tenant_repo_foundations_are_created_then_idempotently_healed() {
        let state = Shared::default();
        let (addr, handle) = spawn(fake_forgejo(state.clone())).await;
        let base = format!("http://{addr}");
        let admin = admin_provider(&base);
        let mut creds = TenantReaderCredentials::generate();
        let mut recorded = Vec::new();

        // First run: everything created; the new account id is recorded immediately.
        let first = ensure_tenant_repo_foundations(&admin, &base, &creds, async |id| {
            recorded.push(id);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(recorded, vec![2]);
        assert_eq!(
            first,
            TenantRepoFoundations {
                org: ProvisionOutcome::Created,
                reader: ProvisionOutcome::Created,
                team: ProvisionOutcome::Created,
                removed_members: vec![],
                stray_credentials: StrayCredentialReport::default(),
            }
        );
        let note = first.note();
        assert!(note.contains(TENANT_REPOS_ORG), "{note}");
        assert!(note.contains(TENANT_REPO_READER_SECRET_NAME), "{note}");
        assert!(!note.contains(creds.password.as_str()));
        assert!(
            !note.contains("removed") && !note.contains("deleted"),
            "{note}"
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(s.org_visibility.as_deref(), Some("private"));
            assert!(s.user_hardened);
            assert_eq!(s.user_email.as_deref(), Some(creds.email.as_str()));
            assert_eq!(s.members, vec![TENANT_REPO_READER_USERNAME.to_string()]);
            let team = s.team_body.as_ref().unwrap();
            assert_eq!(team["units_map"], json!({"repo.code": "read"}));
            assert_eq!(team["includes_all_repositories"], true);
            assert_eq!(
                s.log,
                vec![
                    format!("GET org {TENANT_REPOS_ORG}"),
                    "POST org".to_string(),
                    "GET teams".to_string(),
                    "GET members 1".to_string(),
                    format!("GET user {TENANT_REPO_READER_USERNAME}"),
                    "POST user".to_string(),
                    "PATCH user".to_string(),
                    "GET teams".to_string(),
                    "POST team".to_string(),
                    "GET members 42".to_string(),
                    format!("PUT member 42 {TENANT_REPO_READER_USERNAME}"),
                    "GET self".to_string(),
                    "GET bot tokens".to_string(),
                    "GET bot keys".to_string(),
                    "GET bot gpg keys".to_string(),
                    "GET bot oauth2 apps".to_string(),
                ]
            );
        }

        // Re-run with the recorded id after drift (org made public): nothing re-created,
        // settings re-applied, no id re-recorded.
        creds.user_id = Some(2);
        {
            let mut s = state.lock().unwrap();
            s.log.clear();
            s.org_visibility = Some("public".into());
            s.user_hardened = false;
        }
        let second = ensure_tenant_repo_foundations(&admin, &base, &creds, async |_| {
            panic!("id already recorded")
        })
        .await
        .unwrap();
        assert_eq!(
            (second.org, second.reader, second.team),
            (
                ProvisionOutcome::Existed,
                ProvisionOutcome::Existed,
                ProvisionOutcome::Existed
            )
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(s.org_visibility.as_deref(), Some("private"));
            assert!(s.user_hardened);
            assert_eq!(s.members.len(), 1);
            assert!(s.log.contains(&"PATCH org".to_string()));
            assert!(s.log.contains(&"PATCH team".to_string()));
            assert!(!s.log.iter().any(|l| l.starts_with("POST")), "{:?}", s.log);
        }

        // The bot login check catches a password that does not match Forgejo...
        state.lock().unwrap().user_password = Some("rotated-elsewhere".into());
        let bot = ForgejoGiteaProvider::with_basic_auth(
            &base,
            TENANT_REPOS_ORG,
            "probe",
            TENANT_REPO_READER_USERNAME,
            creds.password.as_str(),
        )
        .unwrap();
        assert!(bot.authenticated_login().await.is_err());
        // ...but a normal re-run re-applies the stored password, so it heals.
        ensure_tenant_repo_foundations(&admin, &base, &creds, async |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            state.lock().unwrap().user_password.as_deref(),
            Some(creds.password.as_str())
        );
        handle.abort();
    }

    #[tokio::test]
    async fn tenant_repo_foundations_refuse_a_bot_keel_did_not_create() {
        // Someone registered `keel-tenant-reader` before Stage 0 (no id in the Secret).
        let state = Shared::default();
        {
            let mut s = state.lock().unwrap();
            s.user_id = Some(9);
            s.user_password = Some("attacker".into());
        }
        let (addr, handle) = spawn(fake_forgejo(state.clone())).await;
        let base = format!("http://{addr}");
        let creds = TenantReaderCredentials::generate();
        let err =
            ensure_tenant_repo_foundations(&admin_provider(&base), &base, &creds, async |_| {
                panic!("a foreign account must never be recorded")
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to adopt"), "{err}");
        let s = state.lock().unwrap();
        // Never patched (password, flags) and never added to the read-only team.
        assert!(!s.log.contains(&"PATCH user".to_string()), "{:?}", s.log);
        assert!(s.members.is_empty());
        assert_eq!(s.user_password.as_deref(), Some("attacker"));
        drop(s);
        handle.abort();
    }

    #[tokio::test]
    async fn tenant_repo_foundations_refuse_a_squatted_organisation() {
        let state = Shared::default();
        {
            let mut s = state.lock().unwrap();
            s.org_visibility = Some("private".into());
            s.org_owners = vec!["mallory".into(), FORGEJO_ADMIN_USERNAME.into()];
        }
        let (addr, handle) = spawn(fake_forgejo(state.clone())).await;
        let base = format!("http://{addr}");
        let creds = TenantReaderCredentials::generate();
        let err =
            ensure_tenant_repo_foundations(&admin_provider(&base), &base, &creds, async |_| Ok(()))
                .await
                .unwrap_err()
                .to_string();
        assert!(err.contains("mallory"), "{err}");
        // Fails before the bot or team exist.
        assert!(
            !state
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|l| l.starts_with("POST"))
        );

        // An unmanaged team that covers every repository is refused too.
        {
            let mut s = state.lock().unwrap();
            s.org_owners = vec![FORGEJO_ADMIN_USERNAME.into()];
            s.other_teams = vec![(7, "spies".into(), true, vec![])];
        }
        let err =
            ensure_tenant_repo_foundations(&admin_provider(&base), &base, &creds, async |_| Ok(()))
                .await
                .unwrap_err()
                .to_string();
        assert!(err.contains("spies"), "{err}");
        handle.abort();
    }

    #[test]
    fn earlier_flux_read_tokens_are_stale() {
        let token = |id: i64, name: &str| AccessTokenInfo {
            id,
            name: name.to_string(),
            scopes: vec!["read:repository".to_string()],
            repositories: None,
        };
        let tokens = [
            token(1, "keel-flux-readonly-1700000000"),
            token(2, "keel-flux-readonly-1700000000123"),
            token(3, "keel-stage0-1700000000"),
            token(4, "keel-flux-readonly-1800000000000"),
        ];
        assert_eq!(stale_flux_read_tokens(&tokens, 4), vec![1, 2]);
        assert!(stale_flux_read_tokens(&tokens[2..3], 4).is_empty());
    }

    #[tokio::test]
    async fn tenant_repo_foundations_remove_extra_members_and_stray_credentials() {
        let state = Shared::default();
        let (addr, handle) = spawn(fake_forgejo(state.clone())).await;
        let base = format!("http://{addr}");
        let admin = admin_provider(&base);
        let mut creds = TenantReaderCredentials::generate();
        ensure_tenant_repo_foundations(&admin, &base, &creds, async |_| Ok(()))
            .await
            .unwrap();
        creds.user_id = Some(2);
        {
            let mut s = state.lock().unwrap();
            s.members.push("mallory".into());
            s.bot_tokens = vec![
                json!({"id": 11, "name": "keel-tenant-repo-acme", "scopes": ["read:repository"], "repositories": [{"owner": TENANT_REPOS_ORG, "name": "tenant-acme"}]}),
                json!({"id": 12, "name": "stray", "scopes": ["all"]}),
            ];
            s.bot_ssh_keys = vec![5];
        }
        let report = ensure_tenant_repo_foundations(&admin, &base, &creds, async |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(report.removed_members, vec!["mallory".to_string()]);
        assert_eq!(report.stray_credentials.tokens, vec!["stray".to_string()]);
        assert_eq!(report.stray_credentials.ssh_keys, 1);
        let note = report.note();
        assert!(note.contains("mallory") && note.contains("stray"), "{note}");
        let s = state.lock().unwrap();
        assert_eq!(s.members, vec![TENANT_REPO_READER_USERNAME.to_string()]);
        assert_eq!(s.bot_tokens.len(), 1);
        assert_eq!(s.bot_tokens[0]["id"], 11);
        assert!(s.bot_ssh_keys.is_empty());
        drop(s);
        handle.abort();
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
            // The owner is resolved explicitly (organisation vs. token user) before creating.
            .route("/api/v1/orgs/{o}", get(|| async { (S::OK, r#"{"id":1}"#) }))
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
