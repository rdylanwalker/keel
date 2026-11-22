//! `keel-core`
//!
//! Core domain models, provider-agnostic Stage 0 `BootstrapTarget` trait & drivers,
//! `keel.io/v1alpha1` catalog parser, JSONSchema validator, Helm `values.yaml` renderer,
//! `AES-256-GCM` envelope encryption, and SOPS + `age` secrets management.

mod bootstrap;
mod catalog;
mod crypto;
mod management_addon;
mod models;
pub mod yaml;

use thiserror::Error;

pub use bootstrap::{
    BootstrapTarget, ClusterConnection, PulumiBootstrapTarget, STAGE0_BACKEND_DIR,
    STAGE0_PASSPHRASE_FILE, Stage0Passphrase, Stage0PassphraseSource,
    clear_stale_kind_stage0_state_in, ensure_private_dir, factory as bootstrap_target_factory,
    keel_config_dir, resolve_stage0_passphrase_in, stage0_cluster_dir_in, stage0_program_dir_in,
    stage0_state_root, validate_kubeconfig_yaml, write_private_file,
};
pub use catalog::{
    PartitionedValues, PreparedHelmValues, SchemaFieldError, changed_immutable_values,
    extract_manifest_variables, gateway_route_hostnames, helm_secret_value_fragments,
    is_secret_schema_node, keel_ingress_parent_refs, manifest_cluster_scoped_kinds, parse_catalog,
    parse_catalog_application, parse_catalog_blueprint, parse_catalog_bundle,
    partition_values_by_schema, platform_reserved_hostnames, prepare_helm_values,
    render_helm_values_yaml, render_manifest_template, resolve_helm_gateway_routes,
    route_hostname_matches, undeclared_value_paths, validate_k8s_manifests_yaml,
    validate_route_hostnames, validate_schema_values, verify_rendered_manifest_structure,
};
pub use crypto::{
    EnvelopeCrypto, SopsConfig, SopsCreationRule, SopsManager, generate_admin_password, sha256_hex,
};
pub use management_addon::{
    MANAGEMENT_CORE_ADDONS_DIR, ManagementAddonKind, ManagementAddonRef,
    apply_management_addon_upgrade, disallowed_value_changes, ensure_management_addon_protected,
    ensure_management_addon_upgradable, management_addon_chart, management_addon_desired_version,
    management_addon_mutable_values, management_addon_ref, merge_json_values,
    render_management_addon_document, validate_management_addon_document,
    validate_management_addon_version,
};
pub use models::{
    AppRuntime, AuthPhase, BootstrapConfig, BootstrapDistro, BootstrapTargetKind, BundleItemKind,
    CATALOG_API_VERSION, CATALOG_APPLICATION_KIND, CATALOG_BUNDLE_KIND,
    CATALOG_INFRA_BLUEPRINT_KIND, CatalogAppMetadata, CatalogAppSpec, CatalogApplication,
    CatalogBlueprintMetadata, CatalogBlueprintSpec, CatalogBundle, CatalogBundleItem,
    CatalogBundleMetadata, CatalogBundleSpec, CatalogInfraBlueprint, CatalogManifest,
    DEFAULT_CLUSTER_NAME, DEFAULT_POLL_INTERVAL, ForgeConfig, ForgeKind, HelmCrdPolicy,
    HelmNetworkPolicySpec, InfraResourceKind, KeelConfig, MANAGEMENT_CORE_CATEGORY, ManifestEngine,
    OIDC_CLIENT_ID_ENV, OIDC_CLIENT_SECRET_ENV, OIDC_ISSUER_URL_ENV, OIDC_REDIRECT_URL_ENV,
    OidcConfig, PlacementScope, PlacementSpec, ProgressiveDeliverySpec, ProviderRegistry,
    StackSshInfoResponse, UpstreamSource, clean_candidate_ssh_host, clean_candidate_ssh_user,
    is_private_or_internal_ip, is_private_or_internal_ssh_host, is_unresolved_ssh_target,
    resolve_ssh_host_key_from_sources, resolve_ssh_target_from_sources, validate_https_origin,
};

pub use yaml::{replace_yaml_document, split_yaml_documents, to_yaml_documents};

/// Flux CD controllers namespace (`SDD.md` §1).
pub const FLUX_SYSTEM_NAMESPACE: &str = "flux-system";

/// Keel control plane namespace (`SDD.md` §1).
pub const KEEL_SYSTEM_NAMESPACE: &str = "keel-system";

/// Isolated namespace for ephemeral PKO v2 PR preview stacks (`SDD.md` §4.2).
pub const KEEL_PREVIEWS_NAMESPACE: &str = "keel-previews";

/// ConfigMap (key `ca.crt`) holding only the **public** Keel CA certificate, present in
/// `keel-system`, `keel-previews` and every `tenant-<id>` namespace so PKO (operator and
/// workspace pods) trusts the in-cluster Forgejo HTTPS ingress. Never the CA private key.
pub const KEEL_CA_BUNDLE_CONFIGMAP: &str = "keel-ca-bundle";

/// Key of the PEM certificate in [`KEEL_CA_BUNDLE_CONFIGMAP`].
pub const KEEL_CA_BUNDLE_KEY: &str = "ca.crt";

/// Private in-cluster Forgejo organisation that holds the per-tenant repositories
/// (`SDD.md` §6.2). Created by `keel bootstrap init --with-forge`; the Stage 0 admin owns it.
pub const TENANT_REPOS_ORG: &str = "keel-tenants";

/// Dedicated non-admin, restricted Forgejo bot user. It only owns repository-scoped read tokens
/// (one per tenant repository); nothing else authenticates as it (`SDD.md` §6.2).
pub const TENANT_REPO_READER_USERNAME: &str = "keel-tenant-reader";

/// [`TENANT_REPOS_ORG`] team granting [`TENANT_REPO_READER_USERNAME`] read-only code access to
/// every current and future tenant repository.
pub const TENANT_REPO_READERS_TEAM: &str = "tenant-readers";

/// `keel-system` Secret (keys `username`, `password`) with the [`TENANT_REPO_READER_USERNAME`]
/// credentials. Forgejo only mints tokens over basic auth as the token owner, so the server uses
/// these to mint per-repository read tokens. Cluster-only, never written to Git.
pub const TENANT_REPO_READER_SECRET_NAME: &str = "keel-forgejo-tenant-reader";

/// Name prefix of the repository-scoped read tokens Keel mints as
/// [`TENANT_REPO_READER_USERNAME`]. Stage 0 deletes every other token the bot owns.
pub const TENANT_REPO_TOKEN_NAME_PREFIX: &str = "keel-tenant-repo-";

/// Unified error type for `keel-core` operations.
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] noyalib::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("{}", format_schema_validation_errors(.0))]
    SchemaValidation(Vec<SchemaFieldError>),

    #[error("Crypto error: {0}")]
    Crypto(String),

    #[error("Command '{program}' failed: {message}")]
    CommandFailed { program: String, message: String },
}

fn format_schema_validation_errors(errors: &[SchemaFieldError]) -> String {
    if errors.is_empty() {
        return "Schema validation failed with 0 field error(s)".to_string();
    }
    let details: Vec<String> = errors
        .iter()
        .map(|e| {
            if e.field_path.is_empty() {
                e.message.clone()
            } else {
                format!("{}: {}", e.field_path, e.message)
            }
        })
        .collect();
    format!(
        "Schema validation failed with {} field error(s): {}",
        errors.len(),
        details.join("; ")
    )
}

/// Validates that `value` conforms to a Kubernetes DNS-1123 label (`[a-z0-9]([-a-z0-9]*[a-z0-9])?`, max 63 chars).
///
/// Used across cluster names, tenant IDs, provider IDs, and catalog IDs to guarantee Kubernetes
/// compatibility and prevent CLI flag smuggling (`-...`), path traversal (`..`), and regex injection.
pub fn validate_dns_label(value: &str, field_name: &str) -> Result<(), CoreError> {
    if value.is_empty() || value.len() > 63 {
        return Err(CoreError::Validation(format!(
            "{field_name} must be between 1 and 63 characters, got {}",
            value.len()
        )));
    }

    let bytes = value.as_bytes();
    let is_lower_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();

    if !is_lower_alnum(bytes[0]) || !is_lower_alnum(bytes[bytes.len() - 1]) {
        return Err(CoreError::Validation(format!(
            "{field_name} ('{value}') must start and end with a lowercase ASCII letter or digit"
        )));
    }

    if !bytes.iter().all(|&b| is_lower_alnum(b) || b == b'-') {
        return Err(CoreError::Validation(format!(
            "{field_name} ('{value}') may only contain lowercase ASCII letters, digits, and '-'"
        )));
    }

    Ok(())
}

/// Computes and validates the dedicated Kubernetes namespace `tenant-<tenant_id>` (`NFR-SEC-02`).
pub fn tenant_namespace(tenant_id: &str) -> Result<String, CoreError> {
    validate_dns_label(tenant_id, "tenant_id")?;
    let ns = format!("tenant-{tenant_id}");
    validate_dns_label(&ns, "tenant_namespace")?;
    Ok(ns)
}

/// Percent-encodes a string according to RFC 3986 unreserved characters (`A-Z`, `a-z`, `0-9`, `-`, `_`, `.`, `~`).
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entity_meta_and_namespace_helpers() {
        assert_eq!(KEEL_SYSTEM_NAMESPACE, "keel-system");
        assert_eq!(FLUX_SYSTEM_NAMESPACE, "flux-system");
        assert_eq!(KEEL_PREVIEWS_NAMESPACE, "keel-previews");

        assert_eq!(tenant_namespace("acme").unwrap(), "tenant-acme");
        assert!(tenant_namespace("").is_err());
        assert!(tenant_namespace("-leading-hyphen").is_err());
        assert!(tenant_namespace("trailing-hyphen-").is_err());
        assert!(tenant_namespace("bad_underscore").is_err());
        // Over 56 chars in tenant_id exceeds 63 chars when prefixed with "tenant-"
        let too_long = "a".repeat(57);
        assert!(tenant_namespace(&too_long).is_err());

        assert_eq!(
            percent_encode("keel-io/infra gitops"),
            "keel-io%2Finfra%20gitops"
        );
    }

    #[test]
    fn test_schema_validation_error_display_includes_fields() {
        let err = CoreError::SchemaValidation(vec![
            SchemaFieldError {
                field_path: String::new(),
                schema_path: "/required".to_string(),
                message: "\"ingressHost\" is a required property".to_string(),
            },
            SchemaFieldError {
                field_path: "/apiToken".to_string(),
                schema_path: "/properties/apiToken/minLength".to_string(),
                message: "must be at least 8 characters".to_string(),
            },
        ]);
        let rendered = err.to_string();
        assert!(rendered.contains("2 field error(s)"));
        assert!(rendered.contains("\"ingressHost\" is a required property"));
        assert!(rendered.contains("/apiToken: must be at least 8 characters"));
    }
}
