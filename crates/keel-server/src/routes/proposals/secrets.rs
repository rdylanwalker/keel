//! SOPS-encrypted proposal secrets: extraction, stored-secret reuse on edit and recipients.

use std::path::Path;

use keel_core::SopsManager;
use keel_kube::TenantAgeKey;
use serde_json::{Value, json};

use crate::error::ServerError;
use crate::routes::{read_contained_to_string, resolve_sops_recipients};
use crate::state::AppState;

use super::manifests::HELM_APP_SECRETS_FILE;

/// Renders schema secret values into `stringData` form (nulls dropped, scalars stringified).
pub(super) fn render_secret_string_data(
    secret_values: &Value,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if let Some(obj) = secret_values.as_object() {
        for (key, val) in obj {
            let rendered = match val {
                Value::Null => continue,
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Array(_) | Value::Object(_) => val.to_string(),
            };
            out.insert(key.clone(), rendered);
        }
    }
    out
}

pub(super) fn build_infra_secret_manifest_yaml(
    secret_name: &str,
    namespace: &str,
    secret_values: &Value,
) -> Result<(String, Vec<String>), ServerError> {
    let mut string_data = serde_json::Map::new();
    let mut keys = Vec::new();
    for (key, rendered) in render_secret_string_data(secret_values) {
        keys.push(key.clone());
        string_data.insert(key, Value::String(rendered));
    }
    let secret_doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": secret_name,
            "namespace": namespace,
            "annotations": {
                "kustomize.toolkit.fluxcd.io/prune": "disabled"
            },
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                // Flux reconciles the HelmRelease (`valuesFrom`) / Kustomization
                // (`substituteFrom`) as soon as the Secret appears or changes, instead of
                // waiting for the next interval after the first apply raced the config.
                "reconcile.fluxcd.io/watch": "Enabled"
            }
        },
        "type": "Opaque",
        "stringData": string_data,
    });
    let yaml = keel_core::yaml::to_string(&secret_doc)
        .map_err(|e| ServerError::Internal(format!("failed to serialize secret manifest: {e}")))?;
    Ok((yaml, keys))
}

/// SOPS-encrypted `<name>-secrets` `Secret` of a committed Flux manifest app.
pub(super) struct StoredManifestAppSecret {
    ciphertext: String,
    /// `stringData` / `data` keys (SOPS keeps key names in plaintext).
    pub(super) keys: std::collections::BTreeSet<String>,
    /// The `keys` holding a non-empty value. SOPS leaves an empty string unencrypted (`""`), so
    /// a manifest app's unset optional secret (stored as `""`) is told apart without decrypting.
    /// Only these count as stored when relaxing `required` on Edit.
    pub(super) valued_keys: std::collections::BTreeSet<String>,
}

/// Whether a SOPS-encrypted `stringData` / `data` value holds an empty string.
fn is_empty_sops_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty() || s.starts_with("ENC[AES256_GCM,data:,"),
        _ => false,
    }
}

/// Reads the committed `<name>-secrets` `Secret` of the Flux manifest app or Helm app in
/// `rel_dir`: `config/secrets.enc.yaml` (a manifest app's substitution sources) or the root
/// `secrets.enc.yaml` (a Helm app).
pub(super) async fn read_stored_manifest_app_secret(
    repo_workdir: &Path,
    rel_dir: &str,
) -> Option<StoredManifestAppSecret> {
    let dir = repo_workdir.join(rel_dir);
    let is_manifest_app = tokio::fs::try_exists(dir.join("manifests.yaml"))
        .await
        .unwrap_or(false);
    let path = if is_manifest_app {
        dir.join(keel_kube::MANIFEST_APP_CONFIG_DIR)
            .join("secrets.enc.yaml")
    } else {
        dir.join(HELM_APP_SECRETS_FILE)
    };
    let ciphertext = read_contained_to_string(repo_workdir, &path).await.ok()?;
    let doc = noyalib::from_str::<Value>(&ciphertext).ok()?;
    if doc.get("sops").is_none() || doc.get("kind").and_then(Value::as_str) != Some("Secret") {
        return None;
    }
    let entries: Vec<(&String, &Value)> = ["stringData", "data"]
        .iter()
        .filter_map(|field| doc.get(*field).and_then(Value::as_object))
        .flat_map(|m| m.iter())
        .collect();
    let keys: std::collections::BTreeSet<String> =
        entries.iter().map(|(k, _)| (*k).clone()).collect();
    let valued_keys = entries
        .iter()
        .filter(|(_, v)| !is_empty_sops_value(v))
        .map(|(k, _)| (*k).clone())
        .collect();
    (!keys.is_empty()).then_some(StoredManifestAppSecret {
        ciphertext,
        keys,
        valued_keys,
    })
}

/// Returns `schema` with every secret property whose value is already stored (`stored_keys`, a
/// [`StoredManifestAppSecret::valued_keys`]: an empty stored value does not count) removed from its parent's `required`: like the UI's edit form, a blank secret field keeps the
/// stored value. Secret fields without a stored value stay required. A manifest app stores
/// top-level fields (`adminPassword`); a Helm app stores dot paths (`gitea.admin.password`), so
/// nested objects are relaxed too.
pub(super) fn relax_stored_secret_requirements(
    schema: &Value,
    stored_keys: &std::collections::BTreeSet<String>,
) -> Value {
    fn relax(node: &mut Value, prefix: &str, stored_keys: &std::collections::BTreeSet<String>) {
        let Some(props) = node.get("properties").and_then(Value::as_object).cloned() else {
            return;
        };
        if let Some(required) = node.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|key| {
                let Some(key) = key.as_str() else {
                    return true;
                };
                let path = format!("{prefix}{key}");
                !(stored_keys.contains(&path)
                    && props.get(key).is_some_and(keel_core::is_secret_schema_node))
            });
        }
        for (key, child) in &props {
            if keel_core::is_secret_schema_node(child) {
                continue;
            }
            if let Some(child_node) = node.get_mut("properties").and_then(|p| p.get_mut(key)) {
                relax(child_node, &format!("{prefix}{key}."), stored_keys);
            }
        }
    }
    let mut out = schema.clone();
    relax(&mut out, "", stored_keys);
    out
}

/// Resolves the SOPS-encrypted `<name>-secrets` `Secret` to commit for a Flux manifest app.
/// See [`resolve_tenant_app_secret`].
///
/// Every top-level secret field (a `${field}` placeholder candidate) always has a key in the
/// Secret: an optional secret that was never set is stored as `""`. Flux substitutes the empty
/// value instead of failing the build with `variable not set`, and the catalog template stays
/// unchanged (no `${field:=}` default, which Keel would render at proposal time). The empty value
/// is only synthesized when `""` satisfies the field's own `schema` constraints (e.g. a
/// `minLength: 1` token must be set), so a catalog that rules out an empty secret fails closed
/// at proposal time instead of deploying without the credential.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_manifest_app_secret(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
    secret_name: &str,
    schema: &Value,
    schema_secret_paths: &[String],
    secret_values: &Value,
    stored: Option<&StoredManifestAppSecret>,
) -> Result<Option<String>, ServerError> {
    let submitted = render_secret_string_data(secret_values);
    let empty_defaults: Vec<String> = manifest_placeholder_secret_fields(schema_secret_paths)
        .into_iter()
        .filter(|key| !submitted.contains_key(key) && !stored.is_some_and(|s| s.keys.contains(key)))
        .collect();
    ensure_empty_secret_allowed(schema, &empty_defaults)?;
    Ok(resolve_tenant_app_secret(
        state,
        tenant,
        tenant_ns,
        secret_name,
        schema_secret_paths,
        submitted,
        &empty_defaults,
        stored,
    )
    .await?
    .map(|(ciphertext, _)| ciphertext))
}

/// Fails when `""` violates the schema of one of the unset top-level secret `fields` (only errors
/// at the field itself count; the object's other fields are not part of the probe).
fn ensure_empty_secret_allowed(schema: &Value, fields: &[String]) -> Result<(), ServerError> {
    for field in fields {
        let probe = json!({ field.as_str(): "" });
        let pointer = format!("/{}", field.replace('~', "~0").replace('/', "~1"));
        let rejected = match keel_core::validate_schema_values(schema, &probe) {
            Ok(()) => false,
            Err(keel_core::CoreError::SchemaValidation(errors)) => {
                errors.iter().any(|e| e.field_path == pointer)
            }
            Err(e) => return Err(e.into()),
        };
        if rejected {
            return Err(ServerError::BadRequest(format!(
                "Secret field '{field}' is not set, and its catalog schema does not allow the empty value Keel would substitute for it; set a value"
            )));
        }
    }
    Ok(())
}

/// Top-level secret fields of a manifest app schema: the only ones a `${field}` placeholder can
/// name (nested and array paths contain `.` / `[`).
fn manifest_placeholder_secret_fields(schema_secret_paths: &[String]) -> Vec<String> {
    schema_secret_paths
        .iter()
        .filter(|p| !p.contains(['.', '[']))
        .cloned()
        .collect()
}

/// Resolves the SOPS-encrypted `<name>-secrets` `Secret` (and its keys) to commit for an app
/// with secret fields, from the `submitted` `stringData` entries:
///
/// * The schema has no secret fields: `None` (any stored secret is removed).
/// * Nothing stored: a fresh `Secret` of the submitted values (`None` when none were submitted).
/// * Stored, no secret values submitted (edit): the stored ciphertext, unchanged.
/// * Stored, every stored key re-submitted: a fresh `Secret` of the submitted values.
/// * Stored, some keys re-submitted: the stored `Secret` is decrypted with the tenant `age` key,
///   the submitted keys are overlaid, and the result is re-encrypted.
///
/// Every `empty_defaults` key that is neither submitted nor stored is added as `""` (a Flux
/// manifest app's unset optional secrets); a stored `Secret` lacking such a key is merged and
/// re-encrypted.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_tenant_app_secret(
    state: &AppState,
    tenant: &str,
    tenant_ns: &str,
    secret_name: &str,
    schema_secret_paths: &[String],
    submitted: std::collections::BTreeMap<String, String>,
    empty_defaults: &[String],
    stored: Option<&StoredManifestAppSecret>,
) -> Result<Option<(String, std::collections::BTreeSet<String>)>, ServerError> {
    if schema_secret_paths.is_empty() {
        return Ok(None);
    }
    let missing_defaults: Vec<&String> = empty_defaults
        .iter()
        .filter(|key| {
            !submitted.contains_key(*key) && !stored.is_some_and(|s| s.keys.contains(*key))
        })
        .collect();
    let mut merged = match stored {
        None if submitted.is_empty() && missing_defaults.is_empty() => return Ok(None),
        Some(stored) if submitted.is_empty() && missing_defaults.is_empty() => {
            return Ok(Some((stored.ciphertext.clone(), stored.keys.clone())));
        }
        Some(stored) if !stored.keys.iter().all(|k| submitted.contains_key(k)) => {
            decrypt_stored_secret_string_data(state, tenant, &stored.ciphertext).await?
        }
        _ => std::collections::BTreeMap::new(),
    };
    // Drop stored keys whose field is no longer a secret field of the catalog schema.
    merged.retain(|key, _| {
        schema_secret_paths.iter().any(|p| {
            p == key || p.starts_with(&format!("{key}.")) || p.starts_with(&format!("{key}["))
        })
    });
    for key in missing_defaults {
        merged.insert(key.clone(), String::new());
    }
    merged.extend(submitted);
    let keys = merged.keys().cloned().collect();
    let merged_values = Value::Object(
        merged
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    );
    let (plain, _) = build_infra_secret_manifest_yaml(secret_name, tenant_ns, &merged_values)?;
    let recipients = tenant_sops_recipients(state, tenant).await?;
    Ok(Some((
        SopsManager::new()
            .encrypt_k8s_secret_yaml(&recipients, &plain)
            .await?,
        keys,
    )))
}

/// Decrypts a stored manifest-app `Secret` (encrypted to the tenant `age` key) into its
/// `stringData` (plus base64-decoded `data`). Never logs or returns the values in errors.
async fn decrypt_stored_secret_string_data(
    state: &AppState,
    tenant: &str,
    ciphertext: &str,
) -> Result<std::collections::BTreeMap<String, String>, ServerError> {
    use base64::Engine as _;
    let reenter = || {
        ServerError::Conflict(
            "The stored secret values could not be decrypted to keep the fields left blank; \
             re-enter every secret field"
                .to_string(),
        )
    };
    let age_key = state
        .secret_store
        .get_tenant_age_key(tenant)
        .await?
        .ok_or_else(reenter)?;
    let plain = SopsManager::new()
        .decrypt_yaml(&age_key.secret_key, ciphertext)
        .await
        .map_err(|err| {
            tracing::warn!(tenant = %tenant, error = %err, "Could not decrypt stored manifest app secret");
            reenter()
        })?;
    let doc: Value = noyalib::from_str(&plain).map_err(|_| reenter())?;
    let mut out = std::collections::BTreeMap::new();
    if let Some(data) = doc.get("data").and_then(Value::as_object) {
        for (key, val) in data {
            let decoded = val
                .as_str()
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(reenter)?;
            out.insert(key.clone(), decoded);
        }
    }
    if let Some(string_data) = doc.get("stringData").and_then(Value::as_object) {
        for (key, val) in string_data {
            let rendered = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(key.clone(), rendered);
        }
    }
    Ok(out)
}

/// SOPS recipients for `tenant` (its `age` key, generated and stored on first use, plus the
/// cluster recipient from `.sops.yaml`).
pub(super) async fn tenant_sops_recipients(
    state: &AppState,
    tenant: &str,
) -> Result<String, ServerError> {
    let age_key = match state.secret_store.get_tenant_age_key(tenant).await? {
        Some(existing) => existing,
        None => {
            let keypair = SopsManager::new().generate_age_keypair().await.map_err(|err| {
                tracing::warn!(tenant = %tenant, error = %err, "Could not generate tenant SOPS age key");
                ServerError::ServiceUnavailable(format!(
                    "Tenant '{tenant}' has no SOPS age key configured and one could not be generated ({err}); install `age-keygen` and `sops` on the keel-server host"
                ))
            })?;
            let generated = TenantAgeKey::new(keypair.public_key.clone(), keypair.secret_key());
            let _ = state
                .secret_store
                .store_tenant_age_key(tenant, &generated)
                .await;
            generated
        }
    };
    Ok(resolve_sops_recipients(&state.repo_workdir, &age_key.public_key).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_secret_fields_are_relaxed_only_when_stored() {
        let schema = json!({
            "type": "object",
            "required": ["hostname", "encKey", "adminPassword"],
            "properties": {
                "hostname": { "type": "string" },
                "encKey": { "type": "string", "writeOnly": true },
                "adminPassword": { "type": "string", "format": "password" },
            },
        });
        let stored = std::collections::BTreeSet::from([
            "encKey".to_string(),
            // Never relaxes a non-secret field, even if a key of that name is stored.
            "hostname".to_string(),
        ]);
        let relaxed = relax_stored_secret_requirements(&schema, &stored);
        assert_eq!(relaxed["required"], json!(["hostname", "adminPassword"]));
        assert_eq!(relaxed["properties"], schema["properties"]);
        let no_required = json!({ "type": "object" });
        assert_eq!(
            relax_stored_secret_requirements(&no_required, &stored),
            no_required
        );
    }

    #[test]
    fn empty_sops_values_are_detected_without_decrypting() {
        assert!(is_empty_sops_value(&json!("")));
        assert!(is_empty_sops_value(&Value::Null));
        assert!(is_empty_sops_value(&json!(
            "ENC[AES256_GCM,data:,iv:abc=,tag:def==,type:str]"
        )));
        assert!(!is_empty_sops_value(&json!(
            "ENC[AES256_GCM,data:scE=,iv:abc=,tag:def==,type:str]"
        )));
    }

    #[tokio::test]
    async fn stored_empty_placeholders_are_not_valued_keys() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("tenants/acme/apps/echo");
        std::fs::create_dir_all(app.join(keel_kube::MANIFEST_APP_CONFIG_DIR)).unwrap();
        std::fs::write(app.join("manifests.yaml"), "").unwrap();
        std::fs::write(
            app.join(keel_kube::MANIFEST_APP_CONFIG_DIR)
                .join("secrets.enc.yaml"),
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: echo-secrets\nstringData:\n  apiToken: \"\"\n  password: ENC[AES256_GCM,data:scE=,iv:a,tag:b,type:str]\nsops:\n  version: 3.13.3\n",
        )
        .unwrap();
        let stored = read_stored_manifest_app_secret(dir.path(), "tenants/acme/apps/echo")
            .await
            .unwrap();
        assert_eq!(
            stored.keys,
            std::collections::BTreeSet::from(["apiToken".to_string(), "password".to_string()])
        );
        assert_eq!(
            stored.valued_keys,
            std::collections::BTreeSet::from(["password".to_string()])
        );
    }

    #[test]
    fn unset_secret_must_accept_an_empty_value() {
        let schema = json!({
            "type": "object",
            "required": ["echoText"],
            "properties": {
                "echoText": { "type": "string" },
                "apiToken": { "type": "string", "writeOnly": true, "pattern": "^[a-z]{0,8}$" },
                "webhook": { "type": "string", "writeOnly": true, "minLength": 1 },
            },
        });
        // The probe ignores the object's other required fields.
        ensure_empty_secret_allowed(&schema, &["apiToken".to_string()]).unwrap();
        ensure_empty_secret_allowed(&schema, &[]).unwrap();
        let err =
            ensure_empty_secret_allowed(&schema, &["apiToken".to_string(), "webhook".to_string()])
                .unwrap_err();
        assert!(
            matches!(&err, ServerError::BadRequest(msg) if msg.contains("'webhook'")),
            "{err}"
        );
    }

    #[test]
    fn only_top_level_secret_fields_are_placeholder_defaults() {
        let paths = [
            "apiToken",
            "auth.password",
            "keys[]",
            "keys[].secret",
            "webhook",
        ]
        .map(String::from);
        assert_eq!(
            manifest_placeholder_secret_fields(&paths),
            vec!["apiToken".to_string(), "webhook".to_string()]
        );
    }
}
