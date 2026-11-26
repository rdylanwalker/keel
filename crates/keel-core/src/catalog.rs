//! `catalog.yaml` parser (`keel.io/v1alpha1`), `jsonschema` validator with structured field
//! error reporting, secret field path extraction (`writeOnly: true` / `format: "password"`),
//! and unmodified Helm `values.yaml` renderer (`SDD.md` §5.1, §6.3, `NFR-EXT-01`).

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::CoreError;
use crate::models::{
    CATALOG_API_VERSION, CATALOG_APPLICATION_KIND, CATALOG_BUNDLE_KIND,
    CATALOG_INFRA_BLUEPRINT_KIND, CatalogApplication, CatalogBundle, CatalogInfraBlueprint,
    CatalogManifest, HelmGatewayRouteSpec,
};
use crate::secret_schema::SchemaResolver;
use crate::yaml::split_yaml_documents;

/// Structured JSON Schema validation error tied to a specific instance field path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaFieldError {
    /// JSON Pointer path to the invalid field (e.g. `"/adminEmail"` or `""` for root object).
    pub field_path: String,
    /// JSON Pointer path inside `spec.schema` that failed validation.
    pub schema_path: String,
    /// Human-readable validation failure description.
    pub message: String,
}

impl fmt::Display for SchemaFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = if self.field_path.is_empty() {
            "<root>"
        } else {
            self.field_path.as_str()
        };
        write!(f, "{}: {}", target, self.message)
    }
}

/// Result of splitting user-submitted form values into plaintext Helm values and secret values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionedValues {
    /// Non-secret configuration values safe to commit as plaintext `values.yaml`.
    pub normal_values: Value,
    /// Secret fields (`writeOnly: true` or `format: "password"`) requiring SOPS + `age` encryption.
    pub secret_values: Value,
    /// Sorted dot-separated paths of secret fields declared in `spec.schema`.
    pub secret_field_paths: Vec<String>,
}

/// Validated, partitioned, and YAML-rendered payload ready for a GitOps PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedHelmValues {
    /// Unmodified Helm `values.yaml` containing non-secret values.
    pub values_yaml: String,
    /// Plaintext YAML of extracted secret values to be passed into `SopsManager::encrypt_yaml`, if any.
    pub plaintext_secrets_yaml: Option<String>,
    /// Partitioned JSON structures and detected secret field paths.
    pub partitioned: PartitionedValues,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogHeader {
    api_version: String,
    kind: String,
}

/// Parses a `catalog.yaml` document into either a `CatalogApplication` or `CatalogInfraBlueprint`.
pub fn parse_catalog(yaml: &str) -> Result<CatalogManifest, CoreError> {
    let header: CatalogHeader = noyalib::from_str(yaml)?;
    if header.api_version != CATALOG_API_VERSION {
        return Err(CoreError::Validation(format!(
            "Unsupported catalog apiVersion '{}', expected '{}'",
            header.api_version, CATALOG_API_VERSION
        )));
    }

    match header.kind.as_str() {
        CATALOG_APPLICATION_KIND => {
            let app = parse_catalog_application(yaml)?;
            Ok(CatalogManifest::Application(Box::new(app)))
        }
        CATALOG_INFRA_BLUEPRINT_KIND => {
            let bp = parse_catalog_blueprint(yaml)?;
            Ok(CatalogManifest::InfraBlueprint(Box::new(bp)))
        }
        CATALOG_BUNDLE_KIND => {
            let bundle = parse_catalog_bundle(yaml)?;
            Ok(CatalogManifest::Bundle(Box::new(bundle)))
        }
        other => Err(CoreError::Validation(format!(
            "Unsupported catalog kind '{}', expected '{}', '{}', or '{}'",
            other, CATALOG_APPLICATION_KIND, CATALOG_INFRA_BLUEPRINT_KIND, CATALOG_BUNDLE_KIND
        ))),
    }
}

/// Parses and validates a `CatalogApplication` from YAML, including compiling `spec.schema`.
pub fn parse_catalog_application(yaml: &str) -> Result<CatalogApplication, CoreError> {
    let app: CatalogApplication = noyalib::from_str(yaml)?;
    app.validate()?;
    compile_schema(&app.spec.schema)?;
    Ok(app)
}

/// Parses and validates a `CatalogInfraBlueprint` from YAML, including compiling `spec.schema`.
pub fn parse_catalog_blueprint(yaml: &str) -> Result<CatalogInfraBlueprint, CoreError> {
    let bp: CatalogInfraBlueprint = noyalib::from_str(yaml)?;
    bp.validate()?;
    compile_schema(&bp.spec.schema)?;
    Ok(bp)
}

/// Parses and validates a `CatalogBundle` from YAML, including compiling `spec.schema` and DAG checks.
pub fn parse_catalog_bundle(yaml: &str) -> Result<CatalogBundle, CoreError> {
    let bundle: CatalogBundle = noyalib::from_str(yaml)?;
    bundle.validate()?;
    compile_schema(&bundle.spec.schema)?;
    Ok(bundle)
}

/// Refuses every external schema resource: catalog schemas must be self-contained, so compiling
/// one never fetches a URL or reads a file (`$ref` is checked by
/// [`validate_secret_schema_support`] before a catalog is accepted).
struct NoExternalSchemas;

impl jsonschema::Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!(
            "external schema reference '{}' is not allowed; catalog schemas must be self-contained",
            uri.as_str()
        )
        .into())
    }
}

fn compile_schema(schema: &Value) -> Result<jsonschema::Validator, CoreError> {
    jsonschema::options()
        .should_validate_formats(true)
        .with_retriever(NoExternalSchemas)
        .build(schema)
        .map_err(|e| CoreError::Validation(format!("Invalid JSON Schema in catalog: {e}")))
}

/// Validates user input `values` against a JSON Schema, returning structured `SchemaFieldError`s on failure.
///
/// Messages never echo a secret (`NFR-SEC-05`): see [`validate_schema_values_with_secrets`].
pub fn validate_schema_values(schema: &Value, values: &Value) -> Result<(), CoreError> {
    validate_schema_values_with_secrets(schema, values, &[])
}

/// [`validate_schema_values`], also treating the values at the JSON Pointers
/// `extra_secret_pointers` as secret (e.g. name-based secret heuristics on top of the schema).
///
/// An error whose instance lies at or inside a secret value, or contains one (a parent object
/// failing `anyOf`/`oneOf`/`maxProperties`/`enum`/`const`/`not`, or the root), gets a generic
/// message naming only the failing keyword. `required` and `additionalProperties` errors on such
/// a parent keep jsonschema's masked message, which names keys, never values. Other errors on an
/// object or array use the masked message too; only scalar non-secret values are quoted.
pub fn validate_schema_values_with_secrets(
    schema: &Value,
    values: &Value,
    extra_secret_pointers: &[String],
) -> Result<(), CoreError> {
    let validator = compile_schema(schema)?;
    let mut secret_pointers: Vec<String> = SchemaResolver::new(schema)
        .secret_value_locations(values)
        .into_iter()
        .map(|l| l.pointer)
        .collect();
    secret_pointers.extend(extra_secret_pointers.iter().cloned());
    let mut errors: Vec<SchemaFieldError> = validator
        .iter_errors(values)
        .map(|err| {
            let field_path = err.instance_path().to_string();
            let schema_path = err.schema_path().to_string();
            let keyword = err.kind().keyword();
            let inside_secret = secret_pointers
                .iter()
                .any(|p| pointer_is_within(&field_path, p));
            let contains_secret = secret_pointers
                .iter()
                .any(|p| pointer_is_within(p, &field_path));
            let names_keys_only = matches!(
                err.kind(),
                jsonschema::error::ValidationErrorKind::Required { .. }
                    | jsonschema::error::ValidationErrorKind::AdditionalProperties { .. }
            );
            let message = if inside_secret {
                format!("Secret value does not satisfy the '{keyword}' constraint")
            } else if contains_secret && !names_keys_only {
                format!(
                    "Value containing secret fields does not satisfy the '{keyword}' constraint"
                )
            } else if err.instance().is_object() || err.instance().is_array() {
                err.masked().to_string()
            } else {
                err.to_string()
            };
            SchemaFieldError {
                field_path,
                schema_path,
                message,
            }
        })
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        errors.sort_by(|a, b| {
            a.field_path
                .cmp(&b.field_path)
                .then_with(|| a.schema_path.cmp(&b.schema_path))
                .then_with(|| a.message.cmp(&b.message))
        });
        Err(CoreError::SchemaValidation(errors))
    }
}

/// Whether JSON Pointer `pointer` equals `ancestor` or lies below it.
fn pointer_is_within(pointer: &str, ancestor: &str) -> bool {
    ancestor.is_empty()
        || pointer == ancestor
        || pointer
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Returns `true` if the JSON Schema `schema_node` marks a value secret (`writeOnly: true` or
/// `format: "password"`), following its `$ref`s (resolved against `schema_node` itself) and
/// every combinator/conditional branch. For a node inside a larger document use
/// [`SchemaResolver::is_secret_node`] on the document, so `$ref`s resolve against its root.
pub fn is_secret_schema_node(schema_node: &Value) -> bool {
    SchemaResolver::new(schema_node).is_secret_node(schema_node)
}

/// Extracts sorted, deduplicated dot-notated paths of all secret fields (`writeOnly: true` or `format: "password"`)
/// a JSON Schema names statically (see [`SchemaResolver::declared_secret_paths`]).
pub fn extract_secret_field_paths(schema: &Value) -> Vec<String> {
    SchemaResolver::new(schema).declared_secret_paths()
}

/// Sorted dot paths of the secret fields of `schema`: the statically named ones plus every
/// secret value actually present in `values` (fields under `patternProperties`/
/// `additionalProperties`, recursive `$ref`s). The building block for "effective" secret paths
/// that callers extend (stored secret keys, heuristics).
pub fn secret_field_paths_for_values(schema: &Value, values: &Value) -> Vec<String> {
    let resolver = SchemaResolver::new(schema);
    let mut paths: BTreeSet<String> = resolver.declared_secret_paths().into_iter().collect();
    paths.extend(
        resolver
            .secret_value_locations(values)
            .into_iter()
            .map(|l| l.path)
            .filter(|p| !p.is_empty()),
    );
    paths.into_iter().collect()
}

/// Returns the `stored_paths` (dot paths of stored secret keys) that `schema` still declares
/// secret, e.g. a `patternProperties` secret left blank on an edit.
pub fn stored_secret_paths_still_secret<'p>(
    schema: &Value,
    stored_paths: impl IntoIterator<Item = &'p String>,
) -> Vec<String> {
    let resolver = SchemaResolver::new(schema);
    stored_paths
        .into_iter()
        .filter(|p| resolver.is_secret_path(p))
        .cloned()
        .collect()
}

/// Partitions a JSON object of user values into `normal_values` and `secret_values` according to `schema`.
pub fn partition_values_by_schema(
    schema: &Value,
    values: &Value,
) -> Result<PartitionedValues, CoreError> {
    if !values.is_object() {
        return Err(CoreError::Validation(
            "Input catalog values must be a JSON object".to_string(),
        ));
    }

    let resolver = SchemaResolver::new(schema);
    let secret_field_paths = secret_field_paths_for_values(schema, values);
    let (normal_opt, secret_opt) = resolver.split(&[schema], values);

    let normal_values = normal_opt.unwrap_or_else(|| Value::Object(Map::new()));
    let secret_values = secret_opt.unwrap_or_else(|| Value::Object(Map::new()));

    Ok(PartitionedValues {
        normal_values,
        secret_values,
        secret_field_paths,
    })
}

/// Fills the JSON Schema `default`s of `schema` into `values`: the server-side twin of the
/// GUI's `extractSchemaDefaults` (`ui/src/lib/schemaBuilder.ts`), so a CLI/API install gets the
/// same values as the `@rjsf/core` form (`SDD.md` §5.1).
///
/// - A property missing from `values` takes its `default` (scalars, arrays and objects alike);
///   a submitted value, including an explicit `null`, is never overridden.
/// - Object properties without a `default` are recursed into; a nested mapping is only added
///   when it ends up non-empty.
/// - Object items of a submitted array get their item schema's defaults.
/// - Secret fields (`writeOnly: true` / `format: "password"`) never receive a default, and
///   secret sub-fields of an object default are dropped: secrets must be submitted explicitly.
///
/// Callers apply this on first install only. An edit submits the complete desired values and
/// must not get defaults back for keys the user removed.
pub fn apply_schema_defaults(schema: &Value, values: &Value) -> Value {
    let resolver = SchemaResolver::new(schema);
    match values {
        Value::Object(map) => Value::Object(fill_object_defaults(&resolver, schema, map.clone())),
        other => other.clone(),
    }
}

fn fill_object_defaults<'a>(
    resolver: &SchemaResolver<'a>,
    schema: &'a Value,
    mut map: Map<String, Value>,
) -> Map<String, Value> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return map;
    };
    for (key, prop) in props {
        if !prop.is_object() || resolver.is_secret_node(prop) {
            continue;
        }
        match map.get(key) {
            None => {
                if let Some(default) = prop.get("default") {
                    // An object default keeps only its non-secret parts.
                    if let (Some(normal), _) = resolver.split(&[prop], default) {
                        map.insert(key.clone(), normal);
                    }
                } else if prop.get("properties").is_some_and(Value::is_object) {
                    let nested = fill_object_defaults(resolver, prop, Map::new());
                    if !nested.is_empty() {
                        map.insert(key.clone(), Value::Object(nested));
                    }
                }
            }
            Some(Value::Object(existing)) => {
                let nested = fill_object_defaults(resolver, prop, existing.clone());
                map.insert(key.clone(), Value::Object(nested));
            }
            Some(Value::Array(items)) => {
                if let Some(item_schema) = prop
                    .get("items")
                    .filter(|s| s.is_object() && !resolver.is_secret_node(s))
                {
                    let filled = items
                        .iter()
                        .map(|item| match item {
                            Value::Object(m) => Value::Object(fill_object_defaults(
                                resolver,
                                item_schema,
                                m.clone(),
                            )),
                            other => other.clone(),
                        })
                        .collect();
                    map.insert(key.clone(), Value::Array(filled));
                }
            }
            Some(_) => {}
        }
    }
    map
}

/// Applies `patch` over `base` with JSON Merge Patch semantics (RFC 7386): objects merge key by
/// key, a `null` in `patch` removes that key, and any other value (scalars, arrays) replaces the
/// base value. Used for CLI edits (`mergeStoredValues`), which send only the keys to change on
/// top of an instance's committed values (`SDD.md` §5.1).
pub fn merge_values_patch(base: &Value, patch: &Value) -> Value {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            let mut merged = base_map.clone();
            for (key, value) in patch_map {
                if value.is_null() {
                    merged.remove(key);
                } else {
                    let next = merge_values_patch(merged.get(key).unwrap_or(&Value::Null), value);
                    merged.insert(key.clone(), next);
                }
            }
            Value::Object(merged)
        }
        (_, Value::Object(_)) => merge_values_patch(&Value::Object(Map::new()), patch),
        (_, other) => other.clone(),
    }
}

/// Removes every `null` object entry, recursively (array items are kept as they are): the values
/// a merge patch leaves once there is nothing to merge into.
pub fn strip_null_values(values: &Value) -> Value {
    merge_values_patch(&Value::Object(Map::new()), values)
}

/// Renders an unmodified upstream Helm `values.yaml` string directly from a JSON value (`crate::yaml::to_string`).
pub fn render_helm_values_yaml(values: &Value) -> Result<String, CoreError> {
    if !values.is_object() {
        return Err(CoreError::Validation(
            "Helm values root must be a JSON/YAML mapping object".to_string(),
        ));
    }
    Ok(crate::yaml::to_string(values)?)
}

/// Validates form input against `app.spec.schema`, partitions secret vs. normal fields,
/// and renders unmodified Helm `values.yaml` (plus optional plaintext secrets YAML for SOPS).
pub fn prepare_helm_values(
    app: &CatalogApplication,
    raw_values: &Value,
) -> Result<PreparedHelmValues, CoreError> {
    validate_schema_values(&app.spec.schema, raw_values)?;
    let partitioned = partition_values_by_schema(&app.spec.schema, raw_values)?;
    let values_yaml = render_helm_values_yaml(&partitioned.normal_values)?;

    let plaintext_secrets_yaml = match partitioned.secret_values.as_object() {
        Some(map) if !map.is_empty() => Some(crate::yaml::to_string(&partitioned.secret_values)?),
        _ => None,
    };

    Ok(PreparedHelmValues {
        values_yaml,
        plaintext_secrets_yaml,
        partitioned,
    })
}

fn is_valid_manifest_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parses single or multi-document Kubernetes YAML, ensuring at least one non-empty document exists
/// and every non-null document is a mapping with non-empty string `apiVersion`, `kind`, and `metadata.name`.
pub fn validate_k8s_manifests_yaml(raw_yaml: &str) -> Result<Vec<Value>, CoreError> {
    if raw_yaml.trim().is_empty() {
        return Err(CoreError::Validation(
            "Kubernetes manifests YAML must not be empty".to_string(),
        ));
    }

    let raw_docs = split_yaml_documents(raw_yaml);

    let mut docs = Vec::new();
    for (idx, doc_str) in raw_docs.into_iter().enumerate() {
        let doc: Value = noyalib::from_str(&doc_str)?;
        if doc.is_null() {
            continue;
        }
        let doc_num = idx + 1;
        let Some(obj) = doc.as_object() else {
            return Err(CoreError::Validation(format!(
                "Manifest document #{doc_num} must be a YAML mapping object"
            )));
        };

        let api_version = obj
            .get("apiVersion")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if api_version.is_empty() {
            return Err(CoreError::Validation(format!(
                "Manifest document #{doc_num} is missing a non-empty string 'apiVersion'"
            )));
        }

        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if kind.is_empty() {
            return Err(CoreError::Validation(format!(
                "Manifest document #{doc_num} is missing a non-empty string 'kind'"
            )));
        }

        let meta_name = obj
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if meta_name.is_empty() {
            return Err(CoreError::Validation(format!(
                "Manifest document #{doc_num} ({kind}) is missing a non-empty string 'metadata.name'"
            )));
        }

        docs.push(doc);
    }

    if docs.is_empty() {
        return Err(CoreError::Validation(
            "Kubernetes manifests YAML must contain at least one non-empty resource document"
                .to_string(),
        ));
    }

    Ok(docs)
}

/// Well-known cluster-scoped Kubernetes kinds a `scope: namespaced` manifest app must not contain
/// (`SDD.md` §5.1 `spec.placement.scope`). A fixed allowlist keyed by `kind`: tenant objects on the
/// management cluster impersonate `tenant-<id>-sa`, whose namespace-scoped RBAC cannot create any
/// of these, so such manifests belong to `scope: cluster` apps on a remote workload cluster.
pub const CLUSTER_SCOPED_KINDS: &[&str] = &[
    "APIService",
    "CertificateSigningRequest",
    "ClusterIssuer",
    "ClusterPolicy",
    "ClusterRole",
    "ClusterRoleBinding",
    "CSIDriver",
    "CustomResourceDefinition",
    "FlowSchema",
    "GatewayClass",
    "IngressClass",
    "MutatingAdmissionPolicy",
    "MutatingAdmissionPolicyBinding",
    "MutatingWebhookConfiguration",
    "Namespace",
    "Node",
    "PersistentVolume",
    "PriorityClass",
    "PriorityLevelConfiguration",
    "RuntimeClass",
    "StorageClass",
    "ValidatingAdmissionPolicy",
    "ValidatingAdmissionPolicyBinding",
    "ValidatingWebhookConfiguration",
    "VolumeAttachment",
    "VolumeSnapshotClass",
];

/// Returns the distinct well-known cluster-scoped kinds (`CLUSTER_SCOPED_KINDS`) declared by a
/// (raw or rendered) multi-document manifest, in first-seen order. Documents that do not parse
/// as YAML mappings are skipped (structure is validated separately).
pub fn manifest_cluster_scoped_kinds(raw_yaml: &str) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for doc_str in split_yaml_documents(raw_yaml) {
        let Ok(doc) = noyalib::from_str::<Value>(&doc_str) else {
            continue;
        };
        let Some(kind) = doc.get("kind").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if CLUSTER_SCOPED_KINDS.contains(&kind) && !kinds.iter().any(|k| k == kind) {
            kinds.push(kind.to_string());
        }
    }
    kinds
}

/// Returns `Kind/name` (or `Kind/name (namespace)`) of every namespaced object (any kind outside
/// [`CLUSTER_SCOPED_KINDS`]) in a multi-document manifest whose `metadata.namespace` is missing or
/// not one of `allowed`. Documents that do not parse as YAML mappings with a `kind` are skipped
/// (structure is validated separately).
pub fn manifest_objects_outside_namespaces(raw_yaml: &str, allowed: &[String]) -> Vec<String> {
    let mut offenders = Vec::new();
    for doc_str in split_yaml_documents(raw_yaml) {
        let Ok(doc) = noyalib::from_str::<Value>(&doc_str) else {
            continue;
        };
        let Some(kind) = doc.get("kind").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if CLUSTER_SCOPED_KINDS.contains(&kind) {
            continue;
        }
        let name = doc
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        match doc.pointer("/metadata/namespace").and_then(Value::as_str) {
            Some(ns) if allowed.iter().any(|a| a == ns) => {}
            Some(ns) => offenders.push(format!("{kind}/{name} ({ns})")),
            None => offenders.push(format!("{kind}/{name}")),
        }
    }
    offenders
}

/// Extracts unique placeholder variable names matching `${var_name}` or `${var_name:=default}`
/// (where `var_name` is `[A-Za-z_][A-Za-z0-9_]*`), preserving first-seen order.
pub fn extract_manifest_variables(raw_yaml: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let bytes = raw_yaml.as_bytes();
    let mut i = 0;

    while i + 2 < bytes.len() {
        if bytes[i] == b'$'
            && bytes[i + 1] == b'{'
            && let Some(rel_end) = raw_yaml[i + 2..].find(['}', '\n'])
        {
            let end = i + 2 + rel_end;
            if bytes[end] == b'}' {
                let inner = &raw_yaml[i + 2..end];
                let var_name = match inner.split_once(":=") {
                    Some((name, _default)) => name,
                    None => inner,
                };
                if is_valid_manifest_var_name(var_name) {
                    let owned = var_name.to_string();
                    if !vars.contains(&owned) {
                        vars.push(owned);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }

    vars
}

/// Returns `true` when a placeholder spanning `prefix`..`suffix` on its line occupies an entire
/// YAML scalar position: either a mapping value (`key: ${var}`) or a block sequence entry
/// (`- ${var}`), optionally followed by a trailing ` # comment`.
fn placeholder_is_whole_scalar(prefix: &str, suffix: &str) -> bool {
    let suffix_ok = suffix.trim().is_empty()
        || (suffix.starts_with(char::is_whitespace) && suffix.trim_start().starts_with('#'));
    if !suffix_ok {
        return false;
    }
    let mut rest = prefix.trim_start();
    let mut saw_seq_dash = false;
    while let Some(after) = rest.strip_prefix('-') {
        if !after.starts_with(char::is_whitespace) {
            break;
        }
        saw_seq_dash = true;
        rest = after.trim_start();
    }
    if rest.is_empty() {
        return saw_seq_dash;
    }
    rest.ends_with(char::is_whitespace) && rest.trim_end().ends_with(':')
}

/// Characters that may alter YAML structure when pasted inside a plain scalar.
const EMBEDDED_FORBIDDEN_CHARS: &[char] = &[
    '"', '\'', '\\', '#', '{', '}', '[', ']', ',', '&', '*', '!', '|', '>', '%', '@', '`',
];

/// Validates a value substituted into the middle of a scalar (e.g. `image: repo/${name}:tag`).
/// Such values cannot be quoted, so anything that could change the YAML structure is rejected.
fn validate_embedded_manifest_value(var_name: &str, value: &str) -> Result<(), CoreError> {
    let reject = |reason: &str| {
        Err(CoreError::Validation(format!(
            "Manifest variable '{var_name}' {reason}"
        )))
    };
    if value.chars().any(char::is_control) {
        return reject("must not contain newlines or control characters");
    }
    if value.contains(EMBEDDED_FORBIDDEN_CHARS) {
        return reject("contains YAML-structural characters that are not allowed inside a scalar");
    }
    if value.contains(": ") || value.ends_with(':') {
        return reject("must not contain ': ' or end with ':' inside a scalar");
    }
    if value.starts_with(['-', '?', ' ']) || value.ends_with(' ') {
        return reject("must not start with '-', '?' or whitespace, or end with whitespace");
    }
    Ok(())
}

/// Validates and renders a user-provided value for a placeholder.
///
/// Whole-scalar string values are emitted as YAML double-quoted strings (JSON string syntax is a
/// valid YAML double-quoted scalar, so newlines/quotes/`---` are escaped and cannot inject
/// structure). Embedded values are validated strictly by [`validate_embedded_manifest_value`].
fn render_manifest_value(
    var_name: &str,
    value: &Value,
    whole_scalar: bool,
) -> Result<Option<String>, CoreError> {
    let rendered = match value {
        Value::Null => return Ok(None),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) if whole_scalar => serde_json::to_string(s)?,
        Value::String(s) => {
            validate_embedded_manifest_value(var_name, s)?;
            s.clone()
        }
        // Compact JSON is valid YAML flow syntax with all strings escaped.
        Value::Array(_) | Value::Object(_) if whole_scalar => value.to_string(),
        Value::Array(_) | Value::Object(_) => {
            return Err(CoreError::Validation(format!(
                "Manifest variable '{var_name}' is a structured value and may only be used as a whole YAML value"
            )));
        }
    };
    Ok(Some(rendered))
}

/// Substitutes `${var_name}` and `${var_name:=default}` placeholders using scalar values
/// from `values` (if a key exists in `values`, replace with its rendered value; else if `:=default`
/// is present, replace with the catalog-authored `default`; else leave `${var_name}` for Flux
/// `postBuild` / Pulumi).
///
/// User-provided strings are rendered safely: when the placeholder is the entire YAML value
/// (`key: ${var}` / `- ${var}`) the string is emitted as an escaped double-quoted scalar; when it is
/// embedded in a larger scalar, values containing newlines, control characters, or YAML-structural
/// characters are rejected with [`CoreError::Validation`] to prevent document/key injection.
pub fn render_manifest_template(raw_yaml: &str, values: &Value) -> Result<String, CoreError> {
    let mut out = String::with_capacity(raw_yaml.len());
    let bytes = raw_yaml.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if i + 2 < bytes.len()
            && bytes[i] == b'$'
            && bytes[i + 1] == b'{'
            && let Some(rel_end) = raw_yaml[i + 2..].find(['}', '\n'])
        {
            let end = i + 2 + rel_end;
            if bytes[end] == b'}' {
                let inner = &raw_yaml[i + 2..end];
                let (var_name, default_opt) = match inner.split_once(":=") {
                    Some((name, def)) => (name, Some(def)),
                    None => (inner, None),
                };
                if is_valid_manifest_var_name(var_name) {
                    let provided = match values.as_object().and_then(|obj| obj.get(var_name)) {
                        Some(v) => {
                            let line_start = raw_yaml[..i].rfind('\n').map_or(0, |p| p + 1);
                            let line_end = raw_yaml[end + 1..]
                                .find('\n')
                                .map_or(raw_yaml.len(), |p| end + 1 + p);
                            let whole = placeholder_is_whole_scalar(
                                &raw_yaml[line_start..i],
                                &raw_yaml[end + 1..line_end],
                            );
                            render_manifest_value(var_name, v, whole)?
                        }
                        None => None,
                    };
                    if let Some(val_str) = provided {
                        out.push_str(&val_str);
                    } else if let Some(def) = default_opt {
                        out.push_str(def);
                    } else {
                        out.push_str(&raw_yaml[i..=end]);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }

        let ch = raw_yaml[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    Ok(out)
}

/// Verifies that rendering a manifest template did not alter its document structure: the rendered
/// YAML must be valid Kubernetes manifests with the same document count and the same
/// `apiVersion`/`kind` sequence as the un-rendered template. Guards against value injection that
/// smuggles in extra documents or changes resource kinds.
pub fn verify_rendered_manifest_structure(
    template_yaml: &str,
    rendered_yaml: &str,
) -> Result<Vec<Value>, CoreError> {
    let rendered_docs = validate_k8s_manifests_yaml(rendered_yaml).map_err(|e| match e {
        CoreError::Yaml(err) => {
            CoreError::Validation(format!("Rendered manifests are not valid YAML: {err}"))
        }
        other => other,
    })?;
    let template_docs: Vec<Value> = split_yaml_documents(template_yaml)
        .iter()
        .map(|d| noyalib::from_str::<Value>(d))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| CoreError::Validation(format!("Manifest template is not valid YAML: {e}")))?
        .into_iter()
        .filter(|d| !d.is_null())
        .collect();
    if template_docs.len() != rendered_docs.len() {
        return Err(CoreError::Validation(format!(
            "Rendered manifests contain {} document(s) but the template defines {}; refusing possible injection",
            rendered_docs.len(),
            template_docs.len()
        )));
    }
    for (idx, (tpl, rendered)) in template_docs.iter().zip(&rendered_docs).enumerate() {
        for field in ["apiVersion", "kind"] {
            let tpl_val = tpl.get(field).and_then(Value::as_str);
            // Templated identity fields (e.g. `kind: ${kind}`) are not pinned.
            if tpl_val.is_some_and(|v| v.contains("${")) {
                continue;
            }
            if tpl_val != rendered.get(field).and_then(Value::as_str) {
                return Err(CoreError::Validation(format!(
                    "Rendered manifest document #{} changed '{field}' from the template; refusing possible injection",
                    idx + 1
                )));
            }
        }
    }
    Ok(rendered_docs)
}

/// Gateway API route kinds whose `spec.hostnames` claim traffic on a shared `Gateway` listener.
pub const GATEWAY_ROUTE_KINDS: &[&str] = &["HTTPRoute", "GRPCRoute", "TLSRoute"];

/// Default local base domain used by Stage 0 when no `--oidc-public-url` is given
/// (`idp.keel.local`, `keel.keel.local`, `forgejo.keel.local`).
pub const DEFAULT_PLATFORM_BASE_DOMAIN: &str = "keel.local";

/// Host labels Stage 0 publishes under the platform base domain on the shared `keel-ingress`
/// Gateway (`<label>.<base-domain>`): the built-in IdP, keel-server and Forgejo.
pub const PLATFORM_HOST_LABELS: &[&str] = &["idp", "keel", "forgejo"];

/// Extracts the lower-cased `spec.hostnames` of every Gateway API route document
/// (`GATEWAY_ROUTE_KINDS`, `gateway.networking.k8s.io/*`) in `docs`.
pub fn gateway_route_hostnames(docs: &[Value]) -> Vec<String> {
    let mut hosts = Vec::new();
    for doc in docs {
        let is_route = doc
            .get("apiVersion")
            .and_then(Value::as_str)
            .is_some_and(|v| v.starts_with("gateway.networking.k8s.io/"))
            && doc
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| GATEWAY_ROUTE_KINDS.contains(&k));
        if !is_route {
            continue;
        }
        if let Some(list) = doc
            .get("spec")
            .and_then(|s| s.get("hostnames"))
            .and_then(Value::as_array)
        {
            hosts.extend(
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|h| h.trim().to_ascii_lowercase()),
            );
        }
    }
    hosts
}

/// `<Kind>/<name>` of every Gateway API route document in `docs` that attaches to a Gateway in
/// another namespace (the shared `keel-system/keel-ingress`) without any `spec.hostnames`. Such a
/// route matches every host on the shared listeners (a catch-all capturing other tenants'
/// traffic); the Kyverno `restrict-tenant-route-hostnames` policy denies it in tenant namespaces,
/// so proposals reject it before anything is committed. A `parentRef` counts as cross-namespace
/// when it names a `namespace` other than the route's own (its `metadata.namespace`, else
/// `default_namespace`, the namespace the route is applied into) and its `kind`/`group` are
/// Gateway API `Gateway` (or unset), exactly as the Kyverno rule decides.
pub fn shared_gateway_catch_all_routes(docs: &[Value], default_namespace: &str) -> Vec<String> {
    let mut found = Vec::new();
    for doc in docs {
        let kind = doc.get("kind").and_then(Value::as_str).unwrap_or_default();
        let is_route = doc
            .get("apiVersion")
            .and_then(Value::as_str)
            .is_some_and(|v| v.starts_with("gateway.networking.k8s.io/"))
            && GATEWAY_ROUTE_KINDS.contains(&kind);
        if !is_route {
            continue;
        }
        let spec = doc.get("spec");
        let has_hostnames = spec
            .and_then(|s| s.get("hostnames"))
            .and_then(Value::as_array)
            .is_some_and(|h| !h.is_empty());
        if has_hostnames {
            continue;
        }
        let route_ns = doc
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            .unwrap_or(default_namespace);
        let shared = spec
            .and_then(|s| s.get("parentRefs"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|p| {
                let field = |k: &str| p.get(k).and_then(Value::as_str);
                field("namespace").is_some_and(|ns| ns != route_ns)
                    && field("kind").is_none_or(|k| k == "Gateway")
                    && field("group").is_none_or(|g| g == "gateway.networking.k8s.io")
            });
        if shared {
            let name = doc
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            found.push(format!("{kind}/{name}"));
        }
    }
    found
}

/// Host of an absolute URL (`https://user@host:port/path` → `host`), lower-cased.
fn url_host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split(':').next().unwrap_or_default().trim();
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Hostnames tenant routes must never claim on the shared Gateway: the platform hosts derived
/// from `DEFAULT_PLATFORM_BASE_DOMAIN` plus, for every platform URL given (OIDC issuer, Keel
/// redirect URL, ...), its host and `<label>.<base-domain>` for each `PLATFORM_HOST_LABELS`
/// entry (base domain = the host minus its first label, as Stage 0 derives it).
pub fn platform_reserved_hostnames<'a>(
    platform_urls: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<String> {
    let mut bases = BTreeSet::from([DEFAULT_PLATFORM_BASE_DOMAIN.to_string()]);
    let mut reserved = BTreeSet::new();
    for url in platform_urls {
        let Some(host) = url_host(url) else {
            continue;
        };
        if let Some((_, base)) = host.split_once('.')
            && base.contains('.')
        {
            bases.insert(base.to_string());
        }
        reserved.insert(host);
    }
    for base in &bases {
        for label in PLATFORM_HOST_LABELS {
            reserved.insert(format!("{label}.{base}"));
        }
    }
    reserved
}

/// Whether a route hostname (exact or `*.` wildcard) would match `host`.
pub fn route_hostname_matches(route_hostname: &str, host: &str) -> bool {
    let route_hostname = route_hostname.trim().to_ascii_lowercase();
    let host = host.trim().to_ascii_lowercase();
    match route_hostname.strip_prefix('*') {
        Some(suffix) if suffix.starts_with('.') => {
            host.ends_with(suffix) && host.len() > suffix.len()
        }
        _ => route_hostname == host,
    }
}

/// Fails closed when any rendered route hostname is unresolved (still a `${...}` placeholder),
/// empty, or matches a `reserved` platform host (exactly or through a wildcard).
pub fn validate_route_hostnames(
    route_hostnames: &[String],
    reserved: &BTreeSet<String>,
) -> Result<(), CoreError> {
    for hostname in route_hostnames {
        if hostname.is_empty() || hostname.contains("${") {
            return Err(CoreError::Validation(format!(
                "Route hostname '{hostname}' must be a concrete DNS name at proposal time"
            )));
        }
        if is_tld_wide_wildcard(hostname) {
            return Err(CoreError::Validation(format!(
                "Route hostname '{hostname}' is a wildcard covering a whole top-level domain; \
                 it would capture every unclaimed host under it on the shared Gateway"
            )));
        }
        if let Some(host) = reserved
            .iter()
            .find(|r| route_hostname_matches(hostname, r))
        {
            return Err(CoreError::Validation(format!(
                "Route hostname '{hostname}' claims the reserved platform host '{host}'; choose another hostname"
            )));
        }
    }
    Ok(())
}

/// `true` for a route hostname wildcard covering everything (`*`) or a whole top-level domain
/// (`*.com`): practically a catch-all on a shared Gateway listener. The Kyverno
/// `restrict-tenant-route-hostnames` policy denies the same hostnames in tenant namespaces.
pub fn is_tld_wide_wildcard(hostname: &str) -> bool {
    hostname.strip_prefix('*').is_some_and(|rest| {
        rest.is_empty() || rest.strip_prefix('.').is_some_and(|tld| !tld.contains('.'))
    })
}

/// `parentRefs` pinning a tenant route to both HTTPS listeners of the shared
/// `keel-system/keel-ingress` Gateway (`https-local` for the local kind CA, `https` otherwise),
/// exactly like the built-in tenant manifest apps (e.g. Rauthy).
pub fn keel_ingress_parent_refs() -> Value {
    serde_json::json!([
        { "name": "keel-ingress", "namespace": "keel-system", "sectionName": "https-local" },
        { "name": "keel-ingress", "namespace": "keel-system", "sectionName": "https" },
    ])
}

fn value_at_path<'a>(values: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(values, |node, seg| node.get(seg))
}

fn set_value_at_path(values: &mut Value, path: &str, value: Value) -> Result<(), CoreError> {
    let mut node = values;
    let segments: Vec<&str> = path.split('.').collect();
    for (i, seg) in segments.iter().enumerate() {
        if !node.is_object() {
            if node.is_null() {
                *node = Value::Object(Map::new());
            } else {
                return Err(CoreError::Validation(format!(
                    "Value '{}' must be an object to set '{path}'",
                    segments[..i].join(".")
                )));
            }
        }
        let map = node.as_object_mut().expect("object checked above");
        if i + 1 == segments.len() {
            map.insert((*seg).to_string(), value);
            return Ok(());
        }
        node = map.entry((*seg).to_string()).or_insert(Value::Null);
    }
    Ok(())
}

/// Applies the `spec.gatewayRoutes` declarations of a Helm catalog app to its submitted
/// (non-secret) `values` and returns the lower-cased hostnames of every enabled route.
///
/// * A route is enabled when it has no `enabledValue`, or the value at that path is `true`.
/// * An enabled route must carry at least one concrete hostname (a string or list of strings at
///   `hostnamesValue`): a route without hostnames would match every host on the shared
///   `keel-ingress` listeners.
/// * `parentRefsValue` is overwritten with [`keel_ingress_parent_refs`], so the route can only
///   attach to the shared Gateway's HTTPS listeners whatever the submitted values say.
///
/// The caller still checks the hostnames with [`validate_route_hostnames`] and against routes of
/// other apps.
pub fn resolve_helm_gateway_routes(
    routes: &[HelmGatewayRouteSpec],
    values: &mut Value,
) -> Result<Vec<String>, CoreError> {
    let mut hostnames = Vec::new();
    for route in routes {
        let enabled = match &route.enabled_value {
            None => true,
            Some(path) => value_at_path(values, path).and_then(Value::as_bool) == Some(true),
        };
        if !enabled {
            continue;
        }
        let raw: Vec<String> = match value_at_path(values, &route.hostnames_value) {
            Some(Value::String(host)) => vec![host.clone()],
            Some(Value::Array(list)) => list
                .iter()
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or_else(|| {
                        CoreError::Validation(format!(
                            "'{}' must be a list of hostnames",
                            route.hostnames_value
                        ))
                    })
                })
                .collect::<Result<_, _>>()?,
            None | Some(Value::Null) => Vec::new(),
            Some(_) => {
                return Err(CoreError::Validation(format!(
                    "'{}' must be a hostname or a list of hostnames",
                    route.hostnames_value
                )));
            }
        };
        let route_hosts: Vec<String> = raw
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        if route_hosts.is_empty() {
            return Err(CoreError::Validation(format!(
                "The route enabled by '{}' needs at least one hostname in '{}': a route without \
                 hostnames would match every host on the shared keel-ingress Gateway",
                route.enabled_value.as_deref().unwrap_or("the catalog"),
                route.hostnames_value
            )));
        }
        if let Some(path) = &route.parent_refs_value {
            set_value_at_path(values, path, keel_ingress_parent_refs())?;
        }
        hostnames.extend(route_hosts);
    }
    Ok(hostnames)
}

/// Whether one of the schemas applying at a location (after `$ref`/combinator expansion)
/// explicitly allows arbitrary extra keys (`additionalProperties: true`).
fn allows_any_additional_property<'a>(resolver: &SchemaResolver<'a>, set: &[&'a Value]) -> bool {
    resolver
        .expand(set)
        .into_iter()
        .any(|node| node.get("additionalProperties") == Some(&Value::Bool(true)))
}

fn collect_undeclared_paths<'a>(
    resolver: &SchemaResolver<'a>,
    set: &[&'a Value],
    value: &Value,
    path: &str,
    out: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let child_set = resolver.property_schemas(set, key);
                if !child_set.is_empty() {
                    collect_undeclared_paths(resolver, &child_set, child, &child_path, out);
                } else if !allows_any_additional_property(resolver, set) {
                    out.push(child_path);
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let item_set = resolver.item_schemas(set, Some(i));
                collect_undeclared_paths(resolver, &item_set, item, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// Returns the paths (`a.b`, `list[0].c`) of submitted `values` the catalog `schema` does not
/// declare, treating every object schema as `additionalProperties: false` unless it explicitly
/// sets `additionalProperties: true` (or a schema for the extra keys).
///
/// Used for Helm catalog apps with [`crate::CatalogApplication::strict_values`]: upstream charts
/// accept far more values than the form exposes (extra routes, `extraDeploy`, sidecars,
/// ServiceAccounts), and those would bypass the locked values and route guards.
pub fn undeclared_value_paths(schema: &Value, values: &Value) -> Vec<String> {
    let resolver = SchemaResolver::new(schema);
    let mut out = Vec::new();
    collect_undeclared_paths(&resolver, &[schema], values, "", &mut out);
    out.sort();
    out
}

/// Returns the create-only `paths` (`spec.immutableValues`) whose value in `submitted` differs
/// from the `committed` values (a missing value equals `null`).
pub fn changed_immutable_values(
    paths: &[String],
    committed: &Value,
    submitted: &Value,
) -> Vec<String> {
    let at = |values: &Value, path: &str| {
        value_at_path(values, path)
            .filter(|v| !v.is_null())
            .cloned()
    };
    paths
        .iter()
        .filter(|path| at(committed, path) != at(submitted, path))
        .cloned()
        .collect()
}

/// Splits the secret values of a Helm app into one Helm values fragment per declared secret
/// field: `(dot path, YAML document)` such as `("gitea.admin.password", "gitea:\n  admin:\n
/// password: ...\n")`. Each fragment becomes one key of the SOPS-encrypted `<instance>-secrets`
/// `Secret` and one `HelmRelease.spec.valuesFrom` entry (merged without `targetPath`, so values
/// keep their exact string content). Secret fields submitted as `null`, `""`, `[]` or `{}` are
/// treated as not submitted (an Edit keeps the stored value).
///
/// Fails closed for secret fields inside lists (`a[].b`): a list fragment would replace the whole
/// list of the plaintext values. Mark the enclosing list itself secret instead.
pub fn helm_secret_value_fragments(
    secret_values: &Value,
    secret_field_paths: &[String],
) -> Result<std::collections::BTreeMap<String, String>, CoreError> {
    let mut out = std::collections::BTreeMap::new();
    for path in secret_field_paths {
        if path.contains('[') {
            let submitted_under_list = path
                .split_once('[')
                .map(|(prefix, _)| prefix)
                .filter(|p| !p.is_empty())
                .and_then(|prefix| value_at_path(secret_values, prefix))
                .is_some_and(|v| !v.is_null());
            if submitted_under_list {
                return Err(CoreError::Validation(format!(
                    "Secret field '{path}' is inside a list; Helm apps support secret fields in \
                     objects only (mark the whole list writeOnly instead)"
                )));
            }
            continue;
        }
        let Some(value) = value_at_path(secret_values, path) else {
            continue;
        };
        let empty = match value {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if empty {
            continue;
        }
        if !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(CoreError::Validation(format!(
                "Secret field path '{path}' must only contain [-._a-zA-Z0-9] to be stored as a \
                 Secret key"
            )));
        }
        let mut fragment = Value::Object(Map::new());
        set_value_at_path(&mut fragment, path, value.clone())?;
        out.insert(path.clone(), render_helm_values_yaml(&fragment)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::models::{AppRuntime, PlacementScope};

    const AUTHENTIK_CATALOG_YAML: &str = r#"
apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: authentik
  name: Authentik Identity Provider
  category: security-identity
  runtime: helm
spec:
  upstream:
    chart: authentik
    repoURL: https://charts.goauthentik.io
    version: "2026.10.1"
  progressiveDelivery:
    flaggerSupported: true
    defaultCanary: false
  schema:
    type: object
    required: [domain, adminEmail, secretKey]
    properties:
      domain:
        type: string
        title: Ingress Hostname
        format: hostname
      adminEmail:
        type: string
        title: Bootstrap Admin Email
        format: email
      secretKey:
        type: string
        title: Authentik Secret Key
        writeOnly: true
      smtp:
        type: object
        properties:
          host:
            type: string
          password:
            type: string
            format: password
      replicas:
        type: integer
        title: Worker Replicas
        default: 2
        minimum: 1
        maximum: 10
  uiSchema:
    adminEmail:
      ui:help: "Used for initial administrator recovery notifications."
"#;

    const POSTGRES_BLUEPRINT_YAML: &str = r#"
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: postgres-managed
  name: Managed PostgreSQL Cluster
  category: database
  provider: proxmox
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: true
  outputs:
    - db_host
    - db_port
  schema:
    type: object
    required: [storageGb, superuserPassword]
    properties:
      storageGb:
        type: integer
        minimum: 10
      superuserPassword:
        type: string
        writeOnly: true
"#;

    #[test]
    fn test_parse_catalog_application_and_blueprint() {
        let manifest = parse_catalog(AUTHENTIK_CATALOG_YAML).unwrap();
        let CatalogManifest::Application(app) = manifest else {
            panic!("expected CatalogApplication");
        };
        assert_eq!(app.metadata.id, "authentik");
        assert_eq!(app.metadata.runtime, AppRuntime::Helm);
        assert_eq!(app.spec.upstream.chart.as_deref(), Some("authentik"));
        assert!(app.spec.progressive_delivery.flagger_supported);
        assert!(!app.spec.progressive_delivery.default_canary);
        assert!(app.spec.ui_schema.is_some());

        let bp_manifest = parse_catalog(POSTGRES_BLUEPRINT_YAML).unwrap();
        let CatalogManifest::InfraBlueprint(bp) = bp_manifest else {
            panic!("expected CatalogInfraBlueprint");
        };
        assert_eq!(bp.metadata.id, "postgres-managed");
        assert_eq!(bp.metadata.provider, "proxmox");
        assert!(bp.spec.protect_by_default);
        assert_eq!(bp.spec.outputs, vec!["db_host", "db_port"]);
    }

    #[test]
    fn test_parse_catalog_rejects_invalid_headers_and_invariants() {
        let bad_api = AUTHENTIK_CATALOG_YAML.replace("keel.io/v1alpha1", "v1");
        assert!(parse_catalog(&bad_api).is_err());

        let bad_kind = AUTHENTIK_CATALOG_YAML.replace("CatalogApplication", "Deployment");
        assert!(parse_catalog(&bad_kind).is_err());

        // Helm app without chart
        let missing_chart = AUTHENTIK_CATALOG_YAML.replace("chart: authentik\n", "");
        assert!(parse_catalog_application(&missing_chart).is_err());

        // defaultCanary true when flaggerSupported false
        let bad_canary = AUTHENTIK_CATALOG_YAML
            .replace("flaggerSupported: true", "flaggerSupported: false")
            .replace("defaultCanary: false", "defaultCanary: true");
        assert!(parse_catalog_application(&bad_canary).is_err());
    }

    #[test]
    fn test_validate_schema_structured_field_errors() {
        let app = parse_catalog_application(AUTHENTIK_CATALOG_YAML).unwrap();

        let invalid_input = json!({
            "domain": "id.example.com",
            "adminEmail": "not-an-email",
            "replicas": 99
        });

        let err = validate_schema_values(&app.spec.schema, &invalid_input).unwrap_err();
        let CoreError::SchemaValidation(field_errors) = err else {
            panic!("expected SchemaValidation error, got {err:?}");
        };

        // Missing secretKey (root ""), invalid email ("/adminEmail"), replicas > 10 ("/replicas")
        assert!(field_errors.len() >= 3);
        assert!(field_errors.iter().any(|e| e.field_path == "/adminEmail"));
        assert!(field_errors.iter().any(|e| e.field_path == "/replicas"));
        assert!(
            field_errors
                .iter()
                .any(|e| e.field_path.is_empty() && e.message.contains("secretKey"))
        );

        let display_str = field_errors[0].to_string();
        assert!(!display_str.is_empty());
    }

    #[test]
    fn test_secret_extraction_partitioning_and_helm_values_rendering() {
        let app = parse_catalog_application(AUTHENTIK_CATALOG_YAML).unwrap();

        let paths = extract_secret_field_paths(&app.spec.schema);
        assert_eq!(paths, vec!["secretKey", "smtp.password"]);

        let valid_input = json!({
            "domain": "auth.keel.internal",
            "adminEmail": "admin@keel.internal",
            "secretKey": "top-secret-signing-key",
            "replicas": 3,
            "smtp": {
                "host": "smtp.keel.internal",
                "password": "smtp-secret-password"
            }
        });

        let prepared = prepare_helm_values(&app, &valid_input).unwrap();
        assert_eq!(
            prepared.partitioned.normal_values,
            json!({
                "domain": "auth.keel.internal",
                "adminEmail": "admin@keel.internal",
                "replicas": 3,
                "smtp": {
                    "host": "smtp.keel.internal"
                }
            })
        );
        assert_eq!(
            prepared.partitioned.secret_values,
            json!({
                "secretKey": "top-secret-signing-key",
                "smtp": {
                    "password": "smtp-secret-password"
                }
            })
        );

        // Verify unmodified Helm values.yaml does NOT leak any secret values
        assert!(prepared.values_yaml.contains("auth.keel.internal"));
        assert!(prepared.values_yaml.contains("smtp.keel.internal"));
        assert!(!prepared.values_yaml.contains("top-secret-signing-key"));
        assert!(!prepared.values_yaml.contains("smtp-secret-password"));

        let secrets_yaml = prepared.plaintext_secrets_yaml.unwrap();
        assert!(secrets_yaml.contains("top-secret-signing-key"));
        assert!(secrets_yaml.contains("smtp-secret-password"));

        // Non-object values rejected
        assert!(partition_values_by_schema(&app.spec.schema, &json!(["not-an-object"])).is_err());
        assert!(render_helm_values_yaml(&json!("scalar")).is_err());
    }

    #[test]
    fn test_combinators_and_array_secret_extraction() {
        let complex_schema = json!({
            "type": "object",
            "allOf": [
                {
                    "properties": {
                        "apiToken": {
                            "type": "string",
                            "writeOnly": true
                        }
                    }
                }
            ],
            "properties": {
                "webhooks": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "format": "password"
                    }
                }
            }
        });

        let paths = extract_secret_field_paths(&complex_schema);
        assert_eq!(paths, vec!["apiToken", "webhooks[]"]);

        let partitioned = partition_values_by_schema(
            &complex_schema,
            &json!({
                "apiToken": "tok_123",
                "webhooks": ["wh_sec_1", "wh_sec_2"],
                "publicFlag": true
            }),
        )
        .unwrap();

        assert_eq!(partitioned.normal_values, json!({ "publicFlag": true }));
        assert_eq!(
            partitioned.secret_values,
            json!({
                "apiToken": "tok_123",
                "webhooks": ["wh_sec_1", "wh_sec_2"]
            })
        );

        // Kustomize app without secrets produces plaintext_secrets_yaml == None
        let kustomize_yaml = r#"
apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: podinfo
  name: Podinfo
  category: demo
  runtime: kustomize
spec:
  upstream:
    repoURL: https://github.com/stefanprodan/podinfo
    path: ./kustomize
    ref: main
  schema:
    type: object
    properties:
      replicas:
        type: integer
        minimum: 1
      extraEnv:
        type: object
      endpoints:
        type: array
        items:
          type: object
          properties:
            url:
              type: string
            token:
              type: string
              writeOnly: true
"#;
        let kust_app = parse_catalog_application(kustomize_yaml).unwrap();
        assert_eq!(kust_app.metadata.runtime, AppRuntime::Kustomize);

        let no_secret_prepared = prepare_helm_values(
            &kust_app,
            &json!({
                "replicas": 2,
                "extraEnv": {}
            }),
        )
        .unwrap();
        assert!(no_secret_prepared.plaintext_secrets_yaml.is_none());

        let nested_array_prepared = prepare_helm_values(
            &kust_app,
            &json!({
                "replicas": 1,
                "endpoints": [
                    { "url": "https://hook.example.com", "token": "sec-1" }
                ]
            }),
        )
        .unwrap();
        assert!(nested_array_prepared.plaintext_secrets_yaml.is_some());
        assert!(!nested_array_prepared.values_yaml.contains("sec-1"));

        // Blueprint validation failure checks
        let bad_bp = POSTGRES_BLUEPRINT_YAML.replace("entrypoint: Pulumi.yaml", "entrypoint: \"\"");
        assert!(parse_catalog_blueprint(&bad_bp).is_err());
    }

    #[test]
    fn test_parse_catalog_bundle_and_dag_validation() {
        let valid_bundle_yaml = r#"
apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: k3s-platform
  name: K3s Cluster + Platform Stack
  category: platform
spec:
  items:
    - id: app
      kind: app
      catalogRef: podinfo
      dependsOn: [cluster, ingress]
      targetClusterFrom: cluster
      targetNamespace: default
      bindValues:
        ingress.host: "${items.cluster.outputs.ingress_domain}"
    - id: ingress
      kind: app
      catalogRef: envoy-gateway
      dependsOn: [cluster]
      targetClusterFrom: cluster
      targetNamespace: envoy-gateway-system
    - id: cluster
      kind: infra
      catalogRef: proxmox-k3s
  schema:
    type: object
    properties:
      cluster:
        type: object
        properties:
          nodeCount:
            type: integer
            default: 3
      ingress:
        type: object
      app:
        type: object
"#;
        let parsed = parse_catalog(valid_bundle_yaml).unwrap();
        let CatalogManifest::Bundle(bundle) = parsed else {
            panic!("Expected CatalogBundle");
        };
        assert_eq!(bundle.metadata.id, "k3s-platform");
        let order: Vec<&str> = bundle
            .topological_order()
            .unwrap()
            .into_iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(order, vec!["cluster", "ingress", "app"]);

        // Cycle detection
        let cycle_yaml = r#"
apiVersion: keel.io/v1alpha1
kind: CatalogBundle
metadata:
  id: cycle-bundle
  name: Cycle Bundle
  category: platform
spec:
  items:
    - id: a
      kind: infra
      catalogRef: kind-cluster
      dependsOn: [b]
    - id: b
      kind: infra
      catalogRef: kind-cluster
      dependsOn: [a]
  schema:
    type: object
"#;
        assert!(parse_catalog_bundle(cycle_yaml).is_err());

        // targetClusterFrom must be in dependsOn and reference an infra item
        let missing_dep_yaml = valid_bundle_yaml.replace("dependsOn: [cluster]", "dependsOn: []");
        assert!(parse_catalog_bundle(&missing_dep_yaml).is_err());

        // canary: true is valid on app items and rejected on infra items
        let valid_canary_yaml = valid_bundle_yaml.replace(
            "      catalogRef: podinfo",
            "      catalogRef: podinfo\n      canary: true",
        );
        let parsed_canary = parse_catalog_bundle(&valid_canary_yaml).unwrap();
        assert!(parsed_canary.spec.items[0].canary);

        let invalid_canary_yaml = valid_bundle_yaml.replace(
            "      catalogRef: proxmox-k3s",
            "      catalogRef: proxmox-k3s\n      canary: true",
        );
        assert!(parse_catalog_bundle(&invalid_canary_yaml).is_err());
    }

    #[test]
    fn test_parse_progressive_delivery_addon_catalog_apps() {
        for raw in [
            include_str!("../../../deploy/catalog/apps/envoy-gateway/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/prometheus/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/flagger/catalog.yaml"),
        ] {
            let app = parse_catalog_application(raw).expect("addon catalog.yaml should parse");
            app.validate().expect("addon catalog.yaml should validate");
            assert!(!app.spec.progressive_delivery.flagger_supported);
        }

        let bundle = parse_catalog_bundle(include_str!(
            "../../../deploy/catalog/bundles/kind-canary-platform/catalog.yaml"
        ))
        .expect("kind-canary-platform bundle should parse");
        assert_eq!(bundle.spec.items.len(), 5);

        let core_addons = parse_catalog_bundle(include_str!(
            "../../../deploy/catalog/bundles/canary-core-addons/catalog.yaml"
        ))
        .expect("canary-core-addons bundle should parse");
        assert_eq!(core_addons.spec.items.len(), 4);

        let gw_cfg = parse_catalog_blueprint(include_str!(
            "../../../deploy/catalog/infra/canary-gateway-config/catalog.yaml"
        ))
        .expect("canary-gateway-config blueprint should parse");
        assert_eq!(gw_cfg.metadata.id, "canary-gateway-config");
    }

    #[test]
    fn test_catalog_application_placement_scope() {
        // Absent placement defaults to namespaced and is not re-serialized.
        let app = parse_catalog_application(AUTHENTIK_CATALOG_YAML).unwrap();
        assert_eq!(app.spec.placement.scope, PlacementScope::Namespaced);
        assert!(!app.spec.placement.requires_remote_cluster());
        assert!(app.ensure_placement_allows(None).is_ok());
        assert!(
            app.ensure_placement_allows(Some("workload-kubeconfig"))
                .is_ok()
        );
        let round_trip = crate::yaml::to_string(&app).unwrap();
        assert!(!round_trip.contains("placement"));

        let with_scope = |scope: &str| {
            AUTHENTIK_CATALOG_YAML.replace(
                "  progressiveDelivery:",
                &format!("  placement:\n    scope: {scope}\n  progressiveDelivery:"),
            )
        };
        let namespaced = parse_catalog_application(&with_scope("namespaced")).unwrap();
        assert_eq!(namespaced.spec.placement.scope, PlacementScope::Namespaced);

        let cluster = parse_catalog_application(&with_scope("cluster")).unwrap();
        assert_eq!(cluster.spec.placement.scope, PlacementScope::Cluster);
        assert!(cluster.spec.placement.requires_remote_cluster());
        let err = cluster
            .ensure_placement_allows(None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("spec.placement.scope: cluster"), "{err}");
        assert!(err.contains("remote workload cluster"), "{err}");
        assert!(cluster.ensure_placement_allows(Some("  ")).is_err());
        assert!(
            cluster
                .ensure_placement_allows(Some("workload-kubeconfig"))
                .is_ok()
        );
        assert!(
            crate::yaml::to_string(&cluster)
                .unwrap()
                .contains("scope: cluster")
        );

        // Unknown scopes are rejected by the parser.
        assert!(parse_catalog_application(&with_scope("global")).is_err());
        assert_eq!(
            "Cluster".parse::<PlacementScope>().unwrap(),
            PlacementScope::Cluster
        );
        assert!("global".parse::<PlacementScope>().is_err());
        assert_eq!(PlacementScope::Namespaced.to_string(), "namespaced");
    }

    #[test]
    fn test_manifest_cluster_scoped_kinds_and_placement_guard() {
        let manifests = r#"
apiVersion: v1
kind: Namespace
metadata:
  name: ${ns}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cfg
---
# comment-only and malformed documents are skipped
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: reader
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: reader-2
---
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: hook
"#;
        assert_eq!(
            manifest_cluster_scoped_kinds(manifests),
            vec![
                "Namespace".to_string(),
                "ClusterRole".to_string(),
                "ValidatingWebhookConfiguration".to_string()
            ]
        );
        let namespaced_only = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n---\napiVersion: rbac.authorization.k8s.io/v1\nkind: RoleBinding\nmetadata:\n  name: b\n";
        assert!(manifest_cluster_scoped_kinds(namespaced_only).is_empty());
        assert!(manifest_cluster_scoped_kinds("not: [valid").is_empty());

        let mut app = parse_catalog_application(AUTHENTIK_CATALOG_YAML).unwrap();
        assert!(
            app.ensure_manifest_matches_placement(namespaced_only)
                .is_ok()
        );
        let err = app
            .ensure_manifest_matches_placement(manifests)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Namespace, ClusterRole"), "{err}");
        assert!(err.contains("placementScope: cluster"), "{err}");
        app.spec.placement.scope = PlacementScope::Cluster;
        assert!(app.ensure_manifest_matches_placement(manifests).is_ok());

        // The shipped manifest apps stay namespaced-only.
        for raw in [
            include_str!("../../../deploy/catalog/apps/rauthy/manifests.yaml"),
            include_str!("../../../deploy/catalog/apps/http-echo-manifest/manifests.yaml"),
        ] {
            assert!(manifest_cluster_scoped_kinds(raw).is_empty());
        }
    }

    /// The shipped `http-echo-manifest` keeps its secret `apiToken` as a Flux placeholder: Keel
    /// renders only the non-secret values into Git.
    #[test]
    fn http_echo_manifest_leaves_the_secret_token_to_flux() {
        let raw = include_str!("../../../deploy/catalog/apps/http-echo-manifest/manifests.yaml");
        let rendered = render_manifest_template(
            raw,
            &serde_json::json!({ "echoText": "hello", "replicas": 2 }),
        )
        .unwrap();
        assert!(rendered.contains("- -text=hello"), "{rendered}");
        assert!(rendered.contains("token: \"${apiToken}\""), "{rendered}");
        let docs = verify_rendered_manifest_structure(raw, &rendered).unwrap();
        let kinds: Vec<_> = docs
            .iter()
            .filter_map(|d| d.get("kind").and_then(Value::as_str))
            .collect();
        assert_eq!(kinds, ["Deployment", "Service", "Secret"]);
        let app = parse_catalog_application(include_str!(
            "../../../deploy/catalog/apps/http-echo-manifest/catalog.yaml"
        ))
        .unwrap();
        let token = &app.spec.schema["properties"]["apiToken"];
        assert_eq!(token["format"], "password");
        assert!(
            token.get("default").is_none(),
            "secret fields are never defaulted"
        );
    }

    #[test]
    fn test_catalog_application_placement_namespace() {
        let with_placement = |placement: &str| {
            AUTHENTIK_CATALOG_YAML.replace(
                "  progressiveDelivery:",
                &format!("  placement:\n{placement}  progressiveDelivery:"),
            )
        };
        // No dedicated namespace: the requested namespace passes through (trimmed).
        let plain = parse_catalog_application(AUTHENTIK_CATALOG_YAML).unwrap();
        assert_eq!(plain.resolve_target_namespace(None).unwrap(), None);
        assert_eq!(plain.resolve_target_namespace(Some("  ")).unwrap(), None);
        assert_eq!(
            plain.resolve_target_namespace(Some(" apps ")).unwrap(),
            Some("apps".to_string())
        );

        let dedicated = parse_catalog_application(&with_placement(
            "    scope: cluster\n    namespace: flux-system\n",
        ))
        .unwrap();
        assert_eq!(
            dedicated.spec.placement.namespace.as_deref(),
            Some("flux-system")
        );
        assert!(
            crate::yaml::to_string(&dedicated)
                .unwrap()
                .contains("namespace: flux-system")
        );
        for requested in [None, Some(""), Some("flux-system"), Some(" flux-system ")] {
            assert_eq!(
                dedicated.resolve_target_namespace(requested).unwrap(),
                Some("flux-system".to_string()),
                "{requested:?}"
            );
        }
        let err = dedicated
            .resolve_target_namespace(Some("tenant-acme"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("spec.placement.namespace"), "{err}");
        assert!(err.contains("tenant-acme"), "{err}");

        // Only for scope: cluster, a valid DNS label and never a tenant namespace.
        for bad in [
            "    namespace: flux-system\n",
            "    scope: namespaced\n    namespace: flux-system\n",
            "    scope: cluster\n    namespace: Flux_System\n",
            "    scope: cluster\n    namespace: tenant-acme\n",
        ] {
            assert!(
                parse_catalog_application(&with_placement(bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn test_catalog_application_placement_additional_namespaces() {
        let manifest_app = |placement: &str, runtime: &str| {
            format!(
                "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: multi\n  \
                 name: Multi\n  description: d\n  category: c\n  runtime: {runtime}\nspec:\n  \
                 upstream:\n    chart: multi\n    repoURL: https://example.com/charts\n    \
                 version: \"1.0.0\"\n  placement:\n{placement}  schema:\n    type: object\n"
            )
        };
        let app = parse_catalog_application(&manifest_app(
            "    scope: cluster\n    namespace: keel-system\n    additionalNamespaces: [keel-previews, flux-system]\n",
            "manifest",
        ))
        .unwrap();
        assert_eq!(
            app.multi_namespace_placement(),
            vec!["keel-system", "keel-previews", "flux-system"]
        );
        assert!(
            crate::yaml::to_string(&app)
                .unwrap()
                .contains("additionalNamespaces:")
        );
        // Single-namespace apps keep the Flux `targetNamespace` path (and serialize unchanged).
        let single = parse_catalog_application(&manifest_app(
            "    scope: cluster\n    namespace: keel-system\n",
            "manifest",
        ))
        .unwrap();
        assert!(single.multi_namespace_placement().is_empty());
        assert!(
            !crate::yaml::to_string(&single)
                .unwrap()
                .contains("additionalNamespaces")
        );
        single
            .ensure_manifest_namespaces("kind: ConfigMap\nmetadata:\n  name: x\n")
            .unwrap();

        let rendered = "apiVersion: v1\nkind: ServiceAccount\nmetadata:\n  name: sa\n  namespace: keel-system\n---\n\
            apiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata:\n  name: cr\n---\n\
            apiVersion: rbac.authorization.k8s.io/v1\nkind: Role\nmetadata:\n  name: r\n  namespace: flux-system\n";
        app.ensure_manifest_namespaces(rendered).unwrap();
        let bad = format!(
            "{rendered}---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: loose\n---\n\
             apiVersion: v1\nkind: Secret\nmetadata:\n  name: elsewhere\n  namespace: kube-system\n"
        );
        let allowed: Vec<String> = app.multi_namespace_placement();
        assert_eq!(
            manifest_objects_outside_namespaces(&bad, &allowed),
            vec!["ConfigMap/loose", "Secret/elsewhere (kube-system)"]
        );
        let err = app
            .ensure_manifest_namespaces(&bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ConfigMap/loose"), "{err}");
        assert!(
            err.contains("keel-system, keel-previews, flux-system"),
            "{err}"
        );

        for (placement, runtime) in [
            // Needs a dedicated namespace (and therefore scope: cluster).
            (
                "    scope: cluster\n    additionalNamespaces: [keel-previews]\n",
                "manifest",
            ),
            // A HelmRelease installs into exactly one namespace.
            (
                "    scope: cluster\n    namespace: keel-system\n    additionalNamespaces: [keel-previews]\n",
                "helm",
            ),
            (
                "    scope: cluster\n    namespace: keel-system\n    additionalNamespaces: [tenant-acme]\n",
                "manifest",
            ),
            (
                "    scope: cluster\n    namespace: keel-system\n    additionalNamespaces: [Bad_NS]\n",
                "manifest",
            ),
            (
                "    scope: cluster\n    namespace: keel-system\n    additionalNamespaces: [keel-system]\n",
                "manifest",
            ),
            (
                "    scope: cluster\n    namespace: keel-system\n    additionalNamespaces: [a, a]\n",
                "manifest",
            ),
        ] {
            assert!(
                parse_catalog_application(&manifest_app(placement, runtime)).is_err(),
                "{placement} ({runtime})"
            );
        }
    }

    #[test]
    fn test_built_in_catalog_app_placement_scopes() {
        // Charts verified (helm template, default values) to install CRDs, ClusterRoles/Bindings or
        // webhooks (dex 0.21.0 and authentik 2024.12.2 render a ClusterRole/ClusterRoleBinding).
        for raw in [
            include_str!("../../../deploy/catalog/apps/cert-manager/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/envoy-gateway/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/flagger/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/prometheus/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/dex/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/authentik/catalog.yaml"),
            // Workload-cluster twins of the management operators (cluster-wide CRDs, RBAC and
            // webhooks: Kyverno, PKO and Flux each own their CRDs once per cluster).
            include_str!("../../../deploy/catalog/apps/kyverno/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/pulumi-kubernetes-operator/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/flux2/catalog.yaml"),
            // keel-server ships ClusterRoles/ClusterRoleBindings (read-only CRD watch, Kyverno
            // policies, preview workspace delegation).
            include_str!("../../../deploy/catalog/apps/keel-server/catalog.yaml"),
        ] {
            let app = parse_catalog_application(raw).unwrap();
            assert_eq!(
                app.spec.placement.scope,
                PlacementScope::Cluster,
                "{} must be scope: cluster",
                app.metadata.id
            );
        }
        for raw in [
            include_str!("../../../deploy/catalog/apps/podinfo/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/http-echo-manifest/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/rauthy/catalog.yaml"),
            // `lockedValues` turn the charts' cluster-scoped RBAC off (seaweedfs
            // `global.seaweedfs.createClusterRole: false`; forgejo renders none).
            include_str!("../../../deploy/catalog/apps/forgejo/catalog.yaml"),
            include_str!("../../../deploy/catalog/apps/seaweedfs/catalog.yaml"),
        ] {
            let app = parse_catalog_application(raw).unwrap();
            assert_eq!(
                app.spec.placement.scope,
                PlacementScope::Namespaced,
                "{} must stay namespaced",
                app.metadata.id
            );
        }
    }

    #[test]
    fn test_validate_k8s_manifests_yaml_and_template_rendering() {
        let multi_doc = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: ${app_name:=demo-app}-config
data:
  LOG_LEVEL: ${log_level:=info}
  SECRET_TOKEN: ${secret_token}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ${app_name:=demo-app}
spec:
  replicas: ${replicas:=2}
"#;
        let docs = validate_k8s_manifests_yaml(multi_doc).expect("valid multi-doc K8s YAML");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Deployment");

        let vars = extract_manifest_variables(multi_doc);
        assert_eq!(
            vars,
            vec![
                "app_name".to_string(),
                "log_level".to_string(),
                "secret_token".to_string(),
                "replicas".to_string(),
            ]
        );

        let rendered = render_manifest_template(
            multi_doc,
            &json!({
                "app_name": "my-service",
                "replicas": 3
            }),
        )
        .expect("render template");
        assert!(rendered.contains("name: my-service-config"));
        assert!(rendered.contains("LOG_LEVEL: info"));
        assert!(rendered.contains("SECRET_TOKEN: ${secret_token}"));
        assert!(rendered.contains("replicas: 3"));

        // Invalid YAML cases
        assert!(validate_k8s_manifests_yaml("   ").is_err());
        assert!(validate_k8s_manifests_yaml("---\n---").is_err());
        assert!(validate_k8s_manifests_yaml("apiVersion: v1\nkind: ConfigMap\n").is_err());
        assert!(validate_k8s_manifests_yaml("kind: ConfigMap\nmetadata:\n  name: test\n").is_err());
    }

    #[test]
    fn test_render_manifest_template_quotes_whole_scalars_and_blocks_injection() {
        let template = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ${name}\ndata:\n  msg: ${msg} # comment\n  list:\n    - ${item}\n  image: repo/${tag}:v1\n";

        // Injection attempt in a whole-scalar position is neutralised by quoting.
        let injected = "x\n---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: pwned";
        let rendered = render_manifest_template(
            template,
            &json!({"name": "cm", "msg": injected, "item": "true", "tag": "app"}),
        )
        .expect("whole-scalar values are quoted");
        assert!(rendered.contains("name: \"cm\""));
        assert!(rendered.contains("- \"true\""));
        assert!(rendered.contains("image: repo/app:v1"));
        let docs =
            verify_rendered_manifest_structure(template, &rendered).expect("structure preserved");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["data"]["msg"], injected);
        assert_eq!(docs[0]["data"]["list"][0], "true");

        // Strings that look like other YAML types stay strings.
        for tricky in ["", "123", "-leading", "null", "a: b", "'q'"] {
            let rendered = render_manifest_template(
                template,
                &json!({"name": "cm", "msg": tricky, "tag": "app"}),
            )
            .unwrap();
            let docs = verify_rendered_manifest_structure(template, &rendered).unwrap();
            assert_eq!(docs[0]["data"]["msg"], tricky);
        }

        // Embedded placeholders reject structural characters and newlines.
        for bad in [
            "a\n---\nkind: Secret",
            "a\rb",
            "x\"y",
            "x # c",
            "x: y",
            "{a}",
            "[a]",
            "-x",
            "a\tb",
        ] {
            let err =
                render_manifest_template(template, &json!({"name": "cm", "msg": "ok", "tag": bad}))
                    .expect_err("embedded value must be rejected");
            assert!(matches!(err, CoreError::Validation(_)), "{bad:?}: {err}");
        }

        // Structured values may only be used as whole values.
        assert!(render_manifest_template(template, &json!({"tag": {"a": 1}})).is_err());
        let rendered =
            render_manifest_template(template, &json!({"name": "cm", "msg": {"k": "v\n"}}))
                .unwrap();
        let docs = verify_rendered_manifest_structure(template, &rendered).unwrap();
        assert_eq!(docs[0]["data"]["msg"]["k"], "v\n");

        // Numbers and booleans are rendered verbatim.
        let rendered = render_manifest_template(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: d\nspec:\n  replicas: ${r}\n  paused: ${p}\n",
            &json!({"r": 3, "p": false}),
        )
        .unwrap();
        assert!(rendered.contains("replicas: 3"));
        assert!(rendered.contains("paused: false"));
    }

    #[test]
    fn test_verify_rendered_manifest_structure_rejects_new_documents_and_kinds() {
        let template = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cm\n";
        let extra_doc =
            format!("{template}---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: pwned\n");
        let err = verify_rendered_manifest_structure(template, &extra_doc).unwrap_err();
        assert!(err.to_string().contains("document"));

        let changed_kind = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: cm\n";
        let err = verify_rendered_manifest_structure(template, changed_kind).unwrap_err();
        assert!(err.to_string().contains("kind"));

        let invalid = "apiVersion: v1\nkind: ConfigMap\nmetadata: [\n";
        assert!(matches!(
            verify_rendered_manifest_structure(template, invalid),
            Err(CoreError::Validation(_))
        ));

        // Templated kinds are not pinned.
        let tpl_kind = "apiVersion: v1\nkind: ${k}\nmetadata:\n  name: cm\n";
        assert!(verify_rendered_manifest_structure(tpl_kind, template).is_ok());
    }

    #[test]
    fn test_parse_and_validate_all_seed_infra_blueprints() {
        use crate::models::InfraResourceKind;

        let test_cases = vec![
            (
                "../../deploy/catalog/infra/kind-cluster/catalog.yaml",
                InfraResourceKind::Cluster,
                true,
            ),
            (
                "../../deploy/catalog/infra/proxmox-k3s-cluster/catalog.yaml",
                InfraResourceKind::Cluster,
                true,
            ),
            (
                "../../deploy/catalog/infra/aws-eks-cluster/catalog.yaml",
                InfraResourceKind::Cluster,
                true,
            ),
            (
                "../../deploy/catalog/infra/gcp-gke-cluster/catalog.yaml",
                InfraResourceKind::Cluster,
                true,
            ),
            (
                "../../deploy/catalog/infra/azure-aks-cluster/catalog.yaml",
                InfraResourceKind::Cluster,
                true,
            ),
            (
                "../../deploy/catalog/infra/aws-vm/catalog.yaml",
                InfraResourceKind::Vm,
                false,
            ),
            (
                "../../deploy/catalog/infra/azure-vm/catalog.yaml",
                InfraResourceKind::Vm,
                false,
            ),
            (
                "../../deploy/catalog/infra/gcp-vm/catalog.yaml",
                InfraResourceKind::Vm,
                false,
            ),
            (
                "../../deploy/catalog/infra/postgres-managed/catalog.yaml",
                InfraResourceKind::Service,
                false,
            ),
            (
                "../../deploy/catalog/infra/canary-gateway-config/catalog.yaml",
                InfraResourceKind::Service,
                false,
            ),
        ];

        for (path, expected_kind, expects_kubeconfig_secret) in test_cases {
            let yaml =
                std::fs::read_to_string(path).unwrap_or_else(|_| panic!("Failed to read {path}"));
            let bp = parse_catalog_blueprint(&yaml).expect("Failed to parse blueprint");

            assert_eq!(bp.effective_resource_kind(), expected_kind);

            // Check Pulumi.yaml
            let pulumi_yaml_path = std::path::Path::new(path)
                .parent()
                .unwrap()
                .join("Pulumi.yaml");
            let pulumi_yaml =
                std::fs::read_to_string(&pulumi_yaml_path).expect("Failed to read Pulumi.yaml");

            let parsed_pulumi: noyalib::Value = noyalib::from_str(&pulumi_yaml).unwrap();
            assert!(parsed_pulumi.is_mapping());

            if expects_kubeconfig_secret {
                let k8s_version_default = bp
                    .spec
                    .schema
                    .pointer("/properties/kubernetesVersion/default")
                    .and_then(|v| v.as_str())
                    .expect("Cluster blueprint must define properties.kubernetesVersion.default");
                assert!(
                    k8s_version_default.starts_with("1.35")
                        || k8s_version_default.starts_with("v1.35"),
                    "Expected cluster blueprint {} kubernetesVersion default to be 1.35.x, got {k8s_version_default}",
                    bp.metadata.id
                );

                let pulumi_k8s_default = parsed_pulumi
                    .get("config")
                    .and_then(|c| c.get("kubernetesVersion"))
                    .and_then(|kv| kv.get("default"))
                    .and_then(|d| d.as_str())
                    .expect("Cluster Pulumi.yaml must define config.kubernetesVersion.default");
                assert!(
                    pulumi_k8s_default.starts_with("1.35")
                        || pulumi_k8s_default.starts_with("v1.35"),
                    "Expected Pulumi.yaml {} config.kubernetesVersion.default to be 1.35.x, got {pulumi_k8s_default}",
                    bp.metadata.id
                );

                let resources = parsed_pulumi
                    .get("resources")
                    .unwrap()
                    .as_mapping()
                    .unwrap();
                let kubeconfig_secret = resources
                    .get("kubeconfigSecret")
                    .expect("Missing kubeconfigSecret in cluster blueprint");
                let resource_type = kubeconfig_secret.get("type").unwrap().as_str().unwrap();
                assert_eq!(resource_type, "kubernetes:core/v1:Secret");

                // Keel sets these when the blueprint outputs `kubeconfig` (proposal flow), so the
                // program must declare them or Pulumi rejects the Stack config.
                for key in ["kubeconfigSecretName", "kubeconfigSecretNamespace"] {
                    assert!(
                        parsed_pulumi
                            .get("config")
                            .and_then(|c| c.get(key))
                            .is_some(),
                        "cluster blueprint {} must declare config.{key}",
                        bp.metadata.id
                    );
                }
                assert!(
                    bp.spec.outputs.iter().any(|o| o == "kubeconfig"),
                    "cluster blueprint {} must output kubeconfig",
                    bp.metadata.id
                );

                // Flux impersonates `tenant-<id>-sa` through the kubeconfig Secret, so the program
                // must grant that identity on the *workload* cluster (not the management one).
                let tenant_flux_binding = resources
                    .values()
                    .find(|r| {
                        r.get("type").and_then(|t| t.as_str())
                            == Some("kubernetes:rbac.authorization.k8s.io/v1:ClusterRoleBinding")
                            && r.get("properties")
                                .and_then(|p| p.get("metadata"))
                                .and_then(|m| m.get("name"))
                                .and_then(|n| n.as_str())
                                == Some("keel-tenant-flux")
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "cluster blueprint {} must bind ClusterRoleBinding keel-tenant-flux",
                            bp.metadata.id
                        )
                    });
                assert_eq!(
                    tenant_flux_binding
                        .get("options")
                        .and_then(|o| o.get("provider"))
                        .and_then(|p| p.as_str()),
                    Some("${workloadProvider}"),
                    "{}: keel-tenant-flux must target the workload cluster",
                    bp.metadata.id
                );
                let subject = tenant_flux_binding
                    .get("properties")
                    .and_then(|p| p.get("subjects"))
                    .and_then(|s| s.get(0))
                    .expect("keel-tenant-flux must have a subject");
                assert_eq!(
                    subject.get("name").and_then(|n| n.as_str()),
                    Some("${kubeconfigSecretNamespace}-sa")
                );
                assert_eq!(
                    subject.get("namespace").and_then(|n| n.as_str()),
                    Some("${kubeconfigSecretNamespace}")
                );

                // Flux controllers cannot run exec credential plugins, so the kubeconfig Flux reads
                // must carry static credentials (resolving a `${variable}` indirection once).
                let kubeconfig = kubeconfig_secret
                    .get("properties")
                    .and_then(|p| p.get("stringData"))
                    .and_then(|s| s.get("kubeconfig"))
                    .expect("kubeconfigSecret must set stringData.kubeconfig");
                let mut kubeconfig_text = noyalib::to_string(kubeconfig).unwrap();
                if let Some(var) = kubeconfig
                    .as_str()
                    .and_then(|s| s.strip_prefix("${"))
                    .and_then(|s| s.strip_suffix('}'))
                    && let Some(resolved) = parsed_pulumi.get("variables").and_then(|v| v.get(var))
                {
                    kubeconfig_text.push_str(&noyalib::to_string(resolved).unwrap());
                }
                assert!(
                    !kubeconfig_text.contains("exec:"),
                    "{}: Flux kubeconfig Secret must not use an exec credential plugin",
                    bp.metadata.id
                );
            }

            // Test protected/tenant serialization round-trips
            let roundtrip: String = crate::yaml::to_string(&bp).unwrap();
            let parsed2 = parse_catalog_blueprint(&roundtrip).unwrap();
            assert_eq!(bp.metadata.protected, parsed2.metadata.protected);
            assert_eq!(bp.metadata.tenant, parsed2.metadata.tenant);
        }
    }

    /// The seeded Proxmox blueprint is a real k3s cluster (LIVE-10): it installs k3s over SSH
    /// with a generated key, keeps the admin kubeconfig secret and out of the logs, and writes it
    /// to the tenant kubeconfig Secret Flux targets, like the Stage 0 Proxmox program.
    #[test]
    fn proxmox_k3s_blueprint_installs_k3s_and_exports_a_secret_kubeconfig() {
        use noyalib::Value;
        let dir = std::path::Path::new("../../deploy/catalog/infra/proxmox-k3s-cluster");
        let bp =
            parse_catalog_blueprint(&std::fs::read_to_string(dir.join("catalog.yaml")).unwrap())
                .unwrap();
        let doc: Value =
            noyalib::from_str(&std::fs::read_to_string(dir.join("Pulumi.yaml")).unwrap()).unwrap();

        assert_eq!(
            bp.effective_resource_kind(),
            crate::models::InfraResourceKind::Cluster
        );
        assert_eq!(
            doc["packages"]["proxmox"]["parameters"][0].as_str(),
            Some("bpg/proxmox")
        );

        // Every non-secret form field is a declared Pulumi config key (populated into
        // `Stack.spec.config`); secret fields become env refs and must not be config keys.
        let config = doc["config"].as_mapping().unwrap();
        let props = bp.spec.schema["properties"].as_object().unwrap();
        for (key, prop) in props {
            let secret = prop.get("writeOnly").and_then(|v| v.as_bool()) == Some(true)
                || prop.get("format").and_then(|v| v.as_str()) == Some("password");
            assert_eq!(
                config.contains_key(key.as_str()),
                !secret,
                "schema property {key}: secret={secret}"
            );
        }
        for secret in ["PROXMOX_VE_ENDPOINT", "PROXMOX_VE_API_TOKEN"] {
            assert_eq!(props[secret]["writeOnly"], serde_json::json!(true));
        }
        assert_eq!(props["PROXMOX_VE_API_TOKEN"]["format"], "password");
        // Config without a default must be a required form field.
        let required: Vec<&str> = bp.spec.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for (key, spec) in config {
            if spec.get("default").is_none() {
                assert!(
                    required.contains(&key.as_str()),
                    "config {key} needs a default"
                );
            }
        }
        // Defaults of the form and the program agree.
        for (key, prop) in props {
            if let (Some(form), Some(spec)) = (prop.get("default"), config.get(key.as_str())) {
                let program = &spec["default"];
                let same = match form {
                    serde_json::Value::String(s) => program.as_str() == Some(s.as_str()),
                    serde_json::Value::Number(n) => program.as_i64() == n.as_i64(),
                    serde_json::Value::Bool(b) => program.as_bool() == Some(*b),
                    _ => false,
                };
                assert!(
                    same,
                    "default of {key} differs between catalog.yaml and Pulumi.yaml"
                );
            }
        }

        let resources = &doc["resources"];
        assert_eq!(
            resources["controlPlaneSshKey"]["type"].as_str(),
            Some("tls:index:PrivateKey")
        );
        let vm = &resources["k3sControlPlaneVm"];
        assert_eq!(
            vm["type"].as_str(),
            Some("proxmox:index/virtualEnvironmentVm:VirtualEnvironmentVm")
        );
        assert_eq!(vm["properties"]["agent"]["enabled"], Value::Bool(true));
        assert_eq!(
            vm["properties"]["initialization"]["userAccount"]["keys"][0].as_str(),
            Some("${controlPlaneSshKey.publicKeyOpenssh}")
        );
        assert_eq!(
            doc["variables"]["controlPlaneIp"].as_str(),
            Some("${k3sControlPlaneVm.ipv4Addresses[1][0]}")
        );

        let install = resources["installK3s"]["properties"]["create"]
            .as_str()
            .unwrap();
        assert!(install.contains("INSTALL_K3S_CHANNEL=\"${kubernetesVersion}\""));
        // The install ignores `create` changes, so the channel only applies at creation; the
        // form must say so instead of implying an edit upgrades k3s.
        assert!(
            resources["installK3s"]["options"]["ignoreChanges"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("create"))
        );
        assert!(
            props["kubernetesVersion"]["description"]
                .as_str()
                .unwrap()
                .contains("does not upgrade k3s")
        );
        assert!(install.contains(
            "server --disable=traefik --tls-san=${controlPlaneIp} --write-kubeconfig-mode=644"
        ));

        let fetch = &resources["fetchKubeconfig"];
        assert_eq!(fetch["type"].as_str(), Some("command:remote:Command"));
        assert_eq!(
            fetch["options"]["dependsOn"][0].as_str(),
            Some("${installK3s}")
        );
        assert_eq!(
            fetch["properties"]["connection"]["privateKey"].as_str(),
            Some("${controlPlaneSshKey.privateKeyOpenssh}")
        );
        assert_eq!(fetch["properties"]["logging"].as_str(), Some("none"));
        assert_eq!(
            fetch["options"]["additionalSecretOutputs"][0].as_str(),
            Some("stdout")
        );
        let create = fetch["properties"]["create"].as_str().unwrap();
        assert!(create.contains("/etc/rancher/k3s/k3s.yaml"));
        assert!(create.contains("s/127.0.0.1/${controlPlaneIp}/g"));

        // Flux reads the kubeconfig Secret; Pulumi reaches the new cluster with the same one.
        assert_eq!(
            doc["variables"]["workloadKubeconfig"]["fn::secret"].as_str(),
            Some("${fetchKubeconfig.stdout}")
        );
        let secret = &resources["kubeconfigSecret"]["properties"];
        assert_eq!(
            secret["metadata"]["name"].as_str(),
            Some("${kubeconfigSecretName}")
        );
        assert_eq!(
            secret["metadata"]["namespace"].as_str(),
            Some("${kubeconfigSecretNamespace}")
        );
        assert_eq!(
            secret["stringData"]["kubeconfig"].as_str(),
            Some("${workloadKubeconfig}")
        );
        assert_eq!(
            resources["tenantFluxAccess"]["properties"]["subjects"][0]["name"].as_str(),
            Some("${kubeconfigSecretNamespace}-sa")
        );

        let outputs = &doc["outputs"];
        assert_eq!(
            outputs["kubeconfig"]["fn::secret"].as_str(),
            Some("${fetchKubeconfig.stdout}")
        );
        for out in &bp.spec.outputs {
            assert!(outputs.get(out.as_str()).is_some(), "missing output {out}");
        }
        for (key, value) in outputs.as_mapping().unwrap() {
            assert!(
                !value.as_str().unwrap_or_default().contains("privateKey"),
                "output {key} must not export the SSH private key"
            );
        }
    }

    fn image_app(runtime: &str, upstream: &str) -> String {
        format!(
            "apiVersion: keel.io/v1alpha1\nkind: CatalogApplication\nmetadata:\n  id: rauthy\n  name: Rauthy\n  category: management-core\n  runtime: {runtime}\nspec:\n  upstream:\n{upstream}  schema:\n    type: object\n    properties: {{}}\n"
        )
    }

    #[test]
    fn image_based_kustomize_application_needs_no_repo_url() {
        let app = parse_catalog_application(&image_app(
            "kustomize",
            "    image: ghcr.io/sebadob/rauthy\n    version: \"0.37.2\"\n",
        ))
        .unwrap();
        assert_eq!(
            app.spec.upstream.image.as_deref(),
            Some("ghcr.io/sebadob/rauthy")
        );
        assert_eq!(app.spec.upstream.version.as_deref(), Some("0.37.2"));
        assert!(app.spec.upstream.repo_url.is_empty());
        // Round-trips under the `image` key and omits it when unset.
        let yaml = crate::yaml::to_string(&app).unwrap();
        assert!(yaml.contains("image: ghcr.io/sebadob/rauthy"));
        let authentik = parse_catalog_application(AUTHENTIK_CATALOG_YAML).unwrap();
        assert!(authentik.spec.upstream.image.is_none());
        assert!(
            !crate::yaml::to_string(&authentik)
                .unwrap()
                .contains("image:")
        );
    }

    #[test]
    fn image_upstream_is_validated() {
        // Without an image, kustomize still requires repoURL.
        let err = parse_catalog_application(&image_app("kustomize", "    version: \"1.0.0\"\n"))
            .unwrap_err();
        assert!(err.to_string().contains("repoURL"), "{err}");
        for (runtime, upstream, needle) in [
            (
                "helm",
                "    image: ghcr.io/a/b\n    version: \"1\"\n    chart: b\n    repoURL: https://x\n",
                "only supported for runtime: kustomize",
            ),
            (
                "kustomize",
                "    image: ghcr.io/a/b\n",
                "requires spec.upstream.version",
            ),
            (
                "kustomize",
                "    image: ghcr.io/a/b:1.0\n    version: \"1.0\"\n",
                "must not contain a tag",
            ),
            (
                "kustomize",
                "    image: ghcr.io/a/b@sha256:abc\n    version: \"1.0\"\n",
                "digest",
            ),
            (
                "kustomize",
                "    image: \"ghcr.io/a b\"\n    version: \"1.0\"\n",
                "whitespace",
            ),
            (
                "kustomize",
                "    image: \"\"\n    version: \"1.0\"\n",
                "1-255",
            ),
            (
                "kustomize",
                "    image: ghcr.io//b\n    version: \"1.0\"\n",
                "repository path",
            ),
        ] {
            let err = parse_catalog_application(&image_app(runtime, upstream)).unwrap_err();
            assert!(err.to_string().contains(needle), "{upstream}: {err}");
        }
        // Self-service (non-management-core) apps cannot be image-based: the tenant install path
        // only synthesizes HelmRelease/HelmRepository objects from `repoURL`.
        let tenant_app = image_app(
            "kustomize",
            "    image: ghcr.io/sebadob/rauthy\n    version: \"0.37.2\"\n",
        )
        .replace("category: management-core", "category: identity");
        let err = parse_catalog_application(&tenant_app).unwrap_err();
        assert!(err.to_string().contains("metadata.category"), "{err}");
        // A registry host with a port is not a tag.
        parse_catalog_application(&image_app(
            "kustomize",
            "    image: registry.local:5000/team/app\n    version: \"2.0.0\"\n",
        ))
        .unwrap();
    }

    #[test]
    fn builtin_rauthy_core_catalog_is_image_based_and_version_only() {
        let app = parse_catalog_application(include_str!(
            "../../../deploy/catalog/core/rauthy/catalog.yaml"
        ))
        .unwrap();
        assert_eq!(app.metadata.runtime, AppRuntime::Kustomize);
        assert_eq!(
            app.spec.upstream.image.as_deref(),
            Some("ghcr.io/sebadob/rauthy")
        );
        assert!(app.spec.upstream.chart.is_none());
        // No Helm-style values (e.g. replicas): Hiqlite raft membership is config-driven.
        assert_eq!(app.spec.schema["properties"], json!({}));
        assert!(validate_schema_values(&app.spec.schema, &json!({"replicas": 3})).is_err());
        assert!(validate_schema_values(&app.spec.schema, &json!({})).is_ok());
    }

    #[test]
    fn platform_reserved_hostnames_cover_defaults_and_configured_urls() {
        let reserved = platform_reserved_hostnames([
            "https://auth.corp.example:8443/auth/v1",
            "https://keel.corp.example/api/v1/auth/callback",
            "not a url",
        ]);
        for host in [
            "idp.keel.local",
            "keel.keel.local",
            "forgejo.keel.local",
            "auth.corp.example",
            "idp.corp.example",
            "keel.corp.example",
            "forgejo.corp.example",
        ] {
            assert!(reserved.contains(host), "{host} should be reserved");
        }
        assert!(!reserved.contains("corp.example"));
    }

    #[test]
    fn route_hostname_validation_rejects_platform_hosts_and_wildcards() {
        let reserved = platform_reserved_hostnames(["https://idp.keel.local:8888/auth/v1"]);
        assert!(validate_route_hostnames(&["rauthy-acme.keel.local".into()], &reserved).is_ok());
        // A wildcard under a registrable domain is fine; one over a whole TLD is a catch-all.
        assert!(validate_route_hostnames(&["*.acme.example".into()], &reserved).is_ok());
        assert!(is_tld_wide_wildcard("*.com") && is_tld_wide_wildcard("*"));
        assert!(!is_tld_wide_wildcard("*.acme.example") && !is_tld_wide_wildcard("web.com"));
        for bad in [
            "idp.keel.local",
            "keel.keel.local",
            "forgejo.keel.local",
            "*.keel.local",
            "${hostname}",
            "",
            "*",
            "*.com",
            "*.local",
        ] {
            assert!(
                validate_route_hostnames(&[bad.to_string()], &reserved).is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(route_hostname_matches("*.keel.local", "idp.keel.local"));
        assert!(!route_hostname_matches("*.keel.local", "keel.local"));
        assert!(!route_hostname_matches(
            "*.idp.keel.local",
            "idp.keel.local"
        ));
    }

    #[test]
    fn gateway_route_hostnames_only_reads_gateway_routes() {
        let docs = vec![
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "spec": {"hostnames": ["Auth.Example.ORG"]}}),
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GRPCRoute",
                   "spec": {"hostnames": ["grpc.example.org"]}}),
            json!({"apiVersion": "v1", "kind": "Service", "spec": {"hostnames": ["x"]}}),
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "spec": {}}),
        ];
        assert_eq!(
            gateway_route_hostnames(&docs),
            vec![
                "auth.example.org".to_string(),
                "grpc.example.org".to_string()
            ]
        );
    }

    #[test]
    fn shared_gateway_catch_all_routes_finds_host_less_routes_on_shared_gateways() {
        let shared = json!([{ "name": "keel-ingress", "namespace": "keel-system" }]);
        let docs = vec![
            // Host-less on keel-ingress: catch-all.
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "open"}, "spec": {"parentRefs": shared}}),
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GRPCRoute",
                   "metadata": {"name": "grpc", "namespace": "tenant-acme"},
                   "spec": {"hostnames": [], "parentRefs": [
                       {"group": "gateway.networking.k8s.io", "kind": "Gateway",
                        "name": "edge", "namespace": "platform-gw"}]}}),
            // With hostnames: fine.
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "hosted"},
                   "spec": {"hostnames": ["web.example.org"], "parentRefs": shared}}),
            // Same-namespace Gateway, implicit namespace, or a Service parent: not shared.
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "own", "namespace": "apps"},
                   "spec": {"parentRefs": [{"name": "gw", "namespace": "apps"}]}}),
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "implicit"}, "spec": {"parentRefs": [{"name": "gw"}]}}),
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "mesh"}, "spec": {"parentRefs": [
                       {"group": "", "kind": "Service", "name": "web", "namespace": "keel-system"}]}}),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "svc"},
                   "spec": {"parentRefs": shared}}),
            // Implicit namespace (applied into the tenant namespace): a Gateway in any other
            // namespace is shared, one in the tenant namespace is not.
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "foreign"},
                   "spec": {"parentRefs": [{"name": "edge", "namespace": "other-gw"}]}}),
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
                   "metadata": {"name": "own-explicit"},
                   "spec": {"parentRefs": [{"name": "gw", "namespace": "tenant-acme"}]}}),
        ];
        assert_eq!(
            shared_gateway_catch_all_routes(&docs, "tenant-acme"),
            vec![
                "HTTPRoute/open".to_string(),
                "GRPCRoute/grpc".to_string(),
                "HTTPRoute/foreign".to_string()
            ]
        );
    }

    #[test]
    fn test_resolve_helm_gateway_routes_guards_and_pins_parent_refs() {
        let routes = vec![
            HelmGatewayRouteSpec {
                enabled_value: Some("httpRoute.enabled".to_string()),
                hostnames_value: "httpRoute.hostnames".to_string(),
                parent_refs_value: Some("httpRoute.parentRefs".to_string()),
            },
            HelmGatewayRouteSpec {
                enabled_value: None,
                hostnames_value: "s3.host".to_string(),
                parent_refs_value: None,
            },
        ];
        let mut values = json!({
            "httpRoute": {
                "enabled": true,
                "hostnames": [" Git.Acme.Example "],
                "parentRefs": [{ "name": "other-gateway", "namespace": "evil" }]
            },
            "s3": { "host": "s3.acme.example" }
        });
        let hosts = resolve_helm_gateway_routes(&routes, &mut values).unwrap();
        assert_eq!(hosts, vec!["git.acme.example", "s3.acme.example"]);
        // Submitted parentRefs never survive: the route is pinned to keel-ingress.
        assert_eq!(
            values["httpRoute"]["parentRefs"],
            keel_ingress_parent_refs()
        );

        // Disabled routes are ignored (and not pinned).
        let mut disabled =
            json!({ "httpRoute": { "enabled": false }, "s3": { "host": "x.example" } });
        assert_eq!(
            resolve_helm_gateway_routes(&routes, &mut disabled).unwrap(),
            vec!["x.example"]
        );
        assert!(disabled["httpRoute"].get("parentRefs").is_none());

        // Enabled without hostnames would be a catch-all route on the shared Gateway.
        for bad in [
            json!({ "httpRoute": { "enabled": true }, "s3": { "host": "x.example" } }),
            json!({ "httpRoute": { "enabled": true, "hostnames": [] }, "s3": { "host": "x.example" } }),
            json!({ "httpRoute": { "enabled": true, "hostnames": ["  "] }, "s3": { "host": "x.example" } }),
            json!({ "httpRoute": { "enabled": false } }),
            json!({ "httpRoute": { "enabled": true, "hostnames": [1] }, "s3": { "host": "x" } }),
            json!({ "httpRoute": { "enabled": true, "hostnames": {} }, "s3": { "host": "x" } }),
        ] {
            let mut v = bad.clone();
            assert!(
                resolve_helm_gateway_routes(&routes, &mut v).is_err(),
                "{bad} must be rejected"
            );
        }

        // parentRefs are created even when the parent object is absent.
        let mut no_parent = json!({ "s3": { "host": "a.example" } });
        let pinned = [HelmGatewayRouteSpec {
            enabled_value: None,
            hostnames_value: "s3.host".to_string(),
            parent_refs_value: Some("s3.httpRoute.parentRefs".to_string()),
        }];
        resolve_helm_gateway_routes(&pinned, &mut no_parent).unwrap();
        assert_eq!(
            no_parent["s3"]["httpRoute"]["parentRefs"][1]["sectionName"],
            "https"
        );
    }

    #[test]
    fn schema_defaults_fill_missing_values_like_the_gui() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replicaCount": { "type": "integer", "default": 1 },
                "tags": { "type": "array", "default": ["a", "b"] },
                "explicitNull": { "type": ["string", "null"], "default": "x" },
                "resources": {
                    "type": "object",
                    "properties": {
                        "limits": {
                            "type": "object",
                            "properties": { "cpu": { "type": "string", "default": "100m" } }
                        },
                        "empty": { "type": "object", "properties": { "x": { "type": "string" } } }
                    }
                },
                "noDefaults": {
                    "type": "object",
                    "properties": { "x": { "type": "string" } }
                },
                "ports": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "port": { "type": "integer" },
                            "protocol": { "type": "string", "default": "TCP" }
                        }
                    }
                },
                "password": { "type": "string", "writeOnly": true, "default": "changeme" },
                "auth": {
                    "type": "object",
                    "default": { "user": "admin", "token": "t0ps3cret" },
                    "properties": {
                        "user": { "type": "string" },
                        "token": { "type": "string", "format": "password" }
                    }
                },
                "onlySecret": {
                    "type": "object",
                    "default": { "key": "k" },
                    "properties": { "key": { "type": "string", "writeOnly": true } }
                }
            }
        });
        let values = json!({
            "replicaCount": 3,
            "explicitNull": null,
            "resources": { "limits": {} },
            "ports": [{ "port": 80 }, { "port": 53, "protocol": "UDP" }]
        });
        let filled = apply_schema_defaults(&schema, &values);
        assert_eq!(
            filled,
            json!({
                "replicaCount": 3,
                "tags": ["a", "b"],
                "explicitNull": null,
                "resources": { "limits": { "cpu": "100m" } },
                "ports": [
                    { "port": 80, "protocol": "TCP" },
                    { "port": 53, "protocol": "UDP" }
                ],
                "auth": { "user": "admin" }
            })
        );
        // Idempotent, and a non-object input is returned untouched.
        assert_eq!(apply_schema_defaults(&schema, &filled), filled);
        assert_eq!(apply_schema_defaults(&schema, &json!("x")), json!("x"));
        assert_eq!(apply_schema_defaults(&json!({}), &values), values);
    }

    #[test]
    fn merge_values_patch_follows_json_merge_patch() {
        let stored = json!({
            "replicaCount": 1,
            "securityContext": { "runAsNonRoot": true, "capabilities": { "drop": ["ALL"] } },
            "tags": ["a", "b"],
            "drop": "me"
        });
        let patch = json!({
            "replicaCount": 3,
            "securityContext": { "readOnlyRootFilesystem": true },
            "tags": ["c"],
            "drop": null,
            "absent": null,
            "added": { "x": null, "y": 1 }
        });
        assert_eq!(
            merge_values_patch(&stored, &patch),
            json!({
                "replicaCount": 3,
                "securityContext": {
                    "runAsNonRoot": true,
                    "capabilities": { "drop": ["ALL"] },
                    "readOnlyRootFilesystem": true
                },
                "tags": ["c"],
                "added": { "y": 1 }
            })
        );
        // A non-object patch replaces the base; an empty patch keeps it.
        assert_eq!(merge_values_patch(&stored, &json!("x")), json!("x"));
        assert_eq!(merge_values_patch(&stored, &json!({})), stored);
        assert_eq!(
            strip_null_values(&json!({ "a": null, "b": { "c": null, "d": [null] } })),
            json!({ "b": { "d": [null] } })
        );
    }

    #[test]
    fn podinfo_install_gets_the_restricted_security_context_default() {
        let app = parse_catalog_application(include_str!(
            "../../../deploy/catalog/apps/podinfo/catalog.yaml"
        ))
        .unwrap();
        let filled = apply_schema_defaults(
            &app.spec.schema,
            &json!({ "ingressHost": "podinfo.keel.local", "replicaCount": 2, "apiToken": "abcdefgh" }),
        );
        assert_eq!(filled["apiToken"], "abcdefgh");
        assert_eq!(filled["replicaCount"], 2);
        assert_eq!(filled["securityContext"]["runAsNonRoot"], true);
        assert_eq!(filled["securityContext"]["allowPrivilegeEscalation"], false);
        assert_eq!(
            filled["securityContext"]["capabilities"]["drop"],
            json!(["ALL"])
        );
        validate_schema_values(&app.spec.schema, &filled).unwrap();
    }

    #[test]
    fn test_undeclared_value_paths_fail_closed() {
        let schema = json!({
            "type": "object",
            "properties": {
                "httpRoute": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "hostnames": { "type": "array", "items": { "type": "string" } }
                    }
                },
                "buckets": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "name": { "type": "string" } } }
                },
                "labels": { "type": "object", "additionalProperties": { "type": "string" } },
                "free": { "type": "object", "additionalProperties": true },
                "oneOf": { "allOf": [{ "properties": { "a": { "type": "string" } } }] },
                "secret": { "type": "string", "writeOnly": true }
            }
        });
        let ok = json!({
            "httpRoute": { "enabled": true, "hostnames": ["a.example"] },
            "buckets": [{ "name": "data" }],
            "labels": { "team": "x" },
            "free": { "anything": { "nested": true } },
            "oneOf": { "a": "x" },
            "secret": "s3cr3t"
        });
        assert!(undeclared_value_paths(&schema, &ok).is_empty());
        let bad = json!({
            "httpRoute": { "enabled": true, "parentRefs": [] },
            "buckets": [{ "name": "data", "anonymousRead": true }],
            "extraDeploy": [{ "kind": "Role" }],
            "filer": { "httpRoute": { "enabled": true } },
            "oneOf": { "b": "x" }
        });
        assert_eq!(
            undeclared_value_paths(&schema, &bad),
            vec![
                "buckets[0].anonymousRead",
                "extraDeploy",
                "filer",
                "httpRoute.parentRefs",
                "oneOf.b"
            ]
        );
    }

    #[test]
    fn test_schema_errors_never_echo_secret_values() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "pattern": "^[a-z]+$" },
                "token": { "type": "string", "writeOnly": true, "pattern": "^[a-z]+$" },
                "oauth": {
                    "type": "array",
                    "writeOnly": true,
                    "items": {
                        "type": "object",
                        "properties": { "secret": { "type": "string", "pattern": "^[a-z]+$" } }
                    }
                }
            }
        });
        let values = json!({
            "name": "Bad-Name",
            "token": "TOP-SECRET-1",
            "oauth": [{ "secret": "TOP-SECRET-2" }]
        });
        let Err(CoreError::SchemaValidation(errors)) = validate_schema_values(&schema, &values)
        else {
            panic!("expected schema errors");
        };
        let rendered = format!("{errors:?}");
        assert!(
            !rendered.contains("TOP-SECRET"),
            "schema errors echoed a writeOnly secret value"
        );
        assert!(
            rendered.contains("Bad-Name"),
            "non-secret values keep the detail"
        );
        assert_eq!(errors.len(), 3);
        assert!(
            errors
                .iter()
                .any(|e| e.field_path == "/oauth/0/secret" && e.message.contains("'pattern'"))
        );
    }

    /// AST-4: an error on a *parent* of a secret (or at the root) must not quote the parent value.
    #[test]
    fn test_parent_and_root_schema_errors_never_echo_secret_children() {
        fn errors_for(schema: &Value, values: &Value) -> Vec<SchemaFieldError> {
            match validate_schema_values(schema, values) {
                Err(CoreError::SchemaValidation(errors)) => errors,
                other => panic!("expected schema errors, got {other:?}"),
            }
        }
        fn assert_no_echo(label: &str, schema: &Value, values: &Value) -> Vec<SchemaFieldError> {
            let errors = errors_for(schema, values);
            let rendered = format!("{errors:?}");
            assert!(
                !rendered.contains("TOP-SECRET"),
                "{label}: schema errors echoed a secret: {rendered}"
            );
            errors
        }
        for marker in [json!({"writeOnly": true}), json!({"format": "password"})] {
            let mut secret = json!({"type": "string"});
            secret
                .as_object_mut()
                .unwrap()
                .extend(marker.as_object().unwrap().clone());
            let child = json!({"properties": {"password": secret, "user": {"type": "string"}}});
            let parent_constraints = [
                (
                    "anyOf",
                    json!({"anyOf": [{"required": ["missing"]}, {"maxProperties": 0}]}),
                ),
                (
                    "oneOf",
                    json!({"oneOf": [{"required": ["missing"]}, {"required": ["other"]}]}),
                ),
                ("maxProperties", json!({"maxProperties": 1})),
                ("minProperties", json!({"minProperties": 5})),
                ("enum", json!({"enum": [{"password": "x"}]})),
                ("const", json!({"const": {"password": "x"}})),
                ("not", json!({"not": {"required": ["password"]}})),
            ];
            for (keyword, constraint) in &parent_constraints {
                let mut auth = child.clone();
                auth.as_object_mut()
                    .unwrap()
                    .extend(constraint.as_object().unwrap().clone());
                let schema = json!({"type": "object", "properties": {"auth": auth}});
                let values = json!({"auth": {"password": "TOP-SECRET-1", "user": "u"}});
                let errors = assert_no_echo(keyword, &schema, &values);
                assert!(
                    errors
                        .iter()
                        .any(|e| e.field_path == "/auth"
                            && e.message.contains(&format!("'{keyword}'"))),
                    "{keyword}: keyword is still named: {errors:?}"
                );

                // The same constraint on the root object, the secret one level below it.
                let mut root = child.clone();
                root.as_object_mut()
                    .unwrap()
                    .extend(constraint.as_object().unwrap().clone());
                let values = json!({"password": "TOP-SECRET-2", "user": "u"});
                let errors = assert_no_echo(&format!("root {keyword}"), &root, &values);
                assert!(errors.iter().any(|e| e.field_path.is_empty()));
            }
        }

        // `required` on a parent of a secret keeps its useful (value-free) message.
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": { "token": { "type": "string", "writeOnly": true }, "name": {} }
        });
        let errors = assert_no_echo("required", &schema, &json!({"token": "TOP-SECRET-3"}));
        assert!(
            errors[0]
                .message
                .contains("\"name\" is a required property")
        );

        // Secrets reached through `$ref` and `patternProperties` are redacted as well.
        let schema = json!({
            "$defs": { "pw": { "type": "string", "writeOnly": true, "minLength": 50 } },
            "properties": {
                "admin": { "$ref": "#/$defs/pw" },
                "env": { "patternProperties": { "^SECRET_": { "format": "password", "pattern": "^x" } } }
            }
        });
        let errors = assert_no_echo(
            "ref/pattern",
            &schema,
            &json!({"admin": "TOP-SECRET-4", "env": {"SECRET_A": "TOP-SECRET-5"}}),
        );
        assert_eq!(errors.len(), 2);

        // Extra (heuristic) secret pointers are honoured.
        let schema = json!({"properties": {"apiToken": {"type": "string", "pattern": "^x"}}});
        let values = json!({"apiToken": "TOP-SECRET-6"});
        let Err(CoreError::SchemaValidation(errors)) =
            validate_schema_values_with_secrets(&schema, &values, &["/apiToken".to_string()])
        else {
            panic!("expected schema errors");
        };
        assert!(!format!("{errors:?}").contains("TOP-SECRET"));
        // Without the extra pointer the (non-secret) value is quoted as before.
        let errors = errors_for(&schema, &values);
        assert!(errors[0].message.contains("TOP-SECRET-6"));

        // A non-secret object value is masked, a non-secret scalar keeps its detail.
        let schema = json!({"properties": {"cfg": {"maxProperties": 0}, "n": {"maximum": 1}}});
        let errors = errors_for(&schema, &json!({"cfg": {"k": "visible"}, "n": 7}));
        let rendered = format!("{errors:?}");
        assert!(!rendered.contains("visible"));
        assert!(rendered.contains('7'));
    }

    /// AST-3: secrets declared through `$ref`, `patternProperties`, conditionals and every
    /// combinator branch never land in plaintext values.
    #[test]
    fn test_partition_follows_refs_patterns_conditionals_and_all_branches() {
        let schema = json!({
            "type": "object",
            "$defs": { "pw": { "type": "string", "writeOnly": true } },
            "properties": {
                "adminPassword": { "$ref": "#/$defs/pw" },
                "extra": { "type": "object", "patternProperties": { "Token$": { "format": "password" } } },
                "mode": { "type": "string" }
            },
            "if": { "properties": { "mode": { "const": "ldap" } } },
            "then": { "properties": { "bindPassword": { "writeOnly": true } } },
            "anyOf": [
                { "properties": { "apiKey": { "type": "string" } } },
                { "properties": { "apiKey": { "writeOnly": true } } }
            ]
        });
        let values = json!({
            "adminPassword": "s1",
            "extra": { "githubToken": "s2", "org": "acme" },
            "mode": "local",
            "bindPassword": "s3",
            "apiKey": "s4"
        });
        let p = partition_values_by_schema(&schema, &values).unwrap();
        assert_eq!(
            p.normal_values,
            json!({"extra": {"org": "acme"}, "mode": "local"})
        );
        assert!(!p.normal_values.to_string().contains("s1"));
        assert_eq!(
            p.secret_field_paths,
            vec![
                "adminPassword",
                "apiKey",
                "bindPassword",
                "extra.githubToken"
            ]
        );
        let fragments =
            helm_secret_value_fragments(&p.secret_values, &p.secret_field_paths).unwrap();
        assert_eq!(fragments.len(), 4);
        assert!(fragments["extra.githubToken"].contains("githubToken: s2"));
        assert_eq!(
            stored_secret_paths_still_secret(
                &schema,
                &["extra.gitlabToken".to_string(), "extra.org".to_string()]
            ),
            vec!["extra.gitlabToken"]
        );
        // Declared-but-unsupplied pattern secrets have no static path.
        assert_eq!(
            extract_secret_field_paths(&schema),
            vec!["adminPassword", "apiKey", "bindPassword"]
        );
        // Defaults are never filled into a `$ref` secret.
        let with_default = json!({
            "$defs": { "pw": { "writeOnly": true, "default": "changeme" } },
            "properties": { "pw": { "$ref": "#/$defs/pw" }, "n": { "default": 1 } }
        });
        assert_eq!(
            apply_schema_defaults(&with_default, &json!({})),
            json!({"n": 1})
        );
        assert!(is_secret_schema_node(
            &json!({"$defs": {"a": {"writeOnly": true}}, "$ref": "#/$defs/a"})
        ));
    }

    #[test]
    fn test_undeclared_value_paths_follow_refs_and_patterns() {
        let schema = json!({
            "$defs": { "res": { "type": "object", "properties": { "cpu": {} } } },
            "properties": {
                "resources": { "$ref": "#/$defs/res" },
                "labels": { "patternProperties": { "^app\\.": {} } },
                "list": { "prefixItems": [{ "properties": { "a": {} } }] }
            }
        });
        let values = json!({
            "resources": { "cpu": 1, "gpu": 1 },
            "labels": { "app.kubernetes.io/name": "x", "team": "y" },
            "list": [{ "a": 1, "b": 2 }]
        });
        assert_eq!(
            undeclared_value_paths(&schema, &values),
            vec!["labels.team", "list[0].b", "resources.gpu"]
        );
    }

    #[test]
    fn test_catalog_parse_fails_closed_on_unreachable_secrets_and_remote_refs() {
        let app = |schema: &str| {
            format!(
                r#"
apiVersion: keel.io/v1alpha1
kind: CatalogApplication
metadata:
  id: demo
  name: Demo
  category: demo
  runtime: helm
spec:
  upstream:
    repoURL: https://charts.example.com
    chart: demo
    version: 1.0.0
  schema:
{schema}
"#
            )
        };
        let hidden = app(
            "    type: object\n    not:\n      properties:\n        pw:\n          writeOnly: true",
        );
        let err = parse_catalog_application(&hidden).unwrap_err().to_string();
        assert!(err.contains("#/not/properties/pw"), "{err}");
        assert!(matches!(
            parse_catalog(&hidden),
            Err(CoreError::Validation(_))
        ));

        let remote = app(
            "    type: object\n    properties:\n      a:\n        $ref: https://example.com/s.json",
        );
        let err = parse_catalog_application(&remote).unwrap_err().to_string();
        assert!(err.contains("not a local reference"), "{err}");

        let supported = app(
            "    type: object\n    $defs:\n      pw:\n        type: string\n        writeOnly: true\n    properties:\n      adminPassword:\n        $ref: '#/$defs/pw'",
        );
        let parsed = parse_catalog_application(&supported).unwrap();
        assert_eq!(
            extract_secret_field_paths(&parsed.spec.schema),
            vec!["adminPassword"]
        );
    }

    #[test]
    fn test_compile_schema_never_fetches_external_references() {
        // Bypasses the catalog check on purpose: compiling alone must not fetch or read files.
        for reference in ["https://example.com/schema.json", "file:///etc/passwd"] {
            let schema = json!({"properties": {"a": {"$ref": reference}}});
            let err = validate_schema_values(&schema, &json!({"a": 1}))
                .unwrap_err()
                .to_string();
            assert!(err.contains("Invalid JSON Schema"), "{reference}: {err}");
        }
    }

    #[test]
    fn test_changed_immutable_values() {
        let paths = vec![
            "persistence.size".to_string(),
            "persistence.storageClass".to_string(),
            "volume.dataDirs".to_string(),
        ];
        let committed = json!({
            "persistence": { "size": "10Gi", "storageClass": null },
            "volume": { "dataDirs": [{ "name": "data1", "size": "10Gi" }] }
        });
        assert!(changed_immutable_values(&paths, &committed, &committed).is_empty());
        // Missing equals null.
        let same = json!({
            "persistence": { "size": "10Gi" },
            "volume": { "dataDirs": [{ "name": "data1", "size": "10Gi" }] }
        });
        assert!(changed_immutable_values(&paths, &committed, &same).is_empty());
        let changed = json!({
            "persistence": { "size": "30Gi", "storageClass": "fast" },
            "volume": { "dataDirs": [{ "name": "data1", "size": "20Gi" }] }
        });
        assert_eq!(
            changed_immutable_values(&paths, &committed, &changed),
            paths
        );
    }

    #[test]
    fn test_helm_secret_value_fragments() {
        let schema = json!({
            "type": "object",
            "properties": {
                "gitea": {
                    "type": "object",
                    "properties": {
                        "admin": {
                            "type": "object",
                            "properties": {
                                "username": { "type": "string" },
                                "password": { "type": "string", "format": "password" }
                            }
                        },
                        "oauth": {
                            "type": "array",
                            "writeOnly": true,
                            "items": { "type": "object" }
                        }
                    }
                },
                "token": { "type": "string", "writeOnly": true },
                "list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "key": { "type": "string", "format": "password" } }
                    }
                }
            }
        });
        let values = json!({
            "gitea": {
                "admin": { "username": "root", "password": "p,a=s{s}\"'1" },
                "oauth": [{ "name": "rauthy", "secret": "s3cr3t" }]
            },
            "token": ""
        });
        let partitioned = partition_values_by_schema(&schema, &values).unwrap();
        let fragments = helm_secret_value_fragments(
            &partitioned.secret_values,
            &partitioned.secret_field_paths,
        )
        .unwrap();
        // Blank secrets are not submitted (an Edit keeps the stored value).
        assert_eq!(
            fragments.keys().collect::<Vec<_>>(),
            vec!["gitea.admin.password", "gitea.oauth"]
        );
        let admin: Value = noyalib::from_str(&fragments["gitea.admin.password"]).unwrap();
        assert_eq!(
            admin,
            json!({ "gitea": { "admin": { "password": "p,a=s{s}\"'1" } } })
        );
        let oauth: Value = noyalib::from_str(&fragments["gitea.oauth"]).unwrap();
        assert_eq!(oauth["gitea"]["oauth"][0]["secret"], "s3cr3t");
        assert!(partitioned.normal_values["gitea"].get("oauth").is_none());

        // Secret fields inside lists fail closed once submitted.
        let in_list = json!({ "list": [{ "key": "abc" }] });
        let partitioned = partition_values_by_schema(&schema, &in_list).unwrap();
        assert!(
            helm_secret_value_fragments(
                &partitioned.secret_values,
                &partitioned.secret_field_paths
            )
            .is_err()
        );
    }
}
