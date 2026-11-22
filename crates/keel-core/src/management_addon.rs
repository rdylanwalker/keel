//! Management-plane base add-on manifests (`clusters/management/core-addons/*.yaml`).
//!
//! Every control-plane add-on is reconciled by exactly one Flux object whose `metadata.name`
//! equals its `catalog/core` application id: a `HelmRelease` for Helm charts, or a Flux
//! `Kustomization` (`kustomize.toolkit.fluxcd.io`) for plain-manifest add-ons. Core add-on files
//! may also carry supporting documents (`ClusterIssuer`, `Gateway`, `ServiceAccount`, ...), so
//! all helpers here are multi-document aware and only ever identify the add-on by its Flux object
//! (`SDD.md` §2.2, §7.3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CoreError, FLUX_SYSTEM_NAMESPACE, KEEL_SYSTEM_NAMESPACE, yaml_leading_comments};

/// Repository-relative directory holding the management-plane core add-on manifests.
pub const MANAGEMENT_CORE_ADDONS_DIR: &str = "clusters/management/core-addons";

/// Maximum accepted length of a requested add-on version / image tag.
pub const MANAGEMENT_ADDON_VERSION_MAX_LEN: usize = 64;

/// Flux object kind reconciling a management-plane add-on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ManagementAddonKind {
    /// `helm.toolkit.fluxcd.io` `HelmRelease` (chart add-ons).
    HelmRelease,
    /// `kustomize.toolkit.fluxcd.io` `Kustomization` (plain-manifest add-ons).
    Kustomization,
}

impl ManagementAddonKind {
    /// Kubernetes `kind` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HelmRelease => "HelmRelease",
            Self::Kustomization => "Kustomization",
        }
    }

    /// Flux API group serving this kind.
    pub fn api_group(self) -> &'static str {
        match self {
            Self::HelmRelease => "helm.toolkit.fluxcd.io",
            Self::Kustomization => "kustomize.toolkit.fluxcd.io",
        }
    }
}

impl std::fmt::Display for ManagementAddonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identity of the Flux object reconciling a management-plane add-on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagementAddonRef {
    pub kind: ManagementAddonKind,
    pub name: String,
    pub namespace: String,
}

/// Returns the add-on identity when `doc` is a Flux `HelmRelease` or Flux `Kustomization`
/// with an explicit `metadata.namespace` of `keel-system` / `flux-system`. Any other document (Services, ServiceAccounts,
/// `kustomize.config.k8s.io` Kustomizations, ...) yields `None`.
pub fn management_addon_ref(doc: &Value) -> Option<ManagementAddonRef> {
    let kind = match doc.get("kind").and_then(Value::as_str)? {
        "HelmRelease" => ManagementAddonKind::HelmRelease,
        "Kustomization" => ManagementAddonKind::Kustomization,
        _ => return None,
    };
    let api_version = doc.get("apiVersion").and_then(Value::as_str)?;
    let (group, version) = api_version.split_once('/')?;
    if group != kind.api_group() || version.is_empty() {
        return None;
    }
    let meta = doc.get("metadata")?;
    let name = meta.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    // The `core-addons` Flux Kustomization has no `targetNamespace`, so add-on Flux objects must
    // name their namespace explicitly; a namespace-less document is not a management add-on.
    let namespace = meta.get("namespace").and_then(Value::as_str)?;
    if namespace != KEEL_SYSTEM_NAMESPACE && namespace != FLUX_SYSTEM_NAMESPACE {
        return None;
    }
    Some(ManagementAddonRef {
        kind,
        name: name.to_string(),
        namespace: namespace.to_string(),
    })
}

fn scalar_string(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.as_number().map(|n| n.to_string()))
}

/// Helm chart name of a `HelmRelease` add-on (`spec.chart.spec.chart`); `None` for Kustomizations.
pub fn management_addon_chart(doc: &Value) -> Option<String> {
    match management_addon_ref(doc)?.kind {
        ManagementAddonKind::HelmRelease => doc
            .pointer("/spec/chart/spec/chart")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ManagementAddonKind::Kustomization => None,
    }
}

/// Desired version: `spec.chart.spec.version` for `HelmRelease`s, the first
/// `spec.images[].newTag` for Flux `Kustomization`s.
pub fn management_addon_desired_version(doc: &Value) -> Option<String> {
    match management_addon_ref(doc)?.kind {
        ManagementAddonKind::HelmRelease => doc
            .pointer("/spec/chart/spec/version")
            .and_then(scalar_string),
        ManagementAddonKind::Kustomization => {
            doc.pointer("/spec/images/0/newTag").and_then(scalar_string)
        }
    }
}

/// Per-add-on allowlist of `HelmRelease.spec.values` paths a management API upgrade or rollback may
/// change, so an upgrade PR cannot weaken security settings through neighbouring keys of the same
/// chart (`NFR-SEC-02`, `NFR-SEC-04`, `NFR-REL-01`).
///
/// `None` means the add-on's values are unrestricted. `Some(patterns)` means every changed value
/// path must lie inside (at or below) one of the patterns. A pattern is a JSON pointer whose
/// segments may list alternatives separated by `|`. For security-critical add-ons only resource
/// sizing, scheduling and log verbosity are mutable: container args, feature flags, CRDs, RBAC,
/// webhook and authentication settings can only be changed by editing the manifest in Git directly.
pub fn management_addon_mutable_values(id: &str) -> Option<&'static [&'static str]> {
    match id {
        // Flux multi-tenancy lockdown (`--no-cross-namespace-refs`, default service-account
        // impersonation) is the tenant isolation boundary; controller `container.additionalArgs`
        // could override it, so only sizing/scheduling is allowlisted.
        "flux2" => Some(&[
            "/logLevel",
            "/helmController|imageAutomationController|imageReflectionController|kustomizeController|notificationController|sourceController|sourceWatcher/resources|tolerations|nodeSelector|affinity|priorityClassName",
        ]),
        // Kyverno admission enforcement: CRDs, features/featuresOverride, container extraArgs,
        // webhook and resource-filter configuration all decide what is enforced.
        "kyverno" => Some(&[
            "/admissionController|backgroundController|cleanupController|reportsController/replicas|resources|tolerations|nodeSelector|affinity|priorityClassName|topologySpreadConstraints|podDisruptionBudget",
            "/admissionController/container|initContainer/resources",
        ]),
        // The seed S3 state backend holds every Pulumi state file (`NFR-REL-01`); S3 endpoints
        // (`s3`, `filer.s3`, `allInOne.s3`) and their authentication must never change via the API.
        "s3-state-backend" => Some(&[
            "/master|volume|filer/resources|tolerations|nodeSelector|affinity|priorityClassName",
        ]),
        // Optional in-cluster Forgejo (`--with-forge`) hosts the management GitOps repository:
        // the admin `existingSecret`, OAuth2 client settings (`gitea.config.oauth2_client`,
        // `gitea.oauth`), `ROOT_URL`, database and CA trust (`extraVolumes`, `deployment.env`)
        // decide who can push to the repository Flux reconciles from, so only sizing and
        // scheduling are mutable. `persistence.enabled` (toggling it replaces the pod and its
        // volume), `persistence.annotations` (`helm.sh/resource-policy: keep` retains the
        // repository PVC) and `persistence.existingClaim`/`claimName` are locked. The add-on is
        // only upgradable at all once persistence is enabled
        // ([`ensure_management_addon_upgradable`]).
        "forgejo" => Some(&[
            "/resources|tolerations|nodeSelector|affinity|topologySpreadConstraints",
            "/persistence/size|storageClass|accessModes|labels",
        ]),
        // PKO runs every management/tenant Stack. Its operator CA trust (`extraEnv`
        // `SSL_CERT_DIR`, `extraVolumes`/`extraVolumeMounts` of the public `keel-ca-bundle`),
        // security contexts, RBAC and service account decide which Git endpoints it trusts and
        // what it may do, so only replicas (`replicaCount`, the key the chart reads), log
        // settings, sizing and scheduling are mutable. The `catalog/core` upgrade form schema
        // exposes only these keys, so its submitted defaults never touch locked values.
        "pulumi-kubernetes-operator" => Some(&[
            "/replicaCount",
            "/controller/logLevel|logFormat",
            "/resources|tolerations|nodeSelector|affinity|priorityClassName|topologySpreadConstraints",
        ]),
        _ => None,
    }
}

/// Rejects management API upgrades and rollbacks of an add-on whose current desired state (`doc`,
/// its Flux object as stored in Git) makes any pod replacement destructive (`NFR-REL-01`).
///
/// The optional in-cluster `forgejo` hosts the management GitOps repository Flux pulls from, its
/// SQLite database and the Keel API token. With `spec.values.persistence.enabled` not `true` that
/// data lives on an `emptyDir`, so a chart bump, values change or failed-upgrade rollback would
/// wipe the repository the PR was merged into and lock the management plane out. Such a Forgejo
/// must be backed up and switched to persistence by editing Git directly.
pub fn ensure_management_addon_upgradable(id: &str, doc: &Value) -> Result<(), CoreError> {
    if id == "forgejo"
        && doc.pointer("/spec/values/persistence/enabled") != Some(&Value::Bool(true))
    {
        return Err(CoreError::Validation(
            "Core add-on 'forgejo' runs without persistence (emptyDir): any upgrade or rollback \
             replaces the pod and wipes the GitOps repository Flux reconciles from. Back up the \
             repository first (`forgejo dump` or push a mirror), then enable \
             `persistence.enabled` by editing the HelmRelease in Git directly"
                .to_string(),
        ));
    }
    Ok(())
}

fn escape_pointer_segment(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

fn collect_changed_paths(
    prefix: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    out: &mut Vec<String>,
) {
    if before == after {
        return;
    }
    // A missing key on one side counts as an empty mapping when the other side is a mapping, so
    // an added (or removed) subtree is reported leaf by leaf: adding an allowlisted leaf under a
    // previously unset parent is not mistaken for a change of the whole parent.
    let empty = serde_json::Map::new();
    let as_map = |v: Option<&Value>| match v {
        Some(Value::Object(m)) => Some(m.clone()),
        None => Some(empty.clone()),
        Some(_) => None,
    };
    match (as_map(before), as_map(after)) {
        (Some(b), Some(a)) => {
            let mut keys: Vec<&String> = b.keys().chain(a.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let path = format!("{prefix}/{}", escape_pointer_segment(key));
                collect_changed_paths(&path, b.get(key), a.get(key), out);
            }
        }
        _ if prefix.is_empty() => out.push("/".to_string()),
        _ => out.push(prefix.to_string()),
    }
}

/// JSON pointers of every leaf (or replaced non-mapping subtree) that differs between two values
/// trees. Keys added or removed as whole mappings are reported per leaf.
pub fn changed_value_paths(before: &Value, after: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_changed_paths("", Some(before), Some(after), &mut out);
    out
}

fn pattern_covers(pattern: &str, path: &str) -> bool {
    let pattern_segs: Vec<&str> = pattern.split('/').skip(1).collect();
    let path_segs: Vec<&str> = path.split('/').skip(1).collect();
    path_segs.len() >= pattern_segs.len()
        && pattern_segs
            .iter()
            .zip(&path_segs)
            .all(|(pat, seg)| pat.split('|').any(|alt| alt == *seg))
}

/// Returns the changed value paths of add-on `id` between `before` and `after` (both
/// `HelmRelease.spec.values` trees; a missing tree is `null`) that its policy
/// ([`management_addon_mutable_values`]) does not allow.
pub fn disallowed_value_changes(id: &str, before: &Value, after: &Value) -> Vec<String> {
    let Some(allowed) = management_addon_mutable_values(id) else {
        return Vec::new();
    };
    let normalize = |v: &Value| match v {
        Value::Null => Value::Object(Default::default()),
        other => other.clone(),
    };
    changed_value_paths(&normalize(before), &normalize(after))
        .into_iter()
        .filter(|path| !allowed.iter().any(|pattern| pattern_covers(pattern, path)))
        .collect()
}

/// Validates the version fields of an add-on Flux object (chart version, or every
/// `spec.images[].newTag`) with [`validate_management_addon_version`].
pub fn validate_management_addon_document(id: &str, doc: &Value) -> Result<(), CoreError> {
    let addon = management_addon_ref(doc)
        .filter(|r| r.name == id)
        .ok_or_else(|| not_addon_document(id))?;
    match addon.kind {
        ManagementAddonKind::HelmRelease => {
            if let Some(v) = doc.pointer("/spec/chart/spec/version") {
                let v = scalar_string(v).ok_or_else(|| {
                    CoreError::Validation(format!(
                        "HelmRelease '{id}' chart version is not a scalar"
                    ))
                })?;
                validate_management_addon_version(&v)?;
            }
        }
        ManagementAddonKind::Kustomization => {
            for image in doc
                .pointer("/spec/images")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(tag) = image.get("newTag") {
                    let tag = scalar_string(tag).ok_or_else(|| {
                        CoreError::Validation(format!(
                            "Flux Kustomization '{id}' has a non-scalar spec.images[].newTag"
                        ))
                    })?;
                    validate_management_addon_version(&tag)?;
                }
            }
        }
    }
    Ok(())
}

/// (Re)asserts the `keel.io/protect: "true"` anti-lockout label on an add-on document
/// (AGENTS.md guardrail 7, `NFR-REL-01`).
pub fn ensure_management_addon_protected(doc: &mut Value) -> Result<(), CoreError> {
    let root = doc.as_object_mut().ok_or_else(|| {
        CoreError::Validation("Core add-on manifest root is not a mapping".to_string())
    })?;
    let metadata = object_entry(root, "metadata", "metadata")?;
    let labels = object_entry(metadata, "labels", "metadata.labels")?;
    labels.insert(
        "keel.io/protect".to_string(),
        Value::String("true".to_string()),
    );
    Ok(())
}

fn not_addon_document(id: &str) -> CoreError {
    CoreError::Validation(format!(
        "Document is not the Flux HelmRelease/Kustomization for core add-on '{id}'"
    ))
}

/// Validates a requested add-on chart version or image tag: semver-ish (`1`, `1.2`, `1.2.3`,
/// optional `v` prefix, optional `-prerelease` / `+build` suffix), no whitespace, at most
/// [`MANAGEMENT_ADDON_VERSION_MAX_LEN`] characters.
pub fn validate_management_addon_version(version: &str) -> Result<(), CoreError> {
    let invalid = || {
        CoreError::Validation(format!(
            "Invalid add-on version '{}': expected a semantic version such as 1.2.3 or v1.2.3 \
             (at most {MANAGEMENT_ADDON_VERSION_MAX_LEN} characters, no whitespace)",
            version.escape_debug()
        ))
    };
    if version.is_empty() || version.len() > MANAGEMENT_ADDON_VERSION_MAX_LEN {
        return Err(invalid());
    }
    let body = version.strip_prefix('v').unwrap_or(version);
    let (core, suffix) = match body.find(['-', '+']) {
        Some(idx) => (&body[..idx], Some(&body[idx + 1..])),
        None => (body, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(invalid());
    }
    if let Some(suffix) = suffix
        && (suffix.is_empty()
            || !suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Deep-merges `patch` into `target` (objects merge recursively; anything else replaces).
pub fn merge_json_values(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target_map), Value::Object(patch_map)) => {
            for (k, v) in patch_map {
                match target_map.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        merge_json_values(existing, v);
                    }
                    _ => {
                        target_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (target_slot, patch_val) => {
            *target_slot = patch_val.clone();
        }
    }
}

fn values_patch_is_empty(values: &Value) -> bool {
    match values {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn object_entry<'a>(
    map: &'a mut serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, CoreError> {
    map.entry(key.to_string())
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .ok_or_else(|| {
            CoreError::Validation(format!("Core add-on manifest {path} is not a mapping"))
        })
}

/// Applies a management API upgrade (`version` and/or Helm `values`) to the add-on's Flux object
/// `doc` in place, enforcing [`management_addon_mutable_values`]:
///
/// * `HelmRelease`: `version` sets `spec.chart.spec.version`; `values` are deep-merged into
///   `spec.values`.
/// * Flux `Kustomization`: `version` sets `newTag` on every `spec.images[]` entry (the list must
///   already exist, and digest-pinned entries are rejected because kustomize prefers `digest`
///   over `newTag`); `values` are rejected.
///
/// Rejects add-ons that cannot be replaced safely ([`ensure_management_addon_upgradable`]).
/// Always (re)asserts the `keel.io/protect: "true"` label. Returns `CoreError::Validation` (HTTP
/// 400) for invalid input, disallowed value changes, or no-op requests.
pub fn apply_management_addon_upgrade(
    id: &str,
    doc: &mut Value,
    version: Option<&str>,
    values: &Value,
) -> Result<(), CoreError> {
    let addon = management_addon_ref(doc)
        .filter(|r| r.name == id)
        .ok_or_else(|| not_addon_document(id))?;
    ensure_management_addon_upgradable(id, doc)?;
    let version = version.map(str::trim).filter(|v| !v.is_empty());
    let has_values = !values_patch_is_empty(values);
    if version.is_none() && !has_values {
        return Err(CoreError::Validation(format!(
            "Upgrade for core add-on '{id}' must set a version or values"
        )));
    }
    if let Some(v) = version {
        validate_management_addon_version(v)?;
    }
    if has_values && !values.is_object() {
        return Err(CoreError::Validation(
            "Add-on values must be a JSON object".to_string(),
        ));
    }

    let root = doc.as_object_mut().ok_or_else(|| {
        CoreError::Validation("Core add-on manifest root is not a mapping".to_string())
    })?;
    match addon.kind {
        ManagementAddonKind::HelmRelease => {
            let spec = object_entry(root, "spec", "spec")?;
            if let Some(v) = version {
                let chart_spec = spec
                    .get_mut("chart")
                    .and_then(|c| c.get_mut("spec"))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| {
                        CoreError::Validation(format!(
                            "HelmRelease '{id}' has no spec.chart.spec mapping; its chart \
                             version cannot be upgraded through the management API"
                        ))
                    })?;
                chart_spec.insert("version".to_string(), Value::String(v.to_string()));
            }
            if has_values {
                let before = spec.get("values").cloned().unwrap_or(Value::Null);
                let mut after = if before.is_object() {
                    before.clone()
                } else {
                    Value::Object(Default::default())
                };
                merge_json_values(&mut after, values);
                let disallowed = disallowed_value_changes(id, &before, &after);
                if !disallowed.is_empty() {
                    return Err(CoreError::Validation(format!(
                        "Values for core add-on '{id}' would change locked settings ({}); only \
                         resource sizing and scheduling values may be changed through the \
                         management API, anything else must be edited in Git directly",
                        disallowed.join(", ")
                    )));
                }
                spec.insert("values".to_string(), after);
            }
        }
        ManagementAddonKind::Kustomization => {
            if has_values {
                return Err(CoreError::Validation(format!(
                    "Core add-on '{id}' is a Flux Kustomization; Helm values are not supported \
                     (only version upgrades via spec.images[].newTag)"
                )));
            }
            if let Some(v) = version {
                let images = root
                    .get_mut("spec")
                    .and_then(|s| s.get_mut("images"))
                    .and_then(Value::as_array_mut)
                    .filter(|imgs| !imgs.is_empty())
                    .ok_or_else(|| {
                        CoreError::Validation(format!(
                            "Flux Kustomization '{id}' has no spec.images entries; its version \
                             cannot be upgraded through the management API"
                        ))
                    })?;
                for image in images.iter_mut() {
                    let obj = image
                        .as_object_mut()
                        .filter(|o| o.get("name").and_then(Value::as_str).is_some())
                        .ok_or_else(|| {
                            CoreError::Validation(format!(
                                "Flux Kustomization '{id}' has a malformed spec.images entry \
                                 (expected a mapping with a name)"
                            ))
                        })?;
                    if obj.contains_key("digest") {
                        return Err(CoreError::Validation(format!(
                            "Flux Kustomization '{id}' pins spec.images entry '{}' by digest, \
                             which kustomize prefers over newTag; change the digest in Git \
                             directly",
                            obj.get("name").and_then(Value::as_str).unwrap_or_default()
                        )));
                    }
                    obj.insert("newTag".to_string(), Value::String(v.to_string()));
                }
            }
        }
    }

    ensure_management_addon_protected(doc)
}

/// Splits a plain (unquoted, non-complex) block-mapping entry `key: value` / `key:`.
fn split_plain_mapping_entry(rest: &str) -> Option<(&str, &str)> {
    if rest.starts_with([
        '"', '\'', '{', '[', '?', '&', '*', '!', '|', '>', '%', '@', '`', '#',
    ]) {
        return None;
    }
    let (key, value) = match rest.find(": ") {
        Some(pos) => (&rest[..pos], rest[pos + 2..].trim_start_matches(' ')),
        None => (rest.strip_suffix(':')?, ""),
    };
    let key = key.trim_end_matches(' ');
    (!key.is_empty() && !key.contains(" #")).then_some((key, value))
}

/// Rewrites, in place, every inline scalar whose block-mapping key path equals `target` (`*`
/// matches a sequence item), keeping indentation, quoting style of the surroundings and trailing
/// comments. Line-based and deliberately conservative: callers must verify the result by
/// re-parsing it. Returns `None` when nothing was rewritten or the text uses tab indentation.
fn rewrite_yaml_scalars(text: &str, target: &[&str], new_value: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + 16);
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut block_scalar_parent: Option<usize> = None;
    let mut rewritten = 0usize;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let content = body.trim_start_matches(' ');
        let indent = body.len() - content.len();
        if let Some(parent) = block_scalar_parent {
            if content.is_empty() || indent > parent {
                out.push_str(line);
                continue;
            }
            block_scalar_parent = None;
        }
        if content.is_empty() || content.starts_with('#') {
            out.push_str(line);
            continue;
        }
        if content.starts_with('\t') {
            return None;
        }
        let mut col = indent;
        let mut rest = content;
        while rest == "-" || rest.starts_with("- ") {
            while stack
                .last()
                .is_some_and(|(i, seg)| *i > col || (*i == col && seg == "*"))
            {
                stack.pop();
            }
            stack.push((col, "*".to_string()));
            let after = rest[1..].trim_start_matches(' ');
            col += rest.len() - after.len();
            rest = after;
        }
        let Some((key, value)) = split_plain_mapping_entry(rest) else {
            out.push_str(line);
            continue;
        };
        while stack.last().is_some_and(|(i, _)| *i >= col) {
            stack.pop();
        }
        let path_matches = stack.len() + 1 == target.len()
            && stack
                .iter()
                .map(|(_, seg)| seg.as_str())
                .chain(std::iter::once(key))
                .zip(target)
                .all(|(seg, want)| seg == *want);
        if value.starts_with(['|', '>']) {
            block_scalar_parent = Some(col);
        }
        let scalar_value =
            !value.is_empty() && !value.starts_with(['{', '[', '&', '*', '!', '|', '>', '#']);
        if path_matches && scalar_value {
            let value_start = body.len() - value.len();
            let core_len = value.find(" #").unwrap_or(value.len());
            let core = value[..core_len].trim_end_matches(' ');
            out.push_str(&body[..value_start]);
            out.push_str(new_value);
            out.push_str(&body[value_start + core.len()..]);
            out.push_str(&line[body.len()..]);
            rewritten += 1;
        } else {
            out.push_str(line);
        }
        stack.push((col, key.to_string()));
    }
    (rewritten > 0).then_some(out)
}

/// Text-preserving version edit: rewrites only the version scalar(s) of the add-on Flux object
/// (`spec.chart.spec.version`, or every `spec.images[].newTag` when they all share one tag).
fn targeted_version_edit(original_text: &str, updated: &Value) -> Option<String> {
    let addon = management_addon_ref(updated)?;
    let (target, version): (&[&str], String) = match addon.kind {
        ManagementAddonKind::HelmRelease => (
            &["spec", "chart", "spec", "version"],
            updated
                .pointer("/spec/chart/spec/version")
                .and_then(scalar_string)?,
        ),
        ManagementAddonKind::Kustomization => {
            let images = updated.pointer("/spec/images")?.as_array()?;
            let tag = images.first()?.get("newTag").and_then(scalar_string)?;
            if images
                .iter()
                .any(|img| img.get("newTag").and_then(scalar_string).as_deref() != Some(&tag))
            {
                return None;
            }
            (&["spec", "images", "*", "newTag"], tag)
        }
    };
    if version.contains(['"', '\\']) || version.chars().any(char::is_control) {
        return None;
    }
    rewrite_yaml_scalars(original_text, target, &format!("\"{version}\""))
}

/// Renders the edited add-on Flux object `updated` as YAML document text, starting from the
/// document's current text:
///
/// 1. unchanged documents are returned verbatim;
/// 2. version-only changes are applied as a targeted scalar edit (keeping key order and every
///    comment), verified by re-parsing;
/// 3. anything else is re-serialized, keeping the document's leading comment block.
pub fn render_management_addon_document(
    original_text: &str,
    updated: &Value,
) -> Result<String, CoreError> {
    let parses_to_updated =
        |text: &str| serde_yml::from_str::<Value>(text).is_ok_and(|v| &v == updated);
    if parses_to_updated(original_text) {
        return Ok(original_text.to_string());
    }
    if let Some(edited) = targeted_version_edit(original_text, updated)
        && parses_to_updated(&edited)
    {
        return Ok(edited);
    }
    let serialized = serde_yml::to_string(updated)?;
    let serialized = serialized.strip_prefix("---\n").unwrap_or(&serialized);
    Ok(format!(
        "{}{serialized}",
        yaml_leading_comments(original_text)
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn helm_release(name: &str, values: Value) -> Value {
        json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": name, "namespace": "keel-system"},
            "spec": {"chart": {"spec": {"chart": name, "version": "1.0.0"}}, "values": values}
        })
    }

    fn flux_kustomization(name: &str) -> Value {
        json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
            "kind": "Kustomization",
            "metadata": {"name": name, "namespace": "flux-system"},
            "spec": {
                "path": "./clusters/management/addons/rauthy",
                "images": [
                    {"name": "ghcr.io/sebadob/rauthy", "newTag": "0.37.2"},
                    {"name": "ghcr.io/sebadob/rauthy-init", "newTag": "0.37.2"}
                ]
            }
        })
    }

    #[test]
    fn addon_ref_only_matches_flux_objects_in_management_namespaces() {
        let hr = helm_release("kyverno", json!({}));
        let r = management_addon_ref(&hr).unwrap();
        assert_eq!(r.kind, ManagementAddonKind::HelmRelease);
        assert_eq!(r.namespace, "keel-system");

        let ks = flux_kustomization("rauthy");
        assert_eq!(
            management_addon_ref(&ks).unwrap().kind,
            ManagementAddonKind::Kustomization
        );

        let svc = json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "rauthy"}});
        assert!(management_addon_ref(&svc).is_none());
        let kustomize_cfg = json!({
            "apiVersion": "kustomize.config.k8s.io/v1beta1",
            "kind": "Kustomization",
            "metadata": {"name": "rauthy"}
        });
        assert!(management_addon_ref(&kustomize_cfg).is_none());
        let wrong_group = json!({
            "apiVersion": "example.com/v1",
            "kind": "HelmRelease",
            "metadata": {"name": "x"}
        });
        assert!(management_addon_ref(&wrong_group).is_none());
        let tenant = json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": "flagger", "namespace": "tenant-a"}
        });
        assert!(management_addon_ref(&tenant).is_none());
    }

    #[test]
    fn desired_version_and_chart_are_kind_aware() {
        let hr = helm_release("flagger", json!({}));
        assert_eq!(management_addon_chart(&hr).as_deref(), Some("flagger"));
        assert_eq!(
            management_addon_desired_version(&hr).as_deref(),
            Some("1.0.0")
        );
        let ks = flux_kustomization("rauthy");
        assert!(management_addon_chart(&ks).is_none());
        assert_eq!(
            management_addon_desired_version(&ks).as_deref(),
            Some("0.37.2")
        );
    }

    #[test]
    fn version_validation() {
        for ok in [
            "1",
            "1.2",
            "1.2.3",
            "v1.2.3",
            "0.37.2",
            "1.0.0-rc.1",
            "2.20.0+build.5",
        ] {
            validate_management_addon_version(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "",
            "latest",
            "1.2.3.4",
            "1..2",
            "1.2.3 ",
            "1.2.3\nevil: true",
            ">=1.0.0",
            "1.2.3-",
            "1.2.3-a b",
            "v",
            &"1".repeat(65),
        ] {
            assert!(
                validate_management_addon_version(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn helm_upgrade_sets_version_merges_values_and_protects() {
        let mut doc = helm_release("flagger", json!({"metricsServer": "a", "nested": {"x": 1}}));
        apply_management_addon_upgrade(
            "flagger",
            &mut doc,
            Some(" 1.41.0 "),
            &json!({"nested": {"y": 2}}),
        )
        .unwrap();
        assert_eq!(doc.pointer("/spec/chart/spec/version").unwrap(), "1.41.0");
        assert_eq!(
            doc.pointer("/spec/values/nested").unwrap(),
            &json!({"x": 1, "y": 2})
        );
        assert_eq!(doc.pointer("/spec/values/metricsServer").unwrap(), "a");
        assert_eq!(
            doc.pointer("/metadata/labels/keel.io~1protect").unwrap(),
            "true"
        );
    }

    #[test]
    fn helm_upgrade_without_chart_spec_is_rejected() {
        let mut doc = json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": "x", "namespace": "keel-system"},
            "spec": {"chartRef": {"kind": "OCIRepository", "name": "x"}}
        });
        let err =
            apply_management_addon_upgrade("x", &mut doc, Some("1.0.0"), &Value::Null).unwrap_err();
        assert!(err.to_string().contains("spec.chart.spec"));
    }

    #[test]
    fn noop_and_non_object_values_are_rejected() {
        let mut doc = helm_release("flagger", json!({}));
        assert!(apply_management_addon_upgrade("flagger", &mut doc, None, &json!({})).is_err());
        assert!(
            apply_management_addon_upgrade("flagger", &mut doc, Some("  "), &Value::Null).is_err()
        );
        assert!(apply_management_addon_upgrade("flagger", &mut doc, None, &json!(["a"])).is_err());
        assert!(
            apply_management_addon_upgrade("other", &mut doc, Some("1.0.0"), &Value::Null).is_err()
        );
    }

    fn flux2_release() -> Value {
        json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": "flux2", "namespace": "flux-system"},
            "spec": {
                "chart": {"spec": {"chart": "flux2", "version": "2.20.0"}},
                "values": {"multitenancy": {"enabled": true, "privileged": true}}
            }
        })
    }

    fn assert_rejected(id: &str, doc: &Value, values: Value, needle: &str) {
        let mut doc = doc.clone();
        let err = apply_management_addon_upgrade(id, &mut doc, None, &values)
            .expect_err(&format!("{values} must be rejected for {id}"));
        assert!(err.to_string().contains(needle), "{err}");
    }

    #[test]
    fn flux2_crd_retention_is_locked_and_source_watcher_is_sizable() {
        let mut flux = flux2_release();
        flux["spec"]["values"]["crds"] =
            json!({"annotations": {"helm.sh/resource-policy": "keep"}});
        // Dropping `helm.sh/resource-policy: keep` would let an uninstall delete every Flux CRD.
        assert_rejected(
            "flux2",
            &flux,
            json!({"crds": {"annotations": {"helm.sh/resource-policy": "delete"}}}),
            "/crds/annotations",
        );
        assert_rejected(
            "flux2",
            &flux,
            json!({"installCRDs": false}),
            "/installCRDs",
        );
        assert_rejected(
            "flux2",
            &flux,
            json!({"sourceWatcher": {"container": {"additionalArgs": ["--x"]}}}),
            "/sourceWatcher/container/additionalArgs",
        );
        apply_management_addon_upgrade(
            "flux2",
            &mut flux,
            None,
            &json!({"sourceWatcher": {"resources": {"requests": {"cpu": "50m"}}}}),
        )
        .unwrap();
    }

    #[test]
    fn flux2_security_values_cannot_be_changed() {
        let flux = flux2_release();
        assert_rejected(
            "flux2",
            &flux,
            json!({"multitenancy": {"enabled": false}}),
            "/multitenancy/enabled",
        );
        assert_rejected(
            "flux2",
            &flux,
            json!({"multitenancy": null}),
            "/multitenancy",
        );
        // Controller args are appended after the multitenancy flags and the last flag wins.
        for controller in [
            "kustomizeController",
            "helmController",
            "sourceController",
            "notificationController",
        ] {
            assert_rejected(
                "flux2",
                &flux,
                json!({controller: {"container": {"additionalArgs": ["--no-cross-namespace-refs=false"]}}}),
                &format!("/{controller}/container/additionalArgs"),
            );
        }
        assert_rejected(
            "flux2",
            &flux,
            json!({"watchAllNamespaces": false}),
            "/watchAllNamespaces",
        );
        assert_rejected(
            "flux2",
            &flux,
            json!({"rbac": {"create": false}}),
            "/rbac/create",
        );

        // Restating the same value plus allowlisted sizing / log changes is allowed.
        let mut flux = flux;
        apply_management_addon_upgrade(
            "flux2",
            &mut flux,
            Some("2.21.0"),
            &json!({
                "multitenancy": {"enabled": true},
                "logLevel": "debug",
                "kustomizeController": {"resources": {"limits": {"memory": "1Gi"}}}
            }),
        )
        .unwrap();
        assert_eq!(flux.pointer("/spec/values/logLevel").unwrap(), "debug");
        assert_eq!(
            flux.pointer("/spec/values/kustomizeController/resources/limits/memory")
                .unwrap(),
            "1Gi"
        );
    }

    #[test]
    fn kyverno_security_values_cannot_be_changed() {
        let kyverno = helm_release(
            "kyverno",
            json!({
                "crds": {"groups": {"kyverno": {"policies": false}}},
                "admissionController": {"replicas": 1},
                "features": {"policyExceptions": {"enabled": false, "namespace": "keel-system"}}
            }),
        );
        for (values, needle) in [
            (
                json!({"features": {"policyExceptions": {"namespace": "tenant-a"}}}),
                "/features/policyExceptions/namespace",
            ),
            (
                json!({"features": {"policyExceptions": {"enabled": true}}}),
                "/features/policyExceptions/enabled",
            ),
            (
                json!({"features": {"forceFailurePolicyIgnore": {"enabled": true}}}),
                "/features/forceFailurePolicyIgnore",
            ),
            (
                json!({"crds": {"groups": {"policies": {"validatingpolicies": false, "policyexceptions": false}}}}),
                "/crds/groups/policies/validatingpolicies",
            ),
            (
                json!({"admissionController": {"featuresOverride": {"policyExceptions": {"namespace": "*"}}}}),
                "/admissionController/featuresOverride",
            ),
            (
                json!({"admissionController": {"container": {"extraArgs": {"exceptionNamespace": "*"}}}}),
                "/admissionController/container/extraArgs",
            ),
            (
                json!({"backgroundController": {"extraArgs": {"x": "y"}}}),
                "/backgroundController/extraArgs",
            ),
            (
                json!({"config": {"matchConditions": [{"name": "skip"}]}}),
                "/config/matchConditions",
            ),
            (
                json!({"config": {"resourceFiltersInclude": ["[Pod,*,*]"]}}),
                "/config/resourceFiltersInclude",
            ),
            (json!({"config": {"webhooks": []}}), "/config/webhooks"),
        ] {
            assert_rejected("kyverno", &kyverno, values, needle);
        }

        let mut kyverno = kyverno;
        apply_management_addon_upgrade(
            "kyverno",
            &mut kyverno,
            None,
            &json!({
                "admissionController": {"replicas": 3, "container": {"resources": {"limits": {"memory": "512Mi"}}}},
                "cleanupController": {"resources": {"limits": {"memory": "128Mi"}}}
            }),
        )
        .unwrap();
        assert_eq!(
            kyverno
                .pointer("/spec/values/admissionController/replicas")
                .unwrap(),
            3
        );
    }

    #[test]
    fn s3_state_backend_endpoints_cannot_be_changed() {
        let s3 = helm_release(
            "s3-state-backend",
            json!({"s3": {"enabled": true, "enableAuth": true}, "filer": {"replicas": 1}}),
        );
        for (values, needle) in [
            (json!({"s3": {"enableAuth": false}}), "/s3/enableAuth"),
            (json!({"s3": {"enabled": false}}), "/s3/enabled"),
            (
                json!({"filer": {"s3": {"enabled": true, "enableAuth": false}}}),
                "/filer/s3/enabled",
            ),
            (
                json!({"allInOne": {"s3": {"enabled": true}}}),
                "/allInOne/s3/enabled",
            ),
            (json!({"filer": {"replicas": 2}}), "/filer/replicas"),
        ] {
            assert_rejected("s3-state-backend", &s3, values, needle);
        }
        let mut s3 = s3;
        apply_management_addon_upgrade(
            "s3-state-backend",
            &mut s3,
            None,
            &json!({"s3": {"enableAuth": true}, "filer": {"resources": {"requests": {"cpu": "100m"}}}}),
        )
        .unwrap();
    }

    fn forgejo_release(persistence: Value) -> Value {
        helm_release(
            "forgejo",
            json!({
                "gitea": {
                    "admin": {"existingSecret": "keel-forgejo-admin", "email": "admin@keel.local"},
                    "config": {
                        "server": {"ROOT_URL": "https://forgejo.keel.local:8888/"},
                        "oauth2_client": {"ENABLE_AUTO_REGISTRATION": true, "ACCOUNT_LINKING": "auto"}
                    }
                },
                "persistence": persistence
            }),
        )
    }

    #[test]
    fn ephemeral_forgejo_cannot_be_upgraded() {
        // Any pod replacement wipes the emptyDir GitOps repository (`NFR-REL-01`).
        for persistence in [
            json!({"enabled": false}),
            json!({}),
            json!({"enabled": "true"}),
        ] {
            let forgejo = forgejo_release(persistence);
            assert!(ensure_management_addon_upgradable("forgejo", &forgejo).is_err());
            for (version, values) in [
                (Some("17.4.0"), json!({})),
                (None, json!({"resources": {"limits": {"memory": "1Gi"}}})),
                (None, json!({"persistence": {"enabled": true}})),
            ] {
                let mut doc = forgejo.clone();
                let err = apply_management_addon_upgrade("forgejo", &mut doc, version, &values)
                    .expect_err("ephemeral forgejo must not be upgradable");
                assert!(err.to_string().contains("without persistence"), "{err}");
                assert!(err.to_string().contains("forgejo dump"), "{err}");
                assert_eq!(
                    doc, forgejo,
                    "rejected upgrade must not mutate the document"
                );
            }
        }
        // Only Forgejo is gated; other add-ons never carry `persistence`.
        assert!(
            ensure_management_addon_upgradable("flagger", &helm_release("flagger", json!({})))
                .is_ok()
        );
        assert!(
            ensure_management_addon_upgradable(
                "forgejo",
                &forgejo_release(json!({"enabled": true}))
            )
            .is_ok()
        );
    }

    #[test]
    fn forgejo_security_values_cannot_be_changed() {
        let forgejo = forgejo_release(json!({
            "enabled": true,
            "annotations": {"helm.sh/resource-policy": "keep"}
        }));
        for (values, needle) in [
            (
                json!({"gitea": {"admin": {"existingSecret": "attacker"}}}),
                "/gitea/admin/existingSecret",
            ),
            (
                json!({"gitea": {"admin": {"password": "x"}}}),
                "/gitea/admin/password",
            ),
            (
                json!({"gitea": {"config": {"oauth2_client": {"ACCOUNT_LINKING": "login"}}}}),
                "/gitea/config/oauth2_client/ACCOUNT_LINKING",
            ),
            (
                json!({"gitea": {"oauth": [{"name": "evil", "provider": "openidConnect"}]}}),
                "/gitea/oauth",
            ),
            (
                json!({"gitea": {"config": {"server": {"ROOT_URL": "https://evil.example/"}}}}),
                "/gitea/config/server/ROOT_URL",
            ),
            (
                json!({"gitea": {"config": {"service": {"DISABLE_REGISTRATION": false}}}}),
                "/gitea/config/service/DISABLE_REGISTRATION",
            ),
            (
                json!({"persistence": {"existingClaim": "other"}}),
                "/persistence/existingClaim",
            ),
            // Disabling persistence swaps the repository PVC for an emptyDir.
            (
                json!({"persistence": {"enabled": false}}),
                "/persistence/enabled",
            ),
            // `helm.sh/resource-policy: keep` retains the repository PVC.
            (
                json!({"persistence": {"annotations": {"helm.sh/resource-policy": null}}}),
                "/persistence/annotations/helm.sh~1resource-policy",
            ),
            (json!({"extraVolumes": []}), "/extraVolumes"),
            (json!({"deployment": {"env": []}}), "/deployment/env"),
        ] {
            assert_rejected("forgejo", &forgejo, values, needle);
        }
        let mut forgejo = forgejo;
        apply_management_addon_upgrade(
            "forgejo",
            &mut forgejo,
            Some("17.4.0"),
            &json!({
                "gitea": {"admin": {"existingSecret": "keel-forgejo-admin"}},
                "persistence": {"size": "20Gi"},
                "resources": {"limits": {"memory": "1Gi"}}
            }),
        )
        .unwrap();
        assert_eq!(
            forgejo.pointer("/spec/values/persistence/size").unwrap(),
            "20Gi"
        );
        assert_eq!(
            forgejo.pointer("/spec/chart/spec/version").unwrap(),
            "17.4.0"
        );
    }

    #[test]
    fn pko_ca_trust_and_security_values_cannot_be_changed() {
        let doc = helm_release(
            "pulumi-kubernetes-operator",
            json!({
                "replicaCount": 1,
                "extraEnv": [{"name": "SSL_CERT_DIR", "value": "/etc/ssl/certs:/etc/keel-ca"}],
                "extraVolumes": [{"name": "keel-ca", "configMap": {"name": "keel-ca-bundle", "optional": true}}],
                "extraVolumeMounts": [{"name": "keel-ca", "mountPath": "/etc/keel-ca", "readOnly": true}]
            }),
        );
        for (values, needle) in [
            (json!({"extraEnv": []}), "/extraEnv"),
            (
                json!({"extraVolumes": [{"name": "keel-ca", "secret": {"secretName": "keel-ca-secret"}}]}),
                "/extraVolumes",
            ),
            (json!({"extraVolumeMounts": []}), "/extraVolumeMounts"),
            (
                json!({"securityContext": {"allowPrivilegeEscalation": true}}),
                "/securityContext",
            ),
        ] {
            assert_rejected("pulumi-kubernetes-operator", &doc, values, needle);
        }
        let mut ok = doc.clone();
        apply_management_addon_upgrade(
            "pulumi-kubernetes-operator",
            &mut ok,
            None,
            &json!({
                "replicaCount": 2,
                "controller": {"logLevel": "debug"},
                "resources": {"limits": {"memory": "256Mi"}}
            }),
        )
        .unwrap();
        assert_eq!(ok.pointer("/spec/values/replicaCount"), Some(&json!(2)));
    }

    /// Mirrors `extractSchemaDefaults` (`ui/src/lib/schemaBuilder.ts`): fills unset keys with
    /// schema defaults, recursing into object properties and dropping empty nested mappings.
    fn extract_schema_defaults(schema: &Value, base: &Value) -> Value {
        let mut result = base.as_object().cloned().unwrap_or_default();
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            return Value::Object(result);
        };
        for (key, prop) in props {
            if !result.contains_key(key)
                && let Some(default) = prop.get("default")
            {
                result.insert(key.clone(), default.clone());
            } else if prop.get("type") == Some(&json!("object")) && prop.get("properties").is_some()
            {
                let nested = extract_schema_defaults(
                    prop,
                    result
                        .get(key)
                        .filter(|v| v.is_object())
                        .unwrap_or(&Value::Null),
                );
                if nested.as_object().is_some_and(|m| !m.is_empty()) {
                    result.insert(key.clone(), nested);
                }
            }
        }
        Value::Object(result)
    }

    #[test]
    fn pko_upgrade_form_schema_defaults_are_accepted() {
        let app = crate::parse_catalog_application(include_str!(
            "../../../deploy/catalog/core/pulumi-kubernetes-operator/catalog.yaml"
        ))
        .unwrap();
        let seed: Value = include_str!("../../../deploy/seed/core-addons/pko.yaml")
            .split("\n---\n")
            .map(|d| serde_yml::from_str::<Value>(d).unwrap())
            .find(|d| d["kind"] == "HelmRelease")
            .unwrap();
        let live = seed["spec"]["values"].clone();
        // Seeded from the live values (edit) and from nothing (no `initialConfig`).
        for base in [live.clone(), Value::Null] {
            let form = extract_schema_defaults(&app.spec.schema, &base);
            let mut merged = live.clone();
            merge_json_values(&mut merged, &form);
            assert!(
                disallowed_value_changes("pulumi-kubernetes-operator", &live, &merged).is_empty(),
                "{form}"
            );
            // Chart-version-only bump: the dialog still submits the form defaults.
            let mut doc = seed.clone();
            apply_management_addon_upgrade(
                "pulumi-kubernetes-operator",
                &mut doc,
                Some("2.11.0"),
                &form,
            )
            .unwrap();
            assert_eq!(doc["spec"]["values"]["extraEnv"], live["extraEnv"]);
        }
        // Every key the schema offers is upgradable.
        let mut doc = seed.clone();
        apply_management_addon_upgrade(
            "pulumi-kubernetes-operator",
            &mut doc,
            None,
            &json!({"replicaCount": 2, "controller": {"logLevel": "debug"}}),
        )
        .unwrap();
    }

    #[test]
    fn unrestricted_addons_accept_any_values() {
        assert!(management_addon_mutable_values("flagger").is_none());
        let mut doc = helm_release("flagger", json!({}));
        apply_management_addon_upgrade(
            "flagger",
            &mut doc,
            None,
            &json!({"meshProvider": "gatewayapi:v1"}),
        )
        .unwrap();
    }

    #[test]
    fn changed_value_paths_reports_leaves_and_replaced_subtrees() {
        let before = json!({"a": {"b": 1, "c": [1]}, "d": {"e/f": true}});
        let after = json!({"a": {"b": 2, "c": [1]}, "d": null, "g": 1});
        assert_eq!(changed_value_paths(&before, &after), ["/a/b", "/d", "/g"]);
        assert!(changed_value_paths(&before, &before).is_empty());
        assert_eq!(
            changed_value_paths(&json!({"d": {"e/f": true}}), &json!({"d": {"e/f": false}})),
            ["/d/e~1f"]
        );
        // A null values tree is treated as empty for policy checks.
        assert!(disallowed_value_changes("flux2", &Value::Null, &json!({})).is_empty());
        assert_eq!(
            disallowed_value_changes(
                "flux2",
                &json!({"multitenancy": {"enabled": true}}),
                &Value::Null
            ),
            ["/multitenancy/enabled"]
        );
    }

    #[test]
    fn namespace_less_flux_objects_are_not_addons() {
        let mut hr = helm_release("flagger", json!({}));
        hr["metadata"].as_object_mut().unwrap().remove("namespace");
        assert!(management_addon_ref(&hr).is_none());
        let mut ks = flux_kustomization("rauthy");
        ks["metadata"].as_object_mut().unwrap().remove("namespace");
        assert!(management_addon_ref(&ks).is_none());
    }

    #[test]
    fn kustomization_upgrade_rejects_digest_pinned_images() {
        let mut doc = flux_kustomization("rauthy");
        doc["spec"]["images"][1]["digest"] = json!("sha256:abc");
        let err = apply_management_addon_upgrade("rauthy", &mut doc, Some("0.38.0"), &Value::Null)
            .unwrap_err();
        assert!(err.to_string().contains("digest"), "{err}");
        assert!(err.to_string().contains("rauthy-init"), "{err}");
    }

    #[test]
    fn document_validation_and_protection() {
        let mut doc = helm_release("flagger", json!({}));
        validate_management_addon_document("flagger", &doc).unwrap();
        assert!(validate_management_addon_document("other", &doc).is_err());
        doc["spec"]["chart"]["spec"]["version"] = json!(">=1.0.0");
        assert!(validate_management_addon_document("flagger", &doc).is_err());
        let mut ks = flux_kustomization("rauthy");
        validate_management_addon_document("rauthy", &ks).unwrap();
        ks["spec"]["images"][0]["newTag"] = json!("latest");
        assert!(validate_management_addon_document("rauthy", &ks).is_err());

        let mut unlabelled = helm_release("flagger", json!({}));
        ensure_management_addon_protected(&mut unlabelled).unwrap();
        assert_eq!(
            unlabelled
                .pointer("/metadata/labels/keel.io~1protect")
                .unwrap(),
            "true"
        );
        assert!(ensure_management_addon_protected(&mut json!([])).is_err());
    }

    const KYVERNO_DOC: &str = r#"# Kyverno admission controller.
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: kyverno
  namespace: keel-system
  labels:
    keel.io/protect: "true"
spec:
  interval: 10m
  chart:
    spec:
      chart: kyverno
      version: "3.10.0" # pinned to the Stage 0 version
      sourceRef:
        kind: HelmRepository
        name: kyverno
  values:
    # CRD groups explanation
    crds:
      groups:
        kyverno:
          policies: false
    notes: |
      version: not-a-key
    features:
      policyExceptions:
        enabled: true # keel:policy-exceptions
        namespace: keel-system
"#;

    #[test]
    fn version_only_upgrade_keeps_key_order_and_comments() {
        let mut doc: Value = serde_yml::from_str(KYVERNO_DOC).unwrap();
        apply_management_addon_upgrade("kyverno", &mut doc, Some("3.11.0"), &Value::Null).unwrap();
        let rendered = render_management_addon_document(KYVERNO_DOC, &doc).unwrap();
        assert_eq!(
            rendered,
            KYVERNO_DOC.replace("\"3.10.0\" # pinned", "\"3.11.0\" # pinned")
        );

        // Unchanged documents are returned verbatim.
        let same: Value = serde_yml::from_str(KYVERNO_DOC).unwrap();
        assert_eq!(
            render_management_addon_document(KYVERNO_DOC, &same).unwrap(),
            KYVERNO_DOC
        );

        // Values changes fall back to re-serialization but keep the header comment.
        let mut doc: Value = serde_yml::from_str(KYVERNO_DOC).unwrap();
        apply_management_addon_upgrade(
            "kyverno",
            &mut doc,
            None,
            &json!({"admissionController": {"replicas": 3}}),
        )
        .unwrap();
        let rendered = render_management_addon_document(KYVERNO_DOC, &doc).unwrap();
        assert!(rendered.starts_with("# Kyverno admission controller.\n"));
        assert_eq!(serde_yml::from_str::<Value>(&rendered).unwrap(), doc);
    }

    #[test]
    fn kustomization_version_upgrade_edits_every_new_tag_in_place() {
        let text = "apiVersion: kustomize.toolkit.fluxcd.io/v1\nkind: Kustomization\nmetadata:\n  name: rauthy\n  namespace: flux-system\n  labels:\n    keel.io/protect: \"true\"\nspec:\n  path: ./clusters/management/addons/rauthy # plain manifests\n  images:\n  - name: ghcr.io/sebadob/rauthy\n    newTag: 0.37.2 # app\n  - name: ghcr.io/sebadob/rauthy-init\n    newTag: '0.37.2'\n";
        let mut doc: Value = serde_yml::from_str(text).unwrap();
        apply_management_addon_upgrade("rauthy", &mut doc, Some("0.38.0"), &Value::Null).unwrap();
        let rendered = render_management_addon_document(text, &doc).unwrap();
        assert_eq!(
            rendered,
            text.replace("newTag: 0.37.2 # app", "newTag: \"0.38.0\" # app")
                .replace("newTag: '0.37.2'", "newTag: \"0.38.0\"")
        );
    }

    #[test]
    fn kustomization_upgrade_sets_every_image_tag_and_rejects_values() {
        let mut doc = flux_kustomization("rauthy");
        apply_management_addon_upgrade("rauthy", &mut doc, Some("0.38.0"), &Value::Null).unwrap();
        let images = doc.pointer("/spec/images").unwrap().as_array().unwrap();
        assert!(images.iter().all(|i| i["newTag"] == "0.38.0"));
        assert_eq!(images[0]["name"], "ghcr.io/sebadob/rauthy");

        let err = apply_management_addon_upgrade(
            "rauthy",
            &mut doc,
            Some("0.38.1"),
            &json!({"replicas": 3}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Kustomization"), "{err}");
    }

    #[test]
    fn kustomization_upgrade_requires_images() {
        let mut doc = flux_kustomization("rauthy");
        doc["spec"].as_object_mut().unwrap().remove("images");
        let err = apply_management_addon_upgrade("rauthy", &mut doc, Some("0.38.0"), &Value::Null)
            .unwrap_err();
        assert!(err.to_string().contains("spec.images"), "{err}");

        let mut malformed = flux_kustomization("rauthy");
        malformed["spec"]["images"] = json!(["ghcr.io/sebadob/rauthy"]);
        assert!(
            apply_management_addon_upgrade("rauthy", &mut malformed, Some("0.38.0"), &Value::Null)
                .is_err()
        );
    }
}
