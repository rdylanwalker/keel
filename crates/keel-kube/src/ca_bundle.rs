//! Public Keel CA trust bundle (`keel-ca-bundle` ConfigMap, `SDD.md` §4.2, §6.2).
//!
//! The in-cluster Forgejo is served over HTTPS by Envoy Gateway (`https://forgejo.<domain>`) with
//! a certificate signed by the self-signed Keel CA (`keel-system/keel-ca-secret`). PKO cannot
//! clone plain `http://` Git URLs, so the PKO operator and the tenant / preview workspace pods
//! (`fetch` init container and `pulumi` container) trust that CA through a ConfigMap holding
//! **only the public certificate** (key `ca.crt`):
//!
//! * `keel-system/keel-ca-bundle` is written by `keel bootstrap init` from the `ca.crt` of
//!   `keel-system/keel-ca-secret` (mounted by the PKO operator);
//! * Kyverno `GeneratingPolicy` `sync-keel-ca-bundle` clones it into `keel-previews` and every
//!   `tenant-*` namespace (new and existing), keeping the copies synchronized;
//! * the preview runner refreshes the `keel-previews` copy before each preview.
//!
//! The CA private key (`keel-ca-secret` `tls.key`) and the wildcard certificate Secret
//! (`keel-wildcard-tls`) are never copied or mounted.

use std::collections::BTreeMap;

use keel_core::{
    KEEL_CA_BUNDLE_CONFIGMAP, KEEL_CA_BUNDLE_KEY, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE,
};
use serde_json::{Value, json};

use crate::KubeError;
use crate::tenant::{TENANT_NAMESPACE_PREFIX, validate_resource_name};

/// cert-manager `Secret` of the self-signed Keel CA (`deploy/bootstrap/manifests/local-ca.yaml`).
/// Holds the CA **private key**: only its public `ca.crt` is ever copied.
pub const KEEL_CA_SECRET_NAME: &str = "keel-ca-secret";

/// Label marking `keel-ca-bundle` ConfigMaps (source and copies).
pub const KEEL_CA_BUNDLE_LABEL: &str = "keel.io/ca-bundle";

/// `true` for namespaces that receive a copy of `keel-system/keel-ca-bundle`: `keel-previews` and
/// every `tenant-<id>` namespace (where PKO workspace pods clone tenant Git sources).
pub fn is_keel_ca_bundle_target_namespace(namespace: &str) -> bool {
    namespace == KEEL_PREVIEWS_NAMESPACE
        || namespace
            .strip_prefix(TENANT_NAMESPACE_PREFIX)
            .is_some_and(|id| !id.is_empty())
}

/// Validates that `pem` is one or more public PEM certificates and nothing else (no private key,
/// no other PEM block types) and returns it with a trailing newline.
pub fn validate_public_ca_pem(pem: &str) -> Result<String, KubeError> {
    let pem = pem.trim();
    let invalid = |why: &str| {
        Err(KubeError::InvalidInput(format!(
            "Keel CA bundle {why}; expected only PEM `CERTIFICATE` blocks (the public `ca.crt`)"
        )))
    };
    if pem.is_empty() {
        return invalid("is empty");
    }
    let mut certificates = 0usize;
    for line in pem.lines().map(str::trim) {
        if let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|l| l.strip_suffix("-----"))
        {
            if label != "CERTIFICATE" {
                return invalid(&format!("contains a `{label}` PEM block"));
            }
            certificates += 1;
        }
    }
    if certificates == 0 {
        return invalid("has no PEM certificate");
    }
    Ok(format!("{pem}\n"))
}

/// Builds the `keel-ca-bundle` ConfigMap (key `ca.crt`, public certificate only) in `namespace`.
pub fn build_keel_ca_bundle_configmap(namespace: &str, ca_pem: &str) -> Result<Value, KubeError> {
    validate_resource_name("namespace", namespace)?;
    if namespace != KEEL_SYSTEM_NAMESPACE && !is_keel_ca_bundle_target_namespace(namespace) {
        return Err(KubeError::InvalidInput(format!(
            "the Keel CA bundle is only distributed to `{KEEL_SYSTEM_NAMESPACE}`, \
             `{KEEL_PREVIEWS_NAMESPACE}` and `tenant-*` namespaces, not `{namespace}`"
        )));
    }
    let pem = validate_public_ca_pem(ca_pem)?;
    Ok(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": KEEL_CA_BUNDLE_CONFIGMAP,
            "namespace": namespace,
            "labels": BTreeMap::from([
                ("app.kubernetes.io/managed-by", "keel"),
                (KEEL_CA_BUNDLE_LABEL, "true"),
            ]),
        },
        "data": { KEEL_CA_BUNDLE_KEY: pem },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIQ\n-----END CERTIFICATE-----";

    #[test]
    fn ca_bundle_holds_only_the_public_certificate() {
        let cm = build_keel_ca_bundle_configmap("tenant-acme", CA).unwrap();
        assert_eq!(cm["kind"], "ConfigMap");
        assert_eq!(cm["metadata"]["name"], "keel-ca-bundle");
        assert_eq!(cm["metadata"]["namespace"], "tenant-acme");
        assert_eq!(cm["metadata"]["labels"]["keel.io/ca-bundle"], "true");
        assert_eq!(cm["data"]["ca.crt"], format!("{CA}\n"));
        assert_eq!(cm["data"].as_object().unwrap().len(), 1);
        for ns in ["keel-system", "keel-previews", "tenant-default"] {
            assert!(build_keel_ca_bundle_configmap(ns, CA).is_ok(), "{ns}");
        }
        for ns in [
            "default",
            "flux-system",
            "tenant-",
            "kube-system",
            "Tenant-x",
        ] {
            assert!(build_keel_ca_bundle_configmap(ns, CA).is_err(), "{ns}");
        }
        // Never a private key (the CA Secret also holds `tls.key`).
        for bad in [
            "",
            "not a pem",
            &format!("{CA}\n-----BEGIN EC PRIVATE KEY-----\nAAA\n-----END EC PRIVATE KEY-----"),
            "-----BEGIN PRIVATE KEY-----\nAAA\n-----END PRIVATE KEY-----",
        ] {
            let err = build_keel_ca_bundle_configmap("keel-system", bad).unwrap_err();
            assert!(!err.to_string().contains("AAA"), "never echo key material");
        }
        // A chain of public certificates is fine.
        assert!(validate_public_ca_pem(&format!("{CA}\n{CA}")).is_ok());
    }

    #[test]
    fn ca_bundle_targets_previews_and_tenant_namespaces() {
        assert!(is_keel_ca_bundle_target_namespace("keel-previews"));
        assert!(is_keel_ca_bundle_target_namespace("tenant-acme"));
        assert!(!is_keel_ca_bundle_target_namespace("tenant-"));
        assert!(!is_keel_ca_bundle_target_namespace("keel-system"));
        assert!(!is_keel_ca_bundle_target_namespace("flux-system"));
    }
}
