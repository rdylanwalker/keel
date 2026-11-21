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
    CatalogManifest,
};

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
    let header: CatalogHeader = serde_yml::from_str(yaml)?;
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
    let app: CatalogApplication = serde_yml::from_str(yaml)?;
    app.validate()?;
    compile_schema(&app.spec.schema)?;
    Ok(app)
}

/// Parses and validates a `CatalogInfraBlueprint` from YAML, including compiling `spec.schema`.
pub fn parse_catalog_blueprint(yaml: &str) -> Result<CatalogInfraBlueprint, CoreError> {
    let bp: CatalogInfraBlueprint = serde_yml::from_str(yaml)?;
    bp.validate()?;
    compile_schema(&bp.spec.schema)?;
    Ok(bp)
}

/// Parses and validates a `CatalogBundle` from YAML, including compiling `spec.schema` and DAG checks.
pub fn parse_catalog_bundle(yaml: &str) -> Result<CatalogBundle, CoreError> {
    let bundle: CatalogBundle = serde_yml::from_str(yaml)?;
    bundle.validate()?;
    compile_schema(&bundle.spec.schema)?;
    Ok(bundle)
}

fn compile_schema(schema: &Value) -> Result<jsonschema::Validator, CoreError> {
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|e| CoreError::Validation(format!("Invalid JSON Schema in catalog: {e}")))
}

/// Validates user input `values` against a JSON Schema, returning structured `SchemaFieldError`s on failure.
pub fn validate_schema_values(schema: &Value, values: &Value) -> Result<(), CoreError> {
    let validator = compile_schema(schema)?;
    let mut errors: Vec<SchemaFieldError> = validator
        .iter_errors(values)
        .map(|err| SchemaFieldError {
            field_path: err.instance_path().to_string(),
            schema_path: err.schema_path().to_string(),
            message: err.to_string(),
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

/// Returns `true` if a JSON Schema property node marks a field as secret (`writeOnly: true` or `format: "password"`).
pub fn is_secret_schema_node(schema_node: &Value) -> bool {
    let Some(obj) = schema_node.as_object() else {
        return false;
    };

    if obj.get("writeOnly").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if obj.get("format").and_then(Value::as_str) == Some("password") {
        return true;
    }

    for combinator in ["allOf", "anyOf", "oneOf"] {
        if let Some(subschemas) = obj.get(combinator).and_then(Value::as_array)
            && subschemas.iter().any(is_secret_schema_node)
        {
            return true;
        }
    }

    false
}

/// Extracts sorted, deduplicated dot-notated paths of all secret fields (`writeOnly: true` or `format: "password"`)
/// defined in a JSON Schema.
pub fn extract_secret_field_paths(schema: &Value) -> Vec<String> {
    let mut paths = BTreeSet::new();
    collect_secret_paths(schema, "", &mut paths);
    paths.into_iter().collect()
}

fn collect_secret_paths(schema_node: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    let Some(obj) = schema_node.as_object() else {
        return;
    };

    if !prefix.is_empty() && is_secret_schema_node(schema_node) {
        out.insert(prefix.to_string());
        return;
    }

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (prop_name, prop_schema) in props {
            let next_prefix = if prefix.is_empty() {
                prop_name.clone()
            } else {
                format!("{prefix}.{prop_name}")
            };
            collect_secret_paths(prop_schema, &next_prefix, out);
        }
    }

    if let Some(items_schema) = obj.get("items") {
        let next_prefix = if prefix.is_empty() {
            "[]".to_string()
        } else {
            format!("{prefix}[]")
        };
        collect_secret_paths(items_schema, &next_prefix, out);
    }

    for combinator in ["allOf", "anyOf", "oneOf"] {
        if let Some(subschemas) = obj.get(combinator).and_then(Value::as_array) {
            for sub in subschemas {
                collect_secret_paths(sub, prefix, out);
            }
        }
    }
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

    let secret_field_paths = extract_secret_field_paths(schema);
    let (normal_opt, secret_opt) = split_node(Some(schema), values);

    let normal_values = normal_opt.unwrap_or_else(|| Value::Object(Map::new()));
    let secret_values = secret_opt.unwrap_or_else(|| Value::Object(Map::new()));

    Ok(PartitionedValues {
        normal_values,
        secret_values,
        secret_field_paths,
    })
}

fn find_property_schema<'a>(schema: &'a Value, prop_name: &str) -> Option<&'a Value> {
    let obj = schema.as_object()?;
    if let Some(prop) = obj
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|p| p.get(prop_name))
    {
        return Some(prop);
    }

    for combinator in ["allOf", "anyOf", "oneOf"] {
        if let Some(subschemas) = obj.get(combinator).and_then(Value::as_array) {
            for sub in subschemas {
                if let Some(found) = find_property_schema(sub, prop_name) {
                    return Some(found);
                }
            }
        }
    }

    obj.get("additionalProperties").filter(|v| v.is_object())
}

fn split_node(schema: Option<&Value>, value: &Value) -> (Option<Value>, Option<Value>) {
    if let Some(s) = schema
        && is_secret_schema_node(s)
    {
        return (None, Some(value.clone()));
    }

    match value {
        Value::Object(map) => {
            if map.is_empty() {
                return (Some(Value::Object(Map::new())), None);
            }
            let mut normal_map = Map::new();
            let mut secret_map = Map::new();

            for (k, v) in map {
                let child_schema = schema.and_then(|s| find_property_schema(s, k));
                let (n_part, s_part) = split_node(child_schema, v);
                if let Some(nv) = n_part {
                    normal_map.insert(k.clone(), nv);
                }
                if let Some(sv) = s_part {
                    secret_map.insert(k.clone(), sv);
                }
            }

            let normal_res = if normal_map.is_empty() {
                None
            } else {
                Some(Value::Object(normal_map))
            };
            let secret_res = if secret_map.is_empty() {
                None
            } else {
                Some(Value::Object(secret_map))
            };
            (normal_res, secret_res)
        }
        Value::Array(items) => {
            let item_schema = schema
                .and_then(Value::as_object)
                .and_then(|o| o.get("items"));
            if let Some(is) = item_schema
                && is_secret_schema_node(is)
            {
                return (None, Some(value.clone()));
            }

            let mut normal_arr = Vec::new();
            let mut secret_arr = Vec::new();
            let mut any_secret = false;

            for item in items {
                let (n_part, s_part) = split_node(item_schema, item);
                if let Some(nv) = n_part {
                    normal_arr.push(nv);
                }
                if let Some(sv) = s_part {
                    any_secret = true;
                    secret_arr.push(sv);
                }
            }

            let normal_res = if normal_arr.is_empty() && any_secret {
                None
            } else {
                Some(Value::Array(normal_arr))
            };
            let secret_res = if any_secret {
                Some(Value::Array(secret_arr))
            } else {
                None
            };
            (normal_res, secret_res)
        }
        _ => (Some(value.clone()), None),
    }
}

/// Renders an unmodified upstream Helm `values.yaml` string directly from a JSON value (`serde_yml::to_string`).
pub fn render_helm_values_yaml(values: &Value) -> Result<String, CoreError> {
    if !values.is_object() {
        return Err(CoreError::Validation(
            "Helm values root must be a JSON/YAML mapping object".to_string(),
        ));
    }
    Ok(serde_yml::to_string(values)?)
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
        Some(map) if !map.is_empty() => Some(serde_yml::to_string(&partitioned.secret_values)?),
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

/// Splits single or multi-document YAML on lines matching `---` or `...`,
/// returning all non-empty document strings.
pub fn split_yaml_documents(raw_yaml: &str) -> Vec<String> {
    let mut raw_docs = Vec::new();
    let mut current = String::new();
    for line in raw_yaml.lines() {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            if !current.trim().is_empty() {
                raw_docs.push(std::mem::take(&mut current));
            }
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        raw_docs.push(current);
    }
    raw_docs
}

fn is_yaml_document_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "---" || trimmed == "..."
}

/// Replaces the `index`-th non-empty document (same numbering as [`split_yaml_documents`]) with
/// `replacement`, preserving every other document, separator line, and line ending verbatim.
/// Returns `None` when `index` is out of range.
pub fn replace_yaml_document(raw_yaml: &str, index: usize, replacement: &str) -> Option<String> {
    let mut out = String::with_capacity(raw_yaml.len() + replacement.len());
    let mut current = String::new();
    let mut doc_index = 0usize;
    let mut replaced = false;

    let mut flush = |current: &mut String, out: &mut String| {
        if current.trim().is_empty() {
            out.push_str(current);
        } else {
            if doc_index == index {
                out.push_str(replacement);
                if !replacement.is_empty() && !replacement.ends_with('\n') {
                    out.push('\n');
                }
                replaced = true;
            } else {
                out.push_str(current);
            }
            doc_index += 1;
        }
        current.clear();
    };

    for line in raw_yaml.split_inclusive('\n') {
        if is_yaml_document_separator(line) {
            flush(&mut current, &mut out);
            out.push_str(line);
        } else {
            current.push_str(line);
        }
    }
    flush(&mut current, &mut out);
    replaced.then_some(out)
}

/// Returns the leading block of blank and `#` comment lines of a YAML document (verbatim), so
/// callers that re-serialize a document can keep its header comments.
pub fn yaml_leading_comments(doc: &str) -> &str {
    let mut end = 0usize;
    for line in doc.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            end += line.len();
        } else {
            break;
        }
    }
    &doc[..end]
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
        let doc: Value = serde_yml::from_str(&doc_str)?;
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
        .map(|d| serde_yml::from_str::<Value>(d))
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::models::AppRuntime;

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
  companionInfrastructure:
    - blueprint: postgres-managed
      bindValues:
        postgresql.enabled: false
        authentik.postgresql.host: "${outputs.db_host}"
        authentik.postgresql.password: "${secrets.db_password}"
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
        assert_eq!(app.spec.companion_infrastructure.len(), 1);
        assert_eq!(
            app.spec.companion_infrastructure[0].blueprint,
            "postgres-managed"
        );
        assert_eq!(
            app.spec.companion_infrastructure[0]
                .bind_values
                .get("postgresql.enabled"),
            Some(&json!(false))
        );
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
                false,
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

            let parsed_pulumi: serde_yml::Value = serde_yml::from_str(&pulumi_yaml).unwrap();
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
            }

            // Test protected/tenant serialization round-trips
            let roundtrip: String = serde_yml::to_string(&bp).unwrap();
            let parsed2 = parse_catalog_blueprint(&roundtrip).unwrap();
            assert_eq!(bp.metadata.protected, parsed2.metadata.protected);
            assert_eq!(bp.metadata.tenant, parsed2.metadata.tenant);
        }
    }

    #[test]
    fn replace_yaml_document_preserves_other_documents_verbatim() {
        let raw =
            "# header\napiVersion: v1\nkind: A # keep\n---\n\n# b comment\nkind: B\n---\nkind: C\n";
        assert_eq!(split_yaml_documents(raw).len(), 3);
        let out = replace_yaml_document(raw, 1, "kind: B2").unwrap();
        assert_eq!(
            out,
            "# header\napiVersion: v1\nkind: A # keep\n---\nkind: B2\n---\nkind: C\n"
        );
        let out = replace_yaml_document(raw, 2, "kind: C2\n").unwrap();
        assert!(out.starts_with(
            "# header\napiVersion: v1\nkind: A # keep\n---\n\n# b comment\nkind: B\n---\n"
        ));
        assert!(out.ends_with("kind: C2\n"));
        assert!(replace_yaml_document(raw, 3, "x").is_none());
        // Leading separator and CRLF line endings are kept.
        let crlf = "---\r\nkind: A\r\n---\r\nkind: B\r\n";
        assert_eq!(
            replace_yaml_document(crlf, 0, "kind: Z\n").unwrap(),
            "---\r\nkind: Z\n---\r\nkind: B\r\n"
        );
    }

    #[test]
    fn yaml_leading_comments_returns_header_block() {
        assert_eq!(
            yaml_leading_comments("\n# a\n  # b\napiVersion: v1\n# c\n"),
            "\n# a\n  # b\n"
        );
        assert_eq!(yaml_leading_comments("kind: A\n"), "");
    }
}
