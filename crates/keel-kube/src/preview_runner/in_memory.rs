//! In-memory [`PreviewCluster`] test double (`test-support` feature; never in release builds).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use super::*;

#[derive(Default)]
struct InMemoryState {
    stacks: BTreeMap<String, Value>,
    updates: HashMap<String, Value>,
    logs: HashMap<String, String>,
    reports: HashMap<String, String>,
    apply_error: Option<String>,
    adopt_error: Option<String>,
    /// Mutating calls in order (`copy:<name>`, `apply:<stack>`, `adopt:<name>`,
    /// `delete:<kind>/<name>`, `delete-secrets:<pr>`), for ordering assertions.
    operations: Vec<String>,
    /// `(tenant, name)` → data of Secrets living in tenant namespaces.
    tenant_secrets: BTreeMap<(String, String), BTreeMap<String, Vec<u8>>>,
    /// `keel-previews` Secret copies: name → (owner Stack, labels, data).
    preview_secrets: BTreeMap<String, InMemoryPreviewSecret>,
}

/// A Secret copy stored by [`InMemoryPreviewCluster`].
#[derive(Clone, PartialEq, Eq)]
pub struct InMemoryPreviewSecret {
    /// Owner preview `Stack` (`None` until [`PreviewCluster::adopt_preview_secrets`]).
    pub owner_stack: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub data: BTreeMap<String, Vec<u8>>,
}

/// In-memory [`PreviewCluster`] for tests.
#[derive(Clone, Default)]
pub struct InMemoryPreviewCluster {
    inner: Arc<StdMutex<InMemoryState>>,
    prerequisites: PreviewPrerequisites,
    missing_tenant_source: bool,
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

    /// Simulates a tenant without its own `keel-gitops` source: prerequisites fail closed with
    /// [`KubeError::PreviewSourceUnavailable`] exactly like [`KubePreviewCluster`].
    pub fn without_tenant_source(mut self) -> Self {
        self.missing_tenant_source = true;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InMemoryState> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes subsequent `apply_preview` calls fail (e.g. simulated admission denial).
    pub fn fail_applies_with(&self, message: impl Into<String>) {
        self.lock().apply_error = Some(message.into());
    }

    /// Makes subsequent `adopt_preview_secrets` calls fail.
    pub fn fail_adoption_with(&self, message: impl Into<String>) {
        self.lock().adopt_error = Some(message.into());
    }

    /// Mutating calls recorded so far, in order (see `InMemoryState`).
    pub fn operations(&self) -> Vec<String> {
        self.lock().operations.clone()
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
        if self.missing_tenant_source {
            tenant_git_source_from_repository(tenant, None)?;
        }
        Ok(self.prerequisites.clone())
    }

    async fn ensure_tenant_repo_preview_prerequisites(
        &self,
        source: &crate::TenantRepoSource,
    ) -> Result<PreviewPrerequisites, KubeError> {
        let tenant = source.tenant_id();
        let keys = crate::tenant_repo_preview_secret_keys(source.transport());
        let present = self
            .lock()
            .tenant_secrets
            .get(&(
                tenant.to_string(),
                TENANT_GITOPS_CREDENTIALS_SECRET_NAME.to_string(),
            ))
            .is_some_and(|data| keys.iter().all(|k| data.contains_key(k)));
        if self.missing_tenant_source || !present {
            return Err(KubeError::PreviewSourceUnavailable(format!(
                "tenant `{tenant}` has no usable repository read credential `tenant-{tenant}/{TENANT_GITOPS_CREDENTIALS_SECRET_NAME}`"
            )));
        }
        Ok(PreviewPrerequisites {
            s3_secret: self.prerequisites.s3_secret.clone(),
            git_url: Some(source.url().to_string()),
            git_secret: Some(TENANT_GITOPS_CREDENTIALS_SECRET_NAME.to_string()),
            git_secret_keys: keys,
        })
    }

    async fn apply_preview(&self, stack: &Value) -> Result<(), KubeError> {
        let stack_name = validate_preview_doc(stack, "Stack")?;
        let mut st = self.lock();
        st.operations.push(format!("apply:{stack_name}"));
        if let Some(err) = st.apply_error.clone() {
            return Err(KubeError::InvalidInput(err));
        }
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
        st.operations.push(format!("delete:{kind}/{name}"));
        Ok(match kind {
            "Stack" => {
                st.reports.remove(name);
                // Simulates ownerReference garbage collection of the adopted Secret copies
                // (copies never adopted are left behind, like on a real cluster).
                st.preview_secrets
                    .retain(|_, s| s.owner_stack.as_deref() != Some(name));
                st.stacks.remove(name).is_some()
            }
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
        pr_id: &str,
        copies: &[PreviewSecretCopy],
    ) -> Result<Vec<String>, KubeError> {
        tenant_namespace(tenant)?;
        let pr_label = preview_pr_label(pr_id)?;
        if copies.is_empty() {
            return Ok(Vec::new());
        }
        let mut st = self.lock();
        // Written one by one like the live cluster, so a failure leaves a partial prefix.
        let mut created = Vec::new();
        for copy in copies {
            let live = st
                .tenant_secrets
                .get(&(tenant.to_string(), copy.source_name.clone()));
            let data = preview_copy_payload(copy, live)?;
            st.operations.push(format!("copy:{}", copy.target_name));
            let owner_stack = st
                .preview_secrets
                .get(&copy.target_name)
                .and_then(|s| s.owner_stack.clone());
            st.preview_secrets.insert(
                copy.target_name.clone(),
                InMemoryPreviewSecret {
                    owner_stack,
                    labels: preview_copy_labels(tenant, &pr_label),
                    data,
                },
            );
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
        let mut st = self.lock();
        if let Some(err) = st.adopt_error.clone() {
            return Err(KubeError::InvalidInput(err));
        }
        if !st.stacks.contains_key(stack_name) {
            return Err(KubeError::InvalidInput(format!(
                "preview Stack `{stack_name}` not found"
            )));
        }
        for name in secret_names {
            st.operations.push(format!("adopt:{name}"));
            let Some(secret) = st.preview_secrets.get_mut(name) else {
                return Err(KubeError::InvalidInput(format!(
                    "preview Secret `{name}` not found"
                )));
            };
            secret.owner_stack = Some(stack_name.to_string());
        }
        Ok(())
    }

    async fn delete_preview_secrets(&self, pr_id: &str) -> Result<usize, KubeError> {
        let pr_label = preview_pr_label(pr_id)?;
        let mut st = self.lock();
        st.operations.push(format!("delete-secrets:{pr_label}"));
        let before = st.preview_secrets.len();
        st.preview_secrets.retain(|_, s| {
            s.labels.get("keel.io/secret-kind").map(String::as_str)
                != Some(PREVIEW_PROVIDER_SECRET_KIND)
                || s.labels.get("keel.io/pr-id") != Some(&pr_label)
        });
        Ok(before - st.preview_secrets.len())
    }

    async fn list_preview_secret_pr_ids(&self) -> Result<BTreeSet<String>, KubeError> {
        Ok(self
            .lock()
            .preview_secrets
            .values()
            .filter(|s| {
                s.labels.get("keel.io/secret-kind").map(String::as_str)
                    == Some(PREVIEW_PROVIDER_SECRET_KIND)
            })
            .filter_map(|s| s.labels.get("keel.io/pr-id").cloned())
            .collect())
    }
}
