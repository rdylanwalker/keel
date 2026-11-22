//! Per-tenant Git repository stubs, credentials and PKO sources (`SDD.md` §3.1, §6.2).
//!
//! With per-tenant repositories the platform monorepo keeps only a small platform-owned stub per
//! tenant under `tenants/<id>/`:
//!
//! * `rbac.yaml`: the tenant baseline ([`TenantManifestBundle`]: namespace, ServiceAccount, quota,
//!   LimitRange, NetworkPolicies, RoleBindings). These objects stay in the monorepo because the
//!   tenant ServiceAccount cannot write them; one of them in the tenant repository would fail the
//!   tenant's whole apply.
//! * `git-repository.yaml`: Flux `GitRepository` `tenant-<id>/keel-gitops` pointing at the
//!   tenant repository ([`build_tenant_repo_git_repository`]).
//! * `sync.yaml`: Flux `Kustomization` `tenant-<id>/keel-tenant-root` applying `./tenants/<id>`
//!   of the tenant repository as `tenant-<id>-sa` ([`build_tenant_root_kustomization`]).
//!
//! All stub objects carry `keel.io/platform-owned` and are applied by `keel-tenants`
//! (`keel-tenant-reconciler`). The read credential ([`build_tenant_gitops_credentials_secret`]) is
//! a cluster-only Secret written at runtime by keel-server / bootstrap and is **never** part of a
//! Git-written bundle ([`ensure_git_safe_manifests`]).
//!
//! The PKO helpers build tenant Stack (`branch`) and preview (commit-pinned) `gitSource`s that
//! clone the tenant repository with the same credential, plus the PKO operator `known_hosts`
//! plumbing needed when a tenant repository is reached over SSH.
//!
//! keel-server (`POST /api/v1/tenants`) and `keel bootstrap init` (`tenant-default`) write these
//! stubs and the credentials Secret (`keel_server::tenant_repos`). Only the development-only
//! Local Git provider keeps tenants inside the monorepo (D8).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use keel_core::KEEL_SYSTEM_NAMESPACE;
use keel_pko::{
    KEEL_SSH_KNOWN_HOSTS_FILE, KEEL_SSH_KNOWN_HOSTS_MOUNT_PATH, KEEL_SSH_KNOWN_HOSTS_VOLUME_NAME,
    PkoEnvRef, PkoGitAuth, PkoGitBasicAuth, PkoGitSource, PkoGitSshAuth, PkoWorkspaceTemplate,
    SSH_KNOWN_HOSTS_ENV,
};
use serde_json::{Value, json};

use crate::KubeError;
use crate::ca_bundle::validate_public_ca_pem;
use crate::preview_runner::{DEFAULT_TENANT_GIT_BRANCH, TENANT_GIT_REPOSITORY_NAME};
use crate::secret_store::TENANT_SOPS_AGE_SECRET_NAME;
use crate::tenant::{
    FluxKustomizationSpecInput, PLATFORM_OWNED_LABEL, TenantIsolationConfig, TenantManifestBundle,
    platform_owned_labels, synthesize_flux_kustomization, tenant_namespace,
};

/// Secret (in `tenant-<id>`) holding the read-only credential for the tenant's own repository,
/// referenced by the `keel-gitops` GitRepository (`secretRef`) and by tenant PKO Stacks
/// (`gitAuth`). Cluster-only: never written to Git.
pub const TENANT_GITOPS_CREDENTIALS_SECRET_NAME: &str = "keel-gitops-credentials";
/// `keel.io/secret-kind` of [`TENANT_GITOPS_CREDENTIALS_SECRET_NAME`].
pub const TENANT_GITOPS_CREDENTIALS_SECRET_KIND: &str = "tenant-gitops-credentials";
/// Annotation recording the credential transport (`https` | `ssh`); never a secret value.
pub const GIT_TRANSPORT_ANNOTATION: &str = "keel.io/git-transport";
/// Name of the per-tenant root Flux `Kustomization` (`tenants/<id>/sync.yaml`).
pub const TENANT_ROOT_KUSTOMIZATION_NAME: &str = "keel-tenant-root";
/// Reconcile interval of [`TENANT_ROOT_KUSTOMIZATION_NAME`] (drift correction; new commits are
/// picked up through the `keel-gitops` source, which polls every minute).
pub const TENANT_ROOT_KUSTOMIZATION_INTERVAL: &str = "5m";
/// Platform-written stub files under `tenants/<id>/` of the monorepo for a tenant with its own
/// repository. None of them may appear in the tenant repository.
pub const TENANT_REPO_STUB_FILES: [&str; 3] = ["rbac.yaml", "git-repository.yaml", "sync.yaml"];
/// HTTPS credential keys (Flux basic auth, PKO `basicAuth`).
pub const HTTPS_USERNAME_KEY: &str = "username";
/// See [`HTTPS_USERNAME_KEY`].
pub const HTTPS_PASSWORD_KEY: &str = "password";
/// Flux `ca.crt` key (public Keel CA, only when the forge is the in-cluster Forgejo).
pub const CA_CERT_KEY: &str = "ca.crt";
/// Flux SSH private key.
pub const SSH_IDENTITY_KEY: &str = "identity";
/// Flux SSH public key.
pub const SSH_IDENTITY_PUB_KEY: &str = "identity.pub";
/// Flux / PKO `known_hosts` (never trust-on-first-use).
pub const SSH_KNOWN_HOSTS_KEY: &str = "known_hosts";
/// PKO `gitAuth.sshAuth.sshPrivateKey` key (same private key as [`SSH_IDENTITY_KEY`]).
pub const SSH_PRIVATE_KEY_KEY: &str = "sshPrivateKey";
/// `keel-system` ConfigMap (key `known_hosts`, public host keys only) the PKO operator reads
/// through `SSH_KNOWN_HOSTS` to track branches of SSH tenant repositories.
pub const PKO_OPERATOR_SSH_KNOWN_HOSTS_CONFIGMAP: &str = "keel-pko-ssh-known-hosts";

/// Transport of a tenant repository (`SDD.md` §6.2, D6/D9): Forgejo/Gitea and GitLab issue
/// repository-scoped HTTPS read tokens, GitHub read-only SSH deploy keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TenantRepoTransport {
    Https,
    Ssh,
}

impl TenantRepoTransport {
    /// `https` | `ssh` (value of [`GIT_TRANSPORT_ANNOTATION`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Ssh => "ssh",
        }
    }
}

impl fmt::Display for TenantRepoTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tenant's own repository as the platform stub references it. The URL is platform input
/// (built by Keel from the forge configuration, never a tenant-editable value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRepoSource {
    tenant_id: String,
    url: String,
    transport: TenantRepoTransport,
}

impl TenantRepoSource {
    /// Validates `url` for both Flux and PKO: `https://` (Forgejo, GitLab) or
    /// `ssh://<user>@<host>[:port]/...` (GitHub deploy keys), with a host and a repository path.
    /// Refused: plain `http://` (the token would cross the network in clear text and PKO cannot
    /// clone it), scp-like `user@host:path` (Flux only accepts `ssh://`), embedded credentials
    /// (including an SSH `user:password`), local paths, git remote helpers and SSH to an
    /// in-cluster host (`*.svc`, `*.cluster.local`, single-label names): the in-cluster Forgejo
    /// SSH port (pod port 2222) is not in the tenant `allow-pko-workspace` egress allow-list and
    /// has no pinned host key yet, so such a source could never clone; use its HTTPS URL.
    pub fn new(tenant_id: &str, url: &str) -> Result<Self, KubeError> {
        tenant_namespace(tenant_id)?;
        let invalid = |why: String| {
            KubeError::InvalidInput(format!("tenant `{tenant_id}` repository URL {why}"))
        };
        keel_pko::validate_git_source_url(url).map_err(|e| invalid(e.to_string()))?;
        let (transport, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (TenantRepoTransport::Https, rest)
        } else if let Some(rest) = url.strip_prefix("ssh://") {
            (TenantRepoTransport::Ssh, rest)
        } else {
            return Err(invalid(format!(
                "'{url}' must be an https:// or ssh:// URL (Flux GitRepository does not accept \
                 scp-like user@host:path URLs)"
            )));
        };
        let path = rest.split_once('/').map(|(_, p)| p).unwrap_or_default();
        if path.trim_matches('/').is_empty() || path.split('/').any(|s| s == "..") {
            return Err(invalid(format!("'{url}' has no repository path")));
        }
        if transport == TenantRepoTransport::Ssh {
            let authority = rest.split('/').next().unwrap_or_default();
            let Some((user, _)) = authority.split_once('@') else {
                return Err(invalid(format!(
                    "'{url}' has no SSH user (expected ssh://git@<host>[:port]/<owner>/<repo>.git)"
                )));
            };
            // Never a password (`git:secret@`): it would be committed in the stub and echoed in
            // errors.
            if user.is_empty()
                || !user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err(invalid(
                    "has an invalid SSH user (expected ssh://git@<host>[:port]/<owner>/<repo>.git; \
                     credentials never belong in the URL)"
                        .to_string(),
                ));
            }
            let host = ssh_url_host(url).unwrap_or_default();
            if host.is_empty()
                || !host.contains(['.', ':'])
                || host.ends_with(".svc")
                || host.ends_with(".cluster.local")
            {
                return Err(invalid(format!(
                    "'{url}' is SSH to an in-cluster host; tenant PKO workspaces cannot reach the \
                     in-cluster Forgejo SSH port, use its https:// URL"
                )));
            }
        }
        Ok(Self {
            tenant_id: tenant_id.to_string(),
            url: url.to_string(),
            transport,
        })
    }

    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    /// Clone URL used by both the Flux `GitRepository` and PKO `spec.projectRepo`.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn transport(&self) -> TenantRepoTransport {
        self.transport
    }

    /// Branch the tenant source tracks (always `main`, protected by `ensure_main_protection`).
    pub fn branch(&self) -> &str {
        DEFAULT_TENANT_GIT_BRANCH
    }

    /// `tenant-<id>`.
    pub fn namespace(&self) -> String {
        format!("tenant-{}", self.tenant_id)
    }

    /// Host field an SSH `known_hosts` entry needs for this URL: the bare host on port 22,
    /// `[host]:port` otherwise. `None` for HTTPS.
    pub fn ssh_known_hosts_host(&self) -> Option<String> {
        if self.transport != TenantRepoTransport::Ssh {
            return None;
        }
        let (host, port) = ssh_url_host_port(&self.url)?;
        Some(match port.as_deref() {
            None | Some("22") => host,
            Some(port) => format!("[{host}]:{port}"),
        })
    }
}

/// Lower-cased host and optional port of an `ssh://[user@]host[:port]/...` URL (IPv6 literals in
/// brackets).
fn ssh_url_host_port(url: &str) -> Option<(String, Option<String>)> {
    let authority = url.strip_prefix("ssh://")?.split('/').next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    Some((host.to_ascii_lowercase(), port.map(str::to_string)))
}

fn ssh_url_host(url: &str) -> Option<String> {
    ssh_url_host_port(url).map(|(h, _)| h)
}

/// Synthesizes the platform-owned Flux `GitRepository` `tenant-<id>/keel-gitops`
/// (`tenants/<id>/git-repository.yaml`) for a tenant with its own repository: `interval: 1m`
/// outbound polling (NAT/firewall friendly), `ref.branch: main`, `secretRef:
/// keel-gitops-credentials`.
pub fn build_tenant_repo_git_repository(source: &TenantRepoSource) -> Result<Value, KubeError> {
    let namespace = tenant_namespace(source.tenant_id())?;
    Ok(json!({
        "apiVersion": keel_pko::FLUX_GIT_REPO_API_VERSION,
        "kind": keel_pko::FLUX_GIT_REPO_KIND,
        "metadata": {
            "name": TENANT_GIT_REPOSITORY_NAME,
            "namespace": namespace,
            "labels": platform_owned_labels(source.tenant_id()),
            "annotations": { GIT_TRANSPORT_ANNOTATION: source.transport().as_str() },
        },
        "spec": {
            "interval": keel_pko::DEFAULT_FLUX_POLL_INTERVAL,
            "url": source.url(),
            "ref": { "branch": source.branch() },
            "secretRef": { "name": TENANT_GITOPS_CREDENTIALS_SECRET_NAME },
        },
    }))
}

/// Synthesizes the platform-owned root Flux `Kustomization` `tenant-<id>/keel-tenant-root`
/// (`tenants/<id>/sync.yaml`): applies `./tenants/<id>` of the tenant's own `keel-gitops` source
/// impersonating `tenant-<id>-sa` (Kyverno `restrict-tenant-flux-sources`), confined to
/// `tenant-<id>` (`targetNamespace`), decrypting SOPS files with the tenant's `sops-age` key
/// (mandatory: a missing key fails the reconcile instead of applying ciphertext), `prune: true`
/// and `wait: false` (every app reports its own health on its own Flux object).
pub fn build_tenant_root_kustomization(tenant_id: &str) -> Result<Value, KubeError> {
    tenant_namespace(tenant_id)?;
    let mut input = FluxKustomizationSpecInput::new(
        tenant_id,
        TENANT_ROOT_KUSTOMIZATION_NAME,
        TENANT_GIT_REPOSITORY_NAME,
        format!("./tenants/{tenant_id}"),
    );
    input.interval = TENANT_ROOT_KUSTOMIZATION_INTERVAL.to_string();
    input.wait = false;
    input.prune = true;
    input.decryption_secret_name = Some(TENANT_SOPS_AGE_SECRET_NAME.to_string());
    input
        .extra_labels
        .insert(PLATFORM_OWNED_LABEL.to_string(), "true".to_string());
    synthesize_flux_kustomization(&input)
}

/// Platform stub for a tenant with its own repository: monorepo files (repository-relative path,
/// YAML) under `tenants/<id>/` ([`TENANT_REPO_STUB_FILES`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRepoStub {
    pub tenant_id: String,
    pub files: Vec<(String, String)>,
}

impl TenantRepoStub {
    /// Entries for the monorepo `tenants/kustomization.yaml` (`resources`, relative to
    /// `tenants/`), in [`TENANT_REPO_STUB_FILES`] order.
    pub fn kustomization_entries(&self) -> Vec<String> {
        tenant_repo_stub_kustomization_entries(&self.tenant_id)
    }
}

/// `tenants/kustomization.yaml` entries of a per-tenant repository stub (`<id>/rbac.yaml`,
/// `<id>/git-repository.yaml`, `<id>/sync.yaml`).
pub fn tenant_repo_stub_kustomization_entries(tenant_id: &str) -> Vec<String> {
    TENANT_REPO_STUB_FILES
        .iter()
        .map(|f| format!("{tenant_id}/{f}"))
        .collect()
}

fn to_yaml_docs(docs: &[Value]) -> Result<String, KubeError> {
    let mut rendered = Vec::with_capacity(docs.len());
    for doc in docs {
        let mut s = serde_yml::to_string(doc)?;
        if !s.ends_with('\n') {
            s.push('\n');
        }
        rendered.push(s);
    }
    Ok(rendered.join("---\n"))
}

/// Synthesizes the monorepo stub of a tenant with its own repository (`rbac.yaml`,
/// `git-repository.yaml`, `sync.yaml`). Every file passes [`ensure_git_safe_manifests`]: the
/// credentials Secret is applied at runtime and never part of the stub.
pub fn synthesize_tenant_repo_stub(
    config: &TenantIsolationConfig,
    source: &TenantRepoSource,
) -> Result<TenantRepoStub, KubeError> {
    if config.tenant_id != source.tenant_id() {
        return Err(KubeError::InvalidInput(format!(
            "tenant repository source belongs to `{}`, not `{}`",
            source.tenant_id(),
            config.tenant_id
        )));
    }
    let tenant = source.tenant_id();
    let rbac = TenantManifestBundle::synthesize(config)?.to_multi_doc_yaml()?;
    let git_repository = to_yaml_docs(&[build_tenant_repo_git_repository(source)?])?;
    let sync = to_yaml_docs(&[build_tenant_root_kustomization(tenant)?])?;
    let files: Vec<(String, String)> = TENANT_REPO_STUB_FILES
        .iter()
        .zip([rbac, git_repository, sync])
        .map(|(file, yaml)| (format!("tenants/{tenant}/{file}"), yaml))
        .collect();
    for (path, yaml) in &files {
        ensure_git_safe_manifests(path, yaml)?;
    }
    Ok(TenantRepoStub {
        tenant_id: tenant.to_string(),
        files,
    })
}

/// `true` when `rel` (a kustomize `resources` entry relative to `tenants/<id>/`) is a plain
/// relative path inside the tenant directory. kustomize-controller (in `flux-system`, outside the
/// tenant NetworkPolicies, without `--no-remote-bases`) would otherwise fetch remote bases
/// (`https://...`, `github.com/o/r?ref=...`, `git@...`) or read outside `tenants/<id>/`.
fn is_contained_tenant_resource(rel: &str) -> bool {
    let segments: Vec<&str> = rel.split('/').collect();
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains(['\\', ':', '?', '#', '@'])
        && !segments
            .iter()
            .any(|s| *s == ".." || *s == "." || s.is_empty() || has_control_or_space(s))
        // A multi-segment entry starting with a domain-like segment is a kustomize remote
        // (`github.com/o/r`), never a tenant directory.
        && !(segments.len() > 1 && segments[0].contains('.'))
}

/// Renders the tenant repository's `tenants/<id>/kustomization.yaml` listing `resources`
/// (relative to `tenants/<id>/`). The root Kustomization must apply exactly the listed files,
/// never every YAML below `tenants/<id>`: manifest apps keep `${var}` templates that only their
/// own Kustomization substitutes. Refuses the platform stub files, which the tenant
/// ServiceAccount cannot apply, and anything that is not a contained relative path (remote
/// bases, `..`, absolute paths).
pub fn build_tenant_repo_kustomization_yaml(
    tenant_id: &str,
    resources: &[String],
) -> Result<String, KubeError> {
    tenant_namespace(tenant_id)?;
    for r in resources {
        if TENANT_REPO_STUB_FILES.contains(&r.as_str()) {
            return Err(KubeError::InvalidInput(format!(
                "`{r}` is a platform stub file and must stay in the platform repository"
            )));
        }
        if !is_contained_tenant_resource(r) {
            return Err(KubeError::InvalidInput(format!(
                "tenant kustomization resource `{r}` is not a contained relative path inside \
                 `tenants/{tenant_id}/` (remote bases are refused)"
            )));
        }
    }
    Ok(serde_yml::to_string(&json!({
        "apiVersion": "kustomize.config.k8s.io/v1beta1",
        "kind": "Kustomization",
        "resources": resources,
    }))?)
}

/// Fails when `yaml` (the content of a file Keel is about to write to Git at `path`) contains a
/// Kubernetes `Secret` that is not fully SOPS-encrypted (every `data`/`stringData` value an
/// `ENC[...]` string, not merely a `sops` key next to plaintext values left by
/// `encrypted_regex`/`unencrypted_suffix`), or any cluster-only Git credential Secret
/// (`keel-gitops-credentials`, the monorepo-layout `git-credentials`, or one labelled with their
/// `keel.io/secret-kind`), even encrypted. Objects nested in `kind: List` (or any `*List`)
/// `items` are checked too. Unparsable documents are refused.
pub fn ensure_git_safe_manifests(path: &str, yaml: &str) -> Result<(), KubeError> {
    let refuse = |why: &str| {
        KubeError::InvalidInput(format!(
            "refusing to write `{path}` to Git: {why} (credentials are cluster-only Secrets)"
        ))
    };
    for doc in keel_core::split_yaml_documents(yaml) {
        let value: Value =
            serde_yml::from_str(&doc).map_err(|_| refuse("it has an unparsable YAML document"))?;
        check_git_safe_object(&value, 0).map_err(refuse)?;
    }
    Ok(())
}

/// Nesting limit for `List` items (a real manifest never nests Lists).
const MAX_LIST_DEPTH: usize = 4;

fn check_git_safe_object(value: &Value, depth: usize) -> Result<(), &'static str> {
    let kind = value.get("kind").and_then(Value::as_str);
    if let Some(items) = value.get("items").and_then(Value::as_array)
        && kind.is_some_and(|k| k.ends_with("List"))
    {
        if depth >= MAX_LIST_DEPTH {
            return Err("it nests Lists too deeply");
        }
        for item in items {
            check_git_safe_object(item, depth + 1)?;
        }
        return Ok(());
    }
    if kind != Some("Secret") {
        return Ok(());
    }
    let name = value.pointer("/metadata/name").and_then(Value::as_str);
    let kind_label = value
        .pointer("/metadata/labels")
        .and_then(|l| l.get("keel.io/secret-kind"))
        .and_then(Value::as_str);
    let credential_names = [
        TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
        crate::tenant::TENANT_GIT_CREDENTIALS_SECRET_NAME,
    ];
    let credential_kinds = [
        TENANT_GITOPS_CREDENTIALS_SECRET_KIND,
        crate::tenant::TENANT_GIT_CREDENTIALS_SECRET_KIND,
    ];
    if name.is_some_and(|n| credential_names.contains(&n))
        || kind_label.is_some_and(|k| credential_kinds.contains(&k))
    {
        return Err("it contains a tenant Git credentials Secret");
    }
    let encrypted = value.get("sops").is_some_and(Value::is_object);
    for field in ["data", "stringData"] {
        let Some(map) = value.get(field) else {
            continue;
        };
        if !encrypted {
            return Err("it contains an unencrypted Secret");
        }
        let all_encrypted = match map {
            Value::Null => true,
            Value::Object(entries) => entries.values().all(|v| {
                v.as_str()
                    .is_some_and(|s| s.starts_with("ENC[") && s.ends_with(']'))
            }),
            _ => false,
        };
        if !all_encrypted {
            return Err("it contains a partially encrypted Secret (plaintext data values)");
        }
    }
    Ok(())
}

/// The read-only credential for a tenant repository. `Debug` never prints secret material.
#[derive(Clone, PartialEq, Eq)]
pub enum TenantRepoCredential {
    /// Repository-scoped HTTPS read token (Forgejo/Gitea bot token, GitLab deploy token).
    HttpsToken {
        username: String,
        token: String,
        /// Public Keel CA (`ca.crt`) when the forge is the in-cluster Forgejo behind the Keel CA.
        ca_crt: Option<String>,
    },
    /// Read-only SSH deploy key (GitHub).
    SshDeployKey {
        /// OpenSSH private key (unencrypted).
        private_key: String,
        /// One-line OpenSSH public key.
        public_key: String,
        /// `known_hosts` content for exactly the repository host (never trust-on-first-use; entries
        /// for other hosts are refused).
        known_hosts: String,
    },
}

impl fmt::Debug for TenantRepoCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpsToken {
                username, ca_crt, ..
            } => f
                .debug_struct("HttpsToken")
                .field("username", username)
                .field("token", &"[REDACTED]")
                .field("ca_crt", &ca_crt.is_some())
                .finish(),
            Self::SshDeployKey {
                public_key,
                known_hosts,
                ..
            } => f
                .debug_struct("SshDeployKey")
                .field("private_key", &"[REDACTED]")
                .field("public_key", public_key)
                .field("known_hosts", known_hosts)
                .finish(),
        }
    }
}

impl TenantRepoCredential {
    pub fn transport(&self) -> TenantRepoTransport {
        match self {
            Self::HttpsToken { .. } => TenantRepoTransport::Https,
            Self::SshDeployKey { .. } => TenantRepoTransport::Ssh,
        }
    }
}

fn has_control_or_space(s: &str) -> bool {
    s.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Validates `known_hosts` content: at least one `<hosts> <key-type> <base64>` entry, no
/// wildcard/negated host patterns or `@cert-authority`/`@revoked` markers. When `required` is
/// given (`host` or `[host]:port`), **every** entry's host list must be exactly that host: an
/// extra entry for another host (say a planted `github.com` key in one tenant's credential) would
/// otherwise be trusted wherever the file is used. Hashed (`|1|...`) entries are refused for the
/// same reason (they cannot be checked). Returns the normalized content (one entry per line,
/// trailing newline).
pub fn validate_known_hosts(
    known_hosts: &str,
    required: Option<&str>,
) -> Result<String, KubeError> {
    let invalid = |why: String| KubeError::InvalidInput(format!("known_hosts {why}"));
    let mut lines = Vec::new();
    let mut covers = required.is_none();
    for line in known_hosts.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('@') {
            return Err(invalid(
                "must not use @cert-authority or @revoked markers".to_string(),
            ));
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            return Err(invalid(format!(
                "entry `{line}` is not `<host> <key-type> <base64>`"
            )));
        }
        let (hosts, key_type, key) = (fields[0], fields[1], fields[2]);
        if hosts.contains(['*', '?', '!']) {
            return Err(invalid(format!(
                "entry for `{hosts}` uses a host pattern; list exact hosts only"
            )));
        }
        let known_type = key_type.starts_with("ssh-")
            || key_type.starts_with("ecdsa-sha2-")
            || key_type.starts_with("sk-");
        if !known_type
            || key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        {
            return Err(invalid(format!(
                "entry for `{hosts}` has no valid SSH host key"
            )));
        }
        if let Some(required) = required {
            if !hosts.eq_ignore_ascii_case(required) {
                return Err(invalid(format!(
                    "entry for `{hosts}` is not for `{required}` (only that exact host is allowed)"
                )));
            }
            covers = true;
        }
        lines.push(fields[..3].join(" "));
    }
    if lines.is_empty() {
        return Err(invalid(
            "has no host key (Keel never trusts an SSH host on first use)".to_string(),
        ));
    }
    if !covers {
        return Err(invalid(format!(
            "has no host key for `{}`",
            required.unwrap_or_default()
        )));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

const OPENSSH_PRIVATE_KEY_BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const OPENSSH_KEY_MAGIC: &[u8] = b"openssh-key-v1\0";

/// Reads one SSH wire-format `string` (u32 big-endian length + bytes) at `*pos`.
fn read_ssh_string<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len_bytes: [u8; 4] = buf.get(*pos..*pos + 4)?.try_into().ok()?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let start = *pos + 4;
    let out = buf.get(start..start.checked_add(len)?)?;
    *pos = start + len;
    Some(out)
}

/// `true` when the base64 body of an `OPENSSH PRIVATE KEY` block declares cipher and KDF `none`
/// (no passphrase). Encrypted keys keep the cipher name inside the body, not in the PEM header.
fn openssh_key_is_unencrypted(body_base64: &str) -> bool {
    use base64::Engine as _;
    let compact: String = body_base64.split_whitespace().collect();
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(compact) else {
        return false;
    };
    let Some(rest) = bytes.strip_prefix(OPENSSH_KEY_MAGIC) else {
        return false;
    };
    let mut pos = 0;
    let cipher = read_ssh_string(rest, &mut pos);
    let kdf = read_ssh_string(rest, &mut pos);
    cipher == Some(b"none".as_slice()) && kdf == Some(b"none".as_slice())
}

/// Accepts only an unencrypted private key (Flux `identity` and PKO `sshPrivateKey` without a
/// `password` cannot use a passphrase): `OPENSSH PRIVATE KEY` blocks must declare cipher/KDF
/// `none`; PEM blocks must not be `ENCRYPTED` (PKCS#8) or carry `Proc-Type: 4,ENCRYPTED`.
fn validate_openssh_private_key(key: &str) -> Result<String, KubeError> {
    let invalid = || {
        KubeError::InvalidInput(
            "SSH deploy key must be an unencrypted PEM/OpenSSH private key (no passphrase)"
                .to_string(),
        )
    };
    let trimmed = key.trim();
    let lines: Vec<&str> = trimmed.lines().map(str::trim_end).collect();
    let (Some(begin), Some(end)) = (lines.first(), lines.last()) else {
        return Err(invalid());
    };
    let pem_label = |line: &str, marker: &str| {
        line.strip_prefix(marker)
            .and_then(|l| l.strip_suffix("-----"))
            .is_some_and(|label| label.ends_with("PRIVATE KEY"))
    };
    if lines.len() < 3
        || !pem_label(begin, "-----BEGIN ")
        || !pem_label(end, "-----END ")
        || trimmed.contains("ENCRYPTED")
        || trimmed.contains("Proc-Type:")
    {
        return Err(invalid());
    }
    if *begin == OPENSSH_PRIVATE_KEY_BEGIN
        && !openssh_key_is_unencrypted(&lines[1..lines.len() - 1].join(""))
    {
        return Err(invalid());
    }
    // ssh/go-git require the trailing newline.
    Ok(format!("{trimmed}\n"))
}

fn validate_ssh_public_key(key: &str) -> Result<String, KubeError> {
    let key = key.trim();
    let fields: Vec<&str> = key.split_whitespace().collect();
    let ok = !key.contains('\n')
        && fields.len() >= 2
        && (fields[0].starts_with("ssh-")
            || fields[0].starts_with("ecdsa-sha2-")
            || fields[0].starts_with("sk-"));
    if !ok {
        return Err(KubeError::InvalidInput(
            "SSH deploy public key must be one OpenSSH line ('<type> <base64> [comment]')"
                .to_string(),
        ));
    }
    Ok(format!("{key}\n"))
}

/// Synthesizes the cluster-only `tenant-<id>/keel-gitops-credentials` Secret for `source`
/// (applied by keel-server / bootstrap at runtime, **never** written to Git; see
/// [`ensure_git_safe_manifests`]). It carries `keel.io/platform-owned: "true"`, so Kyverno
/// `protect-platform-owned-tenant-objects` keeps tenant admins (who hold `admin` in
/// `tenant-<id>`) from replacing, editing or deleting it; tenants may still read it (D1):
///
/// * HTTPS: `username`, `password` (the repository-scoped read token) and, for the in-cluster
///   Forgejo, `ca.crt` (public Keel CA only; private keys are refused). Flux uses all three;
///   PKO `basicAuth` uses `username`/`password` and trusts the CA through the `keel-ca-bundle`
///   ConfigMap.
/// * SSH: `identity`, `identity.pub`, `known_hosts` (Flux) and `sshPrivateKey` (same key, PKO
///   `sshAuth`); every `known_hosts` entry must be for exactly the URL's host (port 22: bare
///   host, otherwise `[host]:port`); entries for other hosts are refused.
///
/// Fails closed when the credential transport does not match the URL.
pub fn build_tenant_gitops_credentials_secret(
    source: &TenantRepoSource,
    credential: &TenantRepoCredential,
) -> Result<Value, KubeError> {
    let namespace = tenant_namespace(source.tenant_id())?;
    if credential.transport() != source.transport() {
        return Err(KubeError::InvalidInput(format!(
            "tenant `{}` repository uses {} but the credential is for {}",
            source.tenant_id(),
            source.transport(),
            credential.transport()
        )));
    }
    let mut data = BTreeMap::new();
    match credential {
        TenantRepoCredential::HttpsToken {
            username,
            token,
            ca_crt,
        } => {
            if username.is_empty() || has_control_or_space(username) || username.contains(':') {
                return Err(KubeError::InvalidInput(
                    "tenant repository username must be non-empty without whitespace or ':'"
                        .to_string(),
                ));
            }
            if token.is_empty() || has_control_or_space(token) {
                return Err(KubeError::InvalidInput(
                    "tenant repository read token must be non-empty without whitespace".to_string(),
                ));
            }
            data.insert(HTTPS_USERNAME_KEY, username.clone());
            data.insert(HTTPS_PASSWORD_KEY, token.clone());
            if let Some(ca) = ca_crt {
                data.insert(CA_CERT_KEY, validate_public_ca_pem(ca)?);
            }
        }
        TenantRepoCredential::SshDeployKey {
            private_key,
            public_key,
            known_hosts,
        } => {
            let identity = validate_openssh_private_key(private_key)?;
            let required = source.ssh_known_hosts_host();
            data.insert(
                SSH_KNOWN_HOSTS_KEY,
                validate_known_hosts(known_hosts, required.as_deref())?,
            );
            data.insert(SSH_IDENTITY_PUB_KEY, validate_ssh_public_key(public_key)?);
            data.insert(SSH_PRIVATE_KEY_KEY, identity.clone());
            data.insert(SSH_IDENTITY_KEY, identity);
        }
    }
    let mut labels = platform_owned_labels(source.tenant_id());
    labels.insert(
        "keel.io/secret-kind".to_string(),
        TENANT_GITOPS_CREDENTIALS_SECRET_KIND.to_string(),
    );
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
            "namespace": namespace,
            "labels": labels,
            "annotations": { GIT_TRANSPORT_ANNOTATION: source.transport().as_str() },
        },
        "type": "Opaque",
        "stringData": data,
    }))
}

/// Annotation of `keel-gitops-credentials` recording the forge id of the credential it holds
/// (Forgejo/GitLab token id, GitHub deploy key id) so a rotation can revoke the previous one.
/// Never a secret value.
pub const CREDENTIAL_ID_ANNOTATION: &str = "keel.io/credential-id";
/// Annotation of `keel-gitops-credentials` recording the repository URL the credential was
/// issued for (a credential for another URL is rotated).
pub const CREDENTIAL_REPO_URL_ANNOTATION: &str = "keel.io/repo-url";

/// [`build_tenant_gitops_credentials_secret`] as a typed `Secret` ready for the Kubernetes API:
/// `stringData` folded into `data`, plus the non-secret [`CREDENTIAL_ID_ANNOTATION`] and
/// [`CREDENTIAL_REPO_URL_ANNOTATION`].
pub fn tenant_gitops_credentials_secret(
    source: &TenantRepoSource,
    credential: &TenantRepoCredential,
    credential_id: &str,
) -> Result<k8s_openapi::api::core::v1::Secret, KubeError> {
    if credential_id.is_empty() || has_control_or_space(credential_id) {
        return Err(KubeError::InvalidInput(
            "tenant repository credential id must be non-empty without whitespace".to_string(),
        ));
    }
    let mut value = build_tenant_gitops_credentials_secret(source, credential)?;
    let string_data = value
        .as_object_mut()
        .and_then(|o| o.remove("stringData"))
        .unwrap_or_default();
    let mut data = BTreeMap::new();
    if let Value::Object(map) = string_data {
        for (k, v) in map {
            let v = v.as_str().unwrap_or_default().as_bytes().to_vec();
            data.insert(k, k8s_openapi::ByteString(v));
        }
    }
    if let Some(annotations) = value
        .pointer_mut("/metadata/annotations")
        .and_then(Value::as_object_mut)
    {
        annotations.insert(CREDENTIAL_ID_ANNOTATION.to_string(), json!(credential_id));
        annotations.insert(
            CREDENTIAL_REPO_URL_ANNOTATION.to_string(),
            json!(source.url()),
        );
    }
    let mut secret: k8s_openapi::api::core::v1::Secret = serde_json::from_value(value)?;
    secret.data = Some(data);
    secret.string_data = None;
    Ok(secret)
}

/// State of a tenant's live `keel-gitops-credentials` Secret for `source`
/// ([`tenant_gitops_credentials_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantGitopsCredentialStatus {
    /// No Secret.
    Missing,
    /// A platform-written Secret for exactly this repository URL with every key PKO and Flux
    /// need.
    Valid { credential_id: Option<String> },
    /// A Secret exists but cannot be used for this repository (wrong URL, labels, missing keys,
    /// foreign `known_hosts`): it must be rotated. `credential_id` is the previous credential to
    /// revoke afterwards.
    Invalid {
        credential_id: Option<String>,
        reason: String,
    },
}

/// Classifies the live `keel-gitops-credentials` Secret for `source` without exposing any value.
pub fn tenant_gitops_credentials_status(
    source: &TenantRepoSource,
    secret: Option<&k8s_openapi::api::core::v1::Secret>,
) -> TenantGitopsCredentialStatus {
    let Some(secret) = secret else {
        return TenantGitopsCredentialStatus::Missing;
    };
    let annotation = |k: &str| {
        secret
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(k))
            .cloned()
    };
    let credential_id = annotation(CREDENTIAL_ID_ANNOTATION).filter(|id| !id.is_empty());
    if annotation(CREDENTIAL_REPO_URL_ANNOTATION).as_deref() != Some(source.url()) {
        return TenantGitopsCredentialStatus::Invalid {
            credential_id,
            reason: "it was issued for another repository URL".to_string(),
        };
    }
    if annotation(GIT_TRANSPORT_ANNOTATION).as_deref() != Some(source.transport().as_str()) {
        return TenantGitopsCredentialStatus::Invalid {
            credential_id,
            reason: format!("it is not a {} credential", source.transport()),
        };
    }
    if source.transport() == TenantRepoTransport::Ssh {
        // Flux reads `identity`; the public half identifies the deploy key to keep on the forge.
        let data = secret.data.as_ref();
        for key in [SSH_IDENTITY_KEY, SSH_IDENTITY_PUB_KEY] {
            if data
                .and_then(|d| d.get(key))
                .is_none_or(|v| v.0.iter().all(u8::is_ascii_whitespace))
            {
                return TenantGitopsCredentialStatus::Invalid {
                    credential_id,
                    reason: format!("it has no `{key}` key"),
                };
            }
        }
    }
    match tenant_repo_preview_secret_payload(source, secret) {
        Ok(_) => TenantGitopsCredentialStatus::Valid { credential_id },
        Err(e) => TenantGitopsCredentialStatus::Invalid {
            credential_id,
            reason: e.to_string(),
        },
    }
}

/// PKO `gitAuth` for a tenant repository credential Secret `secret_name` (the tenant's
/// `keel-gitops-credentials`, or a per-preview copy of it): HTTPS → `basicAuth`
/// (`username`/`password`), SSH → `sshAuth.sshPrivateKey` (key `sshPrivateKey`).
pub fn tenant_repo_git_auth(
    transport: TenantRepoTransport,
    secret_name: &str,
) -> Result<PkoGitAuth, KubeError> {
    crate::tenant::validate_resource_name("git_secret_name", secret_name)?;
    let r = |key: &str| PkoEnvRef::from_secret(secret_name, key);
    Ok(match transport {
        TenantRepoTransport::Https => PkoGitAuth {
            basic_auth: Some(PkoGitBasicAuth {
                user_name: r(HTTPS_USERNAME_KEY),
                password: r(HTTPS_PASSWORD_KEY),
            }),
            ..PkoGitAuth::default()
        },
        TenantRepoTransport::Ssh => PkoGitAuth {
            ssh_auth: Some(PkoGitSshAuth {
                ssh_private_key: r(SSH_PRIVATE_KEY_KEY),
                password: None,
            }),
            ..PkoGitAuth::default()
        },
    })
}

/// Keys of `keel-gitops-credentials` a preview copies into its own per-preview Secret: only
/// what PKO needs (HTTPS: `username`, `password`; SSH: `sshPrivateKey`, `known_hosts`), never
/// `identity`/`identity.pub`/`ca.crt` duplicates.
pub fn tenant_repo_preview_secret_keys(transport: TenantRepoTransport) -> BTreeSet<String> {
    let keys: &[&str] = match transport {
        TenantRepoTransport::Https => &[HTTPS_USERNAME_KEY, HTTPS_PASSWORD_KEY],
        TenantRepoTransport::Ssh => &[SSH_PRIVATE_KEY_KEY, SSH_KNOWN_HOSTS_KEY],
    };
    keys.iter().map(|k| k.to_string()).collect()
}

/// Validates the live `tenant-<id>/keel-gitops-credentials` Secret read back from the cluster
/// and returns exactly the payload a preview copies into `keel-previews`
/// ([`tenant_repo_preview_secret_keys`] for the URL's transport; nothing else, in particular no
/// `ca.crt`, `identity`, `bearerToken` or tenant-added keys). Fails closed unless the Secret is
/// the platform-written one (name, namespace, `keel.io/secret-kind`, `keel.io/platform-owned`)
/// and every needed key is present; an SSH `known_hosts` is re-validated for exactly the
/// repository host. The monorepo-layout `git-credentials` Secret uses `FLUX_GIT_AUTH_KEYS`
/// instead.
pub fn tenant_repo_preview_secret_payload(
    source: &TenantRepoSource,
    secret: &k8s_openapi::api::core::v1::Secret,
) -> Result<BTreeMap<String, k8s_openapi::ByteString>, KubeError> {
    let namespace = source.namespace();
    let refuse = |why: &str| {
        KubeError::PreviewSourceUnavailable(format!(
            "tenant `{}` repository credentials `{namespace}/{TENANT_GITOPS_CREDENTIALS_SECRET_NAME}` \
             {why}; re-run tenant repository provisioning",
            source.tenant_id()
        ))
    };
    let meta = &secret.metadata;
    if meta.name.as_deref() != Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME)
        || meta.namespace.as_deref() != Some(namespace.as_str())
    {
        return Err(refuse("is not the expected Secret"));
    }
    let label = |k: &str| {
        meta.labels
            .as_ref()
            .and_then(|l| l.get(k))
            .map(String::as_str)
    };
    if label("keel.io/secret-kind") != Some(TENANT_GITOPS_CREDENTIALS_SECRET_KIND)
        || label(PLATFORM_OWNED_LABEL) != Some("true")
    {
        return Err(refuse("is not labelled as the platform-written credential"));
    }
    let data = secret.data.clone().unwrap_or_default();
    let mut out = BTreeMap::new();
    for key in tenant_repo_preview_secret_keys(source.transport()) {
        let Some(value) = data.get(&key) else {
            return Err(refuse(&format!("has no `{key}` key")));
        };
        out.insert(key, value.clone());
    }
    if let Some(host) = source.ssh_known_hosts_host() {
        let known_hosts = std::str::from_utf8(&out[SSH_KNOWN_HOSTS_KEY].0)
            .map_err(|_| refuse("has a non-UTF-8 `known_hosts`"))?;
        validate_known_hosts(known_hosts, Some(&host)).map_err(|_| {
            refuse(&format!(
                "has a `known_hosts` that is not only for `{host}`"
            ))
        })?;
    }
    Ok(out)
}

fn validate_tenant_repo_dir(tenant_id: &str, repo_dir: &str) -> Result<(), KubeError> {
    let root = format!("tenants/{tenant_id}");
    let contained = repo_dir == root || repo_dir.starts_with(&format!("{root}/"));
    if !contained {
        return Err(KubeError::InvalidInput(format!(
            "repo_dir `{repo_dir}` must be inside `{root}/` of the tenant repository"
        )));
    }
    Ok(())
}

/// `gitSource` of a tenant PKO `Stack` cloning its own repository: tracks `main` (shallow),
/// `repoDir` inside `tenants/<id>/`, `gitAuth` from `tenant-<id>/keel-gitops-credentials`. For
/// SSH also apply [`tenant_repo_workspace_template`] (workspace `known_hosts`) and
/// [`merge_pko_operator_ssh_known_hosts_values`] (operator branch tracking).
pub fn tenant_repo_stack_git_source(
    source: &TenantRepoSource,
    repo_dir: &str,
) -> Result<PkoGitSource, KubeError> {
    validate_tenant_repo_dir(source.tenant_id(), repo_dir)?;
    let git = PkoGitSource::branch(source.url(), source.branch(), repo_dir).with_auth(Some(
        tenant_repo_git_auth(source.transport(), TENANT_GITOPS_CREDENTIALS_SECRET_NAME)?,
    ));
    git.validate()
        .map_err(|e| KubeError::InvalidInput(e.to_string()))?;
    Ok(git)
}

/// `true` for a full SHA-1 (40) or SHA-256 (64) hex commit id. PKO/go-git turns `spec.commit`
/// into a full hash, so an abbreviated SHA would be zero-padded and never resolve.
pub fn is_full_commit_sha(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64) && commit.chars().all(|c| c.is_ascii_hexdigit())
}

/// `gitSource` of an ephemeral preview `Stack` for a tenant repository PR: pinned to the PR head
/// `commit` (a full SHA, [`is_full_commit_sha`]; never a moving branch), authenticated with the
/// per-preview credential copy `preview_secret_name` in `keel-previews` (keys
/// [`tenant_repo_preview_secret_keys`]). The checkout is not shallow
/// ([`PkoGitSource::commit`]), so the pinned commit still resolves after a later push moves the
/// PR branch tip.
pub fn tenant_repo_preview_git_source(
    source: &TenantRepoSource,
    commit: &str,
    repo_dir: &str,
    preview_secret_name: &str,
) -> Result<PkoGitSource, KubeError> {
    validate_tenant_repo_dir(source.tenant_id(), repo_dir)?;
    if !is_full_commit_sha(commit) {
        return Err(KubeError::InvalidInput(format!(
            "preview commit `{commit}` must be a full 40- or 64-character hex SHA"
        )));
    }
    let git = PkoGitSource::commit(source.url(), commit, repo_dir).with_auth(Some(
        tenant_repo_git_auth(source.transport(), preview_secret_name)?,
    ));
    git.validate()
        .map_err(|e| KubeError::InvalidInput(e.to_string()))?;
    Ok(git)
}

/// Adds the clone trust a tenant repository needs to a workspace template: the public Keel CA
/// (HTTPS to the in-cluster Forgejo; harmless otherwise) and, for SSH, the `known_hosts` item of
/// `secret_name` for the `fetch` init container ([`PkoWorkspaceTemplate::with_ssh_known_hosts`]).
pub fn tenant_repo_workspace_template(
    template: PkoWorkspaceTemplate,
    transport: TenantRepoTransport,
    secret_name: &str,
) -> PkoWorkspaceTemplate {
    let template = template.with_keel_ca_trust();
    match transport {
        TenantRepoTransport::Https => template,
        TenantRepoTransport::Ssh => template.with_ssh_known_hosts(secret_name, SSH_KNOWN_HOSTS_KEY),
    }
}

/// `true` when some tenant repository is reached over SSH, i.e. the PKO operator needs
/// `SSH_KNOWN_HOSTS` ([`merge_pko_operator_ssh_known_hosts_values`]) to track its branch.
pub fn pko_operator_needs_ssh_known_hosts(sources: &[TenantRepoSource]) -> bool {
    sources
        .iter()
        .any(|s| s.transport() == TenantRepoTransport::Ssh)
}

/// Synthesizes `keel-system/keel-pko-ssh-known-hosts` (key `known_hosts`): the public SSH host
/// keys of every SSH tenant repository (deduplicated, sorted), read by the PKO operator through
/// `SSH_KNOWN_HOSTS` for **every** tenant's branch tracking.
///
/// Each `(source, known_hosts)` pair may only contribute entries for exactly that source's host
/// ([`TenantRepoSource::ssh_known_hosts_host`]); anything else is refused. The `known_hosts`
/// content must come from **platform configuration** (forge host keys pinned at bootstrap /
/// forge setup), never from Secrets in tenant namespaces (a tenant-supplied key for a shared
/// host such as `github.com` would let a network attacker impersonate the forge to the operator
/// for all tenants). Host keys are public, so a ConfigMap is appropriate; never pass private
/// material.
pub fn build_pko_operator_known_hosts_configmap(
    platform_host_keys: &[(&TenantRepoSource, &str)],
) -> Result<Value, KubeError> {
    let mut entries = BTreeSet::new();
    for (source, known_hosts) in platform_host_keys {
        let Some(host) = source.ssh_known_hosts_host() else {
            return Err(KubeError::InvalidInput(format!(
                "tenant `{}` repository is not reached over SSH; it needs no operator host key",
                source.tenant_id()
            )));
        };
        for line in validate_known_hosts(known_hosts, Some(&host))?.lines() {
            entries.insert(line.to_string());
        }
    }
    if entries.is_empty() {
        return Err(KubeError::InvalidInput(
            "the PKO operator known_hosts needs at least one host key".to_string(),
        ));
    }
    let mut content = entries.into_iter().collect::<Vec<_>>().join("\n");
    content.push('\n');
    Ok(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": PKO_OPERATOR_SSH_KNOWN_HOSTS_CONFIGMAP,
            "namespace": KEEL_SYSTEM_NAMESPACE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "app.kubernetes.io/part-of": "keel-control-plane",
            },
        },
        "data": { SSH_KNOWN_HOSTS_KEY: content },
    }))
}

/// Upserts the PKO operator SSH host-key trust into the `pulumi-kubernetes-operator` HelmRelease
/// `values` (alongside the existing `SSL_CERT_DIR` Keel CA trust, which is kept):
/// `extraEnv` `SSH_KNOWN_HOSTS=/etc/keel-ssh/known_hosts`, `extraVolumes` from
/// [`PKO_OPERATOR_SSH_KNOWN_HOSTS_CONFIGMAP`] (`optional: true`, so the operator, which also runs
/// the protected management Stack, starts without it: anti-lockout) and the read-only mount.
/// Idempotent. Only needed when [`pko_operator_needs_ssh_known_hosts`].
pub fn merge_pko_operator_ssh_known_hosts_values(values: &mut Value) -> Result<(), KubeError> {
    if values.is_null() {
        *values = json!({});
    }
    let obj = values.as_object_mut().ok_or_else(|| {
        KubeError::InvalidInput("PKO HelmRelease values are not a map".to_string())
    })?;
    let upsert = |obj: &mut serde_json::Map<String, Value>, key: &str, item: Value| {
        let list = obj.entry(key).or_insert_with(|| json!([]));
        let items = list.as_array_mut().ok_or_else(|| {
            KubeError::InvalidInput(format!("PKO HelmRelease values `{key}` is not a list"))
        })?;
        items.retain(|i| i.get("name") != item.get("name"));
        items.push(item);
        Ok::<(), KubeError>(())
    };
    upsert(
        obj,
        "extraEnv",
        json!({
            "name": SSH_KNOWN_HOSTS_ENV,
            "value": format!("{KEEL_SSH_KNOWN_HOSTS_MOUNT_PATH}/{KEEL_SSH_KNOWN_HOSTS_FILE}"),
        }),
    )?;
    upsert(
        obj,
        "extraVolumes",
        json!({
            "name": KEEL_SSH_KNOWN_HOSTS_VOLUME_NAME,
            "configMap": {
                "name": PKO_OPERATOR_SSH_KNOWN_HOSTS_CONFIGMAP,
                "optional": true,
                "items": [{ "key": SSH_KNOWN_HOSTS_KEY, "path": KEEL_SSH_KNOWN_HOSTS_FILE }],
            },
        }),
    )?;
    upsert(
        obj,
        "extraVolumeMounts",
        json!({
            "name": KEEL_SSH_KNOWN_HOSTS_VOLUME_NAME,
            "mountPath": KEEL_SSH_KNOWN_HOSTS_MOUNT_PATH,
            "readOnly": true,
        }),
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "tenant_repo_tests.rs"]
mod tests;
