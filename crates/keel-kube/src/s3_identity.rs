//! Per-tenant SeaweedFS S3 identity provisioning (`NFR-SEC-02`, `NFR-SEC-05`).
//!
//! Each `tenant-<id>` namespace receives its own least-privilege SeaweedFS S3 identity scoped to
//! `<bucket>/tenants/<id>/*` instead of the control-plane admin keys. This module is the
//! server-side (kube-rs) counterpart of `keel bootstrap init`'s `kubectl exec` provisioning:
//!
//! 1. reuse the keys in `tenant-<id>/keel-tenant-s3-credentials` if present, otherwise generate
//!    fresh CSPRNG keys and persist them first (create-if-absent, so concurrent callers converge
//!    on a single key pair),
//! 2. feed the `weed shell` `s3.configure` script over the exec **stdin** channel of the SeaweedFS
//!    filer pod (keys never appear in argv, logs, or error messages),
//! 3. mirror the scoped identity into `tenant-<id>/keel-s3-credentials` (the name PKO
//!    `Stack.spec.envRefs` reference).
//!
//! The same pass ensures the tenant's Pulumi state passphrase Secret
//! (`tenant-<id>/keel-pulumi-passphrase`): generated once, reused forever after.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret};
use keel_core::KEEL_SYSTEM_NAMESPACE;
use kube::api::AttachParams;
use kube::{Api, Client};
use rand::Rng;
use tokio::io::AsyncWriteExt;

use crate::KubeError;
use crate::secret_store::{
    K8sSecretStore, SecretBackend, TENANT_S3_IDENTITY_PENDING_ANNOTATION,
    ensure_pulumi_passphrase_impl, ensure_tenant_scoped_s3_credentials_impl,
};
use crate::tenant::{
    TENANT_S3_CREDENTIALS_SECRET_NAME, build_tenant_s3_credentials_secret,
    s3_bucket_from_backend_url, seaweedfs_configure_tenant_identity_script, tenant_namespace,
};

/// Default SeaweedFS filer pod (`StatefulSet` ordinal 0) hosting `weed shell`.
pub const SEAWEEDFS_FILER_POD: &str = "s3-state-backend-seaweedfs-filer-0";

/// Default SeaweedFS master address passed to `weed shell`.
pub const SEAWEEDFS_MASTER_ADDRESS: &str = "s3-state-backend-seaweedfs-master:9333";

/// Upper bound for a single `weed shell` exec round-trip.
const WEED_SHELL_TIMEOUT: Duration = Duration::from_secs(60);

const ACCESS_KEY_LEN: usize = 20;
const SECRET_KEY_LEN: usize = 40;
const KEY_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Result of a tenant S3 identity provisioning attempt. Never carries key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantS3ProvisionOutcome {
    /// Fresh keys were generated, stored, and applied to SeaweedFS.
    Created,
    /// Existing keys from `keel-tenant-s3-credentials` were (re-)applied to SeaweedFS.
    Reused,
    /// `tenant-<id>` does not exist yet (onboarding PR not merged / reconciled); nothing changed.
    NamespaceMissing,
}

/// Cluster-side operations needed for provisioning (seam for unit tests).
#[async_trait]
pub trait SeaweedFsIdentityOps: Send + Sync {
    /// Returns `true` when `namespace` exists.
    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError>;

    /// Runs a `weed shell` script fed via stdin. Implementations must never log or surface the
    /// script or the command output (both may contain secret keys).
    async fn run_weed_shell(&self, script: &str) -> Result<(), KubeError>;
}

/// Provisions per-tenant scoped S3 state identities (used by `keel-server` tenant onboarding and
/// the proposal path when a tenant has no identity yet).
#[async_trait]
pub trait TenantS3IdentityProvisioner: Send + Sync {
    async fn provision_tenant_s3_identity(
        &self,
        tenant_id: &str,
    ) -> Result<TenantS3ProvisionOutcome, KubeError>;
}

/// Generates a random ASCII-alphanumeric string using the thread-local CSPRNG with rejection
/// sampling (no modulo bias).
fn random_alphanumeric(len: usize) -> String {
    let mut rng = rand::rng();
    let mut out = String::with_capacity(len);
    let limit = (256 / KEY_ALPHABET.len() * KEY_ALPHABET.len()) as u8;
    let mut buf = [0u8; 64];
    while out.len() < len {
        rng.fill_bytes(&mut buf);
        for b in buf {
            if b < limit && out.len() < len {
                out.push(KEY_ALPHABET[(b as usize) % KEY_ALPHABET.len()] as char);
            }
        }
    }
    out
}

/// Generates a fresh `(access_key, secret_key)` pair accepted by
/// [`seaweedfs_configure_tenant_identity_script`].
pub fn generate_tenant_s3_keys() -> (String, String) {
    (
        format!("KEEL{}", random_alphanumeric(ACCESS_KEY_LEN - 4)).to_uppercase(),
        random_alphanumeric(SECRET_KEY_LEN),
    )
}

fn usable_key(data: &BTreeMap<String, ByteString>, key: &str) -> Option<String> {
    let raw = data.get(key)?;
    let value = std::str::from_utf8(&raw.0).ok()?.trim().to_string();
    (value.len() >= 16 && value.chars().all(|c| c.is_ascii_alphanumeric())).then_some(value)
}

fn existing_keys(secret: Option<&Secret>) -> Option<(String, String)> {
    let data = secret?.data.as_ref()?;
    Some((
        usable_key(data, "accessKey")?,
        usable_key(data, "secretKey")?,
    ))
}

/// Converts the JSON `Secret` from [`build_tenant_s3_credentials_secret`] (`stringData`) into a
/// typed `Secret` with base64 `data`, so in-memory and API-server backends behave identically.
fn scoped_secret_from_value(value: serde_json::Value) -> Result<Secret, KubeError> {
    let mut secret: Secret = serde_json::from_value(value)?;
    let string_data = secret.string_data.take().unwrap_or_default();
    secret.data = Some(
        string_data
            .into_iter()
            .map(|(k, v)| (k, ByteString(v.into_bytes())))
            .collect(),
    );
    Ok(secret)
}

/// Idempotently provisions `tenant_id`'s scoped SeaweedFS identity. See module docs.
pub async fn provision_tenant_s3_identity_with<B, O>(
    secrets: &B,
    ops: &O,
    base_backend_url: &str,
    tenant_id: &str,
) -> Result<TenantS3ProvisionOutcome, KubeError>
where
    B: SecretBackend,
    O: SeaweedFsIdentityOps + ?Sized,
{
    let tenant_ns = tenant_namespace(tenant_id)?;
    let bucket = s3_bucket_from_backend_url(base_backend_url)?;
    if !ops.namespace_exists(&tenant_ns).await? {
        return Ok(TenantS3ProvisionOutcome::NamespaceMissing);
    }
    // Tenant state encryption passphrase (create-if-absent, never rotated).
    ensure_pulumi_passphrase_impl(secrets, &tenant_ns).await?;

    let existing = secrets
        .get_secret(&tenant_ns, TENANT_S3_CREDENTIALS_SECRET_NAME)
        .await?;
    let (access_key, secret_key, outcome) = match existing_keys(existing.as_ref()) {
        Some((a, s)) => (a, s, TenantS3ProvisionOutcome::Reused),
        None => {
            let (a, s) = generate_tenant_s3_keys();
            let mut desired = scoped_secret_from_value(build_tenant_s3_credentials_secret(
                tenant_id,
                &a,
                &s,
                base_backend_url,
            )?)?;
            // Not usable (never mirrored) until SeaweedFS has accepted the identity below.
            desired
                .metadata
                .annotations
                .get_or_insert_with(BTreeMap::new)
                .insert(
                    TENANT_S3_IDENTITY_PENDING_ANNOTATION.to_string(),
                    "true".to_string(),
                );
            let stored = if existing.is_some() {
                // Present but unusable (corrupted): replace it.
                secrets.upsert_secret(&tenant_ns, desired).await?
            } else {
                // Create-if-absent so concurrent provisioners converge on one key pair.
                secrets.create_secret_if_absent(&tenant_ns, desired).await?
            };
            let (a, s) =
                existing_keys(Some(&stored)).ok_or_else(|| KubeError::CorruptedSecret {
                    namespace: tenant_ns.clone(),
                    name: TENANT_S3_CREDENTIALS_SECRET_NAME.to_string(),
                    reason: "missing usable `accessKey`/`secretKey` after create".to_string(),
                })?;
            (a, s, TenantS3ProvisionOutcome::Created)
        }
    };

    let script =
        seaweedfs_configure_tenant_identity_script(&bucket, tenant_id, &access_key, &secret_key)?;
    ops.run_weed_shell(&script).await?;

    // Refresh labels/annotations/backend URL on the scoped secret (same keys), then mirror it.
    let refreshed = scoped_secret_from_value(build_tenant_s3_credentials_secret(
        tenant_id,
        &access_key,
        &secret_key,
        base_backend_url,
    )?)?;
    secrets.upsert_secret(&tenant_ns, refreshed).await?;
    if !ensure_tenant_scoped_s3_credentials_impl(secrets, &tenant_ns).await? {
        return Err(KubeError::CorruptedSecret {
            namespace: tenant_ns,
            name: TENANT_S3_CREDENTIALS_SECRET_NAME.to_string(),
            reason: "scoped identity disappeared while mirroring".to_string(),
        });
    }
    Ok(outcome)
}

/// Production provisioner: kube-rs `pods/exec` into the SeaweedFS filer + `Secret` API.
///
/// Requires RBAC for `create` on `pods/exec` (filer pod in `keel-system`), `get` on
/// `namespaces`, and `get/create/update` on `secrets` in `tenant-*` namespaces.
#[derive(Clone)]
pub struct SeaweedFsTenantS3Provisioner {
    client: Client,
    secrets: K8sSecretStore,
    base_backend_url: String,
    filer_namespace: String,
    filer_pod: String,
    master_address: String,
}

impl SeaweedFsTenantS3Provisioner {
    /// Creates a provisioner targeting the default in-cluster SeaweedFS release in `keel-system`.
    pub fn new(client: Client, base_backend_url: impl Into<String>) -> Self {
        Self {
            secrets: K8sSecretStore::new(client.clone()),
            client,
            base_backend_url: base_backend_url.into(),
            filer_namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            filer_pod: SEAWEEDFS_FILER_POD.to_string(),
            master_address: SEAWEEDFS_MASTER_ADDRESS.to_string(),
        }
    }
}

#[async_trait]
impl SeaweedFsIdentityOps for SeaweedFsTenantS3Provisioner {
    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        Ok(api.get_opt(namespace).await?.is_some())
    }

    async fn run_weed_shell(&self, script: &str) -> Result<(), KubeError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.filer_namespace);
        let command = [
            "weed".to_string(),
            "shell".to_string(),
            format!("-master={}", self.master_address),
        ];
        let params = AttachParams::default()
            .stdin(true)
            .stdout(true)
            .stderr(true);
        let run = async {
            let mut process = pods.exec(&self.filer_pod, command, &params).await?;
            let status = process.take_status();
            // Drain (and discard) output so the stream never back-pressures; it may echo keys.
            let mut stdout = process.stdout();
            let mut stderr = process.stderr();
            let drain_out = async {
                if let Some(ref mut s) = stdout {
                    let _ = tokio::io::copy(s, &mut tokio::io::sink()).await;
                }
            };
            let drain_err = async {
                if let Some(ref mut s) = stderr {
                    let _ = tokio::io::copy(s, &mut tokio::io::sink()).await;
                }
            };
            let feed = async {
                if let Some(mut stdin) = process.stdin() {
                    stdin.write_all(script.as_bytes()).await?;
                    stdin.write_all(b"exit\n").await?;
                    stdin.shutdown().await?;
                }
                Ok::<(), std::io::Error>(())
            };
            let (fed, (), ()) = tokio::join!(feed, drain_out, drain_err);
            fed.map_err(|e| KubeError::InvalidInput(format!("weed shell stdin failed: {e}")))?;
            let status = match status {
                Some(fut) => fut.await,
                None => None,
            };
            let _ = process.join().await;
            Ok::<_, KubeError>(status)
        };
        let status = tokio::time::timeout(WEED_SHELL_TIMEOUT, run)
            .await
            .map_err(|_| KubeError::InvalidInput("`weed shell` exec timed out".to_string()))??;
        match status.and_then(|s| s.status) {
            Some(ref s) if s == "Success" => Ok(()),
            other => Err(KubeError::InvalidInput(format!(
                "`weed shell` did not complete successfully (status: {})",
                other.as_deref().unwrap_or("unknown")
            ))),
        }
    }
}

#[async_trait]
impl TenantS3IdentityProvisioner for SeaweedFsTenantS3Provisioner {
    async fn provision_tenant_s3_identity(
        &self,
        tenant_id: &str,
    ) -> Result<TenantS3ProvisionOutcome, KubeError> {
        provision_tenant_s3_identity_with(&self.secrets, self, &self.base_backend_url, tenant_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret_store::{InMemorySecretStore, S3_CREDENTIALS_SECRET_NAME};
    use std::sync::Mutex;

    const BASE: &str = "s3://pulumi-state?endpoint=seaweedfs:8333&s3ForcePathStyle=true";

    #[derive(Default)]
    struct MockOps {
        namespace_present: bool,
        fail_shell: bool,
        scripts: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SeaweedFsIdentityOps for MockOps {
        async fn namespace_exists(&self, _namespace: &str) -> Result<bool, KubeError> {
            Ok(self.namespace_present)
        }
        async fn run_weed_shell(&self, script: &str) -> Result<(), KubeError> {
            self.scripts.lock().unwrap().push(script.to_string());
            if self.fail_shell {
                return Err(KubeError::InvalidInput("weed shell failed".into()));
            }
            Ok(())
        }
    }

    fn data_str(secret: &Secret, key: &str) -> String {
        String::from_utf8(secret.data.as_ref().unwrap()[key].0.clone()).unwrap()
    }

    #[test]
    fn generated_keys_are_valid_and_unique() {
        let (a1, s1) = generate_tenant_s3_keys();
        let (a2, s2) = generate_tenant_s3_keys();
        assert_eq!(a1.len(), ACCESS_KEY_LEN);
        assert_eq!(s1.len(), SECRET_KEY_LEN);
        assert!(a1.starts_with("KEEL"));
        assert!(
            a1.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
        assert!(s1.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a1, a2);
        assert_ne!(s1, s2);
        assert!(
            seaweedfs_configure_tenant_identity_script("pulumi-state", "acme", &a1, &s1).is_ok()
        );
    }

    #[tokio::test]
    async fn namespace_missing_is_a_noop() {
        let store = InMemorySecretStore::new();
        let ops = MockOps::default();
        let outcome = provision_tenant_s3_identity_with(&store, &ops, BASE, "acme")
            .await
            .unwrap();
        assert_eq!(outcome, TenantS3ProvisionOutcome::NamespaceMissing);
        assert!(ops.scripts.lock().unwrap().is_empty());
        assert!(
            store
                .inspect_raw_secret("tenant-acme", TENANT_S3_CREDENTIALS_SECRET_NAME)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn creates_then_reuses_scoped_identity_and_mirror() {
        let store = InMemorySecretStore::new();
        let ops = MockOps {
            namespace_present: true,
            ..MockOps::default()
        };
        let first = provision_tenant_s3_identity_with(&store, &ops, BASE, "acme")
            .await
            .unwrap();
        assert_eq!(first, TenantS3ProvisionOutcome::Created);

        let scoped = store
            .inspect_raw_secret("tenant-acme", TENANT_S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap();
        let access = data_str(&scoped, "accessKey");
        let secret = data_str(&scoped, "secretKey");
        assert_eq!(
            data_str(&scoped, "stateBackendUrl"),
            "s3://pulumi-state/tenants/acme?endpoint=seaweedfs:8333&s3ForcePathStyle=true"
        );
        let mirror = store
            .inspect_raw_secret("tenant-acme", S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(data_str(&mirror, "accessKey"), access);
        assert_eq!(data_str(&mirror, "secretKey"), secret);

        {
            let scripts = ops.scripts.lock().unwrap();
            assert_eq!(scripts.len(), 1);
            assert!(scripts[0].contains("-user=keel-tenant-acme"));
            assert!(scripts[0].contains("Read:pulumi-state/tenants/acme/*"));
            assert!(scripts[0].contains(&format!("-access_key={access}")));
            assert!(!scripts[0].contains("Admin"));
        }

        let second = provision_tenant_s3_identity_with(&store, &ops, BASE, "acme")
            .await
            .unwrap();
        assert_eq!(second, TenantS3ProvisionOutcome::Reused);
        let scoped2 = store
            .inspect_raw_secret("tenant-acme", TENANT_S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(data_str(&scoped2, "accessKey"), access);
        assert_eq!(data_str(&scoped2, "secretKey"), secret);
        let scripts = ops.scripts.lock().unwrap();
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0], scripts[1]);
    }

    #[tokio::test]
    async fn shell_failure_does_not_mirror_credentials_and_retry_reuses_keys() {
        let store = InMemorySecretStore::new();
        let failing = MockOps {
            namespace_present: true,
            fail_shell: true,
            ..MockOps::default()
        };
        let err = provision_tenant_s3_identity_with(&store, &failing, BASE, "acme")
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("access_key="));
        assert!(
            store
                .inspect_raw_secret("tenant-acme", S3_CREDENTIALS_SECRET_NAME)
                .await
                .is_none()
        );
        let pending = store
            .inspect_raw_secret("tenant-acme", TENANT_S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap();
        // Pending (not yet applied) identities are never mirrored for PKO Stacks.
        assert!(
            !ensure_tenant_scoped_s3_credentials_impl(&store, "tenant-acme")
                .await
                .unwrap()
        );

        let ok = MockOps {
            namespace_present: true,
            ..MockOps::default()
        };
        let outcome = provision_tenant_s3_identity_with(&store, &ok, BASE, "acme")
            .await
            .unwrap();
        assert_eq!(outcome, TenantS3ProvisionOutcome::Reused);
        let mirror = store
            .inspect_raw_secret("tenant-acme", S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(
            data_str(&mirror, "accessKey"),
            data_str(&pending, "accessKey")
        );
    }

    #[tokio::test]
    async fn rejects_invalid_inputs_before_touching_cluster() {
        let store = InMemorySecretStore::new();
        let ops = MockOps {
            namespace_present: true,
            ..MockOps::default()
        };
        assert!(
            provision_tenant_s3_identity_with(&store, &ops, BASE, "../evil")
                .await
                .is_err()
        );
        assert!(
            provision_tenant_s3_identity_with(&store, &ops, "file:///tmp", "acme")
                .await
                .is_err()
        );
        assert!(ops.scripts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn corrupted_scoped_secret_is_replaced() {
        let store = InMemorySecretStore::new();
        store
            .upsert_secret(
                "tenant-acme",
                Secret {
                    metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                        name: Some(TENANT_S3_CREDENTIALS_SECRET_NAME.to_string()),
                        ..Default::default()
                    },
                    data: Some(BTreeMap::from([(
                        "accessKey".to_string(),
                        ByteString(b"short".to_vec()),
                    )])),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ops = MockOps {
            namespace_present: true,
            ..MockOps::default()
        };
        let outcome = provision_tenant_s3_identity_with(&store, &ops, BASE, "acme")
            .await
            .unwrap();
        assert_eq!(outcome, TenantS3ProvisionOutcome::Created);
        let scoped = store
            .inspect_raw_secret("tenant-acme", TENANT_S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap();
        assert!(data_str(&scoped, "accessKey").starts_with("KEEL"));
    }

    #[tokio::test]
    async fn provisioning_creates_and_then_reuses_tenant_pulumi_passphrase() {
        use crate::secret_store::{PULUMI_PASSPHRASE_SECRET_KEY, PULUMI_PASSPHRASE_SECRET_NAME};

        let store = InMemorySecretStore::new();
        // Namespace missing: nothing (including the passphrase) is created.
        provision_tenant_s3_identity_with(&store, &MockOps::default(), BASE, "acme")
            .await
            .unwrap();
        assert!(
            store
                .inspect_raw_secret("tenant-acme", PULUMI_PASSPHRASE_SECRET_NAME)
                .await
                .is_none()
        );

        let ops = MockOps {
            namespace_present: true,
            ..MockOps::default()
        };
        provision_tenant_s3_identity_with(&store, &ops, BASE, "acme")
            .await
            .unwrap();
        let first = store
            .inspect_raw_secret("tenant-acme", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .expect("tenant passphrase provisioned with the scoped identity");
        let pass = data_str(&first, PULUMI_PASSPHRASE_SECRET_KEY);
        assert!(pass.len() >= 43);
        // The passphrase never reaches the `weed shell` script.
        assert!(!ops.scripts.lock().unwrap()[0].contains(&pass));

        provision_tenant_s3_identity_with(&store, &ops, BASE, "acme")
            .await
            .unwrap();
        let second = store
            .inspect_raw_secret("tenant-acme", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(data_str(&second, PULUMI_PASSPHRASE_SECRET_KEY), pass);
    }
}
