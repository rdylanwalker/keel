//! Ephemeral PKO v2 preview runner (`SDD.md` §4.2, `NFR-REL-02`).
//!
//! Applies preview `Stack` (`spec.preview: true`) CRs into `keel-previews` with server-side apply
//! (the Stack clones the PR branch through PKO `gitSource`; no Flux `GitRepository`/artifact), reads PKO `Stack`/`Update` status and workspace pod logs, persists the
//! rendered report in a `ConfigMap` owned by the preview `Stack`, and deletes previews.
//!
//! All preview state lives in the cluster (labels/annotations on the `Stack` + report
//! `ConfigMap`), so it survives `keel-server` restarts and is shared across replicas (OPEN-10).
//! The trait keeps `keel-server` testable via `InMemoryPreviewCluster` (feature `test-support`).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use keel_core::{
    KEEL_CA_BUNDLE_CONFIGMAP, KEEL_CA_BUNDLE_KEY, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE,
};
use kube::api::{
    Api, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams, PostParams,
};
use kube::discovery::ApiResource;
use kube::{Client, ResourceExt};
use serde_json::{Value, json};

use keel_pko::{DEFAULT_PREVIEW_SERVICE_ACCOUNT, PREVIEW_REPORT_CONFIGMAP_KEY, PreviewSecretCopy};

use crate::KubeError;
use crate::secret_store::TENANT_S3_IDENTITY_PENDING_ANNOTATION;
use crate::tenant::{
    TENANT_GIT_CREDENTIALS_SECRET_KIND, TENANT_GIT_CREDENTIALS_SECRET_NAME,
    TENANT_S3_CREDENTIALS_SECRET_NAME, tenant_namespace,
};
use crate::tenant_repo::TENANT_GITOPS_CREDENTIALS_SECRET_NAME;

/// Server-side apply field manager for preview CRs.
pub const PREVIEW_FIELD_MANAGER: &str = "keel-server-preview";
/// Label selecting Keel ephemeral preview objects.
pub const PREVIEW_LABEL_SELECTOR: &str = "keel.io/ephemeral-preview=true";
/// Tenant Flux `GitRepository` name (`tenant-<id>/keel-gitops`).
pub const TENANT_GIT_REPOSITORY_NAME: &str = "keel-gitops";
/// `keel.io/secret-kind` for tenant-scoped S3 state credentials mirrored into `keel-previews`.
pub const PREVIEW_S3_SECRET_KIND: &str = "preview-tenant-s3-credentials";
/// Flux `GitRepository` auth keys a preview copies from the tenant's read-only Git credentials;
/// any other key in the tenant Secret is never staged in the shared preview namespace.
pub const FLUX_GIT_AUTH_KEYS: &[&str] = &[
    "username",
    "password",
    "identity",
    "identity.pub",
    "known_hosts",
    "bearerToken",
    "ca.crt",
];
/// `keel.io/secret-kind` for per-preview copies of Stack-referenced provider credentials.
pub const PREVIEW_PROVIDER_SECRET_KIND: &str = "preview-provider-credentials";

/// Name of the `keel-previews` mirror of a tenant's **scoped** S3 identity (never admin keys).
pub fn preview_s3_secret_name(tenant: &str) -> Result<String, KubeError> {
    tenant_namespace(tenant)?;
    Ok(format!("keel-preview-s3-{tenant}"))
}

/// Name of the `ConfigMap` persisting the rendered report for a preview `Stack`.
pub fn preview_report_configmap_name(stack_name: &str) -> String {
    let base = stack_name
        .strip_prefix("preview-stack-")
        .unwrap_or(stack_name);
    format!("preview-report-{base}")
}

/// Name of the PKO v2 workspace pod backing `stack_name` (`<stack>-workspace-0`).
pub fn workspace_pod_name(stack_name: &str) -> String {
    format!("{stack_name}-workspace-0")
}

/// Cluster-side inputs resolved before synthesizing a preview for `tenant`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewPrerequisites {
    /// `keel-previews` Secret holding the tenant's scoped S3 keys (`accessKey`/`secretKey`).
    pub s3_secret: Option<String>,
    /// Cluster-reachable Git URL of the GitOps repository, taken **only** from the tenant's own
    /// `tenant-<id>/keel-gitops` Flux source (never the platform `flux-system` source).
    pub git_url: Option<String>,
    /// The tenant's validated read-only Git credentials Secret in `tenant-<id>` (always
    /// `git-credentials`). Each preview copies only [`Self::git_secret_keys`] of it into its own
    /// `keel-previews/keel-preview-<pr>-git-credentials`, owned by (and GC'd with) the preview
    /// `Stack`. `None` for a public tenant source.
    pub git_secret: Option<String>,
    /// Allow-listed Flux Git auth keys present in [`Self::git_secret`] (selects the PKO `gitAuth`
    /// mode and the keys copied per preview). Never values.
    pub git_secret_keys: BTreeSet<String>,
}

/// A tenant's own Flux Git source (`tenant-<id>/keel-gitops`) resolved for a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantGitSource {
    /// Always `tenant-<id>`: previews never read the platform `flux-system` source.
    pub namespace: String,
    /// Cluster-reachable Git URL of the tenant source.
    pub url: String,
    /// Read-only credentials Secret (in `namespace`) referenced by the tenant source.
    pub secret_name: Option<String>,
    /// `spec.ref.branch` of the tenant source (`None`: Flux default branch, Keel uses `main`).
    pub branch: Option<String>,
}

impl TenantGitSource {
    /// Branch tenant Stacks track (`spec.ref.branch`, default `main`).
    pub fn branch_or_default(&self) -> &str {
        self.branch.as_deref().unwrap_or(DEFAULT_TENANT_GIT_BRANCH)
    }

    /// URL tenant and preview PKO `Stack`s clone (`spec.projectRepo`), derived from the Flux URL
    /// with [`keel_pko::pko_project_repo_url`]: external `https://`/`ssh://` forges unchanged, the
    /// in-cluster Forgejo `http://` Service mapped to the platform-configured Forgejo HTTPS origin
    /// (`forge_public_url`, from `.keel/config.yaml` `forge.publicUrl`), anything else refused
    /// with [`KubeError::TenantSourceUnavailable`]. The Flux `GitRepository` itself stays on its
    /// own URL.
    pub fn pko_project_repo(&self, forge_public_url: Option<&str>) -> Result<String, KubeError> {
        keel_pko::pko_project_repo_url(&self.url, forge_public_url).map_err(|e| {
            KubeError::TenantSourceUnavailable(format!(
                "tenant Git source `{}/{TENANT_GIT_REPOSITORY_NAME}` cannot be cloned by PKO: {e}",
                self.namespace
            ))
        })
    }
}

/// Branch tenant Stacks track when the tenant source does not pin one.
pub const DEFAULT_TENANT_GIT_BRANCH: &str = "main";

/// Repository-relative path of the platform-written tenant Flux source stub
/// (`tenants/<id>/git-repository.yaml`, defining `tenant-<id>/keel-gitops`).
pub fn tenant_git_repository_manifest_path(tenant: &str) -> Result<String, KubeError> {
    tenant_namespace(tenant)?;
    Ok(format!("tenants/{tenant}/git-repository.yaml"))
}

/// Resolves the Git source tenant PKO `Stack`s clone (`spec.projectRepo`/`branch`/`gitAuth`) from
/// the **platform-written** `tenants/<id>/git-repository.yaml` committed on the GitOps `main`
/// branch (`manifest` is its content, `None` when absent), never from the live, tenant-editable
/// `tenant-<id>/keel-gitops` object and never from the platform `flux-system` source.
///
/// Fails closed with [`KubeError::TenantSourceUnavailable`] when the tenant has no such stub, the
/// stub does not define `tenant-<id>/keel-gitops` with a network Git URL, or it references a
/// credentials Secret other than the read-only `git-credentials`.
pub fn tenant_git_source_from_manifest(
    tenant: &str,
    manifest: Option<&str>,
) -> Result<TenantGitSource, KubeError> {
    let tenant_ns = tenant_namespace(tenant)?;
    let path = tenant_git_repository_manifest_path(tenant)?;
    let unavailable = |detail: &str| {
        KubeError::TenantSourceUnavailable(format!(
            "tenant `{tenant}` {detail}; Pulumi Stacks and previews only clone the tenant's own \
             Git source `{tenant_ns}/{TENANT_GIT_REPOSITORY_NAME}` (no fallback to the platform \
             repository). Ask a platform admin to provision `{path}` (Flux GitRepository \
             `{tenant_ns}/{TENANT_GIT_REPOSITORY_NAME}`) and its read-only \
             `{TENANT_GIT_CREDENTIALS_SECRET_NAME}` Secret, then retry"
        ))
    };
    let Some(manifest) = manifest else {
        return Err(unavailable("has no platform-written Flux Git source"));
    };
    let mut repo = None;
    for doc in keel_core::split_yaml_documents(manifest) {
        let Ok(value) = noyalib::from_str::<Value>(&doc) else {
            return Err(unavailable("has an unparsable Flux Git source manifest"));
        };
        let is_repo = value.get("kind").and_then(Value::as_str) == Some("GitRepository")
            && value.pointer("/metadata/name").and_then(Value::as_str)
                == Some(TENANT_GIT_REPOSITORY_NAME);
        if is_repo {
            repo = Some(value);
            break;
        }
    }
    let source = tenant_git_source_from_repository(tenant, repo.as_ref()).map_err(|e| match e {
        KubeError::PreviewSourceUnavailable(_) => {
            unavailable("has no valid Flux GitRepository in its own namespace")
        }
        other => other,
    })?;
    // The Flux URL may be the in-cluster Forgejo `http://` Service; PKO Stacks clone the URL
    // derived from it by [`TenantGitSource::pko_project_repo`].
    if let Err(e) = keel_pko::validate_flux_git_source_url(&source.url) {
        return Err(unavailable(&format!(
            "has a Flux Git source that is not a network Git URL ({e})"
        )));
    }
    // This resolver serves only Local Git provider tenants (D8), whose source is the platform
    // repository with the platform-written `git-credentials` Secret. Tenants with their own
    // repository resolve their source from it (`keel-server` `tenant_repos`), never from here.
    if source.secret_name.as_deref() == Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME) {
        return Err(unavailable(&format!(
            "uses a per-tenant repository credential (`{TENANT_GITOPS_CREDENTIALS_SECRET_NAME}`); \
             Local Git provider tenants only use the platform-written \
             `{TENANT_GIT_CREDENTIALS_SECRET_NAME}` source"
        )));
    }
    // The platform-written `git-credentials` Secret holds HTTP(S) `username`/`password` only; an
    // SSH source would commit a Stack whose `sshAuth` key does not exist and fail at PKO runtime.
    if keel_pko::is_ssh_git_url(&source.url) {
        return Err(unavailable(
            "has an SSH Flux Git source; Local Git provider tenants only clone over HTTP(S) with \
             the platform-written read-only token",
        ));
    }
    if let Some(secret) = source.secret_name.as_deref()
        && secret != TENANT_GIT_CREDENTIALS_SECRET_NAME
    {
        return Err(unavailable(&format!(
            "has a Flux Git source referencing credentials Secret `{secret}` instead of the \
             read-only `{TENANT_GIT_CREDENTIALS_SECRET_NAME}`"
        )));
    }
    Ok(source)
}

/// Validates the tenant's `keel-gitops` `GitRepository` (as fetched from `tenant-<id>`) and
/// extracts the preview Git source.
///
/// Fails closed with [`KubeError::PreviewSourceUnavailable`] when the tenant has no source, the
/// object is not `tenant-<id>/keel-gitops`, or it has no `spec.url`. There is intentionally no
/// fallback to the platform `flux-system/flux-system` source, which grants whole-repository read.
/// `keel-gitops` carries [`crate::PLATFORM_OWNED_LABEL`], so Kyverno
/// (`protect-platform-owned-tenant-objects`) keeps tenants from editing its `spec.url`.
pub fn tenant_git_source_from_repository(
    tenant: &str,
    repo: Option<&Value>,
) -> Result<TenantGitSource, KubeError> {
    let tenant_ns = tenant_namespace(tenant)?;
    let unavailable = |detail: &str| {
        KubeError::PreviewSourceUnavailable(format!(
            "tenant `{tenant}` {detail}; previews only use the tenant's own Flux source \
             `{tenant_ns}/{TENANT_GIT_REPOSITORY_NAME}` (no fallback to the platform repository). \
             Ask a platform admin to provision `{tenant_ns}/{TENANT_GIT_REPOSITORY_NAME}` and its \
             read-only `{TENANT_GIT_CREDENTIALS_SECRET_NAME}` Secret, then re-run the proposal"
        ))
    };
    let Some(repo) = repo else {
        return Err(unavailable("has no Flux GitRepository"));
    };
    let ns = repo.pointer("/metadata/namespace").and_then(Value::as_str);
    let name = repo.pointer("/metadata/name").and_then(Value::as_str);
    if ns != Some(tenant_ns.as_str()) || name != Some(TENANT_GIT_REPOSITORY_NAME) {
        return Err(unavailable(
            "has no Flux GitRepository in its own namespace",
        ));
    }
    let url = repo
        .pointer("/spec/url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| unavailable("has a Flux GitRepository without `spec.url`"))?;
    let secret_name = repo
        .pointer("/spec/secretRef/name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let branch = repo
        .pointer("/spec/ref/branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    Ok(TenantGitSource {
        namespace: tenant_ns,
        url: url.to_string(),
        secret_name,
        branch,
    })
}

/// Validates the tenant credentials Secret referenced by `tenant-<id>/keel-gitops` and returns the
/// exact (allow-listed) payload each preview copies into its own `keel-previews` Secret.
///
/// Fails closed with [`KubeError::PreviewSourceUnavailable`] unless the reference is the
/// platform-written read-only `git-credentials` Secret (labelled
/// `keel.io/secret-kind=tenant-git-credentials-readonly`), so a tenant cannot make `keel-server`
/// copy arbitrary tenant Secrets (kubeconfigs, provider keys, SOPS keys) into `keel-previews`.
/// Only the Flux Git auth keys in [`FLUX_GIT_AUTH_KEYS`] are returned.
pub fn tenant_git_credentials_payload(
    tenant: &str,
    source: &TenantGitSource,
    secret_name: &str,
    secret: Option<&Secret>,
) -> Result<BTreeMap<String, k8s_openapi::ByteString>, KubeError> {
    let namespace = &source.namespace;
    let refusal = |detail: String| {
        KubeError::PreviewSourceUnavailable(format!(
            "tenant `{tenant}` Flux source `{namespace}/{TENANT_GIT_REPOSITORY_NAME}` {detail}; \
             ask a platform admin to restore the read-only `{namespace}/\
             {TENANT_GIT_CREDENTIALS_SECRET_NAME}` Secret, then re-run the proposal"
        ))
    };
    if secret_name != TENANT_GIT_CREDENTIALS_SECRET_NAME {
        return Err(refusal(format!(
            "references credentials Secret `{secret_name}`; previews only mirror the read-only \
             `{TENANT_GIT_CREDENTIALS_SECRET_NAME}` Secret"
        )));
    }
    let Some(secret) = secret else {
        return Err(refusal(format!(
            "references credentials Secret `{namespace}/{secret_name}`, which does not exist"
        )));
    };
    let kind = secret
        .labels()
        .get("keel.io/secret-kind")
        .map(String::as_str);
    if kind != Some(TENANT_GIT_CREDENTIALS_SECRET_KIND) {
        return Err(refusal(format!(
            "references credentials Secret `{namespace}/{secret_name}`, which is not labelled \
             `keel.io/secret-kind={TENANT_GIT_CREDENTIALS_SECRET_KIND}`"
        )));
    }
    Ok(secret
        .data
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| FLUX_GIT_AUTH_KEYS.contains(&k.as_str()))
        .collect())
}

/// Cluster operations required by the ephemeral preview lifecycle.
#[async_trait]
pub trait PreviewCluster: Send + Sync {
    /// Resolves (and mirrors into `keel-previews`) the tenant-scoped state/Git credentials.
    ///
    /// Fails closed with [`KubeError::PreviewSourceUnavailable`] when the tenant has no usable
    /// `tenant-<id>/keel-gitops` source; never falls back to the platform `flux-system` source.
    async fn ensure_preview_prerequisites(
        &self,
        tenant: &str,
    ) -> Result<PreviewPrerequisites, KubeError>;

    /// Preview prerequisites of a tenant with its own repository (`source`, derived by keel-server
    /// from the platform forge configuration, never from the tenant-editable `keel-gitops`
    /// object): validates the platform-written `tenant-<id>/keel-gitops-credentials` Secret
    /// (`tenant_repo_preview_secret_payload`) and mirrors the tenant-scoped state
    /// identity. `git_secret` is that Secret, `git_secret_keys` exactly the keys a preview copies
    /// (HTTPS `username`/`password`, SSH `sshPrivateKey`/`known_hosts`) and `git_url` the tenant
    /// repository URL. Fails closed with [`KubeError::PreviewSourceUnavailable`].
    async fn ensure_tenant_repo_preview_prerequisites(
        &self,
        source: &crate::TenantRepoSource,
    ) -> Result<PreviewPrerequisites, KubeError> {
        Err(KubeError::PreviewSourceUnavailable(format!(
            "tenant `{}` repository previews are not supported by this preview runner",
            source.tenant_id()
        )))
    }

    /// Server-side applies the preview `Stack` document (PKO `gitSource`, no Flux `GitRepository`).
    async fn apply_preview(&self, stack: &Value) -> Result<(), KubeError>;

    /// Lists preview `Stack` objects in `keel-previews`.
    async fn list_preview_stacks(&self) -> Result<Vec<Value>, KubeError>;

    /// Fetches a preview `Stack` by name.
    async fn get_preview_stack(&self, name: &str) -> Result<Option<Value>, KubeError>;

    /// Fetches a PKO `auto.pulumi.com/v1alpha1` `Update` in `keel-previews` by name.
    async fn get_update(&self, name: &str) -> Result<Option<Value>, KubeError>;

    /// Reads the tail of the preview workspace pod's `pulumi` container log.
    async fn workspace_log(
        &self,
        stack_name: &str,
        tail_lines: i64,
    ) -> Result<Option<String>, KubeError>;

    /// Merges `annotations` into the preview `Stack` metadata (no spec change ⇒ no PKO re-run).
    async fn annotate_preview_stack(
        &self,
        name: &str,
        annotations: &BTreeMap<String, String>,
    ) -> Result<(), KubeError>;

    /// Persists the serialized report into a `ConfigMap` owned by the preview `Stack`.
    async fn store_report(&self, stack_name: &str, report_json: &str) -> Result<(), KubeError>;

    /// Loads the persisted report for a preview `Stack`.
    async fn load_report(&self, stack_name: &str) -> Result<Option<String>, KubeError>;

    /// Deletes a preview object (`Stack`, `ConfigMap`). `Ok(false)` if absent.
    async fn delete_preview_object(&self, kind: &str, name: &str) -> Result<bool, KubeError>;

    /// Copies exactly the planned per-preview Secrets (only the referenced keys) from
    /// `tenant-<id>` into `keel-previews` under their per-preview names, labelled
    /// `keel.io/secret-kind=preview-provider-credentials`, `keel.io/tenant` and
    /// `keel.io/pr-id=<pr_id>` (normalized like the preview `Stack` label). Copies carrying
    /// `proposed_data` use those PR values instead of the live tenant Secret. Fails if a source
    /// Secret or referenced key is missing.
    ///
    /// Runs **before** the preview `Stack` is applied: PKO resolves `spec.gitAuth`/`envRefs`
    /// immediately and stalls a `Stack` whose Secret is missing (`SpecInvalid`, never retried
    /// until the spec changes). The copies therefore carry no owner yet; see
    /// [`Self::adopt_preview_secrets`]. Returns the names written (possibly a prefix of `copies`
    /// on error; callers clean up with [`Self::delete_preview_secrets`]).
    async fn copy_preview_secrets(
        &self,
        tenant: &str,
        pr_id: &str,
        copies: &[PreviewSecretCopy],
    ) -> Result<Vec<String>, KubeError>;

    /// Sets the ownerReference of the per-preview Secret copies `secret_names` to the live preview
    /// `stack_name` (`pulumi.com/v1` `Stack`, its UID), so they are garbage-collected with it.
    async fn adopt_preview_secrets(
        &self,
        stack_name: &str,
        secret_names: &[String],
    ) -> Result<(), KubeError>;

    /// Deletes every per-preview Secret copy of `pr_id` (selected by the
    /// `keel.io/secret-kind=preview-provider-credentials` and `keel.io/pr-id` labels, so copies
    /// never adopted by a `Stack` are found too). Returns the number deleted.
    async fn delete_preview_secrets(&self, pr_id: &str) -> Result<usize, KubeError>;

    /// PR ids (`keel.io/pr-id` labels) of all per-preview Secret copies in `keel-previews`
    /// (metadata only; never Secret values).
    async fn list_preview_secret_pr_ids(&self) -> Result<BTreeSet<String>, KubeError>;
}

/// Normalizes a PR id into the `keel.io/pr-id` label value used on preview objects (trimmed,
/// leading `#` stripped, lowercase, a DNS label), refusing anything else so it can never alter a
/// label selector.
pub fn preview_pr_label(pr_id: &str) -> Result<String, KubeError> {
    let normalized = pr_id.trim().trim_start_matches('#').to_ascii_lowercase();
    let valid = !normalized.is_empty()
        && normalized.len() <= 63
        && normalized
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !normalized.starts_with('-')
        && !normalized.ends_with('-');
    if valid {
        Ok(normalized)
    } else {
        Err(KubeError::InvalidInput(format!(
            "invalid preview PR id `{pr_id}`"
        )))
    }
}

fn preview_secret_selector(pr_label: Option<&str>) -> String {
    let mut selector = format!("keel.io/secret-kind={PREVIEW_PROVIDER_SECRET_KIND}");
    if let Some(pr) = pr_label {
        selector.push_str(&format!(",keel.io/pr-id={pr}"));
    }
    selector
}

fn preview_stack_owner(stack_name: &str, uid: Option<String>) -> Result<OwnerReference, KubeError> {
    let uid = uid.filter(|u| !u.is_empty()).ok_or_else(|| {
        KubeError::InvalidInput(format!("preview Stack `{stack_name}` has no UID"))
    })?;
    Ok(OwnerReference {
        api_version: "pulumi.com/v1".to_string(),
        kind: "Stack".to_string(),
        name: stack_name.to_string(),
        uid,
        controller: None,
        block_owner_deletion: None,
    })
}

/// Resolves the exact key → bytes payload for one preview Secret copy.
fn preview_copy_payload(
    copy: &PreviewSecretCopy,
    tenant_secret: Option<&BTreeMap<String, Vec<u8>>>,
) -> Result<BTreeMap<String, Vec<u8>>, KubeError> {
    if !copy.target_name.starts_with("keel-preview-") {
        return Err(KubeError::InvalidInput(format!(
            "preview Secret copy `{}` must be named keel-preview-*",
            copy.target_name
        )));
    }
    let mut out = BTreeMap::new();
    for key in &copy.keys {
        let value = match (&copy.proposed_data, tenant_secret) {
            (Some(proposed), _) => proposed.get(key).map(|v| v.as_bytes().to_vec()),
            (None, Some(live)) => live.get(key).cloned(),
            (None, None) => {
                return Err(KubeError::InvalidInput(format!(
                    "Secret `{}` referenced by the tenant Stack does not exist",
                    copy.source_name
                )));
            }
        };
        let Some(value) = value else {
            return Err(KubeError::InvalidInput(format!(
                "Secret `{}` has no key `{key}` referenced by the tenant Stack",
                copy.source_name
            )));
        };
        out.insert(key.clone(), value);
    }
    Ok(out)
}

fn preview_copy_labels(tenant: &str, pr_label: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "keel".to_string(),
        ),
        (
            "keel.io/secret-kind".to_string(),
            PREVIEW_PROVIDER_SECRET_KIND.to_string(),
        ),
        ("keel.io/tenant".to_string(), tenant.to_string()),
        ("keel.io/pr-id".to_string(), pr_label.to_string()),
    ])
}

/// Field manager of the ownerReference patch on preview Secret copies (kept apart from
/// [`PREVIEW_FIELD_MANAGER`] so re-applying a copy never drops its owner).
pub const PREVIEW_OWNER_FIELD_MANAGER: &str = "keel-server-preview-owner";

fn stack_ar() -> ApiResource {
    ApiResource {
        group: "pulumi.com".to_string(),
        version: "v1".to_string(),
        api_version: "pulumi.com/v1".to_string(),
        kind: "Stack".to_string(),
        plural: "stacks".to_string(),
    }
}

fn git_repo_ar() -> ApiResource {
    ApiResource {
        group: "source.toolkit.fluxcd.io".to_string(),
        version: "v1".to_string(),
        api_version: "source.toolkit.fluxcd.io/v1".to_string(),
        kind: "GitRepository".to_string(),
        plural: "gitrepositories".to_string(),
    }
}

fn update_ar() -> ApiResource {
    ApiResource {
        group: "auto.pulumi.com".to_string(),
        version: "v1alpha1".to_string(),
        api_version: "auto.pulumi.com/v1alpha1".to_string(),
        kind: "Update".to_string(),
        plural: "updates".to_string(),
    }
}

fn validate_preview_doc(doc: &Value, expected_kind: &str) -> Result<String, KubeError> {
    let kind = doc.get("kind").and_then(Value::as_str);
    let ns = doc.pointer("/metadata/namespace").and_then(Value::as_str);
    let name = doc.pointer("/metadata/name").and_then(Value::as_str);
    match (kind, ns, name) {
        (Some(k), Some(n), Some(name)) if k == expected_kind && n == KEEL_PREVIEWS_NAMESPACE => {
            if expected_kind == "Stack" && doc.pointer("/spec/preview") != Some(&json!(true)) {
                return Err(KubeError::InvalidInput(
                    "refusing to apply a preview Stack without spec.preview: true".to_string(),
                ));
            }
            Ok(name.to_string())
        }
        _ => Err(KubeError::InvalidInput(format!(
            "preview {expected_kind} must be namespaced to `{KEEL_PREVIEWS_NAMESPACE}`"
        ))),
    }
}

fn not_found(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(e) if e.code == 404)
}

fn to_value<T: serde::Serialize>(obj: &T) -> Result<Value, KubeError> {
    Ok(serde_json::to_value(obj)?)
}

/// Live `kube-rs` implementation of [`PreviewCluster`].
#[derive(Clone)]
pub struct KubePreviewCluster {
    client: Client,
}

impl KubePreviewCluster {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    fn dyn_api(&self, ar: &ApiResource) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), KEEL_PREVIEWS_NAMESPACE, ar)
    }

    async fn mirror_secret(
        &self,
        source: &Secret,
        target_name: &str,
        tenant: &str,
        kind: &str,
        keys: Option<&[&str]>,
    ) -> Result<(), KubeError> {
        let data = source
            .data
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| keys.is_none_or(|allowed| allowed.contains(&k.as_str())))
            .collect::<BTreeMap<_, _>>();
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(target_name.to_string()),
                namespace: Some(KEEL_PREVIEWS_NAMESPACE.to_string()),
                labels: Some(BTreeMap::from([
                    (
                        "app.kubernetes.io/managed-by".to_string(),
                        "keel".to_string(),
                    ),
                    ("keel.io/secret-kind".to_string(), kind.to_string()),
                    ("keel.io/tenant".to_string(), tenant.to_string()),
                ])),
                ..ObjectMeta::default()
            },
            type_: Some("Opaque".to_string()),
            data: Some(data),
            ..Secret::default()
        };
        let api: Api<Secret> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        api.patch(
            target_name,
            &PatchParams::apply(PREVIEW_FIELD_MANAGER).force(),
            &Patch::Apply(&secret),
        )
        .await?;
        Ok(())
    }

    /// Best effort: refreshes `keel-previews/keel-ca-bundle` from the public
    /// `keel-system/keel-ca-bundle` (Kyverno `sync-keel-ca-bundle` normally maintains it) so
    /// preview workspaces trust the in-cluster Forgejo HTTPS ingress. Without a source (no local
    /// CA, external forge with a public CA) nothing is written; the workspace volume is optional.
    async fn ensure_preview_ca_bundle(&self) {
        let source: Api<ConfigMap> = Api::namespaced(self.client.clone(), KEEL_SYSTEM_NAMESPACE);
        let pem = match source.get_opt(KEEL_CA_BUNDLE_CONFIGMAP).await {
            Ok(Some(cm)) => cm
                .data
                .as_ref()
                .and_then(|d| d.get(KEEL_CA_BUNDLE_KEY))
                .cloned(),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(error = %e, "could not read keel-system/{KEEL_CA_BUNDLE_CONFIGMAP}");
                None
            }
        };
        let Some(pem) = pem else {
            return;
        };
        let doc = match crate::ca_bundle::build_keel_ca_bundle_configmap(
            KEEL_PREVIEWS_NAMESPACE,
            &pem,
        ) {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(error = %e, "keel-system/{KEEL_CA_BUNDLE_CONFIGMAP} is not a public CA bundle; not copied");
                return;
            }
        };
        let target: Api<ConfigMap> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        if let Ok(Some(existing)) = target.get_opt(KEEL_CA_BUNDLE_CONFIGMAP).await
            && existing
                .data
                .as_ref()
                .and_then(|d| d.get(KEEL_CA_BUNDLE_KEY))
                .map(String::as_str)
                == doc["data"][KEEL_CA_BUNDLE_KEY].as_str()
        {
            return;
        }
        if let Err(e) = target
            .patch(
                KEEL_CA_BUNDLE_CONFIGMAP,
                &PatchParams::apply(PREVIEW_FIELD_MANAGER).force(),
                &Patch::Apply(&doc),
            )
            .await
        {
            tracing::warn!(error = %e, "could not refresh keel-previews/{KEEL_CA_BUNDLE_CONFIGMAP}");
        }
    }

    async fn ensure_service_account(&self) {
        let api: Api<ServiceAccount> =
            Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        if matches!(
            api.get_opt(DEFAULT_PREVIEW_SERVICE_ACCOUNT).await,
            Ok(Some(_))
        ) {
            return;
        }
        let sa = ServiceAccount {
            metadata: ObjectMeta {
                name: Some(DEFAULT_PREVIEW_SERVICE_ACCOUNT.to_string()),
                namespace: Some(KEEL_PREVIEWS_NAMESPACE.to_string()),
                labels: Some(BTreeMap::from([
                    (
                        "app.kubernetes.io/managed-by".to_string(),
                        "keel".to_string(),
                    ),
                    ("keel.io/role".to_string(), "preview-readonly".to_string()),
                ])),
                ..ObjectMeta::default()
            },
            ..ServiceAccount::default()
        };
        if let Err(e) = api.create(&PostParams::default(), &sa).await {
            tracing::warn!(error = %e, "could not create keel-previews/{DEFAULT_PREVIEW_SERVICE_ACCOUNT}");
        }
    }

    /// Reads **only** the tenant's own `tenant-<id>/keel-gitops` source. There is deliberately no
    /// fallback to the platform `flux-system/flux-system` (whole-repository) source.
    async fn resolve_git_source(&self, tenant: &str) -> Result<TenantGitSource, KubeError> {
        let tenant_ns = tenant_namespace(tenant)?;
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &tenant_ns, &git_repo_ar());
        let repo = match api.get_opt(TENANT_GIT_REPOSITORY_NAME).await? {
            Some(obj) => Some(to_value(&obj)?),
            None => None,
        };
        tenant_git_source_from_repository(tenant, repo.as_ref())
    }

    /// Resolves the tenant Git source and (when it references one) the validated, allow-listed
    /// read-only credentials payload. Performs reads only.
    /// Mirrors the tenant's scoped S3 identity into `keel-previews` (fail closed: never the
    /// control-plane admin keys); `None` while the tenant has no (finished) identity.
    async fn mirror_tenant_s3_identity(&self, tenant: &str) -> Result<Option<String>, KubeError> {
        let tenant_ns = tenant_namespace(tenant)?;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &tenant_ns);
        let Some(scoped) = secrets.get_opt(TENANT_S3_CREDENTIALS_SECRET_NAME).await? else {
            return Ok(None);
        };
        if scoped
            .annotations()
            .contains_key(TENANT_S3_IDENTITY_PENDING_ANNOTATION)
        {
            return Ok(None);
        }
        let name = preview_s3_secret_name(tenant)?;
        self.mirror_secret(
            &scoped,
            &name,
            tenant,
            PREVIEW_S3_SECRET_KIND,
            Some(&["accessKey", "secretKey"]),
        )
        .await?;
        Ok(Some(name))
    }

    async fn resolve_tenant_git(
        &self,
        tenant: &str,
    ) -> Result<
        (
            TenantGitSource,
            Option<BTreeMap<String, k8s_openapi::ByteString>>,
        ),
        KubeError,
    > {
        let source = self.resolve_git_source(tenant).await?;
        let Some(secret_name) = source.secret_name.clone() else {
            return Ok((source, None));
        };
        let secret = if secret_name == TENANT_GIT_CREDENTIALS_SECRET_NAME {
            let src: Api<Secret> = Api::namespaced(self.client.clone(), &source.namespace);
            src.get_opt(&secret_name).await?
        } else {
            None
        };
        let payload =
            tenant_git_credentials_payload(tenant, &source, &secret_name, secret.as_ref())?;
        Ok((source, Some(payload)))
    }
}

#[async_trait]
impl PreviewCluster for KubePreviewCluster {
    async fn ensure_preview_prerequisites(
        &self,
        tenant: &str,
    ) -> Result<PreviewPrerequisites, KubeError> {
        tenant_namespace(tenant)?;

        // Tenant-owned Git source only (fail closed: never the platform `flux-system` source).
        // Resolved before anything is written to `keel-previews`, so a refusal stages nothing.
        let (source, git_payload) = match self.resolve_tenant_git(tenant).await {
            Ok(resolved) => resolved,
            Err(err @ KubeError::PreviewSourceUnavailable(_)) => {
                tracing::debug!(tenant, error = %err, "tenant has no usable preview Git source");
                return Err(err);
            }
            Err(err) => return Err(err),
        };

        self.ensure_service_account().await;
        self.ensure_preview_ca_bundle().await;
        let mut out = PreviewPrerequisites {
            s3_secret: self.mirror_tenant_s3_identity(tenant).await?,
            ..PreviewPrerequisites::default()
        };
        // Each preview copies exactly these keys into its own Secret owned by the preview Stack
        // (`keel-preview-<pr>-git-credentials`, GC'd with it); no long-lived per-tenant Git
        // credential is kept in the shared `keel-previews` namespace.
        if let Some(data) = git_payload {
            out.git_secret_keys = data.keys().cloned().collect();
            out.git_secret = source.secret_name.clone();
        }
        out.git_url = Some(source.url);
        Ok(out)
    }

    async fn ensure_tenant_repo_preview_prerequisites(
        &self,
        source: &crate::TenantRepoSource,
    ) -> Result<PreviewPrerequisites, KubeError> {
        let tenant = source.tenant_id();
        let tenant_ns = source.namespace();
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &tenant_ns);
        let secret = secrets
            .get_opt(TENANT_GITOPS_CREDENTIALS_SECRET_NAME)
            .await?
            .ok_or_else(|| {
                KubeError::PreviewSourceUnavailable(format!(
                    "tenant `{tenant}` has no repository read credential `{tenant_ns}/{TENANT_GITOPS_CREDENTIALS_SECRET_NAME}`; re-run tenant provisioning (POST /api/v1/tenants)"
                ))
            })?;
        let payload = crate::tenant_repo::tenant_repo_preview_secret_payload(source, &secret)?;
        self.ensure_service_account().await;
        self.ensure_preview_ca_bundle().await;
        Ok(PreviewPrerequisites {
            s3_secret: self.mirror_tenant_s3_identity(tenant).await?,
            git_url: Some(source.url().to_string()),
            git_secret: Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME.to_string()),
            git_secret_keys: payload.into_keys().collect(),
        })
    }

    async fn apply_preview(&self, stack: &Value) -> Result<(), KubeError> {
        let stack_name = validate_preview_doc(stack, "Stack")?;
        let pp = PatchParams::apply(PREVIEW_FIELD_MANAGER).force();
        self.dyn_api(&stack_ar())
            .patch(&stack_name, &pp, &Patch::Apply(stack))
            .await?;
        Ok(())
    }

    async fn list_preview_stacks(&self) -> Result<Vec<Value>, KubeError> {
        let list = self
            .dyn_api(&stack_ar())
            .list(&ListParams::default().labels(PREVIEW_LABEL_SELECTOR))
            .await?;
        list.items.iter().map(to_value).collect()
    }

    async fn get_preview_stack(&self, name: &str) -> Result<Option<Value>, KubeError> {
        match self.dyn_api(&stack_ar()).get_opt(name).await? {
            Some(obj) => Ok(Some(to_value(&obj)?)),
            None => Ok(None),
        }
    }

    async fn get_update(&self, name: &str) -> Result<Option<Value>, KubeError> {
        match self.dyn_api(&update_ar()).get_opt(name).await {
            Ok(Some(obj)) => Ok(Some(to_value(&obj)?)),
            Ok(None) => Ok(None),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn workspace_log(
        &self,
        stack_name: &str,
        tail_lines: i64,
    ) -> Result<Option<String>, KubeError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        let params = LogParams {
            container: Some("pulumi".to_string()),
            tail_lines: Some(tail_lines),
            ..LogParams::default()
        };
        match pods.logs(&workspace_pod_name(stack_name), &params).await {
            Ok(raw) => Ok(Some(raw)),
            Err(e) if not_found(&e) => Ok(None),
            Err(kube::Error::Api(e)) if e.code == 400 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn annotate_preview_stack(
        &self,
        name: &str,
        annotations: &BTreeMap<String, String>,
    ) -> Result<(), KubeError> {
        let patch = json!({ "metadata": { "annotations": annotations } });
        self.dyn_api(&stack_ar())
            .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn store_report(&self, stack_name: &str, report_json: &str) -> Result<(), KubeError> {
        let Some(stack) = self.dyn_api(&stack_ar()).get_opt(stack_name).await? else {
            return Err(KubeError::InvalidInput(format!(
                "preview Stack `{stack_name}` not found"
            )));
        };
        let owner = OwnerReference {
            api_version: "pulumi.com/v1".to_string(),
            kind: "Stack".to_string(),
            name: stack_name.to_string(),
            uid: stack.metadata.uid.clone().unwrap_or_default(),
            controller: None,
            block_owner_deletion: None,
        };
        let mut labels = stack.labels().clone();
        labels.retain(|k, _| k.starts_with("keel.io/") || k == "app.kubernetes.io/managed-by");
        let cm_name = preview_report_configmap_name(stack_name);
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(cm_name.clone()),
                namespace: Some(KEEL_PREVIEWS_NAMESPACE.to_string()),
                labels: Some(labels),
                owner_references: Some(vec![owner]),
                ..ObjectMeta::default()
            },
            data: Some(BTreeMap::from([(
                PREVIEW_REPORT_CONFIGMAP_KEY.to_string(),
                report_json.to_string(),
            )])),
            ..ConfigMap::default()
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        api.patch(
            &cm_name,
            &PatchParams::apply(PREVIEW_FIELD_MANAGER).force(),
            &Patch::Apply(&cm),
        )
        .await?;
        Ok(())
    }

    async fn load_report(&self, stack_name: &str) -> Result<Option<String>, KubeError> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        Ok(api
            .get_opt(&preview_report_configmap_name(stack_name))
            .await?
            .and_then(|cm| cm.data)
            .and_then(|d| d.get(PREVIEW_REPORT_CONFIGMAP_KEY).cloned()))
    }

    async fn delete_preview_object(&self, kind: &str, name: &str) -> Result<bool, KubeError> {
        let dp = DeleteParams::default();
        let res = match kind {
            "Stack" => self
                .dyn_api(&stack_ar())
                .delete(name, &dp)
                .await
                .map(|_| ()),
            "ConfigMap" => {
                let api: Api<ConfigMap> =
                    Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
                api.delete(name, &dp).await.map(|_| ())
            }
            other => {
                return Err(KubeError::InvalidInput(format!(
                    "unsupported preview object kind `{other}`"
                )));
            }
        };
        match res {
            Ok(()) => Ok(true),
            Err(e) if not_found(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn copy_preview_secrets(
        &self,
        tenant: &str,
        pr_id: &str,
        copies: &[PreviewSecretCopy],
    ) -> Result<Vec<String>, KubeError> {
        let tenant_ns = tenant_namespace(tenant)?;
        let pr_label = preview_pr_label(pr_id)?;
        if copies.is_empty() {
            return Ok(Vec::new());
        }
        let tenant_api: Api<Secret> = Api::namespaced(self.client.clone(), &tenant_ns);
        let preview_api: Api<Secret> =
            Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        let mut created = Vec::new();
        for copy in copies {
            let live = if copy.proposed_data.is_some() {
                None
            } else {
                tenant_api.get_opt(&copy.source_name).await?.map(|s| {
                    s.data
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(k, v)| (k, v.0))
                        .collect::<BTreeMap<_, _>>()
                })
            };
            let payload = preview_copy_payload(copy, live.as_ref())?;
            // No ownerReference: the preview Stack does not exist yet (see the trait docs).
            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(copy.target_name.clone()),
                    namespace: Some(KEEL_PREVIEWS_NAMESPACE.to_string()),
                    labels: Some(preview_copy_labels(tenant, &pr_label)),
                    ..ObjectMeta::default()
                },
                type_: Some("Opaque".to_string()),
                data: Some(
                    payload
                        .into_iter()
                        .map(|(k, v)| (k, k8s_openapi::ByteString(v)))
                        .collect(),
                ),
                ..Secret::default()
            };
            preview_api
                .patch(
                    &copy.target_name,
                    &PatchParams::apply(PREVIEW_FIELD_MANAGER).force(),
                    &Patch::Apply(&secret),
                )
                .await?;
            created.push(copy.target_name.clone());
        }
        Ok(created)
    }

    async fn adopt_preview_secrets(
        &self,
        stack_name: &str,
        secret_names: &[String],
    ) -> Result<(), KubeError> {
        if secret_names.is_empty() {
            return Ok(());
        }
        let Some(stack) = self.dyn_api(&stack_ar()).get_opt(stack_name).await? else {
            return Err(KubeError::InvalidInput(format!(
                "preview Stack `{stack_name}` not found"
            )));
        };
        let owner = preview_stack_owner(stack_name, stack.metadata.uid.clone())?;
        // JSON merge patch: fails with 404 instead of re-creating a deleted copy, and replaces
        // any owner left by an earlier (since deleted) preview Stack of the same PR.
        let patch = json!({ "metadata": { "ownerReferences": [owner] } });
        let params = PatchParams {
            field_manager: Some(PREVIEW_OWNER_FIELD_MANAGER.to_string()),
            ..PatchParams::default()
        };
        let api: Api<Secret> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        for name in secret_names {
            api.patch(name, &params, &Patch::Merge(&patch)).await?;
        }
        Ok(())
    }

    async fn delete_preview_secrets(&self, pr_id: &str) -> Result<usize, KubeError> {
        let pr_label = preview_pr_label(pr_id)?;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        let list = api
            .list_metadata(&ListParams::default().labels(&preview_secret_selector(Some(&pr_label))))
            .await?;
        let mut deleted = 0;
        for secret in list.items {
            let name = secret.name_any();
            match api.delete(&name, &DeleteParams::default()).await {
                Ok(_) => deleted += 1,
                Err(e) if not_found(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(deleted)
    }

    async fn list_preview_secret_pr_ids(&self) -> Result<BTreeSet<String>, KubeError> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        let list = api
            .list_metadata(&ListParams::default().labels(&preview_secret_selector(None)))
            .await?;
        Ok(list
            .items
            .iter()
            .filter_map(|s| s.labels().get("keel.io/pr-id").cloned())
            .collect())
    }
}

#[cfg(any(test, feature = "test-support"))]
mod in_memory;
#[cfg(any(test, feature = "test-support"))]
pub use in_memory::InMemoryPreviewCluster;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;

    fn stack_doc(name: &str, preview: bool) -> Value {
        json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": name, "namespace": KEEL_PREVIEWS_NAMESPACE },
            "spec": { "preview": preview }
        })
    }

    fn tenant_repo(ns: &str, name: &str, url: Option<&str>, secret: Option<&str>) -> Value {
        let mut spec = serde_json::Map::new();
        if let Some(url) = url {
            spec.insert("url".to_string(), json!(url));
        }
        if let Some(secret) = secret {
            spec.insert("secretRef".to_string(), json!({ "name": secret }));
        }
        json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": { "name": name, "namespace": ns },
            "spec": spec,
        })
    }

    #[test]
    fn tenant_git_source_is_resolved_from_the_tenant_namespace_only() {
        let repo = tenant_repo(
            "tenant-acme",
            "keel-gitops",
            Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git"),
            Some("git-credentials"),
        );
        let source = tenant_git_source_from_repository("acme", Some(&repo)).unwrap();
        assert_eq!(
            source,
            TenantGitSource {
                namespace: "tenant-acme".to_string(),
                url: "http://forgejo.keel-system.svc:3000/keel/infra-gitops.git".to_string(),
                secret_name: Some("git-credentials".to_string()),
                branch: None,
            }
        );
        let public = tenant_repo("tenant-acme", "keel-gitops", Some("https://x/r.git"), None);
        assert_eq!(
            tenant_git_source_from_repository("acme", Some(&public))
                .unwrap()
                .secret_name,
            None
        );
    }

    #[test]
    fn missing_tenant_git_source_fails_closed_with_actionable_reason() {
        let err = tenant_git_source_from_repository("acme", None).unwrap_err();
        let KubeError::PreviewSourceUnavailable(msg) = &err else {
            panic!("unexpected error {err:?}");
        };
        assert!(msg.contains("tenant-acme/keel-gitops"), "{msg}");
        assert!(
            msg.contains("no fallback to the platform repository"),
            "{msg}"
        );
        assert!(msg.contains("git-credentials"), "{msg}");

        for bad in [
            tenant_repo("tenant-acme", "keel-gitops", None, None),
            tenant_repo("tenant-acme", "keel-gitops", Some("   "), None),
        ] {
            assert!(matches!(
                tenant_git_source_from_repository("acme", Some(&bad)),
                Err(KubeError::PreviewSourceUnavailable(m)) if m.contains("spec.url")
            ));
        }
        assert!(matches!(
            tenant_git_source_from_repository("Bad_Tenant", None),
            Err(KubeError::InvalidInput(_))
        ));
    }

    #[test]
    fn platform_flux_system_source_is_never_accepted_for_a_tenant_preview() {
        let platform = tenant_repo(
            "flux-system",
            "flux-system",
            Some("http://forgejo.keel-system.svc:3000/keel/infra-gitops.git"),
            Some("git-credentials"),
        );
        let wrong_ns = tenant_repo("flux-system", "keel-gitops", Some("https://x/r.git"), None);
        let other_tenant = tenant_repo("tenant-default", "keel-gitops", Some("https://x"), None);
        let wrong_name = tenant_repo("tenant-acme", "flux-system", Some("https://x"), None);
        for repo in [platform, wrong_ns, other_tenant, wrong_name] {
            assert!(
                matches!(
                    tenant_git_source_from_repository("acme", Some(&repo)),
                    Err(KubeError::PreviewSourceUnavailable(_))
                ),
                "accepted {repo}"
            );
        }
    }

    const DEFAULT_TENANT_STUB: &str = "apiVersion: source.toolkit.fluxcd.io/v1
kind: GitRepository
metadata:
  name: keel-gitops
  namespace: tenant-default
spec:
  interval: 1m
  url: http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git
  ref:
    branch: main
  secretRef:
    name: git-credentials
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: allow-pko-workspace
  namespace: tenant-default
spec:
  podSelector: {}
";

    #[test]
    fn tenant_stack_source_comes_from_the_platform_written_stub() {
        assert_eq!(
            tenant_git_repository_manifest_path("default").unwrap(),
            "tenants/default/git-repository.yaml"
        );
        assert!(tenant_git_repository_manifest_path("../x").is_err());
        let source = tenant_git_source_from_manifest("default", Some(DEFAULT_TENANT_STUB)).unwrap();
        assert_eq!(source.namespace, "tenant-default");
        assert_eq!(
            source.url,
            "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git"
        );
        assert_eq!(source.secret_name.as_deref(), Some("git-credentials"));
        assert_eq!(source.branch_or_default(), "main");
        // Flux keeps the in-cluster http Service; PKO clones the Forgejo HTTPS ingress origin.
        assert_eq!(
            source
                .pko_project_repo(Some("https://forgejo.keel.local:8888"))
                .unwrap(),
            "https://forgejo.keel.local:8888/keel/infra-gitops.git"
        );
        match source.pko_project_repo(None) {
            Err(KubeError::TenantSourceUnavailable(msg)) => {
                assert!(msg.contains("forge.publicUrl"), "{msg}");
                assert!(msg.contains("tenant-default/keel-gitops"), "{msg}");
            }
            other => panic!("expected TenantSourceUnavailable, got {other:?}"),
        }
        // Any other plain-http source fails closed for PKO (Flux could still pull it).
        let other_http = tenant_git_source_from_manifest(
            "default",
            Some(&DEFAULT_TENANT_STUB.replace(
                "http://forgejo-http.keel-system.svc.cluster.local:3000",
                "http://gitea.example.org",
            )),
        )
        .unwrap();
        assert!(matches!(
            other_http.pko_project_repo(Some("https://forgejo.keel.local:8888")),
            Err(KubeError::TenantSourceUnavailable(_))
        ));
        // External HTTPS forges pass through unchanged.
        let external = tenant_git_source_from_manifest(
            "default",
            Some(&DEFAULT_TENANT_STUB.replace(
                "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
                "https://github.com/acme/gitops.git",
            )),
        )
        .unwrap();
        assert_eq!(
            external.pko_project_repo(None).unwrap(),
            "https://github.com/acme/gitops.git"
        );

        let public = DEFAULT_TENANT_STUB
            .replace("  secretRef:\n    name: git-credentials\n", "")
            .replace("  ref:\n    branch: main\n", "");
        let public = tenant_git_source_from_manifest("default", Some(&public)).unwrap();
        assert!(public.secret_name.is_none());
        assert_eq!(public.branch_or_default(), DEFAULT_TENANT_GIT_BRANCH);
    }

    #[test]
    fn tenant_stack_source_fails_closed_without_a_usable_stub() {
        let refused = |tenant: &str, manifest: Option<&str>| match tenant_git_source_from_manifest(
            tenant, manifest,
        ) {
            Err(KubeError::TenantSourceUnavailable(msg)) => msg,
            other => panic!("expected TenantSourceUnavailable, got {other:?}"),
        };
        // No stub (e.g. tenants created through `POST /api/v1/tenants`).
        let msg = refused("acme", None);
        assert!(
            msg.contains("tenants/acme/git-repository.yaml"),
            "refusal names the missing stub file"
        );
        assert!(
            msg.contains("tenant-acme/keel-gitops"),
            "refusal names the tenant source"
        );
        assert!(
            msg.contains("no fallback to the platform"),
            "refusal states there is no fallback"
        );
        // Another tenant's stub never resolves (namespace must be `tenant-<id>`).
        refused("acme", Some(DEFAULT_TENANT_STUB));
        // The platform `flux-system` source is never accepted.
        refused(
            "default",
            Some(&DEFAULT_TENANT_STUB.replace(
                "namespace: tenant-default\nspec:\n  interval",
                "namespace: flux-system\nspec:\n  interval",
            )),
        );
        refused("default", Some("kind: GitRepository\nmetadata: [unclosed"));
        // Local paths / remote helpers PKO cannot (or must not) clone.
        for url in ["/tmp/keel-gitops.git", "file:///tmp/r.git", "ext::sh -c id"] {
            let stub = DEFAULT_TENANT_STUB.replace(
                "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
                url,
            );
            let msg = refused("default", Some(&stub));
            assert!(
                msg.contains("not a network Git URL"),
                "local/helper URL refused as non-network"
            );
        }
        // SSH sources would need an `identity` key the platform-written Secret does not have.
        for url in [
            "ssh://git@forge.example:2222/keel/infra-gitops.git",
            "git@forge.example:keel/infra-gitops.git",
        ] {
            let stub = DEFAULT_TENANT_STUB.replace(
                "http://forgejo-http.keel-system.svc.cluster.local:3000/keel/infra-gitops.git",
                url,
            );
            let msg = refused("default", Some(&stub));
            assert!(
                msg.contains("SSH"),
                "SSH source refused until deploy keys exist"
            );
        }
        // Only the read-only `git-credentials` Secret may authenticate tenant Stacks.
        let msg = refused(
            "default",
            Some(&DEFAULT_TENANT_STUB.replace("name: git-credentials", "name: kubeconfig")),
        );
        assert!(
            msg.contains("`kubeconfig`"),
            "refusal names the disallowed Secret"
        );
    }

    fn git_secret(name: &str, kind: Option<&str>, data: &[(&str, &str)]) -> Secret {
        Secret {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("tenant-acme".to_string()),
                labels: kind
                    .map(|k| BTreeMap::from([("keel.io/secret-kind".to_string(), k.to_string())])),
                ..ObjectMeta::default()
            },
            data: Some(
                data.iter()
                    .map(|(k, v)| {
                        (
                            k.to_string(),
                            k8s_openapi::ByteString(v.as_bytes().to_vec()),
                        )
                    })
                    .collect(),
            ),
            ..Secret::default()
        }
    }

    fn acme_source(secret: Option<&str>) -> TenantGitSource {
        TenantGitSource {
            namespace: "tenant-acme".to_string(),
            url: "https://x/r.git".to_string(),
            secret_name: secret.map(str::to_string),
            branch: None,
        }
    }

    #[test]
    fn git_credentials_payload_requires_the_readonly_secret_and_filters_keys() {
        let source = acme_source(Some("git-credentials"));
        let good = git_secret(
            "git-credentials",
            Some(TENANT_GIT_CREDENTIALS_SECRET_KIND),
            &[
                ("username", "git"),
                ("password", "ro"),
                ("known_hosts", "kh"),
                ("kubeconfig", "leak"),
                ("age.agekey", "leak"),
            ],
        );
        let payload =
            tenant_git_credentials_payload("acme", &source, "git-credentials", Some(&good))
                .unwrap();
        assert_eq!(
            payload.keys().cloned().collect::<Vec<_>>(),
            ["known_hosts", "password", "username"]
        );

        // Tenant-chosen secretRef names (e.g. a kubeconfig or SOPS key) are never mirrored.
        let other = git_secret(
            "web-kubeconfig",
            Some(TENANT_GIT_CREDENTIALS_SECRET_KIND),
            &[],
        );
        let err = tenant_git_credentials_payload("acme", &source, "web-kubeconfig", Some(&other))
            .unwrap_err();
        assert!(
            matches!(&err, KubeError::PreviewSourceUnavailable(m) if m.contains("`web-kubeconfig`")),
            "{err:?}"
        );
        // Missing Secret / missing or wrong platform label fail closed too.
        let missing =
            tenant_git_credentials_payload("acme", &source, "git-credentials", None).unwrap_err();
        assert!(missing.to_string().contains("tenant-acme/git-credentials"));
        assert!(missing.to_string().contains("does not exist"));
        for kind in [None, Some("control-plane-git-credentials")] {
            let unlabelled = git_secret("git-credentials", kind, &[("password", "x")]);
            assert!(matches!(
                tenant_git_credentials_payload(
                    "acme",
                    &source,
                    "git-credentials",
                    Some(&unlabelled)
                ),
                Err(KubeError::PreviewSourceUnavailable(m)) if m.contains("not labelled")
            ));
        }
    }

    /// The Local Git provider resolver refuses per-tenant repository stubs
    /// (`keel-gitops-credentials`): those tenants resolve their source from their own repository.
    #[test]
    fn local_provider_source_refuses_tenant_repository_stubs() {
        use crate::tenant_repo::{
            TENANT_GITOPS_CREDENTIALS_SECRET_KIND, TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
        };
        for url in [
            "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-acme.git",
            "ssh://git@github.com/o/r.git",
        ] {
            let repo = crate::TenantRepoSource::new("acme", url).unwrap();
            let stub = crate::synthesize_tenant_repo_stub(
                &crate::TenantIsolationConfig::new("acme"),
                &repo,
            )
            .unwrap();
            let path = tenant_git_repository_manifest_path("acme").unwrap();
            let (_, yaml) = stub.files.iter().find(|(p, _)| p == &path).unwrap();
            assert!(
                matches!(
                    tenant_git_source_from_manifest("acme", Some(yaml)),
                    Err(KubeError::TenantSourceUnavailable(m)) if m.contains("Local Git provider")
                ),
                "{url}"
            );
        }
        // The preview mirror never copies it either, even correctly labelled.
        let source = acme_source(Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME));
        let secret = git_secret(
            TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
            Some(TENANT_GITOPS_CREDENTIALS_SECRET_KIND),
            &[("username", "bot"), ("password", "ro")],
        );
        assert!(matches!(
            tenant_git_credentials_payload(
                "acme",
                &source,
                TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
                Some(&secret)
            ),
            Err(KubeError::PreviewSourceUnavailable(_))
        ));
    }

    type Recorded = Arc<StdMutex<Vec<String>>>;
    type Route = (String, u16, Value);

    /// Mock API server: `routes` maps `"<METHOD> <path>"` to `(status, body)`. Unrouted GETs and
    /// DELETEs return 404; unrouted PATCH/POST echo a stub object. Every request is recorded as
    /// `"<METHOD> <path>"` so tests can assert exactly what was read and written.
    fn mock_client(routes: Vec<Route>, recorded: Recorded) -> Client {
        let routes: Arc<HashMap<String, (u16, Value)>> = Arc::new(
            routes
                .into_iter()
                .map(|(k, status, body)| (k, (status, body)))
                .collect(),
        );
        let service = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let recorded = recorded.clone();
            let routes = routes.clone();
            async move {
                let key = format!("{} {}", req.method(), req.uri().path());
                recorded.lock().unwrap().push(key.clone());
                let not_found = json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "NotFound", "code": 404, "message": "not found"
                });
                let (status, body) = match routes.get(&key) {
                    Some(hit) => hit.clone(),
                    None if req.method() == http::Method::PATCH
                        || req.method() == http::Method::POST =>
                    {
                        let kind = if key.contains("/serviceaccounts") {
                            "ServiceAccount"
                        } else {
                            "Secret"
                        };
                        (
                            200,
                            json!({ "apiVersion": "v1", "kind": kind, "metadata": { "name": "stub" } }),
                        )
                    }
                    None => (404, not_found),
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(body.to_string().into_bytes()))
                        .unwrap(),
                )
            }
        });
        Client::new(service, "default")
    }

    const REPO_URL: &str = "http://forgejo.keel-system.svc:3000/keel/infra-gitops.git";

    fn repo_route(tenant: &str, secret: Option<&str>) -> Route {
        let ns = format!("tenant-{tenant}");
        (
            format!(
                "GET /apis/source.toolkit.fluxcd.io/v1/namespaces/{ns}/gitrepositories/keel-gitops"
            ),
            200,
            tenant_repo(&ns, "keel-gitops", Some(REPO_URL), secret),
        )
    }

    fn secret_route(ns: &str, secret: &Secret) -> Route {
        (
            format!("GET /api/v1/namespaces/{ns}/secrets/{}", secret.name_any()),
            200,
            serde_json::to_value(secret).unwrap(),
        )
    }

    fn deleted_route(path: &str) -> Route {
        (
            format!("DELETE {path}"),
            200,
            json!({ "kind": "Status", "apiVersion": "v1", "status": "Success" }),
        )
    }

    fn assert_never_flux_system(seen: &[String]) {
        assert!(
            seen.iter().all(|p| !p.contains("flux-system")),
            "platform source was read: {seen:?}"
        );
    }

    #[tokio::test]
    async fn live_resolution_reads_only_the_tenant_source_and_never_flux_system() {
        let recorded = Recorded::default();
        let cluster = KubePreviewCluster::new(mock_client(
            vec![repo_route("default", None)],
            recorded.clone(),
        ));

        let source = cluster.resolve_git_source("default").await.unwrap();
        assert_eq!(source.namespace, "tenant-default");
        assert_eq!(source.url, REPO_URL);

        let err = cluster.resolve_git_source("acme").await.unwrap_err();
        assert!(
            matches!(&err, KubeError::PreviewSourceUnavailable(m) if m.contains("tenant-acme/keel-gitops")),
            "{err:?}"
        );

        let seen = recorded.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_never_flux_system(&seen);
    }

    #[tokio::test]
    async fn prerequisites_validate_but_never_mirror_the_tenant_credentials() {
        let creds = git_secret(
            "git-credentials",
            Some(TENANT_GIT_CREDENTIALS_SECRET_KIND),
            &[("username", "git"), ("password", "ro"), ("extra", "nope")],
        );
        let recorded = Recorded::default();
        let cluster = KubePreviewCluster::new(mock_client(
            vec![
                repo_route("acme", Some("git-credentials")),
                secret_route("tenant-acme", &creds),
            ],
            recorded.clone(),
        ));
        let out = cluster.ensure_preview_prerequisites("acme").await.unwrap();
        assert_eq!(out.git_url.as_deref(), Some(REPO_URL));
        // The validated tenant Secret each preview copies (per-preview, owned by its Stack).
        assert_eq!(out.git_secret.as_deref(), Some("git-credentials"));
        // Only allow-listed key names are reported (they select the PKO `gitAuth` mode and the
        // keys copied), never values.
        assert_eq!(
            out.git_secret_keys,
            BTreeSet::from(["password".to_string(), "username".to_string()])
        );
        assert_eq!(out.s3_secret, None);
        let seen = recorded.lock().unwrap().clone();
        // No long-lived per-tenant mirror: nothing is written.
        assert!(
            !seen
                .iter()
                .any(|r| r.starts_with("PATCH ") && r.contains("/secrets/")),
            "{seen:?}"
        );
        assert_never_flux_system(&seen);
    }

    #[tokio::test]
    async fn prerequisites_refusals_write_nothing() {
        let wrong_name = git_secret(
            "web-kubeconfig",
            Some(TENANT_GIT_CREDENTIALS_SECRET_KIND),
            &[("password", "x")],
        );
        let cases: Vec<(&str, Vec<Route>)> = vec![
            ("no source", vec![]),
            (
                "missing secret",
                vec![repo_route("acme", Some("git-credentials"))],
            ),
            (
                "tenant-chosen secretRef",
                vec![
                    repo_route("acme", Some("web-kubeconfig")),
                    secret_route("tenant-acme", &wrong_name),
                ],
            ),
        ];
        for (case, routes) in cases {
            let recorded = Recorded::default();
            let cluster = KubePreviewCluster::new(mock_client(routes, recorded.clone()));
            let err = cluster
                .ensure_preview_prerequisites("acme")
                .await
                .unwrap_err();
            assert!(
                matches!(err, KubeError::PreviewSourceUnavailable(_)),
                "{case}: {err:?}"
            );
            let seen = recorded.lock().unwrap().clone();
            let writes: Vec<_> = seen
                .iter()
                .filter(|r| {
                    (r.starts_with("PATCH ") || r.starts_with("POST "))
                        && r.contains("/namespaces/keel-previews/")
                })
                .collect();
            assert!(
                writes.is_empty(),
                "{case}: wrote to keel-previews {writes:?}"
            );
            assert!(
                !seen.iter().any(|r| r.contains("/secrets/web-kubeconfig")),
                "{case}: read a tenant-chosen Secret {seen:?}"
            );
            assert_never_flux_system(&seen);
        }
    }

    #[tokio::test]
    async fn public_tenant_source_has_no_git_credentials() {
        let recorded = Recorded::default();
        let cluster = KubePreviewCluster::new(mock_client(
            vec![repo_route("acme", None)],
            recorded.clone(),
        ));
        let out = cluster.ensure_preview_prerequisites("acme").await.unwrap();
        assert_eq!(out.git_url.as_deref(), Some(REPO_URL));
        assert_eq!(out.git_secret, None);
        assert!(out.git_secret_keys.is_empty());
        let seen = recorded.lock().unwrap().clone();
        assert!(
            !seen
                .iter()
                .any(|r| !r.starts_with("GET ") && r.contains("/namespaces/keel-previews/secrets")),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn in_memory_tenant_repo_prerequisites_require_the_platform_credential() {
        let source = crate::TenantRepoSource::new(
            "acme",
            "https://forgejo.keel.local:8888/keel-tenants/infra-tenant-acme.git",
        )
        .unwrap();
        let cluster = InMemoryPreviewCluster::new();
        let err = cluster
            .ensure_tenant_repo_preview_prerequisites(&source)
            .await
            .unwrap_err();
        assert!(
            matches!(err, KubeError::PreviewSourceUnavailable(_)),
            "{err}"
        );
        cluster.put_tenant_secret(
            "acme",
            TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
            &[("username", "reader"), ("password", "t")],
        );
        let prereqs = cluster
            .ensure_tenant_repo_preview_prerequisites(&source)
            .await
            .unwrap();
        assert_eq!(prereqs.git_url.as_deref(), Some(source.url()));
        assert_eq!(
            prereqs.git_secret.as_deref(),
            Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME)
        );
        assert_eq!(
            prereqs.git_secret_keys,
            BTreeSet::from(["password".to_string(), "username".to_string()])
        );
    }

    #[tokio::test]
    async fn in_memory_cluster_can_simulate_a_missing_tenant_source() {
        let cluster = InMemoryPreviewCluster::new().without_tenant_source();
        assert!(matches!(
            cluster.ensure_preview_prerequisites("acme").await,
            Err(KubeError::PreviewSourceUnavailable(_))
        ));
    }

    #[test]
    fn naming_helpers_are_tenant_scoped_and_validated() {
        assert_eq!(
            preview_s3_secret_name("acme").unwrap(),
            "keel-preview-s3-acme"
        );
        assert!(preview_s3_secret_name("Bad_Tenant").is_err());
        assert_eq!(
            preview_report_configmap_name("preview-stack-pr-7"),
            "preview-report-pr-7"
        );
        assert_eq!(
            workspace_pod_name("preview-stack-pr-7"),
            "preview-stack-pr-7-workspace-0"
        );
    }

    #[test]
    fn rejects_non_preview_or_out_of_namespace_documents() {
        assert!(validate_preview_doc(&stack_doc("s", true), "Stack").is_ok());
        assert!(validate_preview_doc(&stack_doc("s", false), "Stack").is_err());
        let mut wrong_ns = stack_doc("s", true);
        wrong_ns["metadata"]["namespace"] = json!("tenant-acme");
        assert!(validate_preview_doc(&wrong_ns, "Stack").is_err());
        let flux_source = json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": { "name": "r", "namespace": KEEL_PREVIEWS_NAMESPACE }
        });
        assert!(validate_preview_doc(&flux_source, "Stack").is_err());
    }

    #[tokio::test]
    async fn in_memory_cluster_tracks_preview_lifecycle() {
        let cluster = InMemoryPreviewCluster::new();
        cluster
            .apply_preview(&stack_doc("preview-stack-pr-1", true))
            .await
            .unwrap();
        cluster
            .apply_preview(&stack_doc("preview-stack-pr-1", true))
            .await
            .unwrap();
        let stack = cluster.stack("preview-stack-pr-1").unwrap();
        assert_eq!(stack["metadata"]["generation"], 2);

        cluster
            .annotate_preview_stack(
                "preview-stack-pr-1",
                &BTreeMap::from([("keel.io/x".to_string(), "y".to_string())]),
            )
            .await
            .unwrap();
        assert_eq!(
            cluster.stack("preview-stack-pr-1").unwrap()["metadata"]["annotations"]["keel.io/x"],
            "y"
        );
        cluster
            .store_report("preview-stack-pr-1", "{}")
            .await
            .unwrap();
        assert_eq!(
            cluster
                .load_report("preview-stack-pr-1")
                .await
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        assert!(cluster.store_report("missing", "{}").await.is_err());
        assert!(
            cluster
                .delete_preview_object("Stack", "preview-stack-pr-1")
                .await
                .unwrap()
        );
        assert!(
            cluster
                .load_report("preview-stack-pr-1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !cluster
                .delete_preview_object("Stack", "preview-stack-pr-1")
                .await
                .unwrap()
        );
        assert!(cluster.delete_preview_object("Pod", "x").await.is_err());

        cluster.fail_applies_with("denied by restrict-pulumi-providers");
        assert!(cluster.apply_preview(&stack_doc("s", true)).await.is_err());
    }

    fn copy(source: &str, target: &str, keys: &[&str]) -> PreviewSecretCopy {
        PreviewSecretCopy {
            source_name: source.to_string(),
            target_name: target.to_string(),
            keys: keys.iter().map(|k| k.to_string()).collect(),
            proposed_data: None,
        }
    }

    #[tokio::test]
    async fn in_memory_copies_only_referenced_keys_with_labels_and_owner_gc() {
        let cluster = InMemoryPreviewCluster::new();
        cluster.put_tenant_secret(
            "acme",
            "vm1-secrets",
            &[
                ("awsAccessKeyId", "AKIA"),
                ("awsSecretAccessKey", "s3cr3t"),
                ("unrelated", "x"),
            ],
        );
        cluster.put_tenant_secret("acme", "never-referenced", &[("k", "v")]);

        // Copies are created before the preview Stack exists (PKO resolves `gitAuth` at once).
        let created = cluster
            .copy_preview_secrets(
                "acme",
                "#5",
                &[copy(
                    "vm1-secrets",
                    "keel-preview-5-vm1-secrets",
                    &["awsAccessKeyId", "awsSecretAccessKey"],
                )],
            )
            .await
            .unwrap();
        assert_eq!(created, vec!["keel-preview-5-vm1-secrets".to_string()]);
        assert_eq!(cluster.preview_secret_names(), created);
        let copied = cluster
            .preview_secret("keel-preview-5-vm1-secrets")
            .unwrap();
        assert_eq!(copied.owner_stack, None);
        assert_eq!(
            copied.data.keys().cloned().collect::<Vec<_>>(),
            vec!["awsAccessKeyId", "awsSecretAccessKey"]
        );
        assert_eq!(copied.labels["keel.io/pr-id"], "5");
        assert_eq!(copied.labels["keel.io/tenant"], "acme");
        assert_eq!(
            copied.labels["keel.io/secret-kind"],
            PREVIEW_PROVIDER_SECRET_KIND
        );

        // Adoption needs the live Stack.
        assert!(
            cluster
                .adopt_preview_secrets("preview-stack-pr-5", &created)
                .await
                .is_err()
        );
        let mut stack = stack_doc("preview-stack-pr-5", true);
        stack["metadata"]["labels"] = json!({ "keel.io/pr-id": "5" });
        cluster.apply_preview(&stack).await.unwrap();
        cluster
            .adopt_preview_secrets("preview-stack-pr-5", &created)
            .await
            .unwrap();
        assert_eq!(
            cluster
                .preview_secret("keel-preview-5-vm1-secrets")
                .unwrap()
                .owner_stack
                .as_deref(),
            Some("preview-stack-pr-5")
        );

        // Proposed (PR) values win over the live tenant Secret; re-copying keeps the owner.
        let mut proposed = copy("new-secrets", "keel-preview-5-new-secrets", &["token"]);
        proposed.proposed_data = Some(BTreeMap::from([("token".to_string(), "t".to_string())]));
        cluster
            .copy_preview_secrets("acme", "5", &[proposed])
            .await
            .unwrap();
        cluster
            .copy_preview_secrets(
                "acme",
                "5",
                &[copy(
                    "vm1-secrets",
                    "keel-preview-5-vm1-secrets",
                    &["awsAccessKeyId"],
                )],
            )
            .await
            .unwrap();
        assert!(
            cluster
                .preview_secret("keel-preview-5-vm1-secrets")
                .unwrap()
                .owner_stack
                .is_some()
        );

        // Missing sources/keys, foreign names and label-unsafe PR ids fail closed.
        for bad in [
            copy("absent", "keel-preview-5-absent", &["k"]),
            copy("vm1-secrets", "keel-preview-5-vm1-secrets", &["missingKey"]),
            copy("vm1-secrets", "vm1-secrets", &["awsAccessKeyId"]),
        ] {
            assert!(
                cluster
                    .copy_preview_secrets("acme", "5", &[bad])
                    .await
                    .is_err()
            );
        }
        for bad_pr in ["", "5,keel.io/tenant!=x", "a b"] {
            assert!(
                cluster
                    .copy_preview_secrets(
                        "acme",
                        bad_pr,
                        &[copy("vm1-secrets", "keel-preview-5-x", &["awsAccessKeyId"])]
                    )
                    .await
                    .is_err()
            );
        }
        // Wrong tenant cannot read acme's Secret.
        assert!(
            cluster
                .copy_preview_secrets(
                    "other",
                    "5",
                    &[copy("vm1-secrets", "keel-preview-5-x", &["awsAccessKeyId"])]
                )
                .await
                .is_err()
        );
        assert_eq!(
            cluster.list_preview_secret_pr_ids().await.unwrap(),
            BTreeSet::from(["5".to_string()])
        );

        // Deleting the owner Stack garbage-collects every adopted copy.
        cluster
            .adopt_preview_secrets(
                "preview-stack-pr-5",
                &["keel-preview-5-new-secrets".to_string()],
            )
            .await
            .unwrap();
        cluster
            .delete_preview_object("Stack", "preview-stack-pr-5")
            .await
            .unwrap();
        assert!(cluster.preview_secret_names().is_empty());
    }

    #[tokio::test]
    async fn in_memory_orphan_copies_are_deleted_by_pr_label() {
        let cluster = InMemoryPreviewCluster::new();
        cluster.put_tenant_secret("acme", "vm1-secrets", &[("k", "v")]);
        for pr in ["6", "7"] {
            cluster
                .copy_preview_secrets(
                    "acme",
                    pr,
                    &[copy(
                        "vm1-secrets",
                        &format!("keel-preview-{pr}-vm1-secrets"),
                        &["k"],
                    )],
                )
                .await
                .unwrap();
        }
        // Never adopted (e.g. the Stack apply failed or keel-server stopped in between): Stack
        // deletion cannot GC them, the label-based sweep does.
        assert!(
            !cluster
                .delete_preview_object("Stack", "preview-stack-pr-6")
                .await
                .unwrap()
        );
        assert_eq!(cluster.preview_secret_names().len(), 2);
        assert_eq!(cluster.delete_preview_secrets("#6").await.unwrap(), 1);
        assert_eq!(
            cluster.preview_secret_names(),
            vec!["keel-preview-7-vm1-secrets".to_string()]
        );
        assert_eq!(cluster.delete_preview_secrets("6").await.unwrap(), 0);
        assert!(cluster.delete_preview_secrets("6,x").await.is_err());
    }

    #[test]
    fn preview_pr_labels_are_normalized_dns_labels() {
        assert_eq!(preview_pr_label(" #42 ").unwrap(), "42");
        assert_eq!(preview_pr_label("PR-7").unwrap(), "pr-7");
        for bad in ["", "#", "-1", "1-", "a,b", "a=b", "a b", &"9".repeat(64)] {
            assert!(preview_pr_label(bad).is_err(), "{bad}");
        }
    }

    const PREVIEW_SECRETS: &str = "/api/v1/namespaces/keel-previews/secrets";
    const PREVIEW_STACKS: &str = "/apis/pulumi.com/v1/namespaces/keel-previews/stacks";

    #[tokio::test]
    async fn live_copies_are_written_without_an_owner_and_adopted_after_the_stack_exists() {
        let source = git_secret(
            "vm1-secrets",
            None,
            &[("awsAccessKeyId", "AKIA"), ("unrelated", "x")],
        );
        let recorded = Recorded::default();
        let cluster = KubePreviewCluster::new(mock_client(
            vec![
                secret_route("tenant-acme", &source),
                (
                    format!("GET {PREVIEW_STACKS}/preview-stack-pr-5"),
                    200,
                    json!({
                        "apiVersion": "pulumi.com/v1",
                        "kind": "Stack",
                        "metadata": {
                            "name": "preview-stack-pr-5",
                            "namespace": KEEL_PREVIEWS_NAMESPACE,
                            "uid": "stack-uid-5"
                        }
                    }),
                ),
            ],
            recorded.clone(),
        ));
        let created = cluster
            .copy_preview_secrets(
                "acme",
                "5",
                &[copy(
                    "vm1-secrets",
                    "keel-preview-5-vm1-secrets",
                    &["awsAccessKeyId"],
                )],
            )
            .await
            .unwrap();
        // Copying never reads the (not yet applied) preview Stack.
        assert_eq!(
            recorded.lock().unwrap().clone(),
            vec![
                "GET /api/v1/namespaces/tenant-acme/secrets/vm1-secrets".to_string(),
                format!("PATCH {PREVIEW_SECRETS}/keel-preview-5-vm1-secrets"),
            ]
        );
        cluster
            .adopt_preview_secrets("preview-stack-pr-5", &created)
            .await
            .unwrap();
        let seen = recorded.lock().unwrap().clone();
        assert_eq!(
            seen[2..],
            [
                format!("GET {PREVIEW_STACKS}/preview-stack-pr-5"),
                format!("PATCH {PREVIEW_SECRETS}/keel-preview-5-vm1-secrets"),
            ]
        );

        // A Stack without a UID (or a missing Stack) is never used as an owner.
        let cluster = KubePreviewCluster::new(mock_client(vec![], Recorded::default()));
        assert!(
            cluster
                .adopt_preview_secrets("preview-stack-pr-5", &created)
                .await
                .is_err()
        );
        assert!(preview_stack_owner("s", None).is_err());
        assert!(preview_stack_owner("s", Some(String::new())).is_err());
        let owner = preview_stack_owner("s", Some("u".to_string())).unwrap();
        assert_eq!(
            (owner.api_version.as_str(), owner.kind.as_str()),
            ("pulumi.com/v1", "Stack")
        );
    }

    #[tokio::test]
    async fn live_orphan_copies_are_selected_by_kind_and_pr_labels() {
        let meta_list = json!({
            "apiVersion": "meta.k8s.io/v1",
            "kind": "PartialObjectMetadataList",
            "metadata": {},
            "items": [{
                "apiVersion": "meta.k8s.io/v1",
                "kind": "PartialObjectMetadata",
                "metadata": {
                    "name": "keel-preview-6-git-credentials",
                    "namespace": KEEL_PREVIEWS_NAMESPACE,
                    "labels": {
                        "keel.io/secret-kind": PREVIEW_PROVIDER_SECRET_KIND,
                        "keel.io/pr-id": "6"
                    }
                }
            }]
        });
        let recorded = Recorded::default();
        let cluster = KubePreviewCluster::new(mock_client(
            vec![
                (format!("GET {PREVIEW_SECRETS}"), 200, meta_list),
                deleted_route(&format!("{PREVIEW_SECRETS}/keel-preview-6-git-credentials")),
            ],
            recorded.clone(),
        ));
        assert_eq!(
            cluster.list_preview_secret_pr_ids().await.unwrap(),
            BTreeSet::from(["6".to_string()])
        );
        assert_eq!(cluster.delete_preview_secrets("6").await.unwrap(), 1);
        let seen = recorded.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                format!("GET {PREVIEW_SECRETS}"),
                format!("GET {PREVIEW_SECRETS}"),
                format!("DELETE {PREVIEW_SECRETS}/keel-preview-6-git-credentials"),
            ]
        );
        assert_eq!(
            preview_secret_selector(Some("6")),
            format!("keel.io/secret-kind={PREVIEW_PROVIDER_SECRET_KIND},keel.io/pr-id=6")
        );
        // Selector injection is impossible: the PR id must be a DNS label.
        assert!(cluster.delete_preview_secrets("6,keel.io/x").await.is_err());
    }
}
