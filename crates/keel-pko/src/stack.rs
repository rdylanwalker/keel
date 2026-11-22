//! PKO v2 `Stack` (`pulumi.com/v1`) and Flux CD `GitRepository` (`source.toolkit.fluxcd.io/v1`)
//! Custom Resource synthesis for management anti-lockout stacks, ephemeral PR previews,
//! and multi-tenant infrastructure stacks with companion output `Secret` bindings.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::PkoError;

/// Kubernetes API version for PKO v2 `Stack` resources.
pub const PKO_STACK_API_VERSION: &str = "pulumi.com/v1";
/// Kubernetes Kind for PKO v2 `Stack` resources.
pub const PKO_STACK_KIND: &str = "Stack";

/// Kubernetes API version for Flux CD `GitRepository` resources.
pub const FLUX_GIT_REPO_API_VERSION: &str = "source.toolkit.fluxcd.io/v1";
/// Kubernetes Kind for Flux CD `GitRepository` resources.
pub const FLUX_GIT_REPO_KIND: &str = "GitRepository";

/// Canonical resource name for the protected Stage 0/1 seed management cluster stack.
pub const MANAGEMENT_CORE_STACK_NAME: &str = "management-cluster-core";

/// Default in-cluster S3-compatible Pulumi state backend URL (`keel-system/s3-state-backend-seaweedfs-s3`).
pub const IN_CLUSTER_S3_BACKEND_URL: &str = "s3://pulumi-state?endpoint=s3-state-backend-seaweedfs-s3.keel-system.svc.cluster.local:8333&region=us-east-1&s3ForcePathStyle=true&disableSSL=true";

pub use keel_core::{FLUX_SYSTEM_NAMESPACE, KEEL_PREVIEWS_NAMESPACE};

/// Default outbound pull-polling interval for NAT/firewall resilience (`AGENTS.md` §3.1).
pub const DEFAULT_FLUX_POLL_INTERVAL: &str = "1m";
/// Default read-only ServiceAccount used by ephemeral preview workspace pods (`SDD.md` §4.2).
pub const DEFAULT_PREVIEW_SERVICE_ACCOUNT: &str = "keel-preview-readonly";
/// Default ServiceAccount used by the seed management cluster core stack.
pub const DEFAULT_MANAGEMENT_SERVICE_ACCOUNT: &str = "keel-management-pko";
/// 10-minute termination grace period for clean Pulumi state unlocks (`NFR-REL-04`).
pub const DEFAULT_TERMINATION_GRACE_PERIOD_SECONDS: u64 = 600;

/// Standard Kubernetes and Keel metadata label/annotation keys.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const LABEL_STACK_ROLE: &str = "keel.io/stack-role";
pub const LABEL_PROTECT: &str = "keel.io/protect";
/// `Stack` `spec.config` keys Keel also treats as anti-lockout protection (force-delete and
/// preview GC). Previews never inherit them, and tenants may not set them
/// (Kyverno `restrict-tenant-pulumi-objects`).
pub const CONFIG_PROTECT_KEYS: [&str; 2] = ["protect", "keel:protect"];
pub const LABEL_EPHEMERAL_PREVIEW: &str = "keel.io/ephemeral-preview";
pub const LABEL_PR_ID: &str = "keel.io/pr-id";
pub const LABEL_TENANT: &str = "keel.io/tenant";
pub const ANNOTATION_BRANCH: &str = "keel.io/branch";
/// Provider label enforced by Kyverno `restrict-pulumi-providers` on `tenant-*`/`keel-previews` Stacks.
pub const LABEL_PROVIDER: &str = "keel.io/provider";

/// Namespace-local `Secret` holding the Pulumi state encryption passphrase for every PKO `Stack`
/// in that namespace (`tenant-<id>`, `flux-system`, `keel-system`; per-preview copies in
/// `keel-previews`). Generated once (≥256-bit, URL-safe) and never rotated automatically.
pub const PULUMI_PASSPHRASE_SECRET_NAME: &str = "keel-pulumi-passphrase";
/// Key inside [`PULUMI_PASSPHRASE_SECRET_NAME`] holding the passphrase.
pub const PULUMI_PASSPHRASE_SECRET_KEY: &str = "passphrase";
/// Pulumi passphrase environment variable; only ever injected via `valueFrom.secretKeyRef`.
pub const PULUMI_CONFIG_PASSPHRASE_ENV: &str = "PULUMI_CONFIG_PASSPHRASE";
const PULUMI_CONFIG_PASSPHRASE_FILE_ENV: &str = "PULUMI_CONFIG_PASSPHRASE_FILE";
pub use keel_core::KEEL_CA_BUNDLE_CONFIGMAP;
/// Workspace volume holding the public Keel CA bundle.
pub const KEEL_CA_VOLUME_NAME: &str = "keel-ca";
/// Read-only mount path of [`KEEL_CA_VOLUME_NAME`] in the `fetch` and `pulumi` containers.
pub const KEEL_CA_MOUNT_PATH: &str = "/etc/keel-ca";
/// Go/OpenSSL CA directory list: the image's system roots plus the Keel CA bundle.
pub const KEEL_SSL_CERT_DIR: &str = "/etc/ssl/certs:/etc/keel-ca";
const SSL_CERT_DIR_ENV: &str = "SSL_CERT_DIR";
/// PKO v2 workspace init container that clones `projectRepo` (`/share/agent init --git-url`).
pub const PKO_FETCH_CONTAINER_NAME: &str = "fetch";

/// Standard Kubernetes `metadata` block for synthesized CRs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrObjectMeta {
    pub name: String,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// Local Kubernetes `Secret` or object reference (`{ name: ... }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalObjectReference {
    pub name: String,
}

/// Git reference selector for a Flux `GitRepository` CR.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FluxGitRepositoryRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Spec for a Flux CD `source.toolkit.fluxcd.io/v1` `GitRepository` CR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FluxGitRepositorySpec {
    pub interval: String,
    pub url: String,
    #[serde(rename = "ref")]
    pub git_ref: FluxGitRepositoryRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<LocalObjectReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// Typed Flux CD `source.toolkit.fluxcd.io/v1` `GitRepository` Custom Resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FluxGitRepository {
    pub api_version: String,
    pub kind: String,
    pub metadata: CrObjectMeta,
    pub spec: FluxGitRepositorySpec,
}

impl FluxGitRepository {
    /// Serializes this `GitRepository` CR to a `serde_json::Value`.
    pub fn to_json_value(&self) -> Result<serde_json::Value, PkoError> {
        Ok(serde_json::to_value(self)?)
    }

    /// Deserializes a `GitRepository` CR from a `serde_json::Value`.
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, PkoError> {
        Ok(serde_json::from_value(value)?)
    }

    /// Serializes this `GitRepository` CR to a YAML manifest string.
    pub fn to_yaml(&self) -> Result<String, PkoError> {
        Ok(serde_yml::to_string(self)?)
    }

    /// Deserializes a `GitRepository` CR from a YAML manifest string.
    pub fn from_yaml(yaml: &str) -> Result<Self, PkoError> {
        Ok(serde_yml::from_str(yaml)?)
    }
}

/// Flux source reference inside a PKO v2 `Stack` (`spec.fluxSource.sourceRef`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoFluxSourceRef {
    pub api_version: String,
    pub kind: String,
    pub name: String,
}

/// Flux source binding inside a PKO v2 `Stack` (`spec.fluxSource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoFluxSource {
    pub source_ref: PkoFluxSourceRef,
    pub dir: String,
}

impl PkoFluxSource {
    /// Creates a `PkoFluxSource` referencing a same-namespace Flux `GitRepository`.
    pub fn git_repository(name: impl Into<String>, dir: impl Into<String>) -> Self {
        Self {
            source_ref: PkoFluxSourceRef {
                api_version: FLUX_GIT_REPO_API_VERSION.to_string(),
                kind: FLUX_GIT_REPO_KIND.to_string(),
                name: name.into(),
            },
            dir: dir.into(),
        }
    }
}

/// Flux `GitRepository` Secret keys understood by [`PkoGitAuth::from_flux_secret`].
const FLUX_SECRET_IDENTITY_KEY: &str = "identity";
const FLUX_SECRET_USERNAME_KEY: &str = "username";
const FLUX_SECRET_PASSWORD_KEY: &str = "password";
const FLUX_SECRET_BEARER_TOKEN_KEY: &str = "bearerToken";

/// SSH private-key authentication for a PKO v2 Git source (`spec.gitAuth.sshAuth`).
///
/// Schema: PKO 2.10.1 `stacks.pulumi.com` CRD (chart
/// `oci://ghcr.io/pulumi/helm-charts/pulumi-kubernetes-operator` 2.10.1, `helm show crds`):
/// `sshAuth.sshPrivateKey` (required) and `sshAuth.password` (optional), both `ResourceRef`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoGitSshAuth {
    pub ssh_private_key: PkoEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<PkoEnvRef>,
}

/// Basic authentication for a PKO v2 Git source (`spec.gitAuth.basicAuth`); PKO 2.10.1 requires
/// both `userName` and `password` (`ResourceRef`s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoGitBasicAuth {
    pub user_name: PkoEnvRef,
    pub password: PkoEnvRef,
}

/// PKO v2 `spec.gitAuth` (PKO 2.10.1 CRD: `sshAuth`, `basicAuth`, `accessToken`, `githubApp`).
///
/// Every reference is a `{type: Secret, secret: {name, key}}` `ResourceRef` to a Secret in the
/// Stack's own namespace (PKO namespace isolation rejects a `secret.namespace`). PKO prefers SSH,
/// then the access token, then basic auth; Keel only ever sets one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoGitAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_auth: Option<PkoGitSshAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<PkoGitBasicAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<PkoEnvRef>,
}

impl PkoGitAuth {
    /// Maps a Flux `GitRepository` credentials Secret (`secretRef`) to PKO `gitAuth` referencing the
    /// same Secret (no copy): `identity` (+ optional `password` passphrase) becomes `sshAuth`,
    /// `username` + `password` becomes `basicAuth`, and a lone `password` or `bearerToken` becomes
    /// `accessToken`.
    ///
    /// `keys` are the keys present in the Secret. When unknown (empty), they are inferred from the
    /// URL the way Flux does: SSH URLs use `identity`, HTTP(S) URLs `username`/`password` (the
    /// layout of the platform-written read-only `git-credentials` Secret).
    pub fn from_flux_secret(
        secret_name: &str,
        repo_url: &str,
        keys: &BTreeSet<String>,
    ) -> Result<Self, PkoError> {
        validate_k8s_name("git_secret_name", secret_name)?;
        let ssh = is_ssh_git_url(repo_url);
        let inferred: BTreeSet<String>;
        let keys = if keys.is_empty() {
            inferred = if ssh {
                BTreeSet::from([FLUX_SECRET_IDENTITY_KEY.to_string()])
            } else {
                BTreeSet::from([
                    FLUX_SECRET_USERNAME_KEY.to_string(),
                    FLUX_SECRET_PASSWORD_KEY.to_string(),
                ])
            };
            &inferred
        } else {
            keys
        };
        let has = |k: &str| keys.contains(k);
        let secret_ref = |k: &str| PkoEnvRef::from_secret(secret_name, k);
        if has(FLUX_SECRET_IDENTITY_KEY) {
            return Ok(Self {
                ssh_auth: Some(PkoGitSshAuth {
                    ssh_private_key: secret_ref(FLUX_SECRET_IDENTITY_KEY),
                    password: has(FLUX_SECRET_PASSWORD_KEY)
                        .then(|| secret_ref(FLUX_SECRET_PASSWORD_KEY)),
                }),
                ..Self::default()
            });
        }
        if ssh {
            return Err(PkoError::Validation(format!(
                "Git credentials Secret '{secret_name}' has no `identity` key for SSH URL '{repo_url}'"
            )));
        }
        if has(FLUX_SECRET_USERNAME_KEY) && has(FLUX_SECRET_PASSWORD_KEY) {
            return Ok(Self {
                basic_auth: Some(PkoGitBasicAuth {
                    user_name: secret_ref(FLUX_SECRET_USERNAME_KEY),
                    password: secret_ref(FLUX_SECRET_PASSWORD_KEY),
                }),
                ..Self::default()
            });
        }
        for token_key in [FLUX_SECRET_PASSWORD_KEY, FLUX_SECRET_BEARER_TOKEN_KEY] {
            if has(token_key) {
                return Ok(Self {
                    access_token: Some(secret_ref(token_key)),
                    ..Self::default()
                });
            }
        }
        Err(PkoError::Validation(format!(
            "Git credentials Secret '{secret_name}' has no usable Git auth key (expected \
             `identity`, `username`+`password`, `password` or `bearerToken`)"
        )))
    }

    /// Names of the Secrets referenced by this `gitAuth`.
    pub fn secret_names(&self) -> BTreeSet<String> {
        let mut refs: Vec<&PkoEnvRef> = Vec::new();
        if let Some(ssh) = &self.ssh_auth {
            refs.push(&ssh.ssh_private_key);
            refs.extend(ssh.password.as_ref());
        }
        if let Some(basic) = &self.basic_auth {
            refs.push(&basic.user_name);
            refs.push(&basic.password);
        }
        refs.extend(self.access_token.as_ref());
        refs.into_iter().map(|r| r.secret.name.clone()).collect()
    }
}

/// PKO v2 Git source fields of a `Stack` spec (`projectRepo`, `branch` | `commit`, `repoDir`,
/// `gitAuth`), flattened into [`PkoStackSpec`]. The workspace pod clones `projectRepo` directly,
/// so tenant and preview Stacks never download a Flux artifact from `source-controller`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoGitSource {
    pub project_repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_auth: Option<PkoGitAuth>,
    /// PKO `spec.shallow`: shallow (depth 1) checkout instead of cloning the full history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shallow: Option<bool>,
}

impl PkoGitSource {
    /// Tracks `branch` of `project_repo` (PKO polls it every `resyncFrequencySeconds`) with a
    /// shallow checkout (`shallow: true`): workspaces only need the tip, not the repository history.
    pub fn branch(
        project_repo: impl Into<String>,
        branch: impl Into<String>,
        repo_dir: impl Into<String>,
    ) -> Self {
        Self {
            project_repo: project_repo.into(),
            branch: Some(branch.into()),
            commit: None,
            repo_dir: Some(repo_dir.into()),
            git_auth: None,
            shallow: Some(true),
        }
    }

    /// Sets `gitAuth`.
    pub fn with_auth(mut self, auth: Option<PkoGitAuth>) -> Self {
        self.git_auth = auth;
        self
    }

    /// Validates the source: a Git URL PKO can clone (`https://`, `ssh://` or scp-like
    /// `user@host:path`; never `http://`, a local path, `file://` or a git remote helper such as
    /// `ext::`), exactly one of `branch`/`commit`, a contained `repoDir` and well-formed Secret names.
    pub fn validate(&self) -> Result<(), PkoError> {
        validate_git_source_url(&self.project_repo)?;
        match (&self.branch, &self.commit) {
            (Some(b), None) => validate_git_ref_name("branch", b)?,
            (None, Some(c)) => {
                if c.len() < 7 || !c.chars().all(|ch| ch.is_ascii_hexdigit()) {
                    return Err(PkoError::Validation(format!(
                        "commit '{c}' must be a hexadecimal Git commit SHA"
                    )));
                }
            }
            _ => {
                return Err(PkoError::Validation(
                    "a PKO Git source needs exactly one of branch or commit".to_string(),
                ));
            }
        }
        if let Some(dir) = &self.repo_dir {
            validate_repo_dir(dir)?;
            if dir.starts_with('/') {
                return Err(PkoError::Validation(format!(
                    "repo_dir '{dir}' must be relative to the repository root"
                )));
            }
        }
        if let Some(auth) = &self.git_auth {
            for name in auth.secret_names() {
                validate_k8s_name("git_auth_secret", &name)?;
            }
        }
        Ok(())
    }
}

/// `true` for `ssh://` and scp-like (`user@host:path`) Git URLs.
pub fn is_ssh_git_url(url: &str) -> bool {
    let url = url.trim();
    if url.starts_with("ssh://") {
        return true;
    }
    !url.contains("://")
        && url
            .split_once(':')
            .is_some_and(|(host, _)| host.contains('@') && !host.contains('/'))
}

/// Accepts only Git URLs PKO v2 can clone (`spec.projectRepo`): `https://`, `ssh://` or scp-like
/// `user@host:path`. PKO (Pulumi SDK `gitutil.ParseGitRepoURL`) rejects plain `http://`, so an
/// in-cluster `http://` forge Service URL (fine for Flux) must first be mapped to the forge's
/// HTTPS origin with [`pko_project_repo_url`]. Rejects local paths, `file://`, git remote helpers
/// (`ext::`, `fd::`), option-like values, embedded credentials and whitespace/control characters.
pub fn validate_git_source_url(url: &str) -> Result<(), PkoError> {
    validate_network_git_url(url, false)
}

/// Like [`validate_git_source_url`] but also accepts `http://`: the URL schemes a Flux
/// `GitRepository` may use (e.g. the in-cluster Forgejo Service
/// [`IN_CLUSTER_FORGEJO_HTTP_HOST`]). Never put such a URL in a PKO Stack as is; map it with
/// [`pko_project_repo_url`].
pub fn validate_flux_git_source_url(url: &str) -> Result<(), PkoError> {
    validate_network_git_url(url, true)
}

/// Cluster DNS name of the Stage 0 in-cluster Forgejo HTTP Service. Flux `GitRepository`s pull
/// from it over plain `http://`; PKO Stacks clone the same repository through the Forgejo HTTPS
/// ingress origin instead (see [`pko_project_repo_url`]).
pub const IN_CLUSTER_FORGEJO_HTTP_HOST: &str = "forgejo-http.keel-system.svc.cluster.local";

/// `true` when `url` is an `http://` URL of the in-cluster Forgejo Service
/// (`forgejo-http.keel-system.svc[.cluster.local][:port]`).
pub fn is_in_cluster_forgejo_http_url(url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(authority, |(h, _)| h)
        .to_ascii_lowercase();
    host == IN_CLUSTER_FORGEJO_HTTP_HOST || host == "forgejo-http.keel-system.svc"
}

/// Derives the URL a tenant or preview PKO `Stack` clones (`spec.projectRepo`) from the tenant's
/// Flux `GitRepository` URL:
///
/// * `https://`, `ssh://` and scp-like URLs (external forges) pass through unchanged;
/// * the in-cluster Forgejo `http://` Service URL (Flux pulls over it; PKO rejects `http`) maps to
///   the platform-configured Forgejo HTTPS origin (`forge.publicUrl` in `.keel/config.yaml`, e.g.
///   `https://forgejo.keel.local:8888`, served by Envoy Gateway with the Keel CA) +
///   `/<org>/<repo>.git`;
/// * any other `http://` URL fails closed with an actionable error.
///
/// `forge_https_origin` is platform configuration, never a tenant-editable value.
pub fn pko_project_repo_url(
    flux_url: &str,
    forge_https_origin: Option<&str>,
) -> Result<String, PkoError> {
    validate_flux_git_source_url(flux_url)?;
    if validate_git_source_url(flux_url).is_ok() {
        return Ok(flux_url.to_string());
    }
    if !is_in_cluster_forgejo_http_url(flux_url) {
        return Err(PkoError::Validation(format!(
            "Git source URL '{flux_url}' uses plain http://, which PKO cannot clone (only \
             https://, ssh:// or user@host:path); point the tenant Flux GitRepository at the \
             forge's HTTPS or SSH URL"
        )));
    }
    let origin = forge_https_origin
        .map(|o| o.trim().trim_end_matches('/'))
        .filter(|o| !o.is_empty())
        .ok_or_else(|| {
            PkoError::Validation(format!(
                "Git source URL '{flux_url}' is the in-cluster Forgejo http:// Service, which PKO \
                 cannot clone, and no Forgejo HTTPS origin is configured; set `forge.publicUrl` \
                 (e.g. `https://forgejo.keel.local:8888`) in `.keel/config.yaml` (written by \
                 `keel bootstrap init --with-forge`) and restart keel-server"
            ))
        })?;
    keel_core::validate_https_origin(origin).map_err(|why| {
        PkoError::Validation(format!(
            "forge public URL '{origin}' {why}; expected an https://<host>[:port] origin"
        ))
    })?;
    let authority = origin.trim_start_matches("https://");
    let path = flux_url
        .trim()
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, path)| path)
        .unwrap_or_default();
    if path.is_empty() {
        return Err(PkoError::Validation(format!(
            "Git source URL '{flux_url}' has no repository path"
        )));
    }
    let url = format!("https://{authority}/{path}");
    validate_git_source_url(&url)?;
    Ok(url)
}

fn validate_network_git_url(url: &str, allow_http: bool) -> Result<(), PkoError> {
    // Never echo embedded credentials (`https://user:token@host/...`) into errors or logs.
    let shown = match url.split_once("://") {
        Some((scheme, rest)) if rest.split('/').next().unwrap_or_default().contains('@') => {
            let host = rest.split_once('@').map(|(_, h)| h).unwrap_or_default();
            format!("{scheme}://***@{host}")
        }
        _ => url.to_string(),
    };
    let accepted = if allow_http {
        "https://, http://, ssh:// or user@host:path"
    } else {
        "https://, ssh:// or user@host:path; PKO rejects http://"
    };
    let invalid = |why: &str| {
        Err(PkoError::Validation(format!(
            "Git source URL '{shown}' {why}; only network Git URLs are allowed ({accepted})"
        )))
    };
    if url.is_empty() || url.trim() != url {
        return invalid("is empty or has surrounding whitespace");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return invalid("contains whitespace or control characters");
    }
    if url.starts_with('-') || is_git_remote_helper_url(url) {
        return invalid("is not allowed");
    }
    let lower = url.to_ascii_lowercase();
    if let Some(rest) = ["https://", "http://"]
        .iter()
        .find_map(|scheme| lower.strip_prefix(scheme))
        && rest.split('/').next().unwrap_or_default().contains('@')
    {
        // Credentials belong in the `gitAuth` Secret, never in a committed URL.
        return invalid("embeds credentials");
    }
    if !allow_http && lower.starts_with("http://") {
        return invalid("uses plain http://");
    }
    let host_part = if let Some(rest) = ["https://", "http://", "ssh://"]
        .iter()
        .find_map(|scheme| lower.strip_prefix(scheme))
    {
        rest
    } else if is_ssh_git_url(url) {
        lower.split_once('@').map(|(_, h)| h).unwrap_or_default()
    } else {
        return invalid("is not a network Git URL");
    };
    let host = host_part
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if host.is_empty() {
        return invalid("has no host");
    }
    Ok(())
}

/// `true` for git remote-helper URLs (`<transport>::<address>`, e.g. `ext::`, `fd::`), which can
/// run arbitrary commands. Bracketed IPv6 hosts (`http://[::1]/...`) are not helpers.
fn is_git_remote_helper_url(url: &str) -> bool {
    url.split_once("::")
        .is_some_and(|(transport, _)| !transport.contains('/') && !transport.contains('['))
}

fn validate_git_ref_name(field: &str, value: &str) -> Result<(), PkoError> {
    let ok = !value.is_empty()
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with(".lock")
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok {
        Ok(())
    } else {
        Err(PkoError::Validation(format!(
            "{field} '{value}' is not a valid Git branch name"
        )))
    }
}

/// Secret key selector for environment variable injection in PKO workspace pods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeySelector {
    pub name: String,
    pub key: String,
}

/// Environment variable reference (`spec.envRefs`) for PKO v2 `Stack` CRs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoEnvRef {
    #[serde(rename = "type")]
    pub selector_type: String,
    pub secret: SecretKeySelector,
}

impl PkoEnvRef {
    /// References a key inside a namespace-local Kubernetes `Secret`.
    pub fn from_secret(secret_name: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            selector_type: "Secret".to_string(),
            secret: SecretKeySelector {
                name: secret_name.into(),
                key: key.into(),
            },
        }
    }
}

/// Seccomp profile configuration (`RuntimeDefault`) for `restricted` Pod Security Standards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeccompProfile {
    #[serde(rename = "type")]
    pub profile_type: String,
}

/// Linux capabilities configuration (`drop: ["ALL"]`) for `restricted` Pod Security Standards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub drop: Vec<String>,
}

/// Pod-level security context enforcing non-root execution (`NFR-SEC-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodSecurityContext {
    pub run_as_non_root: bool,
    pub run_as_user: u64,
    pub seccomp_profile: SeccompProfile,
}

/// Container-level security context enforcing `restricted` PSS (`NFR-SEC-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSecurityContext {
    pub allow_privilege_escalation: bool,
    pub read_only_root_filesystem: bool,
    pub capabilities: Capabilities,
}

/// Environment variable entry on a PKO workspace container (`corev1.EnvVar` subset): either a
/// literal `value` or a `valueFrom.secretKeyRef` to a namespace-local `Secret`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<WorkspaceEnvVarSource>,
}

impl WorkspaceEnvVar {
    /// Literal-valued environment variable.
    pub fn literal(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: Some(value.into()),
            value_from: None,
        }
    }

    /// Environment variable sourced from `secret_name`/`key` in the workspace pod's namespace.
    pub fn from_secret(
        name: impl Into<String>,
        secret_name: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: None,
            value_from: Some(WorkspaceEnvVarSource {
                secret_key_ref: SecretKeySelector {
                    name: secret_name.into(),
                    key: key.into(),
                },
            }),
        }
    }
}

/// `valueFrom` source for a workspace environment variable (`corev1.EnvVarSource` subset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEnvVarSource {
    pub secret_key_ref: SecretKeySelector,
}

/// Volume mount entry on a PKO workspace container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceVolumeMount {
    pub name: String,
    pub mount_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// Secret volume source for mounting namespace-local Secrets into a PKO workspace pod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSecretVolumeSource {
    pub secret_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

/// ConfigMap volume source for mounting namespace-local ConfigMaps into a PKO workspace pod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfigMapVolumeSource {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

/// Volume entry on a PKO workspace pod spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceVolume {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<WorkspaceSecretVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map: Option<WorkspaceConfigMapVolumeSource>,
}

/// Strategic-merge patch of an init container PKO itself adds to the workspace `StatefulSet`
/// (`bootstrap`, `fetch`), matched by `name`: only extra `env`/`volumeMounts` are set, image,
/// command and security context stay PKO's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInitContainerPatch {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<WorkspaceEnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<WorkspaceVolumeMount>,
}

/// Resource requests and limits for a PKO workspace container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceResourceRequirements {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requests: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, String>,
}

/// Container entry inside `WorkspacePodSpec` enforcing `restricted` PSS on the `pulumi` container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceContainer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<WorkspaceEnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<WorkspaceVolumeMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<WorkspaceResourceRequirements>,
    pub security_context: ContainerSecurityContext,
}

/// Workspace pod spec configuration for PKO v2 (`NFR-SEC-03`, `NFR-REL-04`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePodSpec {
    pub termination_grace_period_seconds: u64,
    pub security_context: PodSecurityContext,
    /// Patches of PKO's own workspace init containers (e.g. `fetch`, which clones `projectRepo`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init_containers: Vec<WorkspaceInitContainerPatch>,
    pub containers: Vec<WorkspaceContainer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<WorkspaceVolume>,
}

/// Pod template wrapper inside `PkoWorkspaceTemplate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePodTemplate {
    pub spec: WorkspacePodSpec,
}

/// Workspace spec inside `PkoWorkspaceTemplate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoWorkspaceTemplateSpec {
    pub pod_template: WorkspacePodTemplate,
}

/// PKO v2 `workspaceTemplate` enforcing `restricted` Pod Security Standards and graceful state unlocking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoWorkspaceTemplate {
    pub spec: PkoWorkspaceTemplateSpec,
}

impl PkoWorkspaceTemplate {
    /// Returns the hardened workspace template compliant with `NFR-SEC-03` and `NFR-REL-04`.
    ///
    /// `PULUMI_CONFIG_PASSPHRASE` is always sourced from `passphrase_secret_name`
    /// (key [`PULUMI_PASSPHRASE_SECRET_KEY`]) in the Stack's namespace via `secretKeyRef`, so
    /// Pulumi `secret: true` config and outputs are encrypted with a per-namespace key and the
    /// workspace pod fails to start (rather than falling back to an empty passphrase) when the
    /// Secret is missing.
    pub fn restricted(passphrase_secret_name: &str) -> Self {
        Self {
            spec: PkoWorkspaceTemplateSpec {
                pod_template: WorkspacePodTemplate {
                    spec: WorkspacePodSpec {
                        termination_grace_period_seconds: DEFAULT_TERMINATION_GRACE_PERIOD_SECONDS,
                        security_context: PodSecurityContext {
                            run_as_non_root: true,
                            run_as_user: 1000,
                            seccomp_profile: SeccompProfile {
                                profile_type: "RuntimeDefault".to_string(),
                            },
                        },
                        init_containers: Vec::new(),
                        containers: vec![WorkspaceContainer {
                            name: "pulumi".to_string(),
                            env: vec![WorkspaceEnvVar::from_secret(
                                PULUMI_CONFIG_PASSPHRASE_ENV,
                                passphrase_secret_name,
                                PULUMI_PASSPHRASE_SECRET_KEY,
                            )],
                            volume_mounts: Vec::new(),
                            // Explicit limits override the tenant LimitRange 1Gi default so heavy
                            // providers like azure-native do not OOMKill mid-update and leave locks.
                            resources: Some(WorkspaceResourceRequirements {
                                requests: BTreeMap::from([
                                    ("cpu".to_string(), "100m".to_string()),
                                    ("memory".to_string(), "256Mi".to_string()),
                                ]),
                                limits: BTreeMap::from([
                                    ("cpu".to_string(), "2".to_string()),
                                    ("memory".to_string(), "4Gi".to_string()),
                                ]),
                            }),
                            security_context: ContainerSecurityContext {
                                allow_privilege_escalation: false,
                                read_only_root_filesystem: false,
                                capabilities: Capabilities {
                                    drop: vec!["ALL".to_string()],
                                },
                            },
                        }],
                        volumes: Vec::new(),
                    },
                },
            },
        }
    }

    /// Appends a literal environment variable to the `pulumi` workspace container.
    ///
    /// The Pulumi passphrase variables are reserved (Secret-sourced only) and are never
    /// overridden with a literal value.
    pub fn with_env_var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        if name == PULUMI_CONFIG_PASSPHRASE_ENV || name == PULUMI_CONFIG_PASSPHRASE_FILE_ENV {
            return self;
        }
        if let Some(container) = self
            .spec
            .pod_template
            .spec
            .containers
            .iter_mut()
            .find(|c| c.name == "pulumi")
        {
            container.env.push(WorkspaceEnvVar::literal(name, value));
        }
        self
    }

    /// Returns the `Secret` reference `PULUMI_CONFIG_PASSPHRASE` is sourced from, if any.
    pub fn passphrase_secret_ref(&self) -> Option<&SecretKeySelector> {
        self.spec
            .pod_template
            .spec
            .containers
            .iter()
            .find(|c| c.name == "pulumi")?
            .env
            .iter()
            .find(|e| e.name == PULUMI_CONFIG_PASSPHRASE_ENV)?
            .value_from
            .as_ref()
            .map(|v| &v.secret_key_ref)
    }

    /// Mounts a namespace-local Kubernetes `Secret` read-only into the `pulumi` workspace container.
    pub fn with_secret_volume(
        mut self,
        volume_name: impl Into<String>,
        secret_name: impl Into<String>,
        mount_path: impl Into<String>,
    ) -> Self {
        let volume_name = volume_name.into();
        self.spec.pod_template.spec.volumes.push(WorkspaceVolume {
            name: volume_name.clone(),
            secret: Some(WorkspaceSecretVolumeSource {
                secret_name: secret_name.into(),
                optional: Some(true),
            }),
            config_map: None,
        });
        if let Some(container) = self
            .spec
            .pod_template
            .spec
            .containers
            .iter_mut()
            .find(|c| c.name == "pulumi")
        {
            container.volume_mounts.push(WorkspaceVolumeMount {
                name: volume_name,
                mount_path: mount_path.into(),
                read_only: Some(true),
            });
        }
        self
    }
}

impl PkoWorkspaceTemplate {
    /// Makes the workspace trust the platform (Keel) CA in addition to the system roots, so the
    /// `fetch` init container (go-git clone of `projectRepo`) and the `pulumi` container reach the
    /// in-cluster Forgejo through its HTTPS ingress (`https://forgejo.<domain>`, signed by the
    /// self-signed Keel CA):
    ///
    /// * volume [`KEEL_CA_VOLUME_NAME`] from the namespace-local **public** CA ConfigMap
    ///   [`KEEL_CA_BUNDLE_CONFIGMAP`] (key `ca.crt`), `optional: true` so a namespace without it
    ///   (external forge with a public CA) still starts and simply uses the system roots;
    /// * mounted read-only at [`KEEL_CA_MOUNT_PATH`] into `fetch` (a strategic-merge patch of PKO's
    ///   own init container, matched by name) and `pulumi`;
    /// * `SSL_CERT_DIR=/etc/ssl/certs:/etc/keel-ca` on both (Go reads every PEM in those dirs).
    ///
    /// Never mounts `keel-ca-secret` / `keel-wildcard-tls` (they hold private keys). Idempotent.
    pub fn with_keel_ca_trust(mut self) -> Self {
        let spec = &mut self.spec.pod_template.spec;
        if !spec.volumes.iter().any(|v| v.name == KEEL_CA_VOLUME_NAME) {
            spec.volumes.push(WorkspaceVolume {
                name: KEEL_CA_VOLUME_NAME.to_string(),
                secret: None,
                config_map: Some(WorkspaceConfigMapVolumeSource {
                    name: KEEL_CA_BUNDLE_CONFIGMAP.to_string(),
                    optional: Some(true),
                }),
            });
        }
        let mount = || WorkspaceVolumeMount {
            name: KEEL_CA_VOLUME_NAME.to_string(),
            mount_path: KEEL_CA_MOUNT_PATH.to_string(),
            read_only: Some(true),
        };
        let env = || WorkspaceEnvVar::literal(SSL_CERT_DIR_ENV, KEEL_SSL_CERT_DIR);
        let fetch = match spec
            .init_containers
            .iter_mut()
            .find(|c| c.name == PKO_FETCH_CONTAINER_NAME)
        {
            Some(c) => c,
            None => {
                spec.init_containers.push(WorkspaceInitContainerPatch {
                    name: PKO_FETCH_CONTAINER_NAME.to_string(),
                    env: Vec::new(),
                    volume_mounts: Vec::new(),
                });
                spec.init_containers.last_mut().expect("just pushed")
            }
        };
        fetch.env.retain(|e| e.name != SSL_CERT_DIR_ENV);
        fetch.env.push(env());
        fetch
            .volume_mounts
            .retain(|m| m.name != KEEL_CA_VOLUME_NAME);
        fetch.volume_mounts.push(mount());
        if let Some(pulumi) = spec.containers.iter_mut().find(|c| c.name == "pulumi") {
            pulumi.env.retain(|e| e.name != SSL_CERT_DIR_ENV);
            pulumi.env.push(env());
            pulumi
                .volume_mounts
                .retain(|m| m.name != KEEL_CA_VOLUME_NAME);
            pulumi.volume_mounts.push(mount());
        }
        self
    }
}

/// Spec for a PKO v2 `pulumi.com/v1` `Stack` CR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoStackSpec {
    pub stack: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_account_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flux_source: Option<PkoFluxSource>,
    /// PKO Git source (`projectRepo`/`branch`/`commit`/`repoDir`/`gitAuth`), used by tenant and
    /// preview Stacks instead of `fluxSource` (only the `flux-system` management Stack keeps that).
    #[serde(flatten)]
    pub git_source: Option<PkoGitSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destroy_on_finalize: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_resync_on_commit_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resync_frequency_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_secret_name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets_ref: BTreeMap<String, PkoEnvRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_template: Option<PkoWorkspaceTemplate>,
}

/// Flux `HelmRelease` `spec.valuesFrom` Secret reference for consuming companion `PkoStack` outputs (`SDD.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelmValuesFromRef {
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

/// Typed PKO v2 `pulumi.com/v1` `Stack` Custom Resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkoStack {
    pub api_version: String,
    pub kind: String,
    pub metadata: CrObjectMeta,
    pub spec: PkoStackSpec,
}

impl PkoStack {
    /// Serializes this `Stack` CR to a `serde_json::Value`.
    pub fn to_json_value(&self) -> Result<serde_json::Value, PkoError> {
        Ok(serde_json::to_value(self)?)
    }

    /// Deserializes a `Stack` CR from a `serde_json::Value`.
    ///
    /// A spec carrying `projectRepo` must parse into a complete [`PkoGitSource`]: the flattened
    /// `Option` would otherwise silently drop a malformed Git source (e.g. a bad `gitAuth`).
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, PkoError> {
        let has_project_repo = value.pointer("/spec/projectRepo").is_some();
        let stack: Self = serde_json::from_value(value)?;
        if has_project_repo && stack.spec.git_source.is_none() {
            return Err(PkoError::Validation(format!(
                "Stack '{}' has a malformed Git source (spec.projectRepo/branch/commit/repoDir/gitAuth)",
                stack.metadata.name
            )));
        }
        Ok(stack)
    }

    /// Serializes this `Stack` CR to a YAML manifest string.
    pub fn to_yaml(&self) -> Result<String, PkoError> {
        Ok(serde_yml::to_string(self)?)
    }

    /// Deserializes a `Stack` CR from a YAML manifest string (same Git source checks as
    /// [`PkoStack::from_json_value`]).
    pub fn from_yaml(yaml: &str) -> Result<Self, PkoError> {
        let value: serde_json::Value = serde_yml::from_str(yaml)?;
        Self::from_json_value(value)
    }

    /// Returns `true` if anti-lockout protection is active on this stack.
    pub fn is_protected(&self) -> bool {
        self.metadata.labels.get(LABEL_PROTECT).map(String::as_str) == Some("true")
            || self.spec.config.get("protect").map(String::as_str) == Some("true")
    }

    /// Returns `true` if this stack runs strictly in ephemeral preview mode (`spec.preview: true`).
    pub fn is_preview(&self) -> bool {
        self.spec.preview == Some(true)
    }

    /// Produces a Flux `HelmRelease` `spec.valuesFrom` Secret reference if `output_secret_name` is bound.
    pub fn helm_values_from_ref(&self) -> Option<HelmValuesFromRef> {
        self.spec
            .output_secret_name
            .as_ref()
            .map(|secret_name| HelmValuesFromRef {
                kind: "Secret".to_string(),
                name: secret_name.clone(),
                values_key: None,
                target_path: None,
                optional: Some(false),
            })
    }
}

/// Synthesized ephemeral PR preview `Stack` in `keel-previews` (`SDD.md` §4.2). The Stack clones
/// the PR branch directly through PKO `gitSource` (no Flux `GitRepository`/artifact involved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewStack {
    pub pr_id: String,
    pub tenant: String,
    pub branch: String,
    pub stack: PkoStack,
}

impl PreviewStack {
    /// Renders the preview PKO `Stack` as a YAML manifest.
    pub fn to_yaml(&self) -> Result<String, PkoError> {
        self.stack.to_yaml()
    }

    /// Converts the preview PKO `Stack` into a JSON value.
    pub fn to_json_value(&self) -> Result<serde_json::Value, PkoError> {
        self.stack.to_json_value()
    }
}

/// Parameters for synthesizing the anti-lockout `management-cluster-core` `Stack` CR.
#[derive(Debug, Clone)]
pub struct ManagementCoreStackParams {
    pub namespace: String,
    pub pulumi_stack_name: String,
    pub backend_url: String,
    pub git_source_name: String,
    pub repo_dir: String,
    pub service_account_name: Option<String>,
    pub config: BTreeMap<String, String>,
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    /// Namespace-local Secret holding the state passphrase (default [`PULUMI_PASSPHRASE_SECRET_NAME`]).
    pub passphrase_secret_name: String,
}

impl ManagementCoreStackParams {
    /// Creates standard parameters for `management-cluster-core` in `flux-system`.
    pub fn new(
        backend_url: impl Into<String>,
        git_source_name: impl Into<String>,
        repo_dir: impl Into<String>,
    ) -> Self {
        Self {
            namespace: FLUX_SYSTEM_NAMESPACE.to_string(),
            pulumi_stack_name: format!("organization/{MANAGEMENT_CORE_STACK_NAME}/prod"),
            backend_url: backend_url.into(),
            git_source_name: git_source_name.into(),
            repo_dir: repo_dir.into(),
            service_account_name: None,
            config: BTreeMap::new(),
            env_refs: BTreeMap::new(),
            passphrase_secret_name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
        }
    }
}

/// Parameters for synthesizing an ephemeral PR preview (`GitRepository` + PKO `Stack` in `keel-previews`).
#[derive(Debug, Clone)]
pub struct EphemeralPreviewParams {
    pub pr_id: String,
    pub tenant: String,
    pub branch: String,
    /// Git URL the preview `Stack` clones (`spec.projectRepo`). Only ever the tenant's own
    /// platform-written source, set once the preview is going to be applied; `None` leaves
    /// `projectRepo` unset (synthesized-only previews are never applied and never carry the
    /// control-plane remote).
    pub repo_url: Option<String>,
    pub repo_dir: String,
    pub pulumi_stack_name: String,
    pub backend_url: String,
    pub preview_service_account: Option<String>,
    /// `keel-previews` Secret mirroring the tenant's read-only Git credentials (`gitAuth`).
    pub git_secret_name: Option<String>,
    /// Keys present in [`Self::git_secret_name`] (selects the PKO `gitAuth` mode; inferred from the
    /// URL when empty, see [`PkoGitAuth::from_flux_secret`]).
    pub git_secret_keys: BTreeSet<String>,
    /// Git URL of the tenant `Stack` being previewed (its platform-written `gitSource.projectRepo`),
    /// set by [`EphemeralPreviewParams::inherit_tenant_stack`].
    pub tenant_repo_url: Option<String>,
    pub config: BTreeMap<String, String>,
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    /// Pulumi config secrets (`spec.secretsRef`), only ever pointing at per-preview Secret copies
    /// in `keel-previews` (see [`EphemeralPreviewParams::inherit_tenant_credentials`]).
    pub secrets_ref: BTreeMap<String, PkoEnvRef>,
    /// Optional `DOCKER_HOST` endpoint injected into the PKO workspace pod for local `kind`-in-`kind` dev.
    pub docker_host: Option<String>,
    /// Pulumi provider id stamped as `keel.io/provider` (required by Kyverno `restrict-pulumi-providers`).
    pub provider: Option<String>,
    /// PR head commit SHA recorded as `keel.io/head-sha` so restarts/replicas can detect new pushes.
    pub head_sha: Option<String>,
}

impl EphemeralPreviewParams {
    /// Constructs ephemeral preview parameters and builds a canonical `keel/<tenant>/<action>-<timestamp>` branch.
    #[allow(clippy::too_many_arguments)]
    pub fn for_action(
        pr_id: impl Into<String>,
        tenant: impl Into<String>,
        action: impl Into<String>,
        timestamp: u64,
        repo_dir: impl Into<String>,
        pulumi_stack_name: impl Into<String>,
        backend_url: impl Into<String>,
    ) -> Result<Self, PkoError> {
        let tenant_str = tenant.into();
        let action_str = action.into();
        let branch = build_preview_branch(&tenant_str, &action_str, timestamp)?;
        Ok(Self {
            pr_id: pr_id.into(),
            tenant: tenant_str,
            branch,
            repo_url: None,
            repo_dir: repo_dir.into(),
            pulumi_stack_name: pulumi_stack_name.into(),
            backend_url: backend_url.into(),
            preview_service_account: None,
            git_secret_name: None,
            git_secret_keys: BTreeSet::new(),
            tenant_repo_url: None,
            config: BTreeMap::new(),
            env_refs: BTreeMap::new(),
            secrets_ref: BTreeMap::new(),
            docker_host: None,
            provider: None,
            head_sha: None,
        })
    }
}

impl EphemeralPreviewParams {
    /// Aligns the preview with the tenant `Stack` it previews: same Pulumi stack name and
    /// tenant-scoped state backend (so `pulumi preview` diffs against the real checkpoint),
    /// same stack config, and the same `keel.io/provider` label.
    ///
    /// Tenant `envRefs`/`secretsRef` are **not** copied verbatim: they reference Secrets in
    /// `tenant-<id>` that do not exist in `keel-previews`, and control-plane admin credentials
    /// must never be substituted for them (OPEN-9). Use
    /// [`EphemeralPreviewParams::inherit_tenant_credentials`] to plan per-preview copies.
    ///
    /// A backend outside the tenant prefix is never adopted (OPEN-9b): the preview holds only the
    /// tenant-scoped state identity, so [`synthesize_ephemeral_preview`] always pins the preview to
    /// `s3://<bucket>/tenants/<tenant>` regardless of the inherited value.
    ///
    /// The protection keys ([`CONFIG_PROTECT_KEYS`]) are never inherited: a preview must always
    /// stay collectable by preview GC (`NFR-REL-02`).
    pub fn inherit_tenant_stack(&mut self, tenant_stack: &PkoStack) {
        self.pulumi_stack_name = tenant_stack.spec.stack.clone();
        if let Some(backend) = &tenant_stack.spec.backend
            && is_tenant_scoped_backend(backend, &self.tenant)
        {
            self.backend_url = backend.clone();
        }
        for (k, v) in &tenant_stack.spec.config {
            if !CONFIG_PROTECT_KEYS.contains(&k.as_str()) {
                self.config.insert(k.clone(), v.clone());
            }
        }
        if let Some(provider) = tenant_stack.metadata.labels.get(LABEL_PROVIDER) {
            self.provider = Some(provider.clone());
        }
        if let Some(src) = &tenant_stack.spec.git_source {
            self.tenant_repo_url = Some(src.project_repo.clone());
        }
    }

    /// Per-preview name of the tenant state passphrase copy in `keel-previews`
    /// (`keel-preview-<pr>-keel-pulumi-passphrase`), referenced by the preview workspace.
    pub fn passphrase_secret_copy_name(&self) -> Result<String, PkoError> {
        preview_secret_copy_name(&self.pr_id, PULUMI_PASSPHRASE_SECRET_NAME)
    }

    /// Plans the per-preview copy of the tenant's `keel-pulumi-passphrase` Secret so the
    /// preview can decrypt (and encrypt) the tenant's Pulumi state.
    pub fn passphrase_secret_copy(&self) -> Result<PreviewSecretCopy, PkoError> {
        Ok(PreviewSecretCopy {
            source_name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
            target_name: self.passphrase_secret_copy_name()?,
            keys: BTreeSet::from([PULUMI_PASSPHRASE_SECRET_KEY.to_string()]),
            proposed_data: None,
        })
    }

    /// Plans per-preview copies of exactly the Secrets the tenant `Stack` references through
    /// `envRefs` / `secretsRef` (cloud provider credentials) and rewrites this preview's refs to
    /// the copies (`keel-preview-<pr>-<name>` in `keel-previews`). The tenant state passphrase
    /// copy ([`EphemeralPreviewParams::passphrase_secret_copy`]) is always included.
    ///
    /// Secrets named in `excluded` (state-backend / forge credentials the preview runner already
    /// mirrors with tenant-scoped values) are neither copied nor referenced. Only the referenced
    /// keys are planned for each Secret. Non-`Secret` ref types are ignored.
    pub fn inherit_tenant_credentials(
        &mut self,
        tenant_stack: &PkoStack,
        excluded: &BTreeSet<String>,
    ) -> Result<Vec<PreviewSecretCopy>, PkoError> {
        let mut copies: BTreeMap<String, PreviewSecretCopy> = BTreeMap::new();
        let mut plan = |r: &PkoEnvRef| -> Result<Option<PkoEnvRef>, PkoError> {
            if r.selector_type != "Secret" || excluded.contains(&r.secret.name) {
                return Ok(None);
            }
            validate_k8s_name("secret_ref", &r.secret.name)?;
            let entry = match copies.entry(r.secret.name.clone()) {
                std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::btree_map::Entry::Vacant(v) => v.insert(PreviewSecretCopy {
                    source_name: r.secret.name.clone(),
                    target_name: preview_secret_copy_name(&self.pr_id, &r.secret.name)?,
                    keys: BTreeSet::new(),
                    proposed_data: None,
                }),
            };
            entry.keys.insert(r.secret.key.clone());
            Ok(Some(PkoEnvRef::from_secret(
                entry.target_name.clone(),
                r.secret.key.clone(),
            )))
        };
        let mut env_refs = BTreeMap::new();
        for (k, r) in &tenant_stack.spec.env_refs {
            if let Some(rewritten) = plan(r)? {
                env_refs.insert(k.clone(), rewritten);
            }
        }
        let mut secrets_ref = BTreeMap::new();
        for (k, r) in &tenant_stack.spec.secrets_ref {
            if let Some(rewritten) = plan(r)? {
                secrets_ref.insert(k.clone(), rewritten);
            }
        }
        self.env_refs.extend(env_refs);
        self.secrets_ref.extend(secrets_ref);
        let passphrase = self.passphrase_secret_copy()?;
        copies.insert(passphrase.source_name.clone(), passphrase);
        Ok(copies.into_values().collect())
    }
}

/// A tenant Secret copied into `keel-previews` for one preview `Stack` (cloud provider
/// credentials). The copy is labelled `keel.io/pr-id` / `keel.io/tenant` and owned by the
/// preview `Stack`, so Kubernetes garbage-collects it with the preview.
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewSecretCopy {
    /// Secret name in `tenant-<id>` referenced by the tenant `Stack`.
    pub source_name: String,
    /// Per-preview Secret name in `keel-previews` (`keel-preview-<pr>-<source>`).
    pub target_name: String,
    /// Only these keys are copied (exactly those referenced by the Stack).
    pub keys: BTreeSet<String>,
    /// Values proposed by the PR itself (e.g. SOPS-encrypted `<instance>-secrets` not yet
    /// decrypted into the tenant namespace by Flux). When set, they are used instead of the live
    /// tenant Secret so the preview evaluates the PR's credentials.
    pub proposed_data: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for PreviewSecretCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewSecretCopy")
            .field("source_name", &self.source_name)
            .field("target_name", &self.target_name)
            .field("keys", &self.keys)
            .field(
                "proposed_data",
                &self.proposed_data.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Deterministic per-preview Secret copy name: `keel-preview-<pr>-<source>`, shortened with a
/// stable hash suffix when it would exceed a DNS label (63 chars).
pub fn preview_secret_copy_name(pr_id: &str, source_name: &str) -> Result<String, PkoError> {
    let pr = sanitize_pr_id(pr_id)?;
    validate_k8s_name("source_secret", source_name)?;
    let flat = source_name.replace('.', "-");
    let full = format!("keel-preview-{pr}-{flat}");
    if full.len() <= 63 {
        return Ok(full);
    }
    // FNV-1a keeps the shortened name stable across replicas/restarts.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in full.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let suffix = format!("{:010x}", hash & 0xff_ffff_ffff);
    let head = full[..63 - suffix.len() - 1].trim_end_matches('-');
    let name = format!("{head}-{suffix}");
    validate_k8s_dns_label("preview_secret", &name)?;
    Ok(name)
}

/// `true` when `backend_url` is not an S3 DIY backend (nothing to scope) or already lives under
/// the tenant prefix `s3://<bucket>/tenants/<tenant>`.
pub fn is_tenant_scoped_backend(backend_url: &str, tenant: &str) -> bool {
    let Some(rest) = backend_url.strip_prefix("s3://") else {
        return true;
    };
    let location = rest.split('?').next().unwrap_or_default();
    let mut parts = location.splitn(2, '/');
    let _bucket = parts.next();
    let path = parts.next().unwrap_or_default().trim_end_matches('/');
    let prefix = format!("tenants/{tenant}");
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Pins an S3 DIY backend to the tenant prefix (`s3://<bucket>/tenants/<tenant>[?query]`),
/// matching `keel_kube::tenant::tenant_s3_backend_url`. Already-scoped and non-S3 backends are
/// returned unchanged.
pub fn tenant_scoped_preview_backend(backend_url: &str, tenant: &str) -> String {
    if is_tenant_scoped_backend(backend_url, tenant) {
        return backend_url.to_string();
    }
    let rest = backend_url.strip_prefix("s3://").unwrap_or(backend_url);
    let (location, query) = match rest.split_once('?') {
        Some((l, q)) => (l, Some(q)),
        None => (rest, None),
    };
    let bucket = location.split('/').next().unwrap_or_default();
    let mut url = format!("s3://{bucket}/tenants/{tenant}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    url
}

/// Parameters for synthesizing a multi-tenant infrastructure `Stack` CR in `tenant-<tenant>`.
#[derive(Debug, Clone)]
pub struct TenantStackParams {
    pub tenant: String,
    pub name: String,
    pub pulumi_stack_name: String,
    pub backend_url: String,
    /// The tenant's own Git source (platform-written `tenants/<id>/git-repository.yaml`), cloned
    /// directly by the workspace pod with the tenant's read-only credentials (`gitAuth`).
    pub git_source: PkoGitSource,
    pub service_account_name: Option<String>,
    pub output_secret_name: Option<String>,
    pub config: BTreeMap<String, String>,
    pub env_refs: BTreeMap<String, PkoEnvRef>,
    pub secrets_ref: BTreeMap<String, PkoEnvRef>,
    /// Optional `DOCKER_HOST` endpoint injected into the PKO workspace pod for local `kind`-in-`kind` dev.
    pub docker_host: Option<String>,
    /// `tenant-<id>` Secret holding the tenant state passphrase (default
    /// [`PULUMI_PASSPHRASE_SECRET_NAME`]).
    pub passphrase_secret_name: String,
}

impl TenantStackParams {
    /// Creates standard parameters for a tenant infrastructure stack.
    pub fn new(
        tenant: impl Into<String>,
        name: impl Into<String>,
        pulumi_stack_name: impl Into<String>,
        backend_url: impl Into<String>,
        git_source: PkoGitSource,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            name: name.into(),
            pulumi_stack_name: pulumi_stack_name.into(),
            backend_url: backend_url.into(),
            git_source,
            service_account_name: None,
            output_secret_name: None,
            config: BTreeMap::new(),
            env_refs: BTreeMap::new(),
            secrets_ref: BTreeMap::new(),
            docker_host: None,
            passphrase_secret_name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
        }
    }
}

/// Builds a canonical proposal branch `keel/<tenant>/<action>-<timestamp>`.
pub fn build_preview_branch(
    tenant: &str,
    action: &str,
    timestamp: u64,
) -> Result<String, PkoError> {
    validate_k8s_dns_label("tenant", tenant)?;
    validate_k8s_dns_label("action", action)?;
    if timestamp == 0 {
        return Err(PkoError::Validation(
            "timestamp must be greater than zero".to_string(),
        ));
    }
    Ok(format!("keel/{tenant}/{action}-{timestamp}"))
}

/// Validates that a proposal branch conforms to `keel/<tenant>/<action>-<timestamp>`.
pub fn validate_preview_branch(tenant: &str, branch: &str) -> Result<(), PkoError> {
    let expected_prefix = format!("keel/{tenant}/");
    let Some(rest) = branch.strip_prefix(&expected_prefix) else {
        return Err(PkoError::Validation(format!(
            "preview branch '{branch}' must start with '{expected_prefix}' (<action>-<timestamp>)"
        )));
    };

    let Some((action, ts_part)) = rest.rsplit_once('-') else {
        return Err(PkoError::Validation(format!(
            "preview branch '{branch}' must end with '-<timestamp>'"
        )));
    };

    validate_k8s_dns_label("action", action)?;
    if ts_part.is_empty() || !ts_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(PkoError::Validation(format!(
            "preview branch '{branch}' has invalid numeric timestamp '{ts_part}'"
        )));
    }

    Ok(())
}

/// Synthesizes the Stage 0/1 `management-cluster-core` PKO v2 `Stack` CR with mandatory
/// `protect: true` anti-lockout guardrails (`SDD.md` §2.1, §2.2, `NFR-REL-01`).
pub fn synthesize_management_core_stack(
    params: &ManagementCoreStackParams,
) -> Result<PkoStack, PkoError> {
    validate_k8s_dns_label("namespace", &params.namespace)?;
    validate_non_empty("pulumi_stack_name", &params.pulumi_stack_name)?;
    validate_non_empty("backend_url", &params.backend_url)?;
    validate_k8s_name("git_source_name", &params.git_source_name)?;
    validate_repo_dir(&params.repo_dir)?;
    validate_k8s_name("passphrase_secret_name", &params.passphrase_secret_name)?;

    let sa = match &params.service_account_name {
        Some(custom) => {
            validate_k8s_name("service_account_name", custom)?;
            custom.clone()
        }
        None => DEFAULT_MANAGEMENT_SERVICE_ACCOUNT.to_string(),
    };

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), "keel".to_string());
    labels.insert(LABEL_STACK_ROLE.to_string(), "management-core".to_string());
    labels.insert(LABEL_PROTECT.to_string(), "true".to_string());

    let mut config = params.config.clone();
    // Enforce anti-lockout protection unconditionally regardless of caller-supplied config.
    config.insert("protect".to_string(), "true".to_string());
    config.insert("keel:protect".to_string(), "true".to_string());

    Ok(PkoStack {
        api_version: PKO_STACK_API_VERSION.to_string(),
        kind: PKO_STACK_KIND.to_string(),
        metadata: CrObjectMeta {
            name: MANAGEMENT_CORE_STACK_NAME.to_string(),
            namespace: params.namespace.clone(),
            labels,
            annotations: BTreeMap::new(),
        },
        spec: PkoStackSpec {
            stack: params.pulumi_stack_name.clone(),
            service_account_name: Some(sa),
            backend: Some(params.backend_url.clone()),
            flux_source: Some(PkoFluxSource::git_repository(
                &params.git_source_name,
                &params.repo_dir,
            )),
            git_source: None,
            preview: None,
            destroy_on_finalize: Some(false),
            continue_resync_on_commit_match: Some(true),
            resync_frequency_seconds: Some(300),
            output_secret_name: None,
            config,
            env_refs: params.env_refs.clone(),
            secrets_ref: BTreeMap::new(),
            workspace_template: Some(PkoWorkspaceTemplate::restricted(
                &params.passphrase_secret_name,
            )),
        },
    })
}

/// Synthesizes a PKO v2 `Stack` CR with `preview: true` in `keel-previews` that clones the PR
/// branch `keel/<tenant>/<action>-<timestamp>` through PKO `gitSource` (authenticated with the
/// tenant's mirrored read-only credentials) and runs as a read-only preview ServiceAccount
/// (`SDD.md` §4.2, `NFR-REL-02`).
pub fn synthesize_ephemeral_preview(
    params: &EphemeralPreviewParams,
) -> Result<PreviewStack, PkoError> {
    let normalized_pr_id = sanitize_pr_id(&params.pr_id)?;
    validate_k8s_dns_label("tenant", &params.tenant)?;
    validate_preview_branch(&params.tenant, &params.branch)?;
    validate_repo_dir(&params.repo_dir)?;
    validate_non_empty("pulumi_stack_name", &params.pulumi_stack_name)?;
    validate_non_empty("backend_url", &params.backend_url)?;

    let preview_sa = match &params.preview_service_account {
        Some(sa) => {
            validate_k8s_name("preview_service_account", sa)?;
            sa.clone()
        }
        None => DEFAULT_PREVIEW_SERVICE_ACCOUNT.to_string(),
    };

    // Without a resolved tenant source the preview is synthesized only (never applied): leave
    // `projectRepo` unset rather than putting a placeholder (e.g. the control-plane remote) in it.
    let git_source = match &params.repo_url {
        Some(url) => {
            let git_auth = match &params.git_secret_name {
                Some(secret) => Some(PkoGitAuth::from_flux_secret(
                    secret,
                    url,
                    &params.git_secret_keys,
                )?),
                None => None,
            };
            let source =
                PkoGitSource::branch(url, &params.branch, &params.repo_dir).with_auth(git_auth);
            source.validate()?;
            Some(source)
        }
        None => None,
    };

    let passphrase_copy = params.passphrase_secret_copy_name()?;
    let stack_name = format!("preview-stack-pr-{normalized_pr_id}");

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), "keel".to_string());
    labels.insert(LABEL_EPHEMERAL_PREVIEW.to_string(), "true".to_string());
    labels.insert(
        LABEL_STACK_ROLE.to_string(),
        "ephemeral-preview".to_string(),
    );
    labels.insert(LABEL_PR_ID.to_string(), normalized_pr_id.clone());
    labels.insert(LABEL_TENANT.to_string(), params.tenant.clone());
    if let Some(provider) = params
        .provider
        .as_deref()
        .or_else(|| params.config.get("keel:provider").map(String::as_str))
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        validate_k8s_dns_label("provider", provider)?;
        labels.insert(LABEL_PROVIDER.to_string(), provider.to_string());
    }

    let mut annotations = BTreeMap::new();
    annotations.insert(ANNOTATION_BRANCH.to_string(), params.branch.clone());
    if let Some(sha) = params
        .head_sha
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        annotations.insert(crate::run::ANNOTATION_HEAD_SHA.to_string(), sha.to_string());
    }

    let stack = PkoStack {
        api_version: PKO_STACK_API_VERSION.to_string(),
        kind: PKO_STACK_KIND.to_string(),
        metadata: CrObjectMeta {
            name: stack_name,
            namespace: KEEL_PREVIEWS_NAMESPACE.to_string(),
            labels,
            annotations,
        },
        spec: PkoStackSpec {
            stack: params.pulumi_stack_name.clone(),
            service_account_name: Some(preview_sa),
            // OPEN-9b: every tenant preview runs against the tenant state prefix (the only one its
            // scoped identity can reach), for existing and brand-new stacks alike.
            backend: Some(tenant_scoped_preview_backend(
                &params.backend_url,
                &params.tenant,
            )),
            flux_source: None,
            git_source,
            preview: Some(true),
            destroy_on_finalize: Some(false),
            continue_resync_on_commit_match: Some(false),
            resync_frequency_seconds: None,
            output_secret_name: None,
            config: params.config.clone(),
            env_refs: params.env_refs.clone(),
            secrets_ref: params.secrets_ref.clone(),
            workspace_template: Some({
                let tpl = PkoWorkspaceTemplate::restricted(&passphrase_copy).with_keel_ca_trust();
                match &params.docker_host {
                    Some(host) => tpl.with_env_var("DOCKER_HOST", host),
                    None => tpl,
                }
            }),
        },
    };

    Ok(PreviewStack {
        pr_id: normalized_pr_id,
        tenant: params.tenant.clone(),
        branch: params.branch.clone(),
        stack,
    })
}

/// Name of the tenant Flux source (`tenant-<id>/keel-gitops`) referenced by the `fluxSource` of
/// tenant `Stack`s written by releases before tenant Stacks moved to PKO `gitSource`.
pub const LEGACY_TENANT_FLUX_SOURCE_NAME: &str = "keel-gitops";

/// Outcome of [`migrate_legacy_flux_source_stack`] for one `stack.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FluxSourceMigration {
    /// Not a legacy tenant `Stack` (already on PKO `gitSource`, no `fluxSource`, not a `Stack`).
    Unchanged,
    /// The rewritten manifest (`fluxSource` replaced by `gitSource`, everything else kept).
    Migrated(String),
    /// A `fluxSource` Keel does not rewrite automatically (reason, secret-free).
    Skipped(String),
}

/// Rewrites a tenant `Stack` manifest written by earlier releases (`spec.fluxSource` →
/// `tenant-<id>/keel-gitops`, which needs `source-controller` artifacts on port 9090) to PKO
/// `gitSource`: `fluxSource` is removed and `projectRepo`/`branch`/`repoDir`/`gitAuth`/`shallow`
/// from `git_source_for_dir(dir)` are added; every other field is kept verbatim (no program or
/// config change). Only `Stack`s in `tenant-<tenant>` whose `fluxSource` is the tenant's own
/// `keel-gitops` `GitRepository` with a `dir` under `tenants/<tenant>/` are rewritten; anything
/// else is [`FluxSourceMigration::Skipped`] (the protected `flux-system` management Stack is
/// never touched).
pub fn migrate_legacy_flux_source_stack(
    yaml: &str,
    tenant: &str,
    git_source_for_dir: impl FnOnce(&str) -> Result<PkoGitSource, PkoError>,
) -> Result<FluxSourceMigration, PkoError> {
    validate_k8s_dns_label("tenant", tenant)?;
    let mut doc: serde_yml::Value = serde_yml::from_str(yaml)?;
    let str_at = |v: &serde_yml::Value, path: &[&str]| -> Option<String> {
        let mut cur = v;
        for key in path {
            cur = cur.get(*key)?;
        }
        cur.as_str().map(str::to_string)
    };
    let is_stack = str_at(&doc, &["kind"]).as_deref() == Some(PKO_STACK_KIND)
        && str_at(&doc, &["apiVersion"]).is_some_and(|v| v.starts_with("pulumi.com/"));
    if !is_stack || doc.get("spec").and_then(|s| s.get("fluxSource")).is_none() {
        return Ok(FluxSourceMigration::Unchanged);
    }
    let expected_ns = format!("tenant-{tenant}");
    let namespace = str_at(&doc, &["metadata", "namespace"]).unwrap_or_default();
    if namespace != expected_ns {
        return Ok(FluxSourceMigration::Skipped(format!(
            "Stack namespace `{namespace}` is not `{expected_ns}`"
        )));
    }
    if doc.get("spec").and_then(|s| s.get("projectRepo")).is_some() {
        return Ok(FluxSourceMigration::Skipped(
            "Stack sets both fluxSource and projectRepo".to_string(),
        ));
    }
    let source_kind = str_at(&doc, &["spec", "fluxSource", "sourceRef", "kind"]);
    let source_name = str_at(&doc, &["spec", "fluxSource", "sourceRef", "name"]);
    if source_kind.as_deref() != Some(FLUX_GIT_REPO_KIND)
        || source_name.as_deref() != Some(LEGACY_TENANT_FLUX_SOURCE_NAME)
    {
        return Ok(FluxSourceMigration::Skipped(format!(
            "fluxSource is not the tenant's own GitRepository `{LEGACY_TENANT_FLUX_SOURCE_NAME}`"
        )));
    }
    let dir = str_at(&doc, &["spec", "fluxSource", "dir"])
        .map(|d| d.trim().trim_end_matches('/').to_string())
        .unwrap_or_default();
    let tenant_prefix = format!("tenants/{tenant}/");
    if !dir.starts_with(&tenant_prefix) || validate_repo_dir(&dir).is_err() {
        return Ok(FluxSourceMigration::Skipped(format!(
            "fluxSource.dir `{dir}` is not under `{tenant_prefix}`"
        )));
    }
    let source = git_source_for_dir(&dir)?;
    source.validate()?;
    if source.repo_dir.as_deref() != Some(dir.as_str()) {
        return Err(PkoError::Validation(format!(
            "migrated Git source must keep repoDir `{dir}`"
        )));
    }
    let serde_yml::Value::Mapping(fields) = serde_yml::to_value(&source)? else {
        return Err(PkoError::Synthesis(
            "PKO Git source did not serialize to a mapping".to_string(),
        ));
    };
    let Some(serde_yml::Value::Mapping(spec)) = doc.get_mut("spec") else {
        return Ok(FluxSourceMigration::Skipped(
            "Stack spec is not a mapping".to_string(),
        ));
    };
    spec.remove("fluxSource");
    for (key, value) in fields {
        spec.insert(key, value);
    }
    // The workspace now clones the forge itself (`fetch`): trust the public Keel CA bundle so an
    // in-cluster Forgejo HTTPS ingress clone verifies (no-op for external public-CA forges).
    let mut template: serde_json::Value = match spec.get("workspaceTemplate") {
        Some(v) => serde_json::to_value(v)?,
        None => serde_json::Value::Null,
    };
    add_keel_ca_trust_to_workspace_template(&mut template)?;
    spec.insert(
        "workspaceTemplate".to_string(),
        serde_yml::to_value(&template)?,
    );
    let out = serde_yml::to_string(&doc)?;
    // The rewritten manifest must still be a well-formed Stack with a complete Git source.
    let parsed = PkoStack::from_yaml(&out)?;
    if parsed.spec.git_source.is_none() || parsed.spec.flux_source.is_some() {
        return Err(PkoError::Synthesis(
            "migrated Stack lost its Git source".to_string(),
        ));
    }
    Ok(FluxSourceMigration::Migrated(out))
}

/// Adds the Keel CA trust of [`PkoWorkspaceTemplate::with_keel_ca_trust`] to an untyped
/// `spec.workspaceTemplate` (`Null` creates one), keeping every other field verbatim. Used when
/// rewriting Stacks committed by earlier releases.
pub fn add_keel_ca_trust_to_workspace_template(
    template: &mut serde_json::Value,
) -> Result<(), PkoError> {
    use serde_json::{Value, json};
    fn obj<'a>(v: &'a mut Value, key: &str, default: Value) -> Result<&'a mut Value, PkoError> {
        let map = v.as_object_mut().ok_or_else(|| {
            PkoError::Validation(format!(
                "workspaceTemplate: parent of `{key}` is not a mapping"
            ))
        })?;
        Ok(map.entry(key.to_string()).or_insert(default))
    }
    fn named<'a>(list: &'a mut Value, name: &str) -> Result<&'a mut Value, PkoError> {
        let items = list.as_array_mut().ok_or_else(|| {
            PkoError::Validation("workspaceTemplate: container list is not a sequence".to_string())
        })?;
        let index = match items.iter().position(|c| c["name"] == name) {
            Some(i) => i,
            None => {
                items.push(json!({ "name": name }));
                items.len() - 1
            }
        };
        Ok(&mut items[index])
    }
    fn upsert(list: &mut Value, item: Value) -> Result<(), PkoError> {
        let items = list.as_array_mut().ok_or_else(|| {
            PkoError::Validation("workspaceTemplate: entry list is not a sequence".to_string())
        })?;
        items.retain(|i| i["name"] != item["name"]);
        items.push(item);
        Ok(())
    }
    if template.is_null() {
        *template = json!({});
    }
    let pod = obj(template, "spec", json!({}))?;
    let pod = obj(pod, "podTemplate", json!({}))?;
    let pod = obj(pod, "spec", json!({}))?;
    let typed = PkoWorkspaceTemplate::restricted("unused").with_keel_ca_trust();
    let typed = serde_json::to_value(&typed.spec.pod_template.spec)?;
    let volume = typed["volumes"][0].clone();
    // Templates Keel never wrote (no pod spec yet) get the `restricted` defaults too.
    for key in ["terminationGracePeriodSeconds", "securityContext"] {
        obj(pod, key, typed[key].clone())?;
    }
    let env = json!({ "name": SSL_CERT_DIR_ENV, "value": KEEL_SSL_CERT_DIR });
    let mount = json!({
        "name": KEEL_CA_VOLUME_NAME, "mountPath": KEEL_CA_MOUNT_PATH, "readOnly": true
    });
    upsert(obj(pod, "volumes", json!([]))?, volume)?;
    for (list, name) in [
        ("initContainers", PKO_FETCH_CONTAINER_NAME),
        ("containers", "pulumi"),
    ] {
        let container = named(obj(pod, list, json!([]))?, name)?;
        if list == "containers" {
            obj(
                container,
                "securityContext",
                typed["containers"][0]["securityContext"].clone(),
            )?;
        }
        upsert(obj(container, "env", json!([]))?, env.clone())?;
        upsert(obj(container, "volumeMounts", json!([]))?, mount.clone())?;
    }
    Ok(())
}

/// Synthesizes a tenant PKO v2 `Stack` CR strictly isolated to `tenant-<tenant>` with optional
/// `output_secret_name` binding for Flux `HelmRelease` `spec.valuesFrom` consumption (`SDD.md` §5.1, §6.2).
pub fn synthesize_tenant_stack(params: &TenantStackParams) -> Result<PkoStack, PkoError> {
    validate_k8s_dns_label("tenant", &params.tenant)?;
    validate_k8s_name("name", &params.name)?;
    validate_non_empty("pulumi_stack_name", &params.pulumi_stack_name)?;
    validate_non_empty("backend_url", &params.backend_url)?;
    params.git_source.validate()?;
    if params.git_source.repo_dir.is_none() {
        return Err(PkoError::Validation(
            "tenant stack Git source needs a repo_dir".to_string(),
        ));
    }

    validate_k8s_name("passphrase_secret_name", &params.passphrase_secret_name)?;

    if params.name == MANAGEMENT_CORE_STACK_NAME {
        return Err(PkoError::Validation(format!(
            "tenant stack cannot use reserved name '{MANAGEMENT_CORE_STACK_NAME}'"
        )));
    }

    let tenant_namespace = format!("tenant-{}", params.tenant);

    let service_account = match &params.service_account_name {
        Some(sa) => {
            validate_k8s_name("service_account_name", sa)?;
            sa.clone()
        }
        None => format!("tenant-{}-sa", params.tenant),
    };

    let output_secret_name = match &params.output_secret_name {
        Some(secret_name) => {
            validate_k8s_name("output_secret_name", secret_name)?;
            Some(secret_name.clone())
        }
        None => None,
    };

    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), "keel".to_string());
    labels.insert(LABEL_STACK_ROLE.to_string(), "tenant".to_string());
    labels.insert(LABEL_TENANT.to_string(), params.tenant.clone());
    if let Some(provider) = params.config.get("keel:provider") {
        labels.insert(LABEL_PROVIDER.to_string(), provider.clone());
    }

    Ok(PkoStack {
        api_version: PKO_STACK_API_VERSION.to_string(),
        kind: PKO_STACK_KIND.to_string(),
        metadata: CrObjectMeta {
            name: params.name.clone(),
            namespace: tenant_namespace.clone(),
            labels,
            annotations: BTreeMap::new(),
        },
        spec: PkoStackSpec {
            stack: params.pulumi_stack_name.clone(),
            service_account_name: Some(service_account),
            backend: Some(params.backend_url.clone()),
            flux_source: None,
            git_source: Some(params.git_source.clone()),
            preview: None,
            destroy_on_finalize: Some(true),
            continue_resync_on_commit_match: Some(true),
            resync_frequency_seconds: Some(300),
            output_secret_name,
            config: params.config.clone(),
            env_refs: params.env_refs.clone(),
            secrets_ref: params.secrets_ref.clone(),
            workspace_template: Some({
                let base = PkoWorkspaceTemplate::restricted(&params.passphrase_secret_name);
                let mut tpl = match &params.docker_host {
                    Some(host) => base.with_env_var("DOCKER_HOST", host),
                    None => base,
                };
                if let Some(secret_name) = params.config.get("kubeconfigSecretName") {
                    let ns_matches = params
                        .config
                        .get("kubeconfigSecretNamespace")
                        .is_none_or(|ns| ns == &tenant_namespace);
                    if !secret_name.trim().is_empty() && ns_matches {
                        tpl = tpl.with_secret_volume(
                            "target-kubeconfig",
                            secret_name,
                            "/etc/keel/kubeconfig",
                        );
                    }
                }
                tpl.with_keel_ca_trust()
            }),
        },
    })
}

/// Attaches `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` `envRefs` pointing to `secret_name`.
pub fn attach_s3_credentials_env_refs(
    env_refs: &mut BTreeMap<String, PkoEnvRef>,
    secret_name: &str,
) {
    env_refs.insert(
        "AWS_ACCESS_KEY_ID".to_string(),
        PkoEnvRef::from_secret(secret_name, "accessKey"),
    );
    env_refs.insert(
        "AWS_SECRET_ACCESS_KEY".to_string(),
        PkoEnvRef::from_secret(secret_name, "secretKey"),
    );
}

/// Attaches `GITHUB_TOKEN` from a namespace-local Kubernetes `Secret` (`token` key) to `env_refs`.
pub fn attach_github_token_env_ref(env_refs: &mut BTreeMap<String, PkoEnvRef>, secret_name: &str) {
    env_refs.insert(
        "GITHUB_TOKEN".to_string(),
        PkoEnvRef::from_secret(secret_name, "token"),
    );
}

/// Synthesizes a Pulumi YAML program that deploys raw Kubernetes manifests via `kubernetes:yaml/v2:ConfigGroup`,
/// declaring `schema_properties` in `configuration` so `Stack.spec.config` values pass Pulumi validation.
pub fn synthesize_k8s_manifest_pulumi_yaml(
    stack_name: &str,
    rendered_manifests_yaml: &str,
    schema_properties: &[String],
    target_kubeconfig_defined: bool,
) -> Result<String, PkoError> {
    validate_k8s_name("stack_name", stack_name)?;
    if rendered_manifests_yaml.trim().is_empty() {
        return Err(PkoError::Validation(
            "rendered_manifests_yaml must not be empty".to_string(),
        ));
    }

    let mut configuration = serde_json::Map::new();
    for prop in schema_properties {
        let trimmed = prop.trim();
        if !trimmed.is_empty() {
            configuration.insert(
                trimmed.to_string(),
                serde_json::json!({
                    "type": "string",
                    "default": ""
                }),
            );
        }
    }
    if target_kubeconfig_defined {
        configuration
            .entry("kubeconfigSecretName".to_string())
            .or_insert_with(|| {
                serde_json::json!({
                    "type": "string",
                    "default": ""
                })
            });
        configuration
            .entry("kubeconfigSecretNamespace".to_string())
            .or_insert_with(|| {
                serde_json::json!({
                    "type": "string",
                    "default": ""
                })
            });
        configuration
            .entry("kubeconfigPath".to_string())
            .or_insert_with(|| {
                serde_json::json!({
                    "type": "string",
                    "default": "/etc/keel/kubeconfig/kubeconfig"
                })
            });
    }

    let mut resources = serde_json::Map::new();
    let mut manifest_resource = serde_json::Map::new();
    manifest_resource.insert(
        "type".to_string(),
        serde_json::Value::String("kubernetes:yaml/v2:ConfigGroup".to_string()),
    );
    if target_kubeconfig_defined {
        resources.insert(
            "targetCluster".to_string(),
            serde_json::json!({
                "type": "pulumi:providers:kubernetes",
                "properties": {
                    "kubeconfig": {
                        "fn::readFile": "/etc/keel/kubeconfig/kubeconfig"
                    },
                    "enableServerSideApply": true,
                    "upsertExistingObjects": true
                }
            }),
        );
        manifest_resource.insert(
            "options".to_string(),
            serde_json::json!({
                "provider": "${targetCluster}"
            }),
        );
    }
    manifest_resource.insert(
        "properties".to_string(),
        serde_json::json!({
            "yaml": rendered_manifests_yaml
        }),
    );
    resources.insert(
        "manifests".to_string(),
        serde_json::Value::Object(manifest_resource),
    );

    let mut doc = serde_json::Map::new();
    doc.insert(
        "name".to_string(),
        serde_json::Value::String(stack_name.to_string()),
    );
    doc.insert(
        "runtime".to_string(),
        serde_json::Value::String("yaml".to_string()),
    );
    doc.insert(
        "description".to_string(),
        serde_json::Value::String(format!(
            "Keel PKO Kubernetes manifest stack for {stack_name}"
        )),
    );
    if !configuration.is_empty() {
        doc.insert(
            "configuration".to_string(),
            serde_json::Value::Object(configuration),
        );
    }
    doc.insert(
        "resources".to_string(),
        serde_json::Value::Object(resources),
    );
    doc.insert(
        "outputs".to_string(),
        serde_json::json!({
            "stackName": stack_name
        }),
    );

    let yaml = serde_yml::to_string(&serde_json::Value::Object(doc))?;
    Ok(ensure_trailing_newline(&yaml))
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), PkoError> {
    if value.trim().is_empty() {
        return Err(PkoError::Validation(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_repo_dir(dir: &str) -> Result<(), PkoError> {
    validate_non_empty("repo_dir", dir)?;
    if dir.split('/').any(|seg| seg == "..") {
        return Err(PkoError::Validation(format!(
            "repo_dir '{dir}' must not contain parent path traversal ('..')"
        )));
    }
    Ok(())
}

fn validate_k8s_dns_label(field: &str, value: &str) -> Result<(), PkoError> {
    keel_core::validate_dns_label(value, field).map_err(|e| PkoError::Validation(e.to_string()))
}

fn validate_k8s_name(field: &str, value: &str) -> Result<(), PkoError> {
    if value.is_empty() || value.len() > 253 || value.contains('/') {
        return Err(PkoError::Validation(format!(
            "{field} '{value}' must be a valid Kubernetes resource name without namespace slashes"
        )));
    }
    for segment in value.split('.') {
        validate_k8s_dns_label(field, segment)?;
    }
    Ok(())
}

pub(crate) fn sanitize_pr_id(pr_id: &str) -> Result<String, PkoError> {
    let trimmed = pr_id.trim().trim_start_matches('#');
    if trimmed.is_empty() {
        return Err(PkoError::Validation("pr_id must not be empty".to_string()));
    }
    let lowered = trimmed.to_ascii_lowercase();
    validate_k8s_dns_label("pr_id", &lowered)?;
    Ok(lowered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_git_source(repo_dir: &str) -> PkoGitSource {
        PkoGitSource::branch("https://forge.internal/keel/infra.git", "main", repo_dir)
    }

    #[test]
    fn management_core_stack_enforces_protect_true_and_anti_lockout() {
        let mut params = ManagementCoreStackParams::new(
            "s3://pulumi-state",
            "flux-system",
            "clusters/management",
        );
        // Attempt to pass protect=false in user config; synthesis must override to true.
        params
            .config
            .insert("protect".to_string(), "false".to_string());
        params
            .config
            .insert("keel:protect".to_string(), "false".to_string());
        params.env_refs.insert(
            "AWS_SECRET_ACCESS_KEY".to_string(),
            PkoEnvRef::from_secret("s3-creds", "secretKey"),
        );

        let stack = synthesize_management_core_stack(&params).expect("valid management stack");

        let mut custom_sa_params = params.clone();
        custom_sa_params.service_account_name = Some("custom.mgmt-sa".to_string());
        let custom_sa_stack =
            synthesize_management_core_stack(&custom_sa_params).expect("valid custom sa stack");
        assert_eq!(
            custom_sa_stack.spec.service_account_name.as_deref(),
            Some("custom.mgmt-sa")
        );
        assert_eq!(stack.api_version, "pulumi.com/v1");
        assert_eq!(stack.kind, "Stack");
        assert_eq!(stack.metadata.name, "management-cluster-core");
        assert_eq!(stack.metadata.namespace, "flux-system");
        assert!(stack.is_protected());
        assert!(!stack.is_preview());
        assert_eq!(stack.spec.destroy_on_finalize, Some(false));
        assert_eq!(
            stack.spec.config.get("protect").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            stack.spec.config.get("keel:protect").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            stack.spec.service_account_name.as_deref(),
            Some(DEFAULT_MANAGEMENT_SERVICE_ACCOUNT)
        );

        let json = stack.to_json_value().expect("json serialization");
        assert_eq!(json["apiVersion"], "pulumi.com/v1");
        assert_eq!(json["kind"], "Stack");
        assert_eq!(json["metadata"]["labels"]["keel.io/protect"], "true");
        assert_eq!(json["spec"]["destroyOnFinalize"], false);
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["terminationGracePeriodSeconds"],
            600
        );
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["securityContext"]["runAsNonRoot"],
            true
        );
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"][0]["resources"]
                ["requests"]["memory"],
            "256Mi"
        );
        assert_eq!(
            json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"][0]["resources"]
                ["limits"]["memory"],
            "4Gi"
        );

        let yaml = stack.to_yaml().expect("yaml serialization");
        let roundtrip = PkoStack::from_yaml(&yaml).expect("yaml roundtrip");
        assert_eq!(roundtrip, stack);
        let json_roundtrip = PkoStack::from_json_value(json).expect("json roundtrip");
        assert_eq!(json_roundtrip, stack);
    }

    #[test]
    fn ephemeral_preview_synthesizes_pinned_git_repo_and_preview_stack() {
        let params = EphemeralPreviewParams::for_action(
            "#42",
            "payments",
            "provision-postgres",
            1_763_100_000,
            "tenants/payments/infra/postgres",
            "keel/payments-postgres/prod",
            "s3://pulumi-state",
        )
        .expect("valid preview params");

        let mut params = params;
        params.repo_url = Some("https://forge.internal/keel/infra-gitops.git".to_string());
        params.git_secret_name = Some("forge-git-auth".to_string());

        let mut custom_preview_params = params.clone();
        custom_preview_params.preview_service_account = Some("preview-ro.sa".to_string());
        let custom_preview =
            synthesize_ephemeral_preview(&custom_preview_params).expect("valid custom preview sa");
        assert_eq!(
            custom_preview.stack.spec.service_account_name.as_deref(),
            Some("preview-ro.sa")
        );

        let mut labelled = params.clone();
        labelled.provider = Some("kubernetes".to_string());
        labelled.head_sha = Some("abc123".to_string());
        let labelled = synthesize_ephemeral_preview(&labelled).expect("labelled preview");
        assert_eq!(
            labelled
                .stack
                .metadata
                .labels
                .get(LABEL_PROVIDER)
                .map(String::as_str),
            Some("kubernetes")
        );
        assert_eq!(
            labelled
                .stack
                .metadata
                .annotations
                .get(crate::run::ANNOTATION_HEAD_SHA)
                .map(String::as_str),
            Some("abc123")
        );
        let mut bad_provider = params.clone();
        bad_provider.provider = Some("Bad Provider".to_string());
        assert!(synthesize_ephemeral_preview(&bad_provider).is_err());

        let bundle = synthesize_ephemeral_preview(&params).expect("valid ephemeral preview");
        assert_eq!(bundle.pr_id, "42");
        assert_eq!(bundle.branch, "keel/payments/provision-postgres-1763100000");

        // Verify PKO v2 Stack CR
        let stack = &bundle.stack;
        assert_eq!(stack.api_version, "pulumi.com/v1");
        assert_eq!(stack.kind, "Stack");
        assert_eq!(stack.metadata.name, "preview-stack-pr-42");
        assert_eq!(stack.metadata.namespace, "keel-previews");
        assert!(stack.is_preview());
        assert_eq!(stack.spec.preview, Some(true));
        assert_eq!(stack.spec.destroy_on_finalize, Some(false));
        assert_eq!(
            stack.spec.service_account_name.as_deref(),
            Some(DEFAULT_PREVIEW_SERVICE_ACCOUNT)
        );
        // PKO gitSource: clones the PR branch directly, authenticated with the mirrored Secret.
        assert!(stack.spec.flux_source.is_none());
        let src = stack.spec.git_source.as_ref().expect("git source");
        assert_eq!(
            src.project_repo,
            "https://forge.internal/keel/infra-gitops.git"
        );
        assert_eq!(
            src.branch.as_deref(),
            Some("keel/payments/provision-postgres-1763100000")
        );
        assert!(src.commit.is_none());
        assert_eq!(
            src.repo_dir.as_deref(),
            Some("tenants/payments/infra/postgres")
        );
        let basic = src
            .git_auth
            .as_ref()
            .and_then(|a| a.basic_auth.as_ref())
            .expect("basic auth inferred from the HTTPS URL");
        assert_eq!(basic.user_name.secret.name, "forge-git-auth");
        assert_eq!(basic.user_name.secret.key, "username");
        assert_eq!(basic.password.secret.key, "password");

        // Verify JSON & YAML roundtrips (flattened PKO field names)
        let stack_json = bundle.to_json_value().expect("json value");
        assert_eq!(stack_json["spec"]["preview"], true);
        assert_eq!(
            stack_json["spec"]["serviceAccountName"],
            "keel-preview-readonly"
        );
        assert!(stack_json["spec"].get("fluxSource").is_none());
        assert_eq!(
            stack_json["spec"]["projectRepo"],
            "https://forge.internal/keel/infra-gitops.git"
        );
        assert_eq!(
            stack_json["spec"]["branch"],
            "keel/payments/provision-postgres-1763100000"
        );
        assert_eq!(
            stack_json["spec"]["repoDir"],
            "tenants/payments/infra/postgres"
        );
        assert_eq!(stack_json["spec"]["shallow"], true);
        assert_eq!(
            stack_json["spec"]["gitAuth"]["basicAuth"]["userName"],
            serde_json::json!({"type": "Secret", "secret": {"name": "forge-git-auth", "key": "username"}})
        );
        assert_eq!(PkoStack::from_json_value(stack_json).unwrap(), *stack);

        let yaml = bundle.to_yaml().expect("yaml");
        assert!(!yaml.contains("kind: GitRepository"));
        assert!(yaml.contains("kind: Stack"));
        assert!(yaml.contains("preview: true"));
        assert_eq!(PkoStack::from_yaml(&yaml).unwrap(), *stack);
    }

    #[test]
    fn tenant_stack_synthesizes_namespace_isolation_and_output_secret_binding() {
        let mut params = TenantStackParams::new(
            "analytics",
            "postgres-managed",
            "keel/analytics-postgres/prod",
            "s3://pulumi-state",
            test_git_source("tenants/analytics/infra/postgres-managed"),
        );
        params.output_secret_name = Some("postgres-companion-outputs".to_string());

        let stack = synthesize_tenant_stack(&params).expect("valid tenant stack");
        assert_eq!(stack.metadata.name, "postgres-managed");
        assert_eq!(stack.metadata.namespace, "tenant-analytics");
        assert_eq!(
            stack.spec.service_account_name.as_deref(),
            Some("tenant-analytics-sa")
        );
        assert_eq!(
            stack.spec.output_secret_name.as_deref(),
            Some("postgres-companion-outputs")
        );
        assert!(!stack.is_protected());
        assert!(!stack.is_preview());

        let values_from = stack
            .helm_values_from_ref()
            .expect("helm valuesFrom secret binding");
        assert_eq!(values_from.kind, "Secret");
        assert_eq!(values_from.name, "postgres-companion-outputs");
        assert_eq!(values_from.optional, Some(false));

        let json = stack.to_json_value().expect("json value");
        assert_eq!(
            json["spec"]["outputSecretName"],
            "postgres-companion-outputs"
        );

        // Also test tenant stack without companion output_secret_name
        params.output_secret_name = None;
        params.service_account_name = Some("custom-analytics-sa".to_string());
        let stack_no_secret = synthesize_tenant_stack(&params).expect("valid tenant stack");
        assert!(stack_no_secret.helm_values_from_ref().is_none());
        assert_eq!(
            stack_no_secret.spec.service_account_name.as_deref(),
            Some("custom-analytics-sa")
        );

        params.docker_host =
            Some("tcp://keel-docker-proxy.flux-system.svc.cluster.local:2375".to_string());
        let stack_docker =
            synthesize_tenant_stack(&params).expect("valid tenant stack with docker host");
        let docker_json = stack_docker.to_json_value().expect("json value");
        let env =
            docker_json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["containers"]
                [0]["env"]
                .as_array()
                .unwrap();
        assert!(env.iter().any(|e| {
            e["name"] == "DOCKER_HOST"
                && e["value"] == "tcp://keel-docker-proxy.flux-system.svc.cluster.local:2375"
        }));

        // Verify kubeconfigSecretName in same tenant namespace mounts secret volume
        params.config.insert(
            "kubeconfigSecretName".to_string(),
            "workload-dev-kubeconfig".to_string(),
        );
        params.config.insert(
            "kubeconfigSecretNamespace".to_string(),
            "tenant-analytics".to_string(),
        );
        let stack_kc =
            synthesize_tenant_stack(&params).expect("valid tenant stack with kubeconfig");
        let kc_json = stack_kc.to_json_value().expect("json value");
        let volumes =
            kc_json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]["volumes"]
                .as_array()
                .unwrap();
        assert_eq!(volumes[0]["name"], "target-kubeconfig");
        assert_eq!(
            volumes[0]["secret"]["secretName"],
            "workload-dev-kubeconfig"
        );
        let mounts = kc_json["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"]
            ["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap();
        assert_eq!(mounts[0]["name"], "target-kubeconfig");
        assert_eq!(mounts[0]["mountPath"], "/etc/keel/kubeconfig");
        assert_eq!(mounts[0]["readOnly"], true);
    }

    #[test]
    fn validation_rejects_invalid_branches_traversals_and_reserved_names() {
        // Invalid branch prefix
        let mut bad_preview = EphemeralPreviewParams::for_action(
            "10",
            "alpha",
            "deploy",
            1_700_000_000,
            "tenants/alpha/infra/vm",
            "keel/vm/prod",
            "s3://pulumi-state",
        )
        .unwrap();
        bad_preview.branch = "main".to_string();
        assert!(synthesize_ephemeral_preview(&bad_preview).is_err());

        // Branch missing numeric timestamp
        assert!(validate_preview_branch("alpha", "keel/alpha/deploy-notanumber").is_err());
        assert!(validate_preview_branch("alpha", "keel/alpha/notimestamp").is_err());
        assert!(build_preview_branch("alpha", "deploy", 0).is_err());

        // Path traversal in repo_dir
        bad_preview.branch = "keel/alpha/deploy-1700000000".to_string();
        bad_preview.repo_dir = "../secrets".to_string();
        assert!(synthesize_ephemeral_preview(&bad_preview).is_err());

        // Cross-namespace slash in output_secret_name
        let mut bad_tenant = TenantStackParams::new(
            "alpha",
            "db",
            "keel/db/prod",
            "s3://pulumi-state",
            test_git_source("tenants/alpha/infra/db"),
        );
        bad_tenant.output_secret_name = Some("other-ns/secret".to_string());
        assert!(synthesize_tenant_stack(&bad_tenant).is_err());

        // Reserved management stack name in tenant namespace
        bad_tenant.output_secret_name = None;
        bad_tenant.name = MANAGEMENT_CORE_STACK_NAME.to_string();
        assert!(synthesize_tenant_stack(&bad_tenant).is_err());

        // Invalid uppercase DNS label
        bad_tenant.name = "InvalidUpper".to_string();
        assert!(synthesize_tenant_stack(&bad_tenant).is_err());

        // Empty PR ID
        assert!(sanitize_pr_id("   ").is_err());
    }

    #[test]
    fn synthesize_k8s_manifest_pulumi_yaml_creates_config_group_and_configuration() {
        let raw_manifests = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: demo-cm\n";
        let props = vec!["replicas".to_string(), "imageTag".to_string()];

        let yaml =
            synthesize_k8s_manifest_pulumi_yaml("demo-manifest", raw_manifests, &props, false)
                .expect("should synthesize Pulumi.yaml");
        let parsed: serde_json::Value = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(parsed["name"], "demo-manifest");
        assert_eq!(parsed["runtime"], "yaml");
        assert_eq!(parsed["configuration"]["replicas"]["type"], "string");
        assert_eq!(parsed["configuration"]["imageTag"]["type"], "string");
        assert_eq!(
            parsed["resources"]["manifests"]["type"],
            "kubernetes:yaml/v2:ConfigGroup"
        );
        assert_eq!(
            parsed["resources"]["manifests"]["properties"]["yaml"],
            raw_manifests
        );
        assert!(parsed["resources"].get("targetCluster").is_none());

        let yaml_remote =
            synthesize_k8s_manifest_pulumi_yaml("demo-manifest", raw_manifests, &props, true)
                .expect("should synthesize remote Pulumi.yaml");
        let parsed_remote: serde_json::Value = serde_yml::from_str(&yaml_remote).unwrap();
        assert_eq!(
            parsed_remote["resources"]["targetCluster"]["type"],
            "pulumi:providers:kubernetes"
        );
        assert_eq!(
            parsed_remote["resources"]["manifests"]["options"]["provider"],
            "${targetCluster}"
        );
        assert!(
            parsed_remote["configuration"]
                .get("kubeconfigSecretName")
                .is_some()
        );
    }

    #[test]
    fn preview_inherits_tenant_stack_identity_without_tenant_secrets() {
        let mut tparams = TenantStackParams::new(
            "acme",
            "web",
            "organization/web/acme-web",
            "s3://pulumi-state/tenants/acme?region=us-east-1",
            test_git_source("tenants/acme/apps/web"),
        );
        tparams
            .config
            .insert("keel:provider".to_string(), "kubernetes".to_string());
        tparams.env_refs.insert(
            "API_TOKEN".to_string(),
            PkoEnvRef::from_secret("web-secrets", "token"),
        );
        let tenant_stack = synthesize_tenant_stack(&tparams).unwrap();

        let mut params = EphemeralPreviewParams::for_action(
            "9",
            "acme",
            "deploy",
            1_700_000_000,
            "tenants/acme/apps/web",
            "organization/web-pr-9/acme",
            "s3://pulumi-state",
        )
        .unwrap();
        params.inherit_tenant_stack(&tenant_stack);
        let bundle = synthesize_ephemeral_preview(&params).unwrap();
        assert_eq!(bundle.stack.spec.stack, "organization/web/acme-web");
        assert_eq!(
            bundle.stack.spec.backend.as_deref(),
            Some("s3://pulumi-state/tenants/acme?region=us-east-1")
        );
        assert_eq!(
            bundle
                .stack
                .metadata
                .labels
                .get(LABEL_PROVIDER)
                .map(String::as_str),
            Some("kubernetes")
        );
        assert!(bundle.stack.spec.env_refs.is_empty());
        assert!(bundle.stack.is_preview());
        assert_eq!(bundle.stack.spec.destroy_on_finalize, Some(false));
    }

    #[test]
    fn preview_never_inherits_tenant_protect_config() {
        for key in CONFIG_PROTECT_KEYS {
            let mut tparams = TenantStackParams::new(
                "acme",
                "web",
                "organization/web/acme-web",
                "s3://pulumi-state/tenants/acme",
                test_git_source("tenants/acme/apps/web"),
            );
            tparams.config.insert(key.to_string(), "true".to_string());
            tparams
                .config
                .insert("aws:region".to_string(), "eu-west-1".to_string());
            let tenant_stack = synthesize_tenant_stack(&tparams).unwrap();
            assert_eq!(
                tenant_stack.spec.config.get(key).map(String::as_str),
                Some("true")
            );

            let mut params = EphemeralPreviewParams::for_action(
                "9",
                "acme",
                "deploy",
                1_700_000_000,
                "tenants/acme/apps/web",
                "organization/web-pr-9/acme",
                "s3://pulumi-state",
            )
            .unwrap();
            params.inherit_tenant_stack(&tenant_stack);
            assert!(!params.config.contains_key(key), "protect key inherited");
            assert_eq!(
                params.config.get("aws:region").map(String::as_str),
                Some("eu-west-1")
            );
            let bundle = synthesize_ephemeral_preview(&params).unwrap();
            assert!(!bundle.stack.spec.config.contains_key(key));
            assert!(
                !bundle.stack.is_protected(),
                "preview must stay collectable"
            );
        }
    }

    fn tenant_stack_with_creds(backend: &str) -> PkoStack {
        let mut tparams = TenantStackParams::new(
            "acme",
            "vm1",
            "organization/aws-vm/acme-vm1",
            backend,
            test_git_source("tenants/acme/infra/vm1"),
        );
        tparams
            .config
            .insert("keel:provider".to_string(), "aws".to_string());
        attach_s3_credentials_env_refs(&mut tparams.env_refs, "keel-s3-credentials");
        tparams.env_refs.insert(
            "GITHUB_TOKEN".to_string(),
            PkoEnvRef::from_secret("keel-github-token", "token"),
        );
        tparams.secrets_ref.insert(
            "aws:accessKey".to_string(),
            PkoEnvRef::from_secret("vm1-secrets", "awsAccessKeyId"),
        );
        tparams.secrets_ref.insert(
            "aws:secretKey".to_string(),
            PkoEnvRef::from_secret("vm1-secrets", "awsSecretAccessKey"),
        );
        tparams.env_refs.insert(
            "HCLOUD_TOKEN".to_string(),
            PkoEnvRef::from_secret("hetzner-creds", "token"),
        );
        synthesize_tenant_stack(&tparams).unwrap()
    }

    fn base_preview_params(pr: &str) -> EphemeralPreviewParams {
        EphemeralPreviewParams::for_action(
            pr,
            "acme",
            "provision",
            1_700_000_000,
            "tenants/acme/infra/vm1",
            format!("organization/vm1-pr-{pr}/acme"),
            "s3://pulumi-state?region=us-east-1&endpoint=seaweedfs",
        )
        .unwrap()
    }

    /// OPEN-9b: brand-new stacks (no tenant Stack to inherit) and existing stacks both preview
    /// against the tenant prefix; a foreign/base backend on the tenant Stack is never adopted.
    #[test]
    fn preview_backend_is_always_tenant_scoped() {
        let tenant_backend = "s3://pulumi-state/tenants/acme?region=us-east-1&endpoint=seaweedfs";

        let new_stack = synthesize_ephemeral_preview(&base_preview_params("11")).unwrap();
        assert_eq!(
            new_stack.stack.spec.backend.as_deref(),
            Some(tenant_backend)
        );

        let mut existing = base_preview_params("12");
        existing.inherit_tenant_stack(&tenant_stack_with_creds(tenant_backend));
        let existing = synthesize_ephemeral_preview(&existing).unwrap();
        assert_eq!(existing.stack.spec.backend.as_deref(), Some(tenant_backend));

        let mut legacy = base_preview_params("13");
        legacy.inherit_tenant_stack(&tenant_stack_with_creds("s3://pulumi-state"));
        let legacy = synthesize_ephemeral_preview(&legacy).unwrap();
        assert_eq!(legacy.stack.spec.backend.as_deref(), Some(tenant_backend));

        let mut other_tenant = base_preview_params("14");
        other_tenant.backend_url = "s3://pulumi-state/tenants/evil".to_string();
        let other_tenant = synthesize_ephemeral_preview(&other_tenant).unwrap();
        assert_eq!(
            other_tenant.stack.spec.backend.as_deref(),
            Some("s3://pulumi-state/tenants/acme")
        );

        assert!(is_tenant_scoped_backend("file:///tmp/state", "acme"));
        assert!(is_tenant_scoped_backend(
            "s3://b/tenants/acme/sub?x=1",
            "acme"
        ));
        assert!(!is_tenant_scoped_backend("s3://b/tenants/acme2", "acme"));
        assert_eq!(
            tenant_scoped_preview_backend("file:///tmp/state", "acme"),
            "file:///tmp/state"
        );
    }

    #[test]
    fn preview_credentials_copy_only_referenced_secrets_and_rewrite_refs() {
        let tenant_stack =
            tenant_stack_with_creds("s3://pulumi-state/tenants/acme?region=us-east-1");
        let mut params = base_preview_params("#42");
        params.inherit_tenant_stack(&tenant_stack);
        let excluded = BTreeSet::from([
            "keel-s3-credentials".to_string(),
            "keel-github-token".to_string(),
        ]);
        let copies = params
            .inherit_tenant_credentials(&tenant_stack, &excluded)
            .unwrap();

        let names: Vec<_> = copies.iter().map(|c| c.source_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["hetzner-creds", "keel-pulumi-passphrase", "vm1-secrets"]
        );
        let pass = copies
            .iter()
            .find(|c| c.source_name == PULUMI_PASSPHRASE_SECRET_NAME)
            .unwrap();
        assert_eq!(pass.target_name, "keel-preview-42-keel-pulumi-passphrase");
        assert_eq!(
            pass.keys,
            BTreeSet::from([PULUMI_PASSPHRASE_SECRET_KEY.to_string()])
        );
        let vm = copies
            .iter()
            .find(|c| c.source_name == "vm1-secrets")
            .unwrap();
        assert_eq!(vm.target_name, "keel-preview-42-vm1-secrets");
        assert_eq!(
            vm.keys,
            BTreeSet::from([
                "awsAccessKeyId".to_string(),
                "awsSecretAccessKey".to_string()
            ])
        );
        let hz = copies
            .iter()
            .find(|c| c.source_name == "hetzner-creds")
            .unwrap();
        assert_eq!(hz.keys, BTreeSet::from(["token".to_string()]));

        let bundle = synthesize_ephemeral_preview(&params).unwrap();
        let spec = &bundle.stack.spec;
        assert_eq!(
            spec.secrets_ref["aws:accessKey"],
            PkoEnvRef::from_secret("keel-preview-42-vm1-secrets", "awsAccessKeyId")
        );
        assert_eq!(
            spec.env_refs["HCLOUD_TOKEN"],
            PkoEnvRef::from_secret("keel-preview-42-hetzner-creds", "token")
        );
        // Excluded (runner-mirrored) secrets are neither copied nor referenced.
        assert!(!spec.env_refs.contains_key("GITHUB_TOKEN"));
        assert!(!spec.env_refs.contains_key("AWS_ACCESS_KEY_ID"));
        // No ref in the preview points at a tenant-namespace Secret name.
        // Assert on the ref names only; failure output must never echo Secret-sourced structures.
        assert!(
            spec.env_refs
                .values()
                .chain(spec.secrets_ref.values())
                .all(|r| r.secret.name.starts_with("keel-preview-42-")),
            "a preview Stack ref points outside the keel-preview-42-* Secret copies"
        );
        // Proposed values never leak through Debug.
        let mut with_data = vm.clone();
        with_data.proposed_data = Some(BTreeMap::from([(
            "awsSecretAccessKey".to_string(),
            "SUPERSECRET".to_string(),
        )]));
        assert!(!format!("{with_data:?}").contains("SUPERSECRET"));
    }

    #[test]
    fn preview_secret_copy_names_are_bounded_and_stable() {
        let long = "a".repeat(60);
        let n1 = preview_secret_copy_name("7", &long).unwrap();
        let n2 = preview_secret_copy_name("7", &long).unwrap();
        assert_eq!(n1, n2);
        assert!(n1.len() <= 63 && n1.starts_with("keel-preview-7-"));
        assert_ne!(n1, preview_secret_copy_name("8", &long).unwrap());
        assert_eq!(
            preview_secret_copy_name("7", "my.creds").unwrap(),
            "keel-preview-7-my-creds"
        );
        assert!(preview_secret_copy_name("7", "Bad/Name").is_err());
    }

    /// Recursively collects every `PULUMI_CONFIG_PASSPHRASE*` env entry in a serialized document.
    fn passphrase_env_entries(v: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if map
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|n| n.starts_with(PULUMI_CONFIG_PASSPHRASE_ENV))
                {
                    out.push(v.clone());
                }
                map.values().for_each(|c| passphrase_env_entries(c, out));
            }
            serde_json::Value::Array(items) => {
                items.iter().for_each(|c| passphrase_env_entries(c, out));
            }
            _ => {}
        }
    }

    /// Guard: no synthesized Stack (management, tenant, preview; JSON or YAML) ever carries a
    /// literal (let alone empty) `PULUMI_CONFIG_PASSPHRASE`; it is always a `secretKeyRef` to
    /// the namespace-local passphrase Secret.
    #[test]
    fn synthesized_stacks_never_carry_a_literal_or_empty_passphrase() {
        let mgmt = synthesize_management_core_stack(&ManagementCoreStackParams::new(
            IN_CLUSTER_S3_BACKEND_URL,
            "flux-system",
            "clusters/management",
        ))
        .unwrap();
        let mut tparams = TenantStackParams::new(
            "acme",
            "vm1",
            "organization/vm1/acme-vm1",
            "s3://pulumi-state/tenants/acme",
            test_git_source("tenants/acme/infra/vm1"),
        );
        tparams.docker_host = Some("tcp://proxy:2375".to_string());
        let tenant = synthesize_tenant_stack(&tparams).unwrap();
        let mut preview_params = base_preview_params("42");
        preview_params.docker_host = Some("tcp://proxy:2375".to_string());
        let preview = synthesize_ephemeral_preview(&preview_params).unwrap();

        for (stack, expected_secret) in [
            (&mgmt, PULUMI_PASSPHRASE_SECRET_NAME),
            (&tenant, PULUMI_PASSPHRASE_SECRET_NAME),
            (&preview.stack, "keel-preview-42-keel-pulumi-passphrase"),
        ] {
            let json = stack.to_json_value().unwrap();
            let mut entries = Vec::new();
            passphrase_env_entries(&json, &mut entries);
            assert_eq!(entries.len(), 1, "{}: {entries:?}", stack.metadata.name);
            let entry = &entries[0];
            assert_eq!(entry["name"], PULUMI_CONFIG_PASSPHRASE_ENV);
            assert!(entry.get("value").is_none(), "literal passphrase: {entry}");
            assert_eq!(
                entry["valueFrom"]["secretKeyRef"],
                serde_json::json!({
                    "name": expected_secret,
                    "key": PULUMI_PASSPHRASE_SECRET_KEY,
                })
            );
            let tpl = stack.spec.workspace_template.as_ref().unwrap();
            assert_eq!(
                tpl.passphrase_secret_ref().map(|r| r.name.as_str()),
                Some(expected_secret)
            );

            // Text-level guard over the serialized YAML/JSON forms.
            let yaml = stack.to_yaml().unwrap();
            let compact = serde_json::to_string(&json).unwrap();
            for needle in [
                r#""name":"PULUMI_CONFIG_PASSPHRASE","value""#,
                r#""value":"""#,
            ] {
                assert!(!compact.contains(needle), "{needle} in {compact}");
            }
            assert!(!yaml.contains("value: ''"), "{yaml}");
            assert!(!yaml.contains("value: \"\""), "{yaml}");
            // Round-trip keeps the secretKeyRef form.
            assert_eq!(&PkoStack::from_yaml(&yaml).unwrap(), stack);
        }
    }

    #[test]
    fn passphrase_env_cannot_be_overridden_with_a_literal() {
        let tpl = PkoWorkspaceTemplate::restricted("tenant-pass")
            .with_env_var(PULUMI_CONFIG_PASSPHRASE_ENV, "")
            .with_env_var("PULUMI_CONFIG_PASSPHRASE_FILE", "/tmp/x")
            .with_env_var("DOCKER_HOST", "tcp://proxy:2375");
        let env = &tpl.spec.pod_template.spec.containers[0].env;
        assert_eq!(env.len(), 2);
        assert_eq!(
            env[0],
            WorkspaceEnvVar::from_secret(
                PULUMI_CONFIG_PASSPHRASE_ENV,
                "tenant-pass",
                PULUMI_PASSPHRASE_SECRET_KEY
            )
        );
        assert_eq!(
            env[1],
            WorkspaceEnvVar::literal("DOCKER_HOST", "tcp://proxy:2375")
        );
        assert_eq!(tpl.passphrase_secret_ref().unwrap().name, "tenant-pass");

        // An invalid passphrase Secret name is rejected at synthesis time.
        let mut tparams = TenantStackParams::new(
            "acme",
            "vm1",
            "organization/vm1/acme-vm1",
            "s3://pulumi-state/tenants/acme",
            test_git_source("tenants/acme/infra/vm1"),
        );
        tparams.passphrase_secret_name = String::new();
        assert!(synthesize_tenant_stack(&tparams).is_err());
        let mut mparams = ManagementCoreStackParams::new(
            IN_CLUSTER_S3_BACKEND_URL,
            "flux-system",
            "clusters/management",
        );
        mparams.passphrase_secret_name = "Bad Name".to_string();
        assert!(synthesize_management_core_stack(&mparams).is_err());
    }

    fn keys(k: &[&str]) -> BTreeSet<String> {
        k.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn git_auth_maps_flux_secret_keys_to_pko_auth_modes() {
        let https = "https://forge.example/keel/gitops.git";
        // Platform-written read-only `git-credentials` (username/password) -> basicAuth.
        let basic = PkoGitAuth::from_flux_secret("git-credentials", https, &BTreeSet::new())
            .unwrap()
            .basic_auth
            .expect("basic auth");
        assert_eq!(
            basic.user_name,
            PkoEnvRef::from_secret("git-credentials", "username")
        );
        assert_eq!(
            basic.password,
            PkoEnvRef::from_secret("git-credentials", "password")
        );

        // SSH deploy key -> sshAuth (with optional passphrase), inferred or explicit.
        let ssh_url = "ssh://git@forge.example:22/keel/gitops.git";
        let ssh =
            PkoGitAuth::from_flux_secret("git-credentials", ssh_url, &BTreeSet::new()).unwrap();
        assert_eq!(
            ssh.ssh_auth.as_ref().unwrap().ssh_private_key,
            PkoEnvRef::from_secret("git-credentials", "identity")
        );
        assert!(ssh.basic_auth.is_none() && ssh.access_token.is_none());
        let ssh_pw = PkoGitAuth::from_flux_secret(
            "git-credentials",
            "git@forge.example:keel/gitops.git",
            &keys(&["identity", "identity.pub", "known_hosts", "password"]),
        )
        .unwrap();
        assert_eq!(
            ssh_pw.ssh_auth.unwrap().password,
            Some(PkoEnvRef::from_secret("git-credentials", "password"))
        );

        // Token-only Secrets -> accessToken.
        let token = PkoGitAuth::from_flux_secret("c", https, &keys(&["bearerToken"])).unwrap();
        assert_eq!(
            token.access_token,
            Some(PkoEnvRef::from_secret("c", "bearerToken"))
        );
        let pat = PkoGitAuth::from_flux_secret("c", https, &keys(&["password", "ca.crt"])).unwrap();
        assert_eq!(
            pat.access_token,
            Some(PkoEnvRef::from_secret("c", "password"))
        );

        // Unusable Secrets and bad names fail closed.
        assert!(PkoGitAuth::from_flux_secret("c", https, &keys(&["ca.crt"])).is_err());
        assert!(
            PkoGitAuth::from_flux_secret("c", ssh_url, &keys(&["username", "password"])).is_err()
        );
        assert!(PkoGitAuth::from_flux_secret("Bad/Name", https, &BTreeSet::new()).is_err());

        let json = serde_json::to_value(
            PkoGitAuth::from_flux_secret("git-credentials", https, &BTreeSet::new()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"basicAuth": {
                "userName": {"type": "Secret", "secret": {"name": "git-credentials", "key": "username"}},
                "password": {"type": "Secret", "secret": {"name": "git-credentials", "key": "password"}}
            }})
        );
    }

    #[test]
    fn git_source_urls_must_be_network_git_urls() {
        for ok in [
            "https://forge.example/keel/gitops.git",
            "https://forgejo.keel.local:8888/keel/infra-gitops.git",
            "https://[::1]:3000/keel/gitops.git",
            "ssh://git@forge.example:2222/keel/gitops.git",
            "git@github.com:acme/gitops.git",
        ] {
            assert!(validate_git_source_url(ok).is_ok(), "{ok}");
            assert!(validate_flux_git_source_url(ok).is_ok(), "{ok}");
        }
        // PKO (Pulumi SDK `gitutil.ParseGitRepoURL`) rejects plain http; Flux accepts it.
        for http in [
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
            "http://[::1]:3000/keel/gitops.git",
            "HTTP://forge.example/keel/gitops.git",
        ] {
            let err = validate_git_source_url(http).unwrap_err().to_string();
            assert!(err.contains("plain http://"), "{err}");
            assert!(validate_flux_git_source_url(http).is_ok(), "{http}");
        }
        for (i, bad) in [
            "",
            " https://forge.example/x.git",
            "/tmp/keel-gitops.git",
            "./keel-gitops.git",
            "file:///tmp/keel-gitops.git",
            "ext::sh -c touch% /tmp/pwned",
            "fd::17",
            "-uhttps://x",
            "https://",
            "https://forge.example/x.git\nevil",
            "forge.example:keel/gitops.git",
            "https://keel:token@forge.example/keel/gitops.git",
        ]
        .into_iter()
        .enumerate()
        {
            // Index only: some cases embed sample credentials.
            assert!(validate_git_source_url(bad).is_err(), "bad URL case #{i}");
        }
        let err = validate_git_source_url("https://keel:s3cr3t-token@forge.example/x.git")
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("s3cr3t-token"),
            "git source URL error echoed the token"
        );
        assert!(is_ssh_git_url("git@github.com:acme/gitops.git"));
        assert!(!is_ssh_git_url("https://github.com/acme/gitops.git"));

        let mut src = test_git_source("tenants/acme/infra/vm");
        assert!(src.validate().is_ok());
        src.branch = Some("main..evil".to_string());
        assert!(src.validate().is_err());
        src.branch = None;
        assert!(src.validate().is_err(), "needs branch or commit");
        src.commit = Some("0123456789abcdef0123456789abcdef01234567".to_string());
        assert!(src.validate().is_ok());
        src.branch = Some("main".to_string());
        assert!(src.validate().is_err(), "branch and commit are exclusive");
        let mut traversal = test_git_source("tenants/acme/../beta");
        assert!(traversal.validate().is_err());
        traversal.repo_dir = Some("/etc".to_string());
        assert!(traversal.validate().is_err());
    }

    #[test]
    fn tenant_and_preview_workspaces_trust_the_public_keel_ca_bundle() {
        let expected_pod_spec_fragment = serde_json::json!({
            "volumes": [{
                "name": "keel-ca",
                "configMap": {"name": "keel-ca-bundle", "optional": true}
            }],
            "initContainers": [{
                "name": "fetch",
                "env": [{"name": "SSL_CERT_DIR", "value": "/etc/ssl/certs:/etc/keel-ca"}],
                "volumeMounts": [{"name": "keel-ca", "mountPath": "/etc/keel-ca", "readOnly": true}]
            }]
        });
        let check = |stack: &PkoStack| {
            let doc = stack.to_json_value().unwrap();
            let pod = &doc["spec"]["workspaceTemplate"]["spec"]["podTemplate"]["spec"];
            assert_eq!(pod["volumes"], expected_pod_spec_fragment["volumes"]);
            assert_eq!(
                pod["initContainers"],
                expected_pod_spec_fragment["initContainers"]
            );
            let containers = pod["containers"].as_array().unwrap();
            assert_eq!(containers.len(), 1);
            let pulumi = &containers[0];
            assert_eq!(pulumi["name"], "pulumi");
            assert!(pulumi["env"].as_array().unwrap().contains(
                &serde_json::json!({"name": "SSL_CERT_DIR", "value": "/etc/ssl/certs:/etc/keel-ca"})
            ));
            assert!(pulumi["volumeMounts"].as_array().unwrap().contains(
                &serde_json::json!({"name": "keel-ca", "mountPath": "/etc/keel-ca", "readOnly": true})
            ));
            // Only the public CA ConfigMap; never the CA / wildcard Secrets (private keys).
            let text = doc.to_string();
            assert!(!text.contains("keel-ca-secret") && !text.contains("keel-wildcard-tls"));
            // Round-trips through the typed model.
            assert_eq!(&PkoStack::from_json_value(doc).unwrap(), stack);
        };
        let tenant = synthesize_tenant_stack(&TenantStackParams::new(
            "acme",
            "vm",
            "organization/vm/acme",
            "s3://pulumi-state/tenants/acme",
            test_git_source("tenants/acme/infra/vm"),
        ))
        .unwrap();
        check(&tenant);
        let mut params = base_preview_params("42");
        params.repo_url = Some("https://forgejo.keel.local:8888/keel/infra-gitops.git".to_string());
        check(&synthesize_ephemeral_preview(&params).unwrap().stack);

        // Idempotent, and kept alongside other mounts (target kubeconfig Secret volume).
        let tpl = PkoWorkspaceTemplate::restricted("p")
            .with_secret_volume("target-kubeconfig", "kc", "/etc/keel/kubeconfig")
            .with_keel_ca_trust()
            .with_keel_ca_trust();
        let pod = &tpl.spec.pod_template.spec;
        assert_eq!(pod.volumes.len(), 2);
        assert_eq!(pod.init_containers.len(), 1);
        assert_eq!(pod.init_containers[0].env.len(), 1);
        assert_eq!(pod.containers[0].volume_mounts.len(), 2);
        // The management Stack (fluxSource, operator-side fetch) is untouched.
        assert!(
            PkoWorkspaceTemplate::restricted("p")
                .spec
                .pod_template
                .spec
                .init_containers
                .is_empty()
        );
    }

    #[test]
    fn pko_project_repo_maps_the_in_cluster_forgejo_to_its_https_origin() {
        let flux = "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git";
        for origin in [
            "https://forgejo.keel.local:8888",
            "https://forgejo.keel.local:8888/",
        ] {
            assert_eq!(
                pko_project_repo_url(flux, Some(origin)).unwrap(),
                "https://forgejo.keel.local:8888/keel/infra-gitops.git"
            );
        }
        assert_eq!(
            pko_project_repo_url(
                "http://forgejo-http.keel-system.svc:3000/acme/gitops.git",
                Some("https://forgejo.example.org")
            )
            .unwrap(),
            "https://forgejo.example.org/acme/gitops.git"
        );
        // External forges (https / ssh / scp-like) pass through unchanged.
        for external in [
            "https://github.com/acme/gitops.git",
            "ssh://git@forge.example:2222/acme/gitops.git",
            "git@github.com:acme/gitops.git",
        ] {
            assert_eq!(pko_project_repo_url(external, None).unwrap(), external);
            assert_eq!(
                pko_project_repo_url(external, Some("https://forgejo.keel.local:8888")).unwrap(),
                external
            );
        }
        // No configured origin: fail closed with an actionable message.
        let err = pko_project_repo_url(flux, None).unwrap_err().to_string();
        assert!(err.contains("forge.publicUrl"), "{err}");
        // Any other http URL fails closed (never silently upgraded to https).
        let err = pko_project_repo_url(
            "http://gitea.example.org/acme/gitops.git",
            Some("https://forgejo.keel.local:8888"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("plain http://"), "{err}");
        for bad_origin in [
            "http://forgejo.keel.local:8888",
            "https://forgejo.keel.local/sub",
            "https://user:pw@forgejo.keel.local",
            "https://",
            "forgejo.keel.local",
        ] {
            assert!(
                pko_project_repo_url(flux, Some(bad_origin)).is_err(),
                "{bad_origin}"
            );
        }
        assert!(
            pko_project_repo_url(
                "http://forgejo-http.keel-system.svc.cluster.local:3000",
                Some("https://forgejo.keel.local:8888")
            )
            .is_err(),
            "no repository path"
        );
        assert!(pko_project_repo_url("/tmp/repo.git", Some("https://f.example")).is_err());
        assert!(is_in_cluster_forgejo_http_url(flux));
        assert!(!is_in_cluster_forgejo_http_url(
            "https://forgejo-http.keel-system.svc.cluster.local/keel/x.git"
        ));
        assert!(!is_in_cluster_forgejo_http_url(
            "http://forgejo-http.keel-system.svc.cluster.local.evil.example/keel/x.git"
        ));
    }

    #[test]
    fn tenant_stacks_clone_their_git_source_and_management_keeps_flux_source() {
        let source = PkoGitSource::branch(
            "https://forgejo.keel.local:8888/keel/infra-gitops.git",
            "main",
            "tenants/default/infra/kind",
        )
        .with_auth(Some(
            PkoGitAuth::from_flux_secret(
                "git-credentials",
                "https://forgejo.keel.local:8888/keel/infra-gitops.git",
                &BTreeSet::new(),
            )
            .unwrap(),
        ));
        let stack = synthesize_tenant_stack(&TenantStackParams::new(
            "default",
            "kind",
            "organization/kind-cluster/default-kind",
            "s3://pulumi-state/tenants/default",
            source.clone(),
        ))
        .unwrap();
        assert!(stack.spec.flux_source.is_none());
        assert_eq!(stack.spec.git_source.as_ref(), Some(&source));
        let yaml = stack.to_yaml().unwrap();
        assert!(!yaml.contains("fluxSource"));
        let doc: serde_json::Value = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(
            doc["spec"]["projectRepo"],
            "https://forgejo.keel.local:8888/keel/infra-gitops.git"
        );
        assert_eq!(doc["spec"]["repoDir"], "tenants/default/infra/kind");
        assert_eq!(doc["spec"]["branch"], "main");
        assert_eq!(
            doc["spec"]["shallow"], true,
            "shallow clone of the tenant source"
        );
        assert_eq!(
            doc["spec"]["gitAuth"]["basicAuth"]["password"]["secret"]["name"],
            "git-credentials"
        );
        assert_eq!(PkoStack::from_yaml(&yaml).unwrap(), stack);

        // Local paths / helpers are never committed into a tenant Stack.
        let mut local = TenantStackParams::new(
            "default",
            "kind",
            "organization/kind-cluster/default-kind",
            "s3://pulumi-state/tenants/default",
            PkoGitSource::branch("/tmp/keel-gitops.git", "main", "tenants/default/infra/kind"),
        );
        assert!(synthesize_tenant_stack(&local).is_err());
        local.git_source.repo_dir = None;
        local.git_source.project_repo = "https://forge.example/x.git".to_string();
        assert!(
            synthesize_tenant_stack(&local).is_err(),
            "repo_dir is required"
        );

        // The flux-system management Stack keeps its same-namespace Flux source.
        let mgmt = synthesize_management_core_stack(&ManagementCoreStackParams::new(
            "s3://pulumi-state",
            "flux-system",
            "clusters/management",
        ))
        .unwrap();
        assert!(mgmt.spec.git_source.is_none());
        assert_eq!(
            mgmt.spec
                .flux_source
                .as_ref()
                .map(|f| f.source_ref.name.as_str()),
            Some("flux-system")
        );
        let mgmt_yaml = mgmt.to_yaml().unwrap();
        assert!(!mgmt_yaml.contains("projectRepo"));
        assert_eq!(PkoStack::from_yaml(&mgmt_yaml).unwrap(), mgmt);

        // Stacks committed by earlier releases (fluxSource) still parse, without a Git source.
        let legacy = PkoStack::from_yaml(
            "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: vm\n  namespace: tenant-default\nspec:\n  stack: organization/vm/default-vm\n  fluxSource:\n    sourceRef:\n      apiVersion: source.toolkit.fluxcd.io/v1\n      kind: GitRepository\n      name: keel-gitops\n    dir: tenants/default/infra/vm\n  resyncFrequencySeconds: 300\n",
        )
        .unwrap();
        assert!(legacy.spec.git_source.is_none());
        assert_eq!(legacy.spec.resync_frequency_seconds, Some(300));
    }

    #[test]
    fn preview_inherits_the_tenant_stack_repo_and_mirrors_ssh_auth() {
        let tenant_stack = synthesize_tenant_stack(&TenantStackParams::new(
            "acme",
            "vm1",
            "organization/aws-vm/acme-vm1",
            "s3://pulumi-state/tenants/acme",
            PkoGitSource::branch(
                "ssh://git@forge.example/acme/gitops.git",
                "main",
                "tenants/acme/infra/vm1",
            ),
        ))
        .unwrap();
        let mut params = base_preview_params("31");
        params.inherit_tenant_stack(&tenant_stack);
        assert_eq!(
            params.tenant_repo_url.as_deref(),
            Some("ssh://git@forge.example/acme/gitops.git")
        );
        params.repo_url = params.tenant_repo_url.clone();
        params.git_secret_name = Some("keel-preview-31-git-credentials".to_string());
        params.git_secret_keys = keys(&["identity", "known_hosts"]);
        let preview = synthesize_ephemeral_preview(&params).unwrap();
        let src = preview.stack.spec.git_source.unwrap();
        assert_eq!(src.project_repo, "ssh://git@forge.example/acme/gitops.git");
        assert_eq!(src.branch.as_deref(), Some(params.branch.as_str()));
        assert_eq!(src.shallow, Some(true));
        assert_eq!(
            src.git_auth.unwrap().ssh_auth.unwrap().ssh_private_key,
            PkoEnvRef::from_secret("keel-preview-31-git-credentials", "identity")
        );

        // Public source: no gitAuth at all.
        let mut public = base_preview_params("32");
        public.repo_url = Some("https://forge.example/acme/gitops.git".to_string());
        public.git_secret_name = None;
        let public = synthesize_ephemeral_preview(&public).unwrap();
        assert!(public.stack.spec.git_source.unwrap().git_auth.is_none());

        // An applied preview URL is validated strictly: no remote helpers, local paths, file://.
        for bad in [
            "ext::sh -c id",
            "/srv/keel/gitops.git",
            "file:///srv/gitops.git",
        ] {
            let mut helper = base_preview_params("33");
            helper.repo_url = Some(bad.to_string());
            assert!(synthesize_ephemeral_preview(&helper).is_err(), "{bad}");
        }
    }

    /// Synthesized-only previews (no cluster, Flux-only changes, bundles) never carry a
    /// placeholder clone URL such as the control-plane remote or a local bare-repo path.
    #[test]
    fn unapplied_previews_leave_project_repo_unset() {
        let mut params = base_preview_params("34");
        params.git_secret_name = Some("keel-preview-34-git-credentials".to_string());
        let preview = synthesize_ephemeral_preview(&params).unwrap();
        assert!(preview.stack.spec.git_source.is_none());
        assert!(preview.stack.spec.flux_source.is_none());
        let json = preview.to_json_value().unwrap();
        assert!(json["spec"].get("projectRepo").is_none(), "{json}");
        assert!(json["spec"].get("gitAuth").is_none(), "{json}");
    }

    #[test]
    fn malformed_git_sources_are_rejected_instead_of_silently_dropped() {
        let yaml = "apiVersion: pulumi.com/v1\nkind: Stack\nmetadata:\n  name: vm\n  namespace: tenant-acme\nspec:\n  stack: organization/vm/acme-vm\n  projectRepo: https://forge.example/acme.git\n  branch: main\n  repoDir: tenants/acme/infra/vm\n  gitAuth:\n    basicAuth: not-a-mapping\n";
        let err = PkoStack::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("malformed Git source"), "{err}");
        let ok = yaml.replace("  gitAuth:\n    basicAuth: not-a-mapping\n", "");
        assert!(PkoStack::from_yaml(&ok).unwrap().spec.git_source.is_some());
    }

    const LEGACY_TENANT_STACK: &str = "apiVersion: pulumi.com/v1
kind: Stack
metadata:
  name: kind
  namespace: tenant-default
  labels:
    keel.io/provider: kind
spec:
  stack: organization/kind-cluster/default-kind
  backend: s3://pulumi-state/tenants/default
  fluxSource:
    sourceRef:
      apiVersion: source.toolkit.fluxcd.io/v1
      kind: GitRepository
      name: keel-gitops
    dir: tenants/default/infra/kind
  config:
    clusterName: wl
  resyncFrequencySeconds: 300
  destroyOnFinalize: true
";

    fn default_tenant_source(dir: &str) -> Result<PkoGitSource, PkoError> {
        let url = "https://forgejo.keel.local:8888/keel/infra-gitops.git";
        Ok(
            PkoGitSource::branch(url, "main", dir).with_auth(Some(PkoGitAuth::from_flux_secret(
                "git-credentials",
                url,
                &BTreeSet::new(),
            )?)),
        )
    }

    #[test]
    fn legacy_flux_source_tenant_stacks_migrate_to_git_source_verbatim() {
        let FluxSourceMigration::Migrated(out) =
            migrate_legacy_flux_source_stack(LEGACY_TENANT_STACK, "default", default_tenant_source)
                .unwrap()
        else {
            panic!("expected a migration");
        };
        assert!(!out.contains("fluxSource"), "{out}");
        let stack = PkoStack::from_yaml(&out).unwrap();
        let src = stack.spec.git_source.as_ref().unwrap();
        assert_eq!(src.repo_dir.as_deref(), Some("tenants/default/infra/kind"));
        assert_eq!(src.branch.as_deref(), Some("main"));
        assert_eq!(src.shallow, Some(true));
        assert!(src.git_auth.as_ref().unwrap().basic_auth.is_some());
        // The workspace now clones the forge itself: it trusts the public Keel CA bundle.
        let pod = &stack
            .spec
            .workspace_template
            .as_ref()
            .unwrap()
            .spec
            .pod_template
            .spec;
        assert_eq!(
            pod.volumes[0].config_map.as_ref().unwrap().name,
            "keel-ca-bundle"
        );
        assert_eq!(pod.init_containers[0].name, "fetch");
        assert_eq!(pod.containers[0].name, "pulumi");
        assert!(
            !pod.containers[0]
                .security_context
                .allow_privilege_escalation
        );
        // Everything else is kept (no program/config change).
        assert_eq!(stack.spec.stack, "organization/kind-cluster/default-kind");
        assert_eq!(
            stack.spec.config.get("clusterName").map(String::as_str),
            Some("wl")
        );
        assert_eq!(stack.spec.resync_frequency_seconds, Some(300));
        assert_eq!(stack.spec.destroy_on_finalize, Some(true));
        assert_eq!(
            stack
                .metadata
                .labels
                .get(LABEL_PROVIDER)
                .map(String::as_str),
            Some("kind")
        );

        // Idempotent: a migrated (or freshly synthesized) Stack is unchanged.
        assert_eq!(
            migrate_legacy_flux_source_stack(&out, "default", default_tenant_source).unwrap(),
            FluxSourceMigration::Unchanged
        );
    }

    #[test]
    fn ca_trust_is_added_to_existing_workspace_templates_verbatim() {
        let mut template = serde_json::to_value(
            PkoWorkspaceTemplate::restricted("keel-pulumi-passphrase").with_secret_volume(
                "target-kubeconfig",
                "kc",
                "/etc/keel/kubeconfig",
            ),
        )
        .unwrap();
        template["spec"]["podTemplate"]["spec"]["nodeSelector"] = serde_json::json!({"a": "b"});
        add_keel_ca_trust_to_workspace_template(&mut template).unwrap();
        let once = template.clone();
        add_keel_ca_trust_to_workspace_template(&mut template).unwrap();
        assert_eq!(template, once, "idempotent");
        // Same shape as freshly synthesized Stacks; unknown fields are kept.
        let expected = serde_json::to_value(
            PkoWorkspaceTemplate::restricted("keel-pulumi-passphrase")
                .with_secret_volume("target-kubeconfig", "kc", "/etc/keel/kubeconfig")
                .with_keel_ca_trust(),
        )
        .unwrap();
        let pod = &template["spec"]["podTemplate"]["spec"];
        assert_eq!(pod["nodeSelector"], serde_json::json!({"a": "b"}));
        for key in ["volumes", "initContainers", "containers"] {
            assert_eq!(
                pod[key], expected["spec"]["podTemplate"]["spec"][key],
                "{key}"
            );
        }
        let mut bad = serde_json::json!({"spec": "nope"});
        assert!(add_keel_ca_trust_to_workspace_template(&mut bad).is_err());
    }

    #[test]
    fn legacy_flux_source_migration_only_rewrites_the_tenants_own_stacks() {
        let skipped = |yaml: &str, tenant: &str| match migrate_legacy_flux_source_stack(
            yaml,
            tenant,
            default_tenant_source,
        )
        .unwrap()
        {
            FluxSourceMigration::Skipped(reason) => reason,
            other => panic!("expected Skipped, got {other:?}"),
        };
        // Another tenant's Stack / namespace.
        skipped(LEGACY_TENANT_STACK, "acme");
        // The flux-system management Stack is never touched.
        skipped(
            &LEGACY_TENANT_STACK.replace("namespace: tenant-default", "namespace: flux-system"),
            "default",
        );
        // A different Flux source, or a dir outside the tenant tree.
        skipped(
            &LEGACY_TENANT_STACK.replace("name: keel-gitops", "name: flux-system"),
            "default",
        );
        skipped(
            &LEGACY_TENANT_STACK.replace(
                "dir: tenants/default/infra/kind",
                "dir: tenants/acme/infra/kind",
            ),
            "default",
        );
        skipped(
            &LEGACY_TENANT_STACK.replace(
                "dir: tenants/default/infra/kind",
                "dir: tenants/default/../acme",
            ),
            "default",
        );
        // Non-Stack documents are left alone.
        assert_eq!(
            migrate_legacy_flux_source_stack(
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n",
                "default",
                default_tenant_source
            )
            .unwrap(),
            FluxSourceMigration::Unchanged
        );
        // A source that cannot be cloned fails instead of writing a broken Stack.
        assert!(
            migrate_legacy_flux_source_stack(LEGACY_TENANT_STACK, "default", |dir| Ok(
                PkoGitSource::branch("/tmp/local.git", "main", dir)
            ))
            .is_err()
        );
    }
}
