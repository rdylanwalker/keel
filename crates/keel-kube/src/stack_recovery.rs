//! Recovery of tenant PKO `Stack`s stalled on a missing Secret (`SDD.md` §6.2, ADDON-15).
//!
//! PKO v2 reads a Stack's `gitAuth` Secrets (and the legacy `gitAuthSecret`) in the controller at
//! the start of every reconcile. When one is missing (for example
//! `tenant-<id>/keel-gitops-credentials` was deleted or not written yet) it marks the Stack
//! `Stalled` with reason `SpecInvalid` and, by design, never retries until the spec or the
//! reconcile-request annotation changes. Restoring the Secret alone therefore leaves the Stack
//! stuck. (`envRefs` and Secret-backed `config` refs are not read by the controller: PKO turns them
//! into `secretKeyRef`s/volumes of the workspace pod, so a missing one fails the pod instead.)
//!
//! Keel does not run a custom controller for this. keel-server requests a reconcile of such a
//! Stack through PKO's own reconcile-request annotation
//! ([`keel_pko::ANNOTATION_RECONCILE_REQUEST`]) once the Secret named in the stall message
//! exists again:
//!
//! * right after it (re)writes a tenant's credentials (`POST /api/v1/tenants`,
//!   `keel tenant repair-credentials`), scoped to that tenant's namespace;
//! * from a periodic sweep over every `tenant-<id>` namespace, which also covers Secrets restored
//!   by `keel bootstrap init` or by hand.
//!
//! The request value is derived from the Secret's `resourceVersion`
//! ([`reconcile_request_value`]), so every replica and every pass computes the same value. PKO
//! reads Secrets through its informer cache, which can lag the write that restored the Secret: the
//! reconcile triggered by the first request may still see the Secret missing and stall again. So
//! when PKO has observed Keel's request (`status.observedReconcileRequest`) and the Stack is still
//! stalled on the same Secret revision, the request is repeated with an attempt suffix
//! (`...@<rv>#2`), at most [`MAX_RECONCILE_ATTEMPTS`] times per Secret revision: a Secret that is
//! still unusable never causes a reconcile loop. Only `metadata.annotations` changes, so neither
//! Flux (server-side apply of the Stack from Git) nor Kyverno (`restrict-tenant-pulumi-objects`)
//! is affected.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::{Api, DynamicObject, ListParams, Patch, PatchParams};
use kube::discovery::ApiResource;
use serde_json::{Value, json};

use crate::KubeError;

/// Namespace prefix of tenant namespaces, the only ones the recovery sweep touches.
const TENANT_NAMESPACE_PREFIX: &str = "tenant-";

/// A reconcile request Keel made (or plans to make) for a stalled Stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledStackNudge {
    pub namespace: String,
    pub stack: String,
    /// The Secret the Stack stalled on (now present).
    pub secret: String,
    /// Value written to [`keel_pko::ANNOTATION_RECONCILE_REQUEST`].
    pub request: String,
}

/// Cluster access needed by [`recover_stalled_stacks`] (mocked in tests).
#[async_trait]
pub trait StackRecoveryCluster: Send + Sync {
    /// PKO `Stack`s as JSON, in `namespace` or (when `None`) in every namespace.
    async fn list_stacks(&self, namespace: Option<&str>) -> Result<Vec<Value>, KubeError>;
    /// `metadata.resourceVersion` of Secret `namespace/name`, `None` when it does not exist.
    /// Never reads or returns Secret data.
    async fn secret_revision(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<String>, KubeError>;
    /// Sets [`keel_pko::ANNOTATION_RECONCILE_REQUEST`] to `request` on Stack `namespace/name`.
    async fn request_stack_reconcile(
        &self,
        namespace: &str,
        name: &str,
        request: &str,
    ) -> Result<(), KubeError>;
}

/// Reconcile requests Keel makes per Secret revision for a Stack that stays stalled on it after
/// PKO observed the previous request (covers PKO's Secret cache lagging the restore).
pub const MAX_RECONCILE_ATTEMPTS: u32 = 3;

/// Reconcile-request value for a Stack stalled on Secret `secret` at `resource_version` (first
/// attempt; later attempts append `#<n>`, see [`reconcile_request_attempt`]).
pub fn reconcile_request_value(secret: &str, resource_version: &str) -> String {
    format!("keel-secret-restored:{secret}@{resource_version}")
}

/// Reconcile-request value of attempt `attempt` (1-based) for Secret `secret` at
/// `resource_version`.
pub fn reconcile_request_attempt(secret: &str, resource_version: &str, attempt: u32) -> String {
    let base = reconcile_request_value(secret, resource_version);
    if attempt <= 1 {
        base
    } else {
        format!("{base}#{attempt}")
    }
}

/// Attempt number of `current` if it is one of Keel's requests for `base`.
fn request_attempt(current: &str, base: &str) -> Option<u32> {
    let rest = current.strip_prefix(base)?;
    if rest.is_empty() {
        return Some(1);
    }
    rest.strip_prefix('#')?
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= 2)
}

fn is_secret_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'
}

/// Leading Kubernetes object name in `s` (`[a-z0-9.-]+`, trailing `.`/`-` trimmed), if any.
fn leading_name(s: &str) -> Option<&str> {
    let end = s.find(|c| !is_secret_name_char(c)).unwrap_or(s.len());
    let name = s[..end].trim_end_matches(['.', '-']);
    (!name.is_empty() && name.len() <= 253).then_some(name)
}

/// Names of Secrets in `namespace` that PKO's stall `message` refers to. Recognises PKO's
/// `unable to get secret <ns>/<name>: ...` and `no key "<k>" found in secret <ns>/<name>` forms
/// and the Kubernetes `secrets "<name>" not found` error; Secrets of other namespaces are ignored
/// (PKO refuses cross-namespace references anyway).
pub fn secrets_named_in_stall_message(namespace: &str, message: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let scoped = format!("secret {namespace}/");
    let mut rest = message;
    while let Some(i) = rest.find(&scoped) {
        rest = &rest[i + scoped.len()..];
        if let Some(name) = leading_name(rest) {
            names.insert(name.to_string());
        }
    }
    let mut rest = message;
    while let Some(i) = rest.find("secrets \"") {
        rest = &rest[i + "secrets \"".len()..];
        if let Some(end) = rest.find('"')
            && let Some(name) = leading_name(&rest[..end])
            && name.len() == end
        {
            names.insert(name.to_string());
        }
    }
    names
}

/// Secrets a PKO `Stack` is stalled on: non-empty only when its `Stalled` condition is `True`
/// with reason `SpecInvalid` and the message names Secrets of the Stack's own namespace.
/// Terminating Stacks are ignored.
pub fn stack_stalled_on_secrets(stack: &Value) -> BTreeSet<String> {
    let meta = &stack["metadata"];
    let Some(namespace) = meta["namespace"].as_str() else {
        return BTreeSet::new();
    };
    if !meta["deletionTimestamp"].is_null() {
        return BTreeSet::new();
    }
    let stalled = stack["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["type"].as_str() == Some(keel_pko::STACK_CONDITION_STALLED));
    match stalled {
        Some(c)
            if c["status"].as_str() == Some("True")
                && c["reason"].as_str() == Some(keel_pko::STACK_STALLED_SPEC_INVALID_REASON) =>
        {
            secrets_named_in_stall_message(namespace, c["message"].as_str().unwrap_or_default())
        }
        _ => BTreeSet::new(),
    }
}

fn is_tenant_namespace(namespace: &str) -> bool {
    namespace
        .strip_prefix(TENANT_NAMESPACE_PREFIX)
        .is_some_and(|id| !id.is_empty())
}

/// Plans reconcile requests: for every Stack in a `tenant-<id>` namespace stalled on Secrets that
/// all exist now (`revisions[(namespace, secret)]` = current `resourceVersion`), one request
/// derived from the first such Secret. A Stack already carrying a request for that revision is
/// skipped while PKO has not observed it yet (`status.observedReconcileRequest`); once observed
/// (and the Stack is still stalled), the request is repeated with the next attempt number, up to
/// [`MAX_RECONCILE_ATTEMPTS`].
pub fn plan_stalled_stack_nudges(
    stacks: &[Value],
    revisions: &BTreeMap<(String, String), String>,
) -> Vec<StalledStackNudge> {
    let mut nudges = Vec::new();
    for stack in stacks {
        let meta = &stack["metadata"];
        let (Some(namespace), Some(name)) = (meta["namespace"].as_str(), meta["name"].as_str())
        else {
            continue;
        };
        if !is_tenant_namespace(namespace) {
            continue;
        }
        let secrets = stack_stalled_on_secrets(stack);
        let Some(first) = secrets.first() else {
            continue;
        };
        let all_present = secrets
            .iter()
            .all(|s| revisions.contains_key(&(namespace.to_string(), s.clone())));
        if !all_present {
            continue;
        }
        let revision = &revisions[&(namespace.to_string(), first.clone())];
        let base = reconcile_request_value(first, revision);
        let current = meta["annotations"][keel_pko::ANNOTATION_RECONCILE_REQUEST].as_str();
        let attempt = match current.and_then(|c| request_attempt(c, &base)) {
            None => 1,
            Some(n) => {
                let observed = stack["status"]["observedReconcileRequest"].as_str();
                if observed != current || n >= MAX_RECONCILE_ATTEMPTS {
                    // PKO has not processed it yet, or the Secret revision is used up.
                    continue;
                }
                n + 1
            }
        };
        let request = reconcile_request_attempt(first, revision, attempt);
        nudges.push(StalledStackNudge {
            namespace: namespace.to_string(),
            stack: name.to_string(),
            secret: first.clone(),
            request,
        });
    }
    nudges
}

/// Requests a reconcile of every tenant Stack (in `namespace`, or in every `tenant-<id>`
/// namespace when `None`) that PKO stalled on a Secret which exists again. Best effort per Stack:
/// a failed annotation is logged and skipped. Returns the requests made.
pub async fn recover_stalled_stacks(
    cluster: &dyn StackRecoveryCluster,
    namespace: Option<&str>,
) -> Result<Vec<StalledStackNudge>, KubeError> {
    if let Some(ns) = namespace
        && !is_tenant_namespace(ns)
    {
        return Ok(Vec::new());
    }
    let stacks = cluster.list_stacks(namespace).await?;
    let mut wanted = BTreeSet::new();
    for stack in &stacks {
        let Some(ns) = stack["metadata"]["namespace"].as_str() else {
            continue;
        };
        if !is_tenant_namespace(ns) {
            continue;
        }
        for secret in stack_stalled_on_secrets(stack) {
            wanted.insert((ns.to_string(), secret));
        }
    }
    let mut revisions = BTreeMap::new();
    for (ns, secret) in wanted {
        match cluster.secret_revision(&ns, &secret).await {
            Ok(Some(rv)) => {
                revisions.insert((ns, secret), rv);
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(namespace = %ns, secret = %secret, error = %err, "Could not read a Secret a stalled Stack waits for");
            }
        }
    }
    let mut done = Vec::new();
    for nudge in plan_stalled_stack_nudges(&stacks, &revisions) {
        match cluster
            .request_stack_reconcile(&nudge.namespace, &nudge.stack, &nudge.request)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    target: "keel::audit",
                    action = "stack.reconcile-request",
                    namespace = %nudge.namespace,
                    stack = %nudge.stack,
                    secret = %nudge.secret,
                    "Requested reconciliation of a Stack stalled on a restored Secret"
                );
                done.push(nudge);
            }
            Err(err) => {
                tracing::warn!(
                    namespace = %nudge.namespace,
                    stack = %nudge.stack,
                    error = %err,
                    "Could not request reconciliation of a stalled Stack"
                );
            }
        }
    }
    Ok(done)
}

fn stack_api_resource() -> ApiResource {
    ApiResource {
        group: "pulumi.com".to_string(),
        version: "v1".to_string(),
        api_version: "pulumi.com/v1".to_string(),
        kind: "Stack".to_string(),
        plural: "stacks".to_string(),
    }
}

/// [`StackRecoveryCluster`] backed by the Kubernetes API (keel-server's identity: cluster-wide
/// `list` on Stacks, `get` on Secrets and `patch` on Stacks in tenant namespaces).
#[derive(Clone)]
pub struct KubeStackRecovery {
    client: Client,
}

impl KubeStackRecovery {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl StackRecoveryCluster for KubeStackRecovery {
    async fn list_stacks(&self, namespace: Option<&str>) -> Result<Vec<Value>, KubeError> {
        let ar = stack_api_resource();
        let api: Api<DynamicObject> = match namespace {
            Some(ns) => Api::namespaced_with(self.client.clone(), ns, &ar),
            None => Api::all_with(self.client.clone(), &ar),
        };
        let list = api.list(&ListParams::default()).await?;
        list.items
            .into_iter()
            .map(|o| serde_json::to_value(o).map_err(|e| KubeError::InvalidInput(e.to_string())))
            .collect()
    }

    async fn secret_revision(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<String>, KubeError> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        Ok(api
            .get_metadata_opt(name)
            .await?
            .map(|m| m.metadata.resource_version.unwrap_or_default()))
    }

    async fn request_stack_reconcile(
        &self,
        namespace: &str,
        name: &str,
        request: &str,
    ) -> Result<(), KubeError> {
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &stack_api_resource());
        let patch = json!({
            "metadata": { "annotations": { keel_pko::ANNOTATION_RECONCILE_REQUEST: request } }
        });
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// PKO v2.10.1 `resolveGitAuth` message for a missing `gitAuth.basicAuth` Secret.
    const MISSING: &str = "resolving gitAuth password: unable to get secret tenant-acme/keel-gitops-credentials: secrets \"keel-gitops-credentials\" not found";

    /// Stack whose `request` annotation PKO has already processed (and is still stalled).
    fn observed(mut stack: Value) -> Value {
        let request =
            stack["metadata"]["annotations"][keel_pko::ANNOTATION_RECONCILE_REQUEST].clone();
        stack["status"]["observedReconcileRequest"] = request;
        stack
    }

    fn stack(ns: &str, name: &str, stalled: Option<(&str, &str)>, request: Option<&str>) -> Value {
        let mut conditions =
            vec![json!({"type": "Ready", "status": "False", "reason": "NotReadyStalled"})];
        if let Some((reason, message)) = stalled {
            conditions.push(
                json!({"type": "Stalled", "status": "True", "reason": reason, "message": message}),
            );
        }
        let mut meta = json!({"namespace": ns, "name": name});
        if let Some(r) = request {
            meta["annotations"] = json!({ keel_pko::ANNOTATION_RECONCILE_REQUEST: r });
        }
        json!({"apiVersion": "pulumi.com/v1", "kind": "Stack", "metadata": meta, "status": {"conditions": conditions}})
    }

    #[test]
    fn stall_messages_name_secrets_of_the_stack_namespace_only() {
        assert_eq!(
            secrets_named_in_stall_message("tenant-acme", MISSING),
            BTreeSet::from(["keel-gitops-credentials".to_string()])
        );
        // Exact PKO v2.10.1 messages: `resolveGitAuth` wrapping `resolveSecretResourceRef`
        // (git.go / stack_controller.go) for `spec.gitAuth`...
        assert_eq!(
            secrets_named_in_stall_message(
                "tenant-acme",
                "resolving gitAuth password: unable to get secret tenant-acme/keel-gitops-credentials: secrets \"keel-gitops-credentials\" not found"
            ),
            BTreeSet::from(["keel-gitops-credentials".to_string()])
        );
        assert_eq!(
            secrets_named_in_stall_message(
                "tenant-acme",
                "resolving gitAuth SSH private key: unable to get secret tenant-acme/keel-gitops-credentials: secrets \"keel-gitops-credentials\" not found"
            ),
            BTreeSet::from(["keel-gitops-credentials".to_string()])
        );
        assert_eq!(
            secrets_named_in_stall_message(
                "tenant-acme",
                "resolving gitAuth username: no key \"username\" found in secret tenant-acme/git-credentials"
            ),
            BTreeSet::from(["git-credentials".to_string()])
        );
        // ...and for the legacy `spec.gitAuthSecret` (bare kube error / `%s` of a NamespacedName).
        assert_eq!(
            secrets_named_in_stall_message(
                "tenant-acme",
                "creating gitAuth: no key \"password\" found in secret tenant-acme/git-credentials"
            ),
            BTreeSet::from(["git-credentials".to_string()])
        );
        assert_eq!(
            secrets_named_in_stall_message("tenant-acme", "secrets \"git-credentials\" not found"),
            BTreeSet::from(["git-credentials".to_string()])
        );
        // Other namespaces, malformed names and unrelated errors yield nothing.
        assert!(
            secrets_named_in_stall_message(
                "tenant-acme",
                "unable to get secret tenant-other/x: boom"
            )
            .is_empty()
        );
        assert!(
            secrets_named_in_stall_message("tenant-acme", "secrets \"Bad Name\" not found")
                .is_empty()
        );
        assert!(secrets_named_in_stall_message("tenant-acme", "invalid gitSource url").is_empty());
    }

    #[test]
    fn only_spec_invalid_stalls_on_secrets_count() {
        let s = stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None);
        assert_eq!(stack_stalled_on_secrets(&s).len(), 1);
        let s = stack(
            "tenant-acme",
            "net",
            Some(("SourceUnavailable", MISSING)),
            None,
        );
        assert!(stack_stalled_on_secrets(&s).is_empty());
        let mut s = stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None);
        s["status"]["conditions"][1]["status"] = json!("False");
        assert!(stack_stalled_on_secrets(&s).is_empty());
        let mut s = stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None);
        s["metadata"]["deletionTimestamp"] = json!("2026-11-23T00:00:00Z");
        assert!(stack_stalled_on_secrets(&s).is_empty());
        assert!(stack_stalled_on_secrets(&stack("tenant-acme", "ok", None, None)).is_empty());
    }

    #[test]
    fn nudges_are_planned_once_per_secret_revision_in_tenant_namespaces() {
        let revs = BTreeMap::from([(
            (
                "tenant-acme".to_string(),
                "keel-gitops-credentials".to_string(),
            ),
            "42".to_string(),
        )]);
        let request = reconcile_request_value("keel-gitops-credentials", "42");
        let stacks = vec![
            stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None),
            // Already nudged for this revision.
            stack(
                "tenant-acme",
                "db",
                Some(("SpecInvalid", MISSING)),
                Some(&request),
            ),
            // Nudged for an older revision: nudge again.
            stack(
                "tenant-acme",
                "dns",
                Some(("SpecInvalid", MISSING)),
                Some(&reconcile_request_value("keel-gitops-credentials", "7")),
            ),
            // Secret still missing (no revision): no nudge.
            stack(
                "tenant-acme",
                "s3",
                Some(("SpecInvalid", "secrets \"keel-s3-credentials\" not found")),
                None,
            ),
            // Not a tenant namespace.
            stack(
                "flux-system",
                "core",
                Some((
                    "SpecInvalid",
                    "secrets \"keel-gitops-credentials\" not found",
                )),
                None,
            ),
            stack("tenant-acme", "healthy", None, None),
        ];
        let plan = plan_stalled_stack_nudges(&stacks, &revs);
        let names: Vec<_> = plan.iter().map(|n| n.stack.as_str()).collect();
        assert_eq!(names, ["net", "dns"]);
        assert!(
            plan.iter()
                .all(|n| n.request == request && n.secret == "keel-gitops-credentials")
        );
    }

    #[test]
    fn a_stack_still_stalled_after_pko_observed_the_request_is_retried_a_bounded_number_of_times() {
        // PKO reads Secrets through its cache: the reconcile triggered by the first request can
        // still miss the restored Secret and stall again with the same message.
        let revs = BTreeMap::from([(
            (
                "tenant-acme".to_string(),
                "keel-gitops-credentials".to_string(),
            ),
            "42".to_string(),
        )]);
        let attempt = |n| reconcile_request_attempt("keel-gitops-credentials", "42", n);
        assert_eq!(
            attempt(1),
            reconcile_request_value("keel-gitops-credentials", "42")
        );
        assert_eq!(
            attempt(2),
            "keel-secret-restored:keel-gitops-credentials@42#2"
        );
        let stalled = |request: &str| {
            stack(
                "tenant-acme",
                "net",
                Some(("SpecInvalid", MISSING)),
                Some(request),
            )
        };
        let plan = |s: Value| plan_stalled_stack_nudges(&[s], &revs);

        // Observed and still stalled: next attempt.
        let p = plan(observed(stalled(&attempt(1))));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].request, attempt(2));
        assert_eq!(plan(observed(stalled(&attempt(2))))[0].request, attempt(3));
        // Not observed by PKO yet: wait.
        assert!(plan(stalled(&attempt(1))).is_empty());
        let mut pending = stalled(&attempt(2));
        pending["status"]["observedReconcileRequest"] = json!(attempt(1));
        assert!(plan(pending).is_empty());
        // Attempts for this Secret revision used up: never loop.
        assert!(plan(observed(stalled(&attempt(MAX_RECONCILE_ATTEMPTS)))).is_empty());
        // A request for another revision (or a foreign value, e.g. Reconcile Now) starts over,
        // and a similar-looking revision is not mistaken for an attempt suffix.
        for other in [
            reconcile_request_value("keel-gitops-credentials", "4"),
            reconcile_request_value("keel-gitops-credentials", "421"),
            format!("{}#x", attempt(1)),
            "2026-11-23T00:00:00Z".to_string(),
        ] {
            assert_eq!(
                plan(observed(stalled(&other)))[0].request,
                attempt(1),
                "{other}"
            );
        }
    }

    #[derive(Default)]
    struct FakeCluster {
        stacks: Vec<Value>,
        secrets: BTreeMap<(String, String), String>,
        fail_patch: BTreeSet<String>,
        listed: Mutex<Vec<Option<String>>>,
        patched: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl StackRecoveryCluster for FakeCluster {
        async fn list_stacks(&self, namespace: Option<&str>) -> Result<Vec<Value>, KubeError> {
            self.listed
                .lock()
                .unwrap()
                .push(namespace.map(str::to_string));
            Ok(self
                .stacks
                .iter()
                .filter(|s| namespace.is_none_or(|ns| s["metadata"]["namespace"] == ns))
                .cloned()
                .collect())
        }
        async fn secret_revision(&self, ns: &str, name: &str) -> Result<Option<String>, KubeError> {
            if name == "forbidden" {
                return Err(KubeError::InvalidInput("forbidden".into()));
            }
            Ok(self
                .secrets
                .get(&(ns.to_string(), name.to_string()))
                .cloned())
        }
        async fn request_stack_reconcile(
            &self,
            ns: &str,
            name: &str,
            request: &str,
        ) -> Result<(), KubeError> {
            if self.fail_patch.contains(name) {
                return Err(KubeError::InvalidInput("conflict".into()));
            }
            self.patched.lock().unwrap().push((
                ns.to_string(),
                name.to_string(),
                request.to_string(),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn recovery_nudges_stacks_whose_secret_is_back_and_survives_errors() {
        let other = "unable to get secret tenant-beta/keel-gitops-credentials: secrets \"keel-gitops-credentials\" not found";
        let cluster = FakeCluster {
            stacks: vec![
                stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None),
                stack(
                    "tenant-acme",
                    "broken",
                    Some(("SpecInvalid", MISSING)),
                    None,
                ),
                stack(
                    "tenant-acme",
                    "denied",
                    Some(("SpecInvalid", "secrets \"forbidden\" not found")),
                    None,
                ),
                stack("tenant-beta", "web", Some(("SpecInvalid", other)), None),
            ],
            secrets: BTreeMap::from([
                (
                    ("tenant-acme".into(), "keel-gitops-credentials".into()),
                    "5".into(),
                ),
                (
                    ("tenant-beta".into(), "keel-gitops-credentials".into()),
                    "9".into(),
                ),
            ]),
            fail_patch: BTreeSet::from(["broken".to_string()]),
            ..FakeCluster::default()
        };
        let done = recover_stalled_stacks(&cluster, None).await.unwrap();
        let names: Vec<_> = done
            .iter()
            .map(|n| (n.namespace.as_str(), n.stack.as_str()))
            .collect();
        assert_eq!(names, [("tenant-acme", "net"), ("tenant-beta", "web")]);
        assert_eq!(
            cluster.patched.lock().unwrap()[0],
            (
                "tenant-acme".to_string(),
                "net".to_string(),
                "keel-secret-restored:keel-gitops-credentials@5".to_string()
            )
        );

        // Scoped to one tenant namespace; non-tenant namespaces are never listed.
        let done = recover_stalled_stacks(&cluster, Some("tenant-beta"))
            .await
            .unwrap();
        assert_eq!(done.len(), 1);
        assert!(
            recover_stalled_stacks(&cluster, Some("flux-system"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            *cluster.listed.lock().unwrap(),
            [None, Some("tenant-beta".to_string())]
        );
    }

    type Recorded = Arc<Mutex<Vec<(String, String, String, Value)>>>;

    fn scripted(responses: Vec<(u16, Value)>) -> (KubeStackRecovery, Recorded) {
        let recorded: Recorded = Arc::default();
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
        let rec = recorded.clone();
        let service = tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let rec = rec.clone();
            let queue = queue.clone();
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let accept = req
                    .headers()
                    .get("accept")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let bytes = req.into_body().collect_bytes().await.unwrap();
                let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                rec.lock().unwrap().push((method, path, accept, body));
                let (status, body) = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unscripted request");
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(body.to_string().into_bytes()))
                        .unwrap(),
                )
            }
        });
        (
            KubeStackRecovery::new(Client::new(service, "default")),
            recorded,
        )
    }

    #[tokio::test]
    async fn kube_backend_lists_reads_metadata_only_and_merge_patches_the_annotation() {
        let listed = json!({
            "apiVersion": "pulumi.com/v1", "kind": "StackList", "metadata": {},
            "items": [stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None)]
        });
        let secret_meta = json!({
            "apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata",
            "metadata": {"name": "keel-gitops-credentials", "namespace": "tenant-acme", "resourceVersion": "77"}
        });
        let (cluster, recorded) = scripted(vec![
            (200, listed),
            (200, secret_meta),
            (200, stack("tenant-acme", "net", None, None)),
        ]);
        let done = recover_stalled_stacks(&cluster, None).await.unwrap();
        assert_eq!(done.len(), 1);
        let rec = recorded.lock().unwrap();
        assert_eq!(rec.len(), 3);
        assert_eq!(
            (rec[0].0.as_str(), rec[0].1.as_str()),
            ("GET", "/apis/pulumi.com/v1/stacks")
        );
        assert_eq!(
            (rec[1].0.as_str(), rec[1].1.as_str()),
            (
                "GET",
                "/api/v1/namespaces/tenant-acme/secrets/keel-gitops-credentials"
            )
        );
        assert!(
            rec[1].2.contains("PartialObjectMetadata"),
            "Secret data must not be read: {}",
            rec[1].2
        );
        assert_eq!(
            (rec[2].0.as_str(), rec[2].1.as_str()),
            (
                "PATCH",
                "/apis/pulumi.com/v1/namespaces/tenant-acme/stacks/net"
            )
        );
        assert_eq!(
            rec[2].3,
            json!({"metadata": {"annotations": {"pulumi.com/reconciliation-request": "keel-secret-restored:keel-gitops-credentials@77"}}})
        );
    }

    #[tokio::test]
    async fn kube_backend_treats_a_missing_secret_as_absent() {
        let listed = json!({
            "apiVersion": "pulumi.com/v1", "kind": "StackList", "metadata": {},
            "items": [stack("tenant-acme", "net", Some(("SpecInvalid", MISSING)), None)]
        });
        let not_found = json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404, "message": "not found"});
        let (cluster, recorded) = scripted(vec![(200, listed), (404, not_found)]);
        assert!(
            recover_stalled_stacks(&cluster, Some("tenant-acme"))
                .await
                .unwrap()
                .is_empty()
        );
        let rec = recorded.lock().unwrap();
        assert_eq!(
            rec[0].1,
            "/apis/pulumi.com/v1/namespaces/tenant-acme/stacks"
        );
        assert_eq!(rec.len(), 2);
    }
}
