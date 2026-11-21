//! Ephemeral PKO v2 preview runner (`SDD.md` §4.2, `NFR-REL-02`).
//!
//! Applies preview `GitRepository` + `Stack` (`spec.preview: true`) CRs into `keel-previews` with
//! server-side apply, reads PKO `Stack`/`Update` status and workspace pod logs, persists the
//! rendered report in a `ConfigMap` owned by the preview `Stack`, and deletes previews.
//!
//! All preview state lives in the cluster (labels/annotations on the `Stack` + report
//! `ConfigMap`), so it survives `keel-server` restarts and is shared across replicas (OPEN-10).
//! The trait keeps `keel-server` testable via [`InMemoryPreviewCluster`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use keel_core::{FLUX_SYSTEM_NAMESPACE, KEEL_PREVIEWS_NAMESPACE};
use kube::api::{
    Api, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams, PostParams,
};
use kube::discovery::ApiResource;
use kube::{Client, ResourceExt};
use serde_json::{Value, json};

use keel_pko::PreviewSecretCopy;

use crate::KubeError;
use crate::secret_store::TENANT_S3_IDENTITY_PENDING_ANNOTATION;
use crate::tenant::{TENANT_S3_CREDENTIALS_SECRET_NAME, tenant_namespace};

/// Server-side apply field manager for preview CRs.
pub const PREVIEW_FIELD_MANAGER: &str = "keel-server-preview";
/// Read-only ServiceAccount the preview workspace pods run as (must exist in `keel-previews`).
pub const PREVIEW_SERVICE_ACCOUNT: &str = "keel-preview-readonly";
/// `ConfigMap` key holding the report JSON (mirrors `keel_pko::PREVIEW_REPORT_CONFIGMAP_KEY`).
pub const PREVIEW_REPORT_KEY: &str = "report.json";
/// Label selecting Keel ephemeral preview objects.
pub const PREVIEW_LABEL_SELECTOR: &str = "keel.io/ephemeral-preview=true";
/// Tenant Flux `GitRepository` name (`tenant-<id>/keel-gitops`).
pub const TENANT_GIT_REPOSITORY_NAME: &str = "keel-gitops";
/// `keel.io/secret-kind` for tenant-scoped S3 state credentials mirrored into `keel-previews`.
pub const PREVIEW_S3_SECRET_KIND: &str = "preview-tenant-s3-credentials";
/// `keel.io/secret-kind` for tenant read-only Git credentials mirrored into `keel-previews`.
pub const PREVIEW_GIT_SECRET_KIND: &str = "preview-tenant-git-credentials";
/// `keel.io/secret-kind` for per-preview copies of Stack-referenced provider credentials.
pub const PREVIEW_PROVIDER_SECRET_KIND: &str = "preview-provider-credentials";

/// Name of the `keel-previews` mirror of a tenant's **scoped** S3 identity (never admin keys).
pub fn preview_s3_secret_name(tenant: &str) -> Result<String, KubeError> {
    tenant_namespace(tenant)?;
    Ok(format!("keel-preview-s3-{tenant}"))
}

/// Name of the `keel-previews` mirror of a tenant's read-only Git credentials.
pub fn preview_git_secret_name(tenant: &str) -> Result<String, KubeError> {
    tenant_namespace(tenant)?;
    Ok(format!("keel-preview-git-{tenant}"))
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
    /// Cluster-reachable Git URL of the GitOps repository (from the tenant's Flux source).
    pub git_url: Option<String>,
    /// `keel-previews` Secret mirroring the tenant's read-only Git credentials.
    pub git_secret: Option<String>,
}

/// Cluster operations required by the ephemeral preview lifecycle.
#[async_trait]
pub trait PreviewCluster: Send + Sync {
    /// Resolves (and mirrors into `keel-previews`) the tenant-scoped state/Git credentials.
    async fn ensure_preview_prerequisites(
        &self,
        tenant: &str,
    ) -> Result<PreviewPrerequisites, KubeError>;

    /// Server-side applies the preview `GitRepository` and `Stack` documents.
    async fn apply_preview(&self, git_repository: &Value, stack: &Value) -> Result<(), KubeError>;

    /// Lists preview `Stack` objects in `keel-previews`.
    async fn list_preview_stacks(&self) -> Result<Vec<Value>, KubeError>;

    /// Lists preview `GitRepository` objects in `keel-previews`.
    async fn list_preview_git_repositories(&self) -> Result<Vec<Value>, KubeError>;

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

    /// Deletes a preview object (`Stack`, `GitRepository`, `ConfigMap`). `Ok(false)` if absent.
    async fn delete_preview_object(&self, kind: &str, name: &str) -> Result<bool, KubeError>;

    /// Copies exactly the planned provider-credential Secrets (only the referenced keys) from
    /// `tenant-<id>` into `keel-previews` under their per-preview names, labelled
    /// `keel.io/pr-id` / `keel.io/tenant` and owned by the already-applied preview `stack_name`
    /// (garbage-collected with it). Copies carrying `proposed_data` use those PR values instead of
    /// the live tenant Secret. Fails if a source Secret or referenced key is missing.
    async fn copy_preview_secrets(
        &self,
        tenant: &str,
        stack_name: &str,
        copies: &[PreviewSecretCopy],
    ) -> Result<Vec<String>, KubeError>;
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

fn preview_copy_labels(tenant: &str, pr_id: Option<&str>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "keel".to_string(),
        ),
        (
            "keel.io/secret-kind".to_string(),
            PREVIEW_PROVIDER_SECRET_KIND.to_string(),
        ),
        ("keel.io/tenant".to_string(), tenant.to_string()),
    ]);
    if let Some(pr) = pr_id {
        labels.insert("keel.io/pr-id".to_string(), pr.to_string());
    }
    labels
}

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

    async fn ensure_service_account(&self) {
        let api: Api<ServiceAccount> =
            Api::namespaced(self.client.clone(), KEEL_PREVIEWS_NAMESPACE);
        if matches!(api.get_opt(PREVIEW_SERVICE_ACCOUNT).await, Ok(Some(_))) {
            return;
        }
        let sa = ServiceAccount {
            metadata: ObjectMeta {
                name: Some(PREVIEW_SERVICE_ACCOUNT.to_string()),
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
            tracing::warn!(error = %e, "could not create keel-previews/{PREVIEW_SERVICE_ACCOUNT}");
        }
    }

    async fn resolve_git_source(&self, tenant: &str) -> Option<(String, String, Option<String>)> {
        let ar = git_repo_ar();
        let tenant_ns = tenant_namespace(tenant).ok()?;
        for (ns, name) in [
            (tenant_ns.as_str(), TENANT_GIT_REPOSITORY_NAME),
            (FLUX_SYSTEM_NAMESPACE, "flux-system"),
        ] {
            let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), ns, &ar);
            if let Ok(Some(obj)) = api.get_opt(name).await {
                let url = obj.data.pointer("/spec/url").and_then(Value::as_str)?;
                let secret = obj
                    .data
                    .pointer("/spec/secretRef/name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                return Some((ns.to_string(), url.to_string(), secret));
            }
        }
        None
    }
}

#[async_trait]
impl PreviewCluster for KubePreviewCluster {
    async fn ensure_preview_prerequisites(
        &self,
        tenant: &str,
    ) -> Result<PreviewPrerequisites, KubeError> {
        let tenant_ns = tenant_namespace(tenant)?;
        self.ensure_service_account().await;
        let mut out = PreviewPrerequisites::default();

        // Tenant-scoped S3 identity only (fail closed: never fall back to admin keys).
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &tenant_ns);
        if let Some(scoped) = secrets.get_opt(TENANT_S3_CREDENTIALS_SECRET_NAME).await?
            && !scoped
                .annotations()
                .contains_key(TENANT_S3_IDENTITY_PENDING_ANNOTATION)
        {
            let name = preview_s3_secret_name(tenant)?;
            self.mirror_secret(
                &scoped,
                &name,
                tenant,
                PREVIEW_S3_SECRET_KIND,
                Some(&["accessKey", "secretKey"]),
            )
            .await?;
            out.s3_secret = Some(name);
        }

        if let Some((src_ns, url, secret_name)) = self.resolve_git_source(tenant).await {
            out.git_url = Some(url);
            if let Some(secret_name) = secret_name {
                let src: Api<Secret> = Api::namespaced(self.client.clone(), &src_ns);
                if let Some(secret) = src.get_opt(&secret_name).await? {
                    let name = preview_git_secret_name(tenant)?;
                    self.mirror_secret(&secret, &name, tenant, PREVIEW_GIT_SECRET_KIND, None)
                        .await?;
                    out.git_secret = Some(name);
                }
            }
        }
        Ok(out)
    }

    async fn apply_preview(&self, git_repository: &Value, stack: &Value) -> Result<(), KubeError> {
        let repo_name = validate_preview_doc(git_repository, "GitRepository")?;
        let stack_name = validate_preview_doc(stack, "Stack")?;
        let pp = PatchParams::apply(PREVIEW_FIELD_MANAGER).force();
        self.dyn_api(&git_repo_ar())
            .patch(&repo_name, &pp, &Patch::Apply(git_repository))
            .await?;
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

    async fn list_preview_git_repositories(&self) -> Result<Vec<Value>, KubeError> {
        let list = self
            .dyn_api(&git_repo_ar())
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
                PREVIEW_REPORT_KEY.to_string(),
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
            .and_then(|d| d.get(PREVIEW_REPORT_KEY).cloned()))
    }

    async fn delete_preview_object(&self, kind: &str, name: &str) -> Result<bool, KubeError> {
        let dp = DeleteParams::default();
        let res = match kind {
            "Stack" => self
                .dyn_api(&stack_ar())
                .delete(name, &dp)
                .await
                .map(|_| ()),
            "GitRepository" => self
                .dyn_api(&git_repo_ar())
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
        stack_name: &str,
        copies: &[PreviewSecretCopy],
    ) -> Result<Vec<String>, KubeError> {
        if copies.is_empty() {
            return Ok(Vec::new());
        }
        let tenant_ns = tenant_namespace(tenant)?;
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
        let pr_id = stack.labels().get("keel.io/pr-id").cloned();
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
            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(copy.target_name.clone()),
                    namespace: Some(KEEL_PREVIEWS_NAMESPACE.to_string()),
                    labels: Some(preview_copy_labels(tenant, pr_id.as_deref())),
                    owner_references: Some(vec![owner.clone()]),
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
}

#[derive(Default)]
struct InMemoryState {
    stacks: BTreeMap<String, Value>,
    repos: BTreeMap<String, Value>,
    updates: HashMap<String, Value>,
    logs: HashMap<String, String>,
    reports: HashMap<String, String>,
    apply_error: Option<String>,
    /// `(tenant, name)` → data of Secrets living in tenant namespaces.
    tenant_secrets: BTreeMap<(String, String), BTreeMap<String, Vec<u8>>>,
    /// `keel-previews` Secret copies: name → (owner Stack, labels, data).
    preview_secrets: BTreeMap<String, InMemoryPreviewSecret>,
}

/// A Secret copy stored by [`InMemoryPreviewCluster`].
#[derive(Clone, PartialEq, Eq)]
pub struct InMemoryPreviewSecret {
    pub owner_stack: String,
    pub labels: BTreeMap<String, String>,
    pub data: BTreeMap<String, Vec<u8>>,
}

/// In-memory [`PreviewCluster`] for tests and offline development.
#[derive(Clone, Default)]
pub struct InMemoryPreviewCluster {
    inner: Arc<StdMutex<InMemoryState>>,
    prerequisites: PreviewPrerequisites,
}

impl InMemoryPreviewCluster {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the prerequisites returned for every tenant.
    pub fn with_prerequisites(mut self, prereqs: PreviewPrerequisites) -> Self {
        self.prerequisites = prereqs;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InMemoryState> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes subsequent `apply_preview` calls fail (e.g. simulated admission denial).
    pub fn fail_applies_with(&self, message: impl Into<String>) {
        self.lock().apply_error = Some(message.into());
    }

    /// Replaces `status` on a stored preview `Stack` (simulates PKO progress).
    pub fn set_stack_status(&self, name: &str, status: Value) {
        if let Some(stack) = self.lock().stacks.get_mut(name) {
            stack["status"] = status;
        }
    }

    /// Stores a PKO `Update` object.
    pub fn put_update(&self, name: &str, update: Value) {
        self.lock().updates.insert(name.to_string(), update);
    }

    /// Stores workspace logs for `stack_name`.
    pub fn put_workspace_log(&self, stack_name: &str, log: impl Into<String>) {
        self.lock().logs.insert(stack_name.to_string(), log.into());
    }

    /// Returns stored preview `Stack` names.
    pub fn stack_names(&self) -> Vec<String> {
        self.lock().stacks.keys().cloned().collect()
    }

    /// Returns a stored preview `Stack`.
    pub fn stack(&self, name: &str) -> Option<Value> {
        self.lock().stacks.get(name).cloned()
    }

    /// Returns stored preview `GitRepository` names.
    pub fn git_repository_names(&self) -> Vec<String> {
        self.lock().repos.keys().cloned().collect()
    }

    /// Seeds a Secret in `tenant-<tenant>` (source for preview credential copies).
    pub fn put_tenant_secret(&self, tenant: &str, name: &str, data: &[(&str, &str)]) {
        self.lock().tenant_secrets.insert(
            (tenant.to_string(), name.to_string()),
            data.iter()
                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                .collect(),
        );
    }

    /// Returns a `keel-previews` Secret copy.
    pub fn preview_secret(&self, name: &str) -> Option<InMemoryPreviewSecret> {
        self.lock().preview_secrets.get(name).cloned()
    }

    /// Returns all `keel-previews` Secret copy names.
    pub fn preview_secret_names(&self) -> Vec<String> {
        self.lock().preview_secrets.keys().cloned().collect()
    }
}

#[async_trait]
impl PreviewCluster for InMemoryPreviewCluster {
    async fn ensure_preview_prerequisites(
        &self,
        tenant: &str,
    ) -> Result<PreviewPrerequisites, KubeError> {
        tenant_namespace(tenant)?;
        Ok(self.prerequisites.clone())
    }

    async fn apply_preview(&self, git_repository: &Value, stack: &Value) -> Result<(), KubeError> {
        let repo_name = validate_preview_doc(git_repository, "GitRepository")?;
        let stack_name = validate_preview_doc(stack, "Stack")?;
        let mut st = self.lock();
        if let Some(err) = st.apply_error.clone() {
            return Err(KubeError::InvalidInput(err));
        }
        st.repos.insert(repo_name, git_repository.clone());
        let mut doc = stack.clone();
        let generation = st
            .stacks
            .get(&stack_name)
            .and_then(|s| s.pointer("/metadata/generation"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        doc["metadata"]["generation"] = json!(generation);
        st.stacks.insert(stack_name, doc);
        Ok(())
    }

    async fn list_preview_stacks(&self) -> Result<Vec<Value>, KubeError> {
        Ok(self.lock().stacks.values().cloned().collect())
    }

    async fn list_preview_git_repositories(&self) -> Result<Vec<Value>, KubeError> {
        Ok(self.lock().repos.values().cloned().collect())
    }

    async fn get_preview_stack(&self, name: &str) -> Result<Option<Value>, KubeError> {
        Ok(self.lock().stacks.get(name).cloned())
    }

    async fn get_update(&self, name: &str) -> Result<Option<Value>, KubeError> {
        Ok(self.lock().updates.get(name).cloned())
    }

    async fn workspace_log(
        &self,
        stack_name: &str,
        _tail_lines: i64,
    ) -> Result<Option<String>, KubeError> {
        Ok(self.lock().logs.get(stack_name).cloned())
    }

    async fn annotate_preview_stack(
        &self,
        name: &str,
        annotations: &BTreeMap<String, String>,
    ) -> Result<(), KubeError> {
        let mut st = self.lock();
        let Some(stack) = st.stacks.get_mut(name) else {
            return Err(KubeError::InvalidInput(format!("Stack `{name}` not found")));
        };
        if !stack["metadata"]["annotations"].is_object() {
            stack["metadata"]["annotations"] = json!({});
        }
        for (k, v) in annotations {
            stack["metadata"]["annotations"][k] = json!(v);
        }
        Ok(())
    }

    async fn store_report(&self, stack_name: &str, report_json: &str) -> Result<(), KubeError> {
        let mut st = self.lock();
        if !st.stacks.contains_key(stack_name) {
            return Err(KubeError::InvalidInput(format!(
                "preview Stack `{stack_name}` not found"
            )));
        }
        st.reports
            .insert(stack_name.to_string(), report_json.to_string());
        Ok(())
    }

    async fn load_report(&self, stack_name: &str) -> Result<Option<String>, KubeError> {
        Ok(self.lock().reports.get(stack_name).cloned())
    }

    async fn delete_preview_object(&self, kind: &str, name: &str) -> Result<bool, KubeError> {
        let mut st = self.lock();
        Ok(match kind {
            "Stack" => {
                st.reports.remove(name);
                // Simulates ownerReference garbage collection of the Secret copies.
                st.preview_secrets.retain(|_, s| s.owner_stack != name);
                st.stacks.remove(name).is_some()
            }
            "GitRepository" => st.repos.remove(name).is_some(),
            "ConfigMap" => false,
            other => {
                return Err(KubeError::InvalidInput(format!(
                    "unsupported preview object kind `{other}`"
                )));
            }
        })
    }

    async fn copy_preview_secrets(
        &self,
        tenant: &str,
        stack_name: &str,
        copies: &[PreviewSecretCopy],
    ) -> Result<Vec<String>, KubeError> {
        tenant_namespace(tenant)?;
        if copies.is_empty() {
            return Ok(Vec::new());
        }
        let mut st = self.lock();
        let Some(stack) = st.stacks.get(stack_name) else {
            return Err(KubeError::InvalidInput(format!(
                "preview Stack `{stack_name}` not found"
            )));
        };
        let pr_id = stack
            .pointer("/metadata/labels/keel.io~1pr-id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut staged = Vec::new();
        for copy in copies {
            let live = st
                .tenant_secrets
                .get(&(tenant.to_string(), copy.source_name.clone()));
            let data = preview_copy_payload(copy, live)?;
            staged.push((copy.target_name.clone(), data));
        }
        let mut created = Vec::new();
        for (name, data) in staged {
            st.preview_secrets.insert(
                name.clone(),
                InMemoryPreviewSecret {
                    owner_stack: stack_name.to_string(),
                    labels: preview_copy_labels(tenant, pr_id.as_deref()),
                    data,
                },
            );
            created.push(name);
        }
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack_doc(name: &str, preview: bool) -> Value {
        json!({
            "apiVersion": "pulumi.com/v1",
            "kind": "Stack",
            "metadata": { "name": name, "namespace": KEEL_PREVIEWS_NAMESPACE },
            "spec": { "preview": preview }
        })
    }

    fn repo_doc(name: &str) -> Value {
        json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": { "name": name, "namespace": KEEL_PREVIEWS_NAMESPACE }
        })
    }

    #[test]
    fn naming_helpers_are_tenant_scoped_and_validated() {
        assert_eq!(
            preview_s3_secret_name("acme").unwrap(),
            "keel-preview-s3-acme"
        );
        assert_eq!(
            preview_git_secret_name("acme").unwrap(),
            "keel-preview-git-acme"
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
        assert!(validate_preview_doc(&repo_doc("r"), "Stack").is_err());
    }

    #[tokio::test]
    async fn in_memory_cluster_tracks_preview_lifecycle() {
        let cluster = InMemoryPreviewCluster::new();
        cluster
            .apply_preview(
                &repo_doc("preview-source-pr-1"),
                &stack_doc("preview-stack-pr-1", true),
            )
            .await
            .unwrap();
        cluster
            .apply_preview(
                &repo_doc("preview-source-pr-1"),
                &stack_doc("preview-stack-pr-1", true),
            )
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
        assert!(
            cluster
                .apply_preview(&repo_doc("r"), &stack_doc("s", true))
                .await
                .is_err()
        );
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
        let mut stack = stack_doc("preview-stack-pr-5", true);
        stack["metadata"]["labels"] = json!({ "keel.io/pr-id": "5" });
        cluster
            .apply_preview(&repo_doc("preview-source-pr-5"), &stack)
            .await
            .unwrap();

        let created = cluster
            .copy_preview_secrets(
                "acme",
                "preview-stack-pr-5",
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
        assert_eq!(copied.owner_stack, "preview-stack-pr-5");
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

        // Proposed (PR) values win over the live tenant Secret.
        let mut proposed = copy("new-secrets", "keel-preview-5-new-secrets", &["token"]);
        proposed.proposed_data = Some(BTreeMap::from([("token".to_string(), "t".to_string())]));
        cluster
            .copy_preview_secrets("acme", "preview-stack-pr-5", &[proposed])
            .await
            .unwrap();

        // Missing sources/keys and foreign names fail closed.
        for bad in [
            copy("absent", "keel-preview-5-absent", &["k"]),
            copy("vm1-secrets", "keel-preview-5-vm1-secrets", &["missingKey"]),
            copy("vm1-secrets", "vm1-secrets", &["awsAccessKeyId"]),
        ] {
            assert!(
                cluster
                    .copy_preview_secrets("acme", "preview-stack-pr-5", &[bad])
                    .await
                    .is_err()
            );
        }
        // Wrong tenant cannot read acme's Secret.
        assert!(
            cluster
                .copy_preview_secrets(
                    "other",
                    "preview-stack-pr-5",
                    &[copy("vm1-secrets", "keel-preview-5-x", &["awsAccessKeyId"])]
                )
                .await
                .is_err()
        );

        // Deleting the owner Stack garbage-collects every copy.
        cluster
            .delete_preview_object("Stack", "preview-stack-pr-5")
            .await
            .unwrap();
        assert!(cluster.preview_secret_names().is_empty());
    }
}
