//! Name-based secret heuristic for values the catalog schema does not declare secret (AST-2,
//! `NFR-SEC-05`, `SDD.md` §6.3 "Zero Plaintext in Git").
//!
//! Catalog schemas mark secrets with `writeOnly: true` / `format: "password"` (see
//! [`crate::SchemaResolver`]). Values the schema does not describe that way (an upstream-derived
//! schema, an open `additionalProperties` Helm app, a `--values-file` key the form never showed)
//! would otherwise be committed to Git in plaintext. This module treats such a value as secret
//! when its **key name** looks like one, so it follows the same SOPS pipeline as declared
//! secrets, and reports every decision as a [`SecretNotice`] (never invisibly).
//!
//! # Who decides
//!
//! * Declared secrets (`writeOnly: true` / `format: "password"`) are always secret; a user can
//!   never mark them not secret ([`validate_non_secret_paths`]).
//! * A catalog author opts a field out of the heuristic with `writeOnly: false` (only that: an
//!   `enum`/`const` in one `anyOf` branch, such as "empty or a strong password", is no opt-out).
//! * A user opts a heuristic match out per instance with a "not secret" dot path
//!   (`nonSecretPaths`, `keel app install --not-secret`), persisted next to the instance in
//!   [`SECRET_OVERRIDES_FILE`] so edits re-apply it.
//!
//! # Key rule ([`looks_like_secret_key`])
//!
//! A key containing `/` is a Kubernetes annotation or label key
//! (`nginx.ingress.kubernetes.io/auth-secret`, which holds a Secret name) and never matches;
//! neither does any key directly under an `annotations`/`labels` map (`podAnnotations`,
//! `commonLabels`, ...: [`looks_like_secret_key_under`]). A dotted key (`ssl.keystore.password`
//! in a flat Kafka/Spring property map) is judged by its last dot segment.
//!
//! That segment is reduced to its ASCII letters and digits, lowercased (`DB_PASSWORD` ->
//! `dbpassword`, `apiKey` -> `apikey`). It looks like a secret when it contains one of
//! [`SECRET_KEY_WORDS`] or ends with `pass` (`DB_PASS`, `smtpPass`; not [`PASS_NON_SECRETS`]
//! such as `bypass`), unless it is a *reference* to a secret rather than the secret itself:
//!
//! * it starts with `existing` (`existingSecret`, `existingSecretPasswordKey`);
//! * it ends with one of [`REFERENCE_SUFFIXES`] (`secretName`, `secretKeyRef`, `passwordFile`,
//!   `privateKeyPath`, `tokenUrl`, `tokenTTL`, `passwordMode`, `tokenExpirySeconds`, ...);
//! * it ends with `key` naming a key *inside* a Secret: the rest ends with one of
//!   [`KEY_NAME_SUBJECTS`] (`passwordKey`, `adminPasswordKey`, `tokenKey`), or with `secret`
//!   preceded by a secret word (`passwordSecretKey`). A bare `secretKey`/`awsSecretKey` (MinIO,
//!   S3) is the secret itself and stays secret.
//!
//! # Value rule ([`heuristic_secret_candidates`])
//!
//! A value is a heuristic secret when the schema does not already make it (or an enclosing
//! value) secret, its key looks like a secret, no applying schema says `writeOnly: false`, and
//! it is a non-empty string or a number (YAML reads `password: 123456` as a number), or a list of
//! strings/numbers with at least one non-empty item (`apiTokens: [..]`, matched as a whole).
//! Booleans are flags (`automountServiceAccountToken: true`), never secrets.
//!
//! [`partition_values_with_heuristics`] rejects (fails closed on) a match it cannot store as its
//! own Helm values fragment, until the user marks it not secret or the catalog author marks it
//! (or the enclosing list) `writeOnly`: values inside list items (`users[].password`, matched
//! with their list path) and values under a key containing `.` (`config.ssl.keystore.password`:
//! a dot path cannot address it).
//!
//! The UI mirrors both rules; `ui/src/test/fixtures/secret-key-heuristic.json` is the shared
//! corpus both test suites run.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::CoreError;
use crate::catalog::{PartitionedValues, partition_values_by_schema};
use crate::secret_schema::{SchemaResolver, SecretValueLocation, escape_pointer_token};

/// Words that make a key look like it holds a secret (matched against the normalized key).
pub const SECRET_KEY_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "credential",
    "privatekey",
    "signingkey",
    "encryptionkey",
];

/// Normalized key endings in `pass` that are ordinary words, not a password.
pub const PASS_NON_SECRETS: &[&str] = &[
    "bypass",
    "compass",
    "overpass",
    "underpass",
    "surpass",
    "trespass",
    "onepass",
    "multipass",
];

/// Normalized key endings that name, locate or describe a secret instead of holding it.
pub const REFERENCE_SUFFIXES: &[&str] = &[
    // Names and references of Kubernetes objects / Secret keys.
    "name",
    "names",
    "ref",
    "refs",
    "selector",
    "namespace",
    "store",
    "class",
    "provider",
    "backend",
    "version",
    // Files and mounts.
    "file",
    "files",
    "filename",
    "path",
    "paths",
    "dir",
    "directory",
    "mount",
    // Endpoints.
    "url",
    "uri",
    "endpoint",
    "header",
    "issuer",
    "audience",
    // Settings about the secret.
    "type",
    "kind",
    "mode",
    "policy",
    "format",
    "length",
    "ttl",
    "expiry",
    "expiration",
    "lifetime",
    "duration",
    "timeout",
    "interval",
    "seconds",
    "secs",
    "minutes",
    "hours",
    "days",
    "count",
    "size",
    "limit",
    "port",
    "enabled",
    "prefix",
    "suffix",
    "template",
    // Not a token at all (ML model tokenizers).
    "tokenizer",
];

/// What precedes a trailing `key` when the key names an entry inside a Secret
/// (`passwordKey`, `tokenKey`, `credentialsKey`).
pub const KEY_NAME_SUBJECTS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "token",
    "credential",
    "credentials",
    "apikey",
];

/// Per-instance file (next to `values.yaml`) persisting the user's "not secret" overrides:
/// `{"nonSecretPaths": ["a.b"]}`. JSON, so no Kustomize/Flux tooling mistakes it for a manifest.
pub const SECRET_OVERRIDES_FILE: &str = "secret-overrides.json";

/// Most "not secret" paths a request may carry.
const MAX_NON_SECRET_PATHS: usize = 256;
/// Longest accepted "not secret" path.
const MAX_NON_SECRET_PATH_LEN: usize = 512;

/// The key reduced to lowercase ASCII letters and digits (`DB_PASSWORD` -> `dbpassword`).
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether the normalized key names a secret word, or ends in `pass` (`dbpass`).
fn has_secret_word(n: &str) -> bool {
    SECRET_KEY_WORDS.iter().any(|w| n.contains(w))
        || (n.ends_with("pass") && !PASS_NON_SECRETS.iter().any(|w| n.ends_with(w)))
}

/// Whether a value under `key` looks like a secret by name (see the module docs for the rules
/// and their exemptions). Ignores the parent map: see [`looks_like_secret_key_under`].
pub fn looks_like_secret_key(key: &str) -> bool {
    if key.contains('/') {
        return false;
    }
    let last = key.rsplit('.').next().unwrap_or(key);
    let n = normalize_key(last);
    if !has_secret_word(&n) {
        return false;
    }
    if n.starts_with("existing") || REFERENCE_SUFFIXES.iter().any(|s| n.ends_with(s)) {
        return false;
    }
    if let Some(rest) = n.strip_suffix("key") {
        if KEY_NAME_SUBJECTS.iter().any(|s| rest.ends_with(s)) {
            return false;
        }
        if let Some(before) = rest.strip_suffix("secret")
            && has_secret_word(before)
        {
            return false;
        }
    }
    true
}

/// Whether a map named `parent` holds annotations or labels (`annotations`, `podAnnotations`,
/// `commonLabels`): its keys are Kubernetes metadata keys and its values Secret names at most.
fn is_metadata_map(parent: &str) -> bool {
    let n = normalize_key(parent);
    n.ends_with("annotations") || n.ends_with("labels")
}

/// [`looks_like_secret_key`] for a key of the map under the key `parent` (`None`: the root or a
/// list item): never for keys of an annotations/labels map.
pub fn looks_like_secret_key_under(parent: Option<&str>, key: &str) -> bool {
    !parent.is_some_and(is_metadata_map) && looks_like_secret_key(key)
}

/// Why a value is reported in [`SecretNotice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretNoticeReason {
    /// The key looks like a secret: the value is stored encrypted with SOPS.
    HeuristicSecret,
    /// The key looks like a secret, but the user marked it not secret: stored in plaintext.
    MarkedNotSecret,
}

/// A secret-handling decision the user should see (never carries the value).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretNotice {
    /// Dot path of the value (`auth.password`, list items as `users[].password`).
    pub path: String,
    pub reason: SecretNoticeReason,
    /// Human-readable explanation.
    pub message: String,
}

impl SecretNotice {
    /// A value treated as secret because of its key name.
    pub fn heuristic_secret(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: SecretNoticeReason::HeuristicSecret,
            message: "looks like a secret; stored encrypted with SOPS".to_string(),
        }
    }

    /// A heuristic match the user marked not secret.
    pub fn marked_not_secret(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: SecretNoticeReason::MarkedNotSecret,
            message: "marked not-secret by you; stored in plaintext".to_string(),
        }
    }
}

/// Whether the catalog author opted the location out of the heuristic (`writeOnly: false` in
/// an applying schema, and no applying schema marks it secret). An `enum`/`const` is no opt-out:
/// in one `anyOf` branch (`anyOf: [{const: ""}, {minLength: 8}]`) it does not constrain the value.
fn declared_not_secret<'a>(resolver: &SchemaResolver<'a>, set: &[&'a Value]) -> bool {
    resolver.is_declared_not_secret(set)
}

/// Whether `value` is a scalar the heuristic stores as a secret: a non-empty string or a number.
fn is_secret_scalar(value: &Value) -> bool {
    match value {
        Value::String(s) => !s.is_empty(),
        Value::Number(_) => true,
        _ => false,
    }
}

fn join_dot(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn collect_candidates<'a>(
    resolver: &SchemaResolver<'a>,
    set: Vec<&'a Value>,
    value: &Value,
    location: (&str, &str),
    key: (Option<&str>, Option<&str>),
    out: &mut Vec<SecretValueLocation>,
) {
    let (pointer, path) = location;
    let (parent, key) = key;
    if resolver.is_secret(&set) {
        return;
    }
    let named_secret = || {
        key.is_some_and(|k| looks_like_secret_key_under(parent, k))
            && !declared_not_secret(resolver, &set)
    };
    let push = |out: &mut Vec<SecretValueLocation>| {
        out.push(SecretValueLocation {
            pointer: pointer.to_string(),
            path: path.to_string(),
        });
    };
    match value {
        Value::String(_) | Value::Number(_) if is_secret_scalar(value) && named_secret() => {
            push(out)
        }
        Value::Array(items)
            if !items.is_empty()
                && items.iter().all(|v| v.is_string() || v.is_number())
                && items.iter().any(is_secret_scalar)
                && named_secret() =>
        {
            push(out)
        }
        Value::Object(map) => {
            for (child_key, child) in map {
                let child_set = resolver.property_schemas(&set, child_key);
                collect_candidates(
                    resolver,
                    child_set,
                    child,
                    (
                        &format!("{pointer}/{}", escape_pointer_token(child_key)),
                        &join_dot(path, child_key),
                    ),
                    (key, Some(child_key)),
                    out,
                );
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let item_set = resolver.item_schemas(&set, Some(i));
                collect_candidates(
                    resolver,
                    item_set,
                    item,
                    (&format!("{pointer}/{i}"), &format!("{path}[]")),
                    (None, None),
                    out,
                );
            }
        }
        _ => {}
    }
}

/// Every value of `values` the name heuristic treats as secret (see the module docs), sorted.
/// Values `schema` already makes secret are not listed. User overrides are not applied here.
pub fn heuristic_secret_candidates(schema: &Value, values: &Value) -> Vec<SecretValueLocation> {
    let resolver = SchemaResolver::new(schema);
    let mut out = Vec::new();
    collect_candidates(
        &resolver,
        vec![schema],
        values,
        ("", ""),
        (None, None),
        &mut out,
    );
    out.sort();
    out
}

/// JSON Pointers of the [`heuristic_secret_candidates`]: the `extra_secret_pointers` that keep
/// schema validation errors from echoing them ([`crate::validate_schema_values_with_secrets`]).
pub fn heuristic_secret_pointers(schema: &Value, values: &Value) -> Vec<String> {
    heuristic_secret_candidates(schema, values)
        .into_iter()
        .map(|l| l.pointer)
        .collect()
}

/// Normalizes and checks user "not secret" `paths` (dot paths, list items as `list[]`): trimmed,
/// deduplicated and sorted. Fails for a path the catalog schema declares secret (`writeOnly:
/// true` / `format: "password"` on the field or an enclosing one): only heuristic matches can be
/// overridden.
pub fn validate_non_secret_paths(
    schema: &Value,
    paths: &[String],
) -> Result<Vec<String>, CoreError> {
    if paths.len() > MAX_NON_SECRET_PATHS {
        return Err(CoreError::Validation(format!(
            "At most {MAX_NON_SECRET_PATHS} not-secret paths are accepted"
        )));
    }
    let resolver = SchemaResolver::new(schema);
    let mut out = BTreeSet::new();
    for raw in paths {
        let path = raw.trim();
        if path.is_empty()
            || path.len() > MAX_NON_SECRET_PATH_LEN
            || path.chars().any(char::is_control)
        {
            return Err(CoreError::Validation(format!(
                "Invalid not-secret path '{}': expected a non-empty dot path such as auth.password",
                path.escape_default()
            )));
        }
        if resolver.is_secret_path(path) {
            return Err(CoreError::Validation(format!(
                "'{path}' is declared secret by the catalog (writeOnly / format: password) and \
                 cannot be marked not secret"
            )));
        }
        out.insert(path.to_string());
    }
    Ok(out.into_iter().collect())
}

/// [`partition_values_by_schema`] plus the name heuristic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeuristicPartition {
    /// Declared and heuristic secrets in `secret_values` (their paths in `secret_field_paths`).
    pub partitioned: PartitionedValues,
    /// One notice per heuristic match (secret, or marked not secret by the user), sorted.
    pub notices: Vec<SecretNotice>,
    /// The user's not-secret paths that matched a heuristic candidate: the overrides to persist.
    pub applied_non_secret_paths: Vec<String>,
}

/// Removes the value at JSON Pointer `pointer` (objects only), pruning objects it leaves empty.
fn take_at_pointer(root: &mut Value, tokens: &[String]) -> Option<Value> {
    let (first, rest) = tokens.split_first()?;
    let map = root.as_object_mut()?;
    if rest.is_empty() {
        return map.remove(first);
    }
    let child = map.get_mut(first)?;
    let taken = take_at_pointer(child, rest);
    if child.as_object().is_some_and(Map::is_empty) {
        map.remove(first);
    }
    taken
}

fn insert_at_pointer(root: &mut Value, tokens: &[String], value: Value) {
    let Some((first, rest)) = tokens.split_first() else {
        return;
    };
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let map = root.as_object_mut().expect("object");
    if rest.is_empty() {
        map.insert(first.clone(), value);
    } else {
        let child = map
            .entry(first.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        insert_at_pointer(child, rest, value);
    }
}

fn pointer_tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Partitions `values` like [`partition_values_by_schema`], then moves every heuristic secret
/// ([`heuristic_secret_candidates`]) not listed in `non_secret_paths` (already
/// [`validate_non_secret_paths`]-checked) from `normal_values` to `secret_values`, so it is
/// encrypted with SOPS like a declared secret.
///
/// Fails closed for a heuristic secret it cannot store as its own Helm values fragment: inside a
/// list item (`users[].password`), or under a key containing `.` (`config.ssl.keystore.password`
/// in a flat property map; a dot path cannot address it). The user marks it not secret, or the
/// catalog author marks it (or the enclosing list or map) `writeOnly`.
pub fn partition_values_with_heuristics(
    schema: &Value,
    values: &Value,
    non_secret_paths: &[String],
) -> Result<HeuristicPartition, CoreError> {
    let mut partitioned = partition_values_by_schema(schema, values)?;
    let overrides: BTreeSet<&str> = non_secret_paths.iter().map(String::as_str).collect();
    let mut notices = Vec::new();
    let mut applied = BTreeSet::new();
    let mut unfragmentable = Vec::new();
    let mut paths: BTreeSet<String> = partitioned.secret_field_paths.drain(..).collect();
    for candidate in heuristic_secret_candidates(schema, values) {
        if overrides.contains(candidate.path.as_str()) {
            applied.insert(candidate.path.clone());
            notices.push(SecretNotice::marked_not_secret(&candidate.path));
            continue;
        }
        let tokens = pointer_tokens(&candidate.pointer);
        if candidate.path.contains('[') || tokens.iter().any(|t| t.contains('.')) {
            unfragmentable.push(candidate.path);
            continue;
        }
        if let Some(value) = take_at_pointer(&mut partitioned.normal_values, &tokens) {
            insert_at_pointer(&mut partitioned.secret_values, &tokens, value);
        }
        notices.push(SecretNotice::heuristic_secret(&candidate.path));
        paths.insert(candidate.path);
    }
    if !unfragmentable.is_empty() {
        unfragmentable.sort();
        unfragmentable.dedup();
        return Err(CoreError::Validation(format!(
            "Values that look like secrets are inside list items or under keys containing '.', \
             which Keel cannot store encrypted on their own: {}. Mark them not secret \
             (nonSecretPaths / --not-secret) if they are not secrets, or ask the catalog author \
             to mark them (or the enclosing list or map) writeOnly",
            unfragmentable.join(", ")
        )));
    }
    partitioned.secret_field_paths = paths.into_iter().collect();
    notices.sort();
    notices.dedup();
    Ok(HeuristicPartition {
        partitioned,
        notices,
        applied_non_secret_paths: applied.into_iter().collect(),
    })
}

/// The `stored_keys` (dot paths of an instance's stored SOPS secret keys) that are heuristic
/// secrets: not declared secret by `schema` (those are kept by
/// [`crate::stored_secret_paths_still_secret`]), last segment looks like a secret, not opted out
/// by the catalog author, and not in `non_secret_paths`. An edit that leaves one blank keeps the
/// stored ciphertext, like a declared secret.
pub fn stored_heuristic_secret_paths<'p>(
    schema: &Value,
    stored_keys: impl IntoIterator<Item = &'p String>,
    non_secret_paths: &[String],
) -> Vec<String> {
    let resolver = SchemaResolver::new(schema);
    stored_keys
        .into_iter()
        .filter(|k| !k.is_empty() && !k.contains('['))
        .filter(|k| !non_secret_paths.contains(k))
        .filter(|k| !resolver.is_secret_path(k))
        .filter(|k| {
            let mut segments = k.rsplit('.');
            let key = segments.next().unwrap_or_default();
            looks_like_secret_key_under(segments.next(), key)
        })
        .filter(|k| !declared_not_secret(&resolver, &resolver.schemas_at_path(k)))
        .cloned()
        .collect()
}

/// Whether `value` is blank: absent, `null`, `""`, `[]` or `{}`.
fn is_blank(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        Some(_) => false,
    }
}

/// The `non_secret_paths` that would turn a stored heuristic secret (one of `stored_keys`,
/// otherwise kept encrypted by [`stored_heuristic_secret_paths`]) into plaintext while `values`
/// leaves it blank. An edit form shows a stored secret blank, so marking it not secret without
/// re-entering it would silently replace the stored value by nothing: the caller rejects it.
pub fn overridden_stored_secrets_left_blank<'p>(
    schema: &Value,
    stored_keys: impl IntoIterator<Item = &'p String>,
    non_secret_paths: &[String],
    values: &Value,
) -> Vec<String> {
    stored_heuristic_secret_paths(schema, stored_keys, &[])
        .into_iter()
        .filter(|path| non_secret_paths.contains(path))
        .filter(|path| {
            let pointer: String = path
                .split('.')
                .map(|t| format!("/{}", escape_pointer_token(t)))
                .collect();
            is_blank(values.pointer(&pointer))
        })
        .collect()
}

/// Returns `values` without the blank (`null`, `""`, `[]`, `{}`) values at the dot `paths`
/// (objects only; parents left empty are kept): an edit leaving a stored heuristic secret blank
/// keeps the stored value.
pub fn drop_blank_values_at_paths(values: &Value, paths: &[String]) -> Value {
    let mut out = values.clone();
    for path in paths {
        let tokens: Vec<&str> = path.split('.').collect();
        let Some((last, parents)) = tokens.split_last() else {
            continue;
        };
        let parent_pointer: String = parents
            .iter()
            .map(|t| format!("/{}", escape_pointer_token(t)))
            .collect();
        let Some(node) = out.pointer_mut(&parent_pointer) else {
            continue;
        };
        let Some(map) = node.as_object_mut() else {
            continue;
        };
        if map.contains_key(*last) && is_blank(map.get(*last)) {
            map.remove(*last);
        }
    }
    out
}

/// Returns `values` (an instance's values as read back from Git) without any value the effective
/// secret rules make secret: schema-declared secrets ([`SchemaResolver::secret_value_locations`])
/// and name-heuristic secrets ([`heuristic_secret_candidates`]) not listed in
/// `non_secret_paths` (list items included). Secret values are omitted, never replaced, so an
/// edit form renders them blank and a blank secret keeps its stored ciphertext (AST-1).
///
/// Writers never commit a secret to `values.yaml`; this read-time mask also covers a secret
/// committed by hand or a field a catalog schema declared secret only later. A user override
/// for a path the schema declares secret has no effect.
pub fn mask_secret_values(schema: &Value, values: &Value, non_secret_paths: &[String]) -> Value {
    let resolver = SchemaResolver::new(schema);
    let overrides: BTreeSet<&str> = non_secret_paths.iter().map(String::as_str).collect();
    let masked: BTreeSet<String> = resolver
        .secret_value_locations(values)
        .into_iter()
        .chain(
            heuristic_secret_candidates(schema, values)
                .into_iter()
                .filter(|l| !overrides.contains(l.path.as_str())),
        )
        .map(|l| l.pointer)
        .collect();
    if masked.contains("") {
        return Value::Object(Map::new());
    }
    fn strip(value: &Value, pointer: &str, masked: &BTreeSet<String>) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter_map(|(key, child)| {
                        let child_pointer = format!("{pointer}/{}", escape_pointer_token(key));
                        (!masked.contains(&child_pointer))
                            .then(|| (key.clone(), strip(child, &child_pointer, masked)))
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| {
                        let item_pointer = format!("{pointer}/{i}");
                        (!masked.contains(&item_pointer))
                            .then(|| strip(item, &item_pointer, masked))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    strip(values, "", &masked)
}

/// Parses a [`SECRET_OVERRIDES_FILE`] (`{"nonSecretPaths": [...]}`); malformed content counts
/// as no overrides.
pub fn parse_secret_overrides(raw: &str) -> Vec<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct File {
        #[serde(default)]
        non_secret_paths: Vec<String>,
    }
    serde_json::from_str::<File>(raw)
        .map(|f| f.non_secret_paths)
        .unwrap_or_default()
}

/// Renders a [`SECRET_OVERRIDES_FILE`] for `paths`.
pub fn render_secret_overrides(paths: &[String]) -> String {
    let mut raw = serde_json::to_string_pretty(&serde_json::json!({ "nonSecretPaths": paths }))
        .unwrap_or_default();
    raw.push('\n');
    raw
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CORPUS: &str = include_str!("../../../ui/src/test/fixtures/secret-key-heuristic.json");

    fn corpus() -> Value {
        serde_json::from_str(CORPUS).expect("corpus is JSON")
    }

    #[test]
    fn shared_corpus_key_cases() {
        let corpus = corpus();
        let cases = corpus["keys"].as_array().expect("keys");
        assert!(cases.len() > 50, "corpus covers real chart keys");
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|case| {
                let key = case["key"].as_str().expect("key");
                let expected = case["secret"].as_bool().expect("secret");
                (looks_like_secret_key(key) != expected)
                    .then(|| format!("{key:?}: expected secret={expected}"))
            })
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn shared_corpus_value_cases() {
        let corpus = corpus();
        for case in corpus["values"].as_array().expect("values") {
            let got: Vec<String> = heuristic_secret_candidates(&case["schema"], &case["values"])
                .into_iter()
                .map(|l| l.path)
                .collect();
            let expected: Vec<String> = case["expected"]
                .as_array()
                .expect("expected")
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(got, expected, "{}", case["name"]);
        }
    }

    #[test]
    fn candidates_follow_the_schema_resolver() {
        // `$ref`, patternProperties and combinators resolve like for declared secrets: a `$ref`
        // to a writeOnly:false definition opts the field out, a pattern-declared secret is not a
        // heuristic match.
        let schema = json!({
            "$defs": { "plain": { "type": "string", "writeOnly": false } },
            "properties": {
                "tlsSecret": { "$ref": "#/$defs/plain" },
                "opts": {
                    "patternProperties": { "^.*Token$": { "format": "password" } }
                }
            }
        });
        let values = json!({
            "tlsSecret": "my-tls",
            "opts": { "apiToken": "a", "webhookSecret": "w" }
        });
        let got = heuristic_secret_candidates(&schema, &values);
        assert_eq!(
            got,
            vec![SecretValueLocation {
                pointer: "/opts/webhookSecret".into(),
                path: "opts.webhookSecret".into()
            }]
        );
    }

    #[test]
    fn partition_moves_heuristic_secrets_and_reports_them() {
        let schema = json!({ "type": "object" });
        let values = json!({
            "auth": { "password": "hunter2", "username": "admin" },
            "cloudflare": { "apiToken": "cf" },
            "replicas": 2
        });
        let out = partition_values_with_heuristics(&schema, &values, &[]).unwrap();
        assert_eq!(
            out.partitioned.normal_values,
            json!({ "auth": { "username": "admin" }, "replicas": 2 })
        );
        assert_eq!(
            out.partitioned.secret_values,
            json!({ "auth": { "password": "hunter2" }, "cloudflare": { "apiToken": "cf" } })
        );
        assert_eq!(
            out.partitioned.secret_field_paths,
            vec![
                "auth.password".to_string(),
                "cloudflare.apiToken".to_string()
            ]
        );
        assert_eq!(
            out.notices,
            vec![
                SecretNotice::heuristic_secret("auth.password"),
                SecretNotice::heuristic_secret("cloudflare.apiToken"),
            ]
        );
        assert!(out.applied_non_secret_paths.is_empty());
        // Notices never carry the value.
        let rendered = serde_json::to_string(&out.notices).unwrap();
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("\"cf\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("\"reason\":\"heuristicSecret\""),
            "{rendered}"
        );
    }

    #[test]
    fn partition_keeps_declared_secrets_and_merges_paths() {
        let schema = json!({
            "properties": {
                "smtp": {
                    "properties": { "password": { "type": "string", "writeOnly": true } }
                }
            }
        });
        let values = json!({ "smtp": { "password": "p", "token": "t", "host": "h" } });
        let out = partition_values_with_heuristics(&schema, &values, &[]).unwrap();
        assert_eq!(
            out.partitioned.normal_values,
            json!({ "smtp": { "host": "h" } })
        );
        assert_eq!(
            out.partitioned.secret_values,
            json!({ "smtp": { "password": "p", "token": "t" } })
        );
        assert_eq!(
            out.partitioned.secret_field_paths,
            vec!["smtp.password".to_string(), "smtp.token".to_string()]
        );
        // Only the heuristic match gets a notice.
        assert_eq!(
            out.notices,
            vec![SecretNotice::heuristic_secret("smtp.token")]
        );
    }

    #[test]
    fn user_overrides_keep_values_plaintext_and_are_reported() {
        let schema = json!({ "type": "object" });
        let values = json!({ "ingress": { "tlsSecret": "my-tls" }, "token": "t" });
        let overrides = vec![
            "ingress.tlsSecret".to_string(),
            "unrelated.path".to_string(),
        ];
        let out = partition_values_with_heuristics(&schema, &values, &overrides).unwrap();
        assert_eq!(
            out.partitioned.normal_values,
            json!({ "ingress": { "tlsSecret": "my-tls" } })
        );
        assert_eq!(out.partitioned.secret_values, json!({ "token": "t" }));
        assert_eq!(
            out.applied_non_secret_paths,
            vec!["ingress.tlsSecret".to_string()]
        );
        assert_eq!(
            out.notices,
            vec![
                SecretNotice::marked_not_secret("ingress.tlsSecret"),
                SecretNotice::heuristic_secret("token"),
            ]
        );
    }

    #[test]
    fn list_item_matches_fail_closed_unless_overridden() {
        let schema = json!({ "type": "object" });
        let values = json!({ "users": [{ "name": "a", "password": "p" }] });
        let err = partition_values_with_heuristics(&schema, &values, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("users[].password"), "{msg}");
        assert!(!msg.contains("\"p\""), "{msg}");
        let out =
            partition_values_with_heuristics(&schema, &values, &["users[].password".to_string()])
                .unwrap();
        assert_eq!(out.partitioned.normal_values, values);
        // A whole list of tokens is one fragment-able value.
        let out =
            partition_values_with_heuristics(&schema, &json!({ "apiTokens": ["a"] }), &[]).unwrap();
        assert_eq!(out.partitioned.secret_values, json!({ "apiTokens": ["a"] }));
    }

    #[test]
    fn declared_secrets_cannot_be_marked_not_secret() {
        let schema = json!({
            "$defs": { "pw": { "type": "string", "format": "password" } },
            "properties": {
                "admin": { "properties": { "password": { "$ref": "#/$defs/pw" } } },
                "smtp": { "type": "object", "writeOnly": true }
            }
        });
        for path in ["admin.password", "smtp.password", "smtp"] {
            let err = validate_non_secret_paths(&schema, &[path.to_string()]).unwrap_err();
            assert!(
                err.to_string().contains("cannot be marked not secret"),
                "{path}: {err}"
            );
        }
        assert_eq!(
            validate_non_secret_paths(
                &schema,
                &[
                    " b.token ".to_string(),
                    "a.secret".to_string(),
                    "b.token".to_string()
                ]
            )
            .unwrap(),
            vec!["a.secret".to_string(), "b.token".to_string()]
        );
        assert!(validate_non_secret_paths(&schema, &[" ".to_string()]).is_err());
        assert!(validate_non_secret_paths(&schema, &["a\nb".to_string()]).is_err());
        let too_many: Vec<String> = (0..=MAX_NON_SECRET_PATHS)
            .map(|i| format!("k{i}"))
            .collect();
        assert!(validate_non_secret_paths(&schema, &too_many).is_err());
    }

    #[test]
    fn stored_heuristic_paths_and_blank_drop() {
        let schema = json!({
            "properties": {
                "admin": { "properties": { "password": { "writeOnly": true } } },
                "tlsSecret": { "type": "string", "writeOnly": false }
            }
        });
        let stored: BTreeSet<String> = [
            "admin.password",
            "auth.password",
            "cloudflare.apiToken",
            "tlsSecret",
            "users[].password",
            "db",
            "ingress.token",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let got = stored_heuristic_secret_paths(&schema, &stored, &["ingress.token".to_string()]);
        assert_eq!(
            got,
            vec![
                "auth.password".to_string(),
                "cloudflare.apiToken".to_string()
            ]
        );

        let values = json!({
            "auth": { "password": "", "username": "u" },
            "cloudflare": { "apiToken": "new" },
            "token": null
        });
        let dropped = drop_blank_values_at_paths(
            &values,
            &[
                "auth.password".to_string(),
                "cloudflare.apiToken".to_string(),
                "token".to_string(),
                "missing.path".to_string(),
            ],
        );
        assert_eq!(
            dropped,
            json!({ "auth": { "username": "u" }, "cloudflare": { "apiToken": "new" } })
        );
    }

    #[test]
    fn overrides_file_round_trip() {
        let paths = vec!["a.b".to_string(), "c".to_string()];
        let raw = render_secret_overrides(&paths);
        assert!(raw.ends_with('\n'));
        assert_eq!(parse_secret_overrides(&raw), paths);
        assert!(parse_secret_overrides("not json").is_empty());
        assert!(parse_secret_overrides("{}").is_empty());
    }

    #[test]
    fn heuristic_pointers_are_redacted_from_validation_errors() {
        // An undeclared heuristic secret on a parent failing `maxProperties` is not echoed.
        let schema = json!({
            "properties": { "auth": { "type": "object", "maxProperties": 1 } }
        });
        let values = json!({ "auth": { "user": "u", "apiToken": "TOP-SECRET-9" } });
        let pointers = heuristic_secret_pointers(&schema, &values);
        assert_eq!(pointers, vec!["/auth/apiToken".to_string()]);
        let err = crate::validate_schema_values_with_secrets(&schema, &values, &pointers)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("TOP-SECRET-9"), "{err}");
    }

    /// Every built-in catalog keeps its secret-looking string fields either declared secret or
    /// explicitly opted out (`writeOnly: false`), so the heuristic never changes how a built-in
    /// app stores a schema field.
    #[test]
    fn built_in_catalog_fields_are_classified_explicitly() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.file_name().is_some_and(|n| n == "catalog.yaml") {
                    out.push(path);
                }
            }
        }
        fn props<'a>(
            resolver: &SchemaResolver<'a>,
            set: Vec<&'a Value>,
            path: &str,
            out: &mut Vec<String>,
        ) {
            if resolver.is_secret(&set) {
                return;
            }
            let mut keys = BTreeSet::new();
            for node in resolver.expand(&set) {
                if let Some(p) = node.get("properties").and_then(Value::as_object) {
                    keys.extend(p.keys().cloned());
                }
            }
            for key in keys {
                let child = resolver.property_schemas(&set, &key);
                let child_path = join_dot(path, &key);
                let is_object = resolver
                    .expand(&child)
                    .iter()
                    .any(|n| n.get("properties").is_some());
                if is_object {
                    props(resolver, child, &child_path, out);
                } else if looks_like_secret_key(&key)
                    && !resolver.is_secret(&child)
                    && !declared_not_secret(resolver, &child)
                {
                    out.push(child_path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/catalog");
        let mut files = Vec::new();
        walk(&root, &mut files);
        assert!(!files.is_empty());
        let mut unclassified = Vec::new();
        for file in files {
            let raw = std::fs::read_to_string(&file).unwrap();
            let doc: Value = noyalib::from_str(&raw).unwrap();
            let Some(schema) = doc.pointer("/spec/schema") else {
                continue;
            };
            let resolver = SchemaResolver::new(schema);
            let mut found = Vec::new();
            props(&resolver, vec![schema], "", &mut found);
            unclassified.extend(
                found
                    .into_iter()
                    .map(|p| format!("{}: {p}", file.display())),
            );
        }
        assert!(
            unclassified.is_empty(),
            "secret-looking catalog fields must be writeOnly: true or writeOnly: false: \
             {unclassified:#?}"
        );
    }

    #[test]
    fn masking_omits_declared_and_heuristic_secrets_but_honours_overrides() {
        let schema = json!({
            "type": "object",
            "$defs": {"pw": {"type": "string", "writeOnly": true}},
            "properties": {
                "admin": {"type": "object", "properties": {
                    "user": {"type": "string"},
                    "pass": {"$ref": "#/$defs/pw"}
                }},
                "smtpPassword": {"type": "string", "writeOnly": false},
                "tokens": {"type": "array", "items": {"type": "string", "format": "password"}}
            },
            "additionalProperties": true
        });
        let values = json!({
            "admin": {"user": "root", "pass": "s3cret-declared"},
            "smtpPassword": "author-says-plain",
            "tokens": ["t1", "t2"],
            "apiToken": "heuristic-secret",
            "webhookSecret": "user-says-plain",
            "existingSecret": "my-secret",
            "users": [{"name": "a", "password": "list-secret"}, {"name": "b"}],
            "extra": {"dbPassword": "nested"}
        });
        let overrides = vec!["webhookSecret".to_string(), "admin.pass".to_string()];
        let masked = mask_secret_values(&schema, &values, &overrides);
        assert_eq!(
            masked,
            json!({
                "admin": {"user": "root"},
                "smtpPassword": "author-says-plain",
                "webhookSecret": "user-says-plain",
                "existingSecret": "my-secret",
                "users": [{"name": "a"}, {"name": "b"}],
                "extra": {}
            })
        );
        let rendered = masked.to_string();
        for secret in [
            "s3cret-declared",
            "t1",
            "heuristic-secret",
            "list-secret",
            "nested",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }

        // A secret root schema masks everything; list items marked secret are dropped by index.
        assert_eq!(
            mask_secret_values(&json!({"writeOnly": true}), &json!({"a": 1}), &[]),
            json!({})
        );
        let item_schema = json!({"type": "array", "prefixItems": [{}, {"writeOnly": true}]});
        assert_eq!(
            mask_secret_values(
                &json!({"properties": {"l": item_schema}}),
                &json!({"l": ["a", "b", "c"]}),
                &[]
            ),
            json!({"l": ["a", "c"]})
        );
    }

    #[test]
    fn numeric_secrets_are_encrypted_and_redacted() {
        // YAML reads `password: 123456` as a number: still a secret.
        let schema = json!({
            "properties": { "redis": { "properties": { "password": { "type": "string" } } } }
        });
        let values = json!({ "redis": { "password": 12345678, "port": 6379 } });
        let out = partition_values_with_heuristics(&schema, &values, &[]).unwrap();
        assert_eq!(
            out.partitioned.secret_values,
            json!({ "redis": { "password": 12345678 } })
        );
        assert_eq!(
            out.partitioned.normal_values,
            json!({ "redis": { "port": 6379 } })
        );
        let pointers = heuristic_secret_pointers(&schema, &values);
        assert_eq!(pointers, vec!["/redis/password".to_string()]);
        let err = crate::validate_schema_values_with_secrets(&schema, &values, &pointers)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("12345678"), "{err}");
        // Booleans are flags, never secrets.
        let flags = json!({ "serviceAccount": { "automountServiceAccountToken": true } });
        assert!(heuristic_secret_candidates(&json!({}), &flags).is_empty());
    }

    #[test]
    fn dotted_keys_fail_closed_unless_overridden() {
        let schema = json!({ "type": "object" });
        let values = json!({ "config": { "ssl.keystore.password": "ks", "sasl_password": "s" } });
        let err = partition_values_with_heuristics(&schema, &values, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("config.ssl.keystore.password"), "{err}");
        assert!(!err.contains("\"ks\""), "{err}");
        // A secret inside a dotted parent key (`grafana.ini`) cannot be addressed either.
        let grafana = json!({ "grafana.ini": { "security": { "admin_password": "pw" } } });
        let err = partition_values_with_heuristics(&schema, &grafana, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("grafana.ini.security.admin_password"), "{err}");
        let out = partition_values_with_heuristics(
            &schema,
            &values,
            &["config.ssl.keystore.password".to_string()],
        )
        .unwrap();
        assert_eq!(
            out.partitioned.normal_values,
            json!({ "config": { "ssl.keystore.password": "ks" } })
        );
        assert_eq!(
            out.partitioned.secret_values,
            json!({ "config": { "sasl_password": "s" } })
        );
        // The read-time mask omits it all the same.
        assert_eq!(
            mask_secret_values(&schema, &values, &[]),
            json!({ "config": {} })
        );
    }

    #[test]
    fn annotation_and_label_maps_are_exempt() {
        assert!(looks_like_secret_key_under(None, "apiToken"));
        assert!(looks_like_secret_key_under(Some("auth"), "apiToken"));
        for parent in ["annotations", "podAnnotations", "commonLabels", "labels"] {
            assert!(
                !looks_like_secret_key_under(Some(parent), "apiToken"),
                "{parent}"
            );
        }
        let stored: BTreeSet<String> = ["podAnnotations.vault-token", "auth.dbPass"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            stored_heuristic_secret_paths(&json!({}), &stored, &[]),
            vec!["auth.dbPass".to_string()]
        );
    }

    #[test]
    fn enum_or_const_in_one_branch_is_no_opt_out() {
        let schema = json!({
            "properties": {
                "adminPassword": { "anyOf": [{ "const": "" }, { "type": "string", "minLength": 8 }] }
            }
        });
        let values = json!({ "adminPassword": "hunter2hunter2" });
        let out = partition_values_with_heuristics(&schema, &values, &[]).unwrap();
        assert_eq!(out.partitioned.secret_values, values);
        assert_eq!(mask_secret_values(&schema, &values, &[]), json!({}));
    }

    #[test]
    fn overriding_a_stored_heuristic_secret_needs_its_value() {
        let stored: BTreeSet<String> = ["cloudflare.apiToken", "auth.password"]
            .into_iter()
            .map(String::from)
            .collect();
        let overrides = vec!["cloudflare.apiToken".to_string()];
        for values in [
            json!({}),
            json!({ "cloudflare": { "apiToken": "" } }),
            json!({ "cloudflare": { "apiToken": null } }),
        ] {
            assert_eq!(
                overridden_stored_secrets_left_blank(&json!({}), &stored, &overrides, &values),
                vec!["cloudflare.apiToken".to_string()],
                "{values}"
            );
        }
        let values = json!({ "cloudflare": { "apiToken": "re-entered" } });
        assert!(
            overridden_stored_secrets_left_blank(&json!({}), &stored, &overrides, &values)
                .is_empty()
        );
        // No override: the blank keeps the stored ciphertext as usual.
        assert!(
            overridden_stored_secrets_left_blank(&json!({}), &stored, &[], &json!({})).is_empty()
        );
    }
}
