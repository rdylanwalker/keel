//! Stateless Kubernetes `Secret`-backed state persistence (`SDD.md` §6.3).
//!
//! `keel-server` uses zero external or embedded databases (no SQLite/Postgres).
//! Runtime credentials, single-use Stage 0 bootstrap tokens, OIDC session signing keys,
//! `AES-256-GCM`-encrypted user Git credentials, and per-tenant SOPS `age` keys are
//! persisted natively as Kubernetes `Secret` resources.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use async_trait::async_trait;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use keel_core::KEEL_SYSTEM_NAMESPACE;
use kube::api::PostParams;
use kube::{Api, Client};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::KubeError;
use crate::tenant::{
    TENANT_NAMESPACE_PREFIX, TENANT_S3_CREDENTIALS_SECRET_KIND, TENANT_S3_CREDENTIALS_SECRET_NAME,
    tenant_namespace,
};

/// Secret name holding the cluster-managed 256-bit session key in `keel-system`.
pub const SESSION_KEY_SECRET_NAME: &str = "session-key";

/// Secret name prefix for `AES-256-GCM`-encrypted user Git credentials in `keel-system`.
pub const USER_GIT_CREDS_SECRET_PREFIX: &str = "user-git-creds-";

/// Secret name holding the per-tenant SOPS `age` keypair in `tenant-<id>`.
pub const TENANT_SOPS_AGE_SECRET_NAME: &str = "sops-age";

/// Secret name holding the in-cluster S3 state backend credentials (`accessKey`, `secretKey`).
///
/// In `keel-system`/`flux-system`/`keel-previews` this holds the SeaweedFS admin identity. In
/// `tenant-*` namespaces it is only ever a mirror of the tenant-scoped
/// `keel-tenant-s3-credentials` identity (never the admin keys).
pub const S3_CREDENTIALS_SECRET_NAME: &str = "keel-s3-credentials";

/// Annotation set on `keel-tenant-s3-credentials` while its keys are stored but not yet applied
/// to SeaweedFS; such identities are never mirrored for PKO `Stack`s.
pub const TENANT_S3_IDENTITY_PENDING_ANNOTATION: &str = "keel.io/s3-identity-pending";

/// Optional secret name holding a GitHub token (`token`) for PKO workspace plugin downloads.
pub const GITHUB_TOKEN_SECRET_NAME: &str = "keel-github-token";

pub use keel_pko::{PULUMI_PASSPHRASE_SECRET_KEY, PULUMI_PASSPHRASE_SECRET_NAME};

/// `keel.io/secret-kind` label value for Pulumi state passphrase Secrets.
pub const PULUMI_PASSPHRASE_SECRET_KIND: &str = "pulumi-passphrase";

/// Random bytes behind a generated Pulumi state passphrase (256-bit).
pub const PULUMI_PASSPHRASE_BYTES: usize = 32;

/// Length in bytes of the cluster session / symmetric encryption key (256-bit).
pub const SESSION_KEY_LEN: usize = 32;

/// Length in bytes of an `AES-256-GCM` random nonce (96-bit).
const AES_GCM_NONCE_LEN: usize = 12;

const SESSION_KEY_FIELD: &str = "session-key";
const CIPHERTEXT_FIELD: &str = "ciphertext";
const NONCE_FIELD: &str = "nonce";
const ALGORITHM_FIELD: &str = "algorithm";
const ALGORITHM_AES_256_GCM: &[u8] = b"AES-256-GCM";
const AGE_SECRET_FILE_FIELD: &str = "age.agekey";
const AGE_PUBLIC_KEY_FIELD: &str = "public-key";
const AGE_SECRET_KEY_FIELD: &str = "secret-key";

/// Computes the deterministic Kubernetes `Secret` name (`user-git-creds-<sub_hash>`)
/// in `keel-system` for an authenticated OIDC `sub` subject identifier.
pub fn user_git_creds_secret_name(oidc_sub: &str) -> Result<String, KubeError> {
    let trimmed = oidc_sub.trim();
    if trimmed.is_empty() {
        return Err(KubeError::InvalidInput(
            "OIDC subject (`sub`) must not be empty".to_string(),
        ));
    }
    let sub_hash = hex::encode(Sha256::digest(trimmed.as_bytes()));
    Ok(format!("{USER_GIT_CREDS_SECRET_PREFIX}{sub_hash}"))
}

/// User Git credentials stored encrypted at rest in `keel-system/user-git-creds-<sub_hash>`.
///
/// Custom [`fmt::Debug`] redacts secret material to satisfy `NFR-SEC-05`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserGitCredentials {
    pub ssh_private_key: Option<String>,
    pub personal_access_token: Option<String>,
    pub username: Option<String>,
}

impl UserGitCredentials {
    pub fn new(
        ssh_private_key: Option<String>,
        personal_access_token: Option<String>,
        username: Option<String>,
    ) -> Self {
        Self {
            ssh_private_key,
            personal_access_token,
            username,
        }
    }
}

impl fmt::Debug for UserGitCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserGitCredentials")
            .field(
                "ssh_private_key",
                &self.ssh_private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "personal_access_token",
                &self.personal_access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("username", &self.username)
            .finish()
    }
}

/// Per-tenant SOPS `age` key material stored in `tenant-<id>/sops-age`.
///
/// Custom [`fmt::Debug`] redacts `secret_key` to satisfy `NFR-SEC-05`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantAgeKey {
    pub public_key: String,
    pub secret_key: String,
}

impl TenantAgeKey {
    pub fn new(public_key: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self {
            public_key: public_key.into(),
            secret_key: secret_key.into(),
        }
    }

    /// Formats the keypair in standard `.agekey` format expected by Flux `kustomize-controller`.
    pub fn to_sops_agekey_file(&self) -> String {
        format!(
            "# public key: {}\n{}\n",
            self.public_key.trim(),
            self.secret_key.trim()
        )
    }
}

impl fmt::Debug for TenantAgeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantAgeKey")
            .field("public_key", &self.public_key)
            .field("secret_key", &"[REDACTED]")
            .finish()
    }
}

/// Stateless runtime persistence interface backed exclusively by Kubernetes `Secret`s.
#[async_trait]
pub trait SecretStateStore: Send + Sync {
    /// Retrieves the 256-bit session key from `keel-system/session-key`, generating and persisting
    /// a new CSPRNG key atomically if absent.
    async fn get_or_create_session_key(&self) -> Result<[u8; SESSION_KEY_LEN], KubeError>;

    /// Encrypts `creds` with `AES-256-GCM` (binding `oidc_sub` as AAD) and persists only the
    /// ciphertext and random nonce in `keel-system/user-git-creds-<sub_hash>`.
    async fn put_user_git_creds(
        &self,
        oidc_sub: &str,
        creds: &UserGitCredentials,
        encryption_key: &[u8; SESSION_KEY_LEN],
    ) -> Result<(), KubeError>;

    /// Reads and decrypts `AES-256-GCM` user Git credentials from `keel-system/user-git-creds-<sub_hash>`.
    async fn get_user_git_creds(
        &self,
        oidc_sub: &str,
        encryption_key: &[u8; SESSION_KEY_LEN],
    ) -> Result<Option<UserGitCredentials>, KubeError>;

    /// Stores the per-tenant SOPS `age` keypair in `tenant-<id>/sops-age`.
    async fn store_tenant_age_key(
        &self,
        tenant_id: &str,
        age_key: &TenantAgeKey,
    ) -> Result<(), KubeError>;

    /// Retrieves the per-tenant SOPS `age` keypair from `tenant-<id>/sops-age`.
    async fn get_tenant_age_key(&self, tenant_id: &str) -> Result<Option<TenantAgeKey>, KubeError>;

    /// Returns `true` when `keel-system/keel-s3-credentials` exists in the cluster.
    async fn has_s3_credentials(&self) -> Result<bool, KubeError>;

    /// Ensures `keel-s3-credentials` exists in `target_namespace`.
    ///
    /// Control-plane namespaces receive a copy of `keel-system/keel-s3-credentials`. `tenant-*`
    /// namespaces only ever receive a mirror of their own scoped `keel-tenant-s3-credentials`
    /// (never the admin keys). Returns `Ok(false)` when no suitable source exists.
    async fn ensure_s3_credentials_in_namespace(
        &self,
        target_namespace: &str,
    ) -> Result<bool, KubeError>;

    /// Ensures the Pulumi state passphrase Secret (`keel-pulumi-passphrase`, key `passphrase`)
    /// exists in `namespace`: an existing usable passphrase is always reused (never rotated);
    /// otherwise a fresh 256-bit URL-safe passphrase is generated and created-if-absent (so
    /// concurrent callers converge on one value). A present but empty/unreadable passphrase is
    /// reported as corrupted and never silently replaced (it may guard existing state).
    async fn ensure_pulumi_passphrase(
        &self,
        namespace: &str,
    ) -> Result<PulumiPassphraseOutcome, KubeError>;

    /// Returns `true` when `namespace` holds a usable (non-empty) Pulumi state passphrase.
    async fn has_pulumi_passphrase(&self, namespace: &str) -> Result<bool, KubeError>;

    /// Ensures `keel-github-token` exists in `target_namespace` by copying it from
    /// `keel-system/keel-github-token` if present. Returns `Ok(true)` if propagated or already
    /// present, or `Ok(false)` if the source secret does not exist in `keel-system`.
    async fn ensure_github_token_in_namespace(
        &self,
        target_namespace: &str,
    ) -> Result<bool, KubeError>;

    /// `true` when `namespace` exists. A tenant namespace only exists once `keel-tenants` has
    /// applied the merged tenant stub, so runtime tenant Secrets are written after that.
    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError>;

    /// Creates `tenant-<id>/sops-age` holding `age_key` only when the tenant has no key yet
    /// (create-if-absent: an existing key is never replaced, because Git content may already be
    /// encrypted to it). Returns `true` when this call created it.
    async fn create_tenant_age_key_if_absent(
        &self,
        tenant_id: &str,
        age_key: &TenantAgeKey,
    ) -> Result<bool, KubeError>;

    /// Reads the cluster-only `tenant-<id>/keel-gitops-credentials` Secret (per-tenant
    /// repository read credential), if any. Callers must never log or return its data.
    async fn get_tenant_gitops_credentials(
        &self,
        tenant_id: &str,
    ) -> Result<Option<Secret>, KubeError>;

    /// Creates or replaces `tenant-<id>/keel-gitops-credentials` with `secret` (built by
    /// [`crate::tenant_gitops_credentials_secret`]). Refuses any other Secret.
    async fn put_tenant_gitops_credentials(
        &self,
        tenant_id: &str,
        secret: Secret,
    ) -> Result<(), KubeError>;

    /// Creates `tenant-<id>/keel-gitops-credentials` only when it does not exist yet
    /// (create-if-absent, so concurrent writers cannot overwrite each other). Returns `true` when
    /// this call created it. Refuses any other Secret.
    async fn create_tenant_gitops_credentials_if_absent(
        &self,
        tenant_id: &str,
        secret: Secret,
    ) -> Result<bool, KubeError>;
}

/// Result of [`SecretStateStore::ensure_pulumi_passphrase`]. Never carries the passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulumiPassphraseOutcome {
    /// A new passphrase was generated and stored.
    Created,
    /// An existing passphrase was kept.
    Reused,
}

/// Generates a Pulumi state passphrase: 256 bits from the OS-seeded CSPRNG, base64url (no
/// padding, 43 chars) so it is safe in env vars, YAML and URLs.
pub fn generate_pulumi_passphrase() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; PULUMI_PASSPHRASE_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Builds the `keel-pulumi-passphrase` Secret for `namespace` holding `passphrase`.
pub fn build_pulumi_passphrase_secret(namespace: &str, passphrase: &str) -> Secret {
    Secret {
        metadata: ObjectMeta {
            name: Some(PULUMI_PASSPHRASE_SECRET_NAME.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(managed_labels(PULUMI_PASSPHRASE_SECRET_KIND)),
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(BTreeMap::from([(
            PULUMI_PASSPHRASE_SECRET_KEY.to_string(),
            ByteString(passphrase.as_bytes().to_vec()),
        )])),
        ..Secret::default()
    }
}

fn usable_passphrase(secret: &Secret) -> bool {
    secret
        .data
        .as_ref()
        .and_then(|d| d.get(PULUMI_PASSPHRASE_SECRET_KEY))
        .and_then(|v| std::str::from_utf8(&v.0).ok())
        .is_some_and(|p| !p.trim().is_empty())
}

async fn has_pulumi_passphrase_impl(
    backend: &impl SecretBackend,
    namespace: &str,
) -> Result<bool, KubeError> {
    Ok(backend
        .get_secret(namespace, PULUMI_PASSPHRASE_SECRET_NAME)
        .await?
        .as_ref()
        .is_some_and(usable_passphrase))
}

pub(crate) async fn ensure_pulumi_passphrase_impl(
    backend: &impl SecretBackend,
    namespace: &str,
) -> Result<PulumiPassphraseOutcome, KubeError> {
    let corrupted = || KubeError::CorruptedSecret {
        namespace: namespace.to_string(),
        name: PULUMI_PASSPHRASE_SECRET_NAME.to_string(),
        reason: format!("missing or empty `{PULUMI_PASSPHRASE_SECRET_KEY}`"),
    };
    if let Some(existing) = backend
        .get_secret(namespace, PULUMI_PASSPHRASE_SECRET_NAME)
        .await?
    {
        return if usable_passphrase(&existing) {
            Ok(PulumiPassphraseOutcome::Reused)
        } else {
            Err(corrupted())
        };
    }
    let desired = build_pulumi_passphrase_secret(namespace, &generate_pulumi_passphrase());
    let stored = backend.create_secret_if_absent(namespace, desired).await?;
    if !usable_passphrase(&stored) {
        return Err(corrupted());
    }
    Ok(PulumiPassphraseOutcome::Created)
}

/// Low-level `Secret` CRUD abstraction shared between `K8sSecretStore` and `InMemorySecretStore`.
#[async_trait]
pub trait SecretBackend: Send + Sync {
    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>, KubeError>;
    async fn upsert_secret(&self, namespace: &str, secret: Secret) -> Result<Secret, KubeError>;
    async fn create_secret_if_absent(
        &self,
        namespace: &str,
        secret: Secret,
    ) -> Result<Secret, KubeError>;
    /// `true` when `namespace` exists (in-memory stores: unless marked absent).
    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError>;
}

/// Production `SecretStateStore` backed by the Kubernetes API server (`kube::Api<Secret>`).
#[derive(Clone)]
pub struct K8sSecretStore {
    client: Client,
}

impl K8sSecretStore {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[async_trait]
impl SecretBackend for K8sSecretStore {
    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>, KubeError> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        Ok(api.get_opt(name).await?)
    }

    async fn upsert_secret(
        &self,
        namespace: &str,
        mut secret: Secret,
    ) -> Result<Secret, KubeError> {
        let name =
            secret.metadata.name.clone().ok_or_else(|| {
                KubeError::InvalidInput("Secret metadata.name is required".into())
            })?;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);

        if let Some(existing) = api.get_opt(&name).await? {
            secret.metadata.resource_version = existing.metadata.resource_version;
            secret.metadata.uid = existing.metadata.uid;
            return Ok(api.replace(&name, &PostParams::default(), &secret).await?);
        }

        match api.create(&PostParams::default(), &secret).await {
            Ok(created) => Ok(created),
            Err(kube::Error::Api(err)) if err.code == 409 => {
                let existing = api.get(&name).await?;
                secret.metadata.resource_version = existing.metadata.resource_version;
                secret.metadata.uid = existing.metadata.uid;
                Ok(api.replace(&name, &PostParams::default(), &secret).await?)
            }
            Err(err) => Err(KubeError::Kube(err)),
        }
    }

    async fn create_secret_if_absent(
        &self,
        namespace: &str,
        secret: Secret,
    ) -> Result<Secret, KubeError> {
        let name =
            secret.metadata.name.clone().ok_or_else(|| {
                KubeError::InvalidInput("Secret metadata.name is required".into())
            })?;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);

        match api.create(&PostParams::default(), &secret).await {
            Ok(created) => Ok(created),
            Err(kube::Error::Api(err)) if err.code == 409 => Ok(api.get(&name).await?),
            Err(err) => Err(KubeError::Kube(err)),
        }
    }

    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError> {
        let api: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(self.client.clone());
        Ok(api.get_opt(namespace).await?.is_some())
    }
}

#[derive(Default)]
struct InMemoryState {
    secrets: HashMap<(String, String), Secret>,
    revision_counter: u64,
    /// Namespaces reported as missing by [`SecretBackend::namespace_exists`] (tests).
    absent_namespaces: std::collections::HashSet<String>,
}

/// In-memory `SecretStateStore` storing real `k8s_openapi::api::core::v1::Secret` objects
/// with optimistic concurrency (`resourceVersion` + `uid`) for unit tests and cluster-less local dev.
#[derive(Clone, Default)]
pub struct InMemorySecretStore {
    state: Arc<RwLock<InMemoryState>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspects a raw Kubernetes `Secret` stored in memory (useful in tests to verify
    /// that sensitive values are stored strictly as `AES-256-GCM` ciphertext).
    pub async fn inspect_raw_secret(&self, namespace: &str, name: &str) -> Option<Secret> {
        let guard = self.state.read().await;
        guard
            .secrets
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
    }

    /// Marks `namespace` as missing (`present = false`) or existing for
    /// [`SecretStateStore::namespace_exists`] (every namespace exists by default). Writes into a
    /// missing namespace fail like on a real API server.
    pub async fn set_namespace_present(&self, namespace: &str, present: bool) {
        let mut guard = self.state.write().await;
        if present {
            guard.absent_namespaces.remove(namespace);
        } else {
            guard.absent_namespaces.insert(namespace.to_string());
        }
    }

    /// Removes `namespace/name` (tests: simulate a lost Secret).
    pub async fn remove_secret(&self, namespace: &str, name: &str) {
        let mut guard = self.state.write().await;
        guard
            .secrets
            .remove(&(namespace.to_string(), name.to_string()));
    }
}

fn in_memory_missing_namespace(namespace: &str) -> KubeError {
    KubeError::InvalidInput(format!("namespaces \"{namespace}\" not found"))
}

#[async_trait]
impl SecretBackend for InMemorySecretStore {
    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>, KubeError> {
        let guard = self.state.read().await;
        Ok(guard
            .secrets
            .get(&(namespace.to_string(), name.to_string()))
            .cloned())
    }

    async fn upsert_secret(
        &self,
        namespace: &str,
        mut secret: Secret,
    ) -> Result<Secret, KubeError> {
        let name =
            secret.metadata.name.clone().ok_or_else(|| {
                KubeError::InvalidInput("Secret metadata.name is required".into())
            })?;
        let mut guard = self.state.write().await;
        if guard.absent_namespaces.contains(namespace) {
            return Err(in_memory_missing_namespace(namespace));
        }
        guard.revision_counter += 1;
        let rev = guard.revision_counter.to_string();

        let key = (namespace.to_string(), name.clone());
        let uid = guard
            .secrets
            .get(&key)
            .and_then(|s| s.metadata.uid.clone())
            .unwrap_or_else(|| format!("mem-uid-{rev}"));

        secret.metadata.namespace = Some(namespace.to_string());
        secret.metadata.resource_version = Some(rev);
        secret.metadata.uid = Some(uid);
        guard.secrets.insert(key, secret.clone());
        Ok(secret)
    }

    async fn create_secret_if_absent(
        &self,
        namespace: &str,
        mut secret: Secret,
    ) -> Result<Secret, KubeError> {
        let name =
            secret.metadata.name.clone().ok_or_else(|| {
                KubeError::InvalidInput("Secret metadata.name is required".into())
            })?;
        let mut guard = self.state.write().await;
        if guard.absent_namespaces.contains(namespace) {
            return Err(in_memory_missing_namespace(namespace));
        }
        let key = (namespace.to_string(), name);

        if let Some(existing) = guard.secrets.get(&key) {
            return Ok(existing.clone());
        }

        guard.revision_counter += 1;
        let rev = guard.revision_counter.to_string();
        secret.metadata.namespace = Some(namespace.to_string());
        secret.metadata.resource_version = Some(rev.clone());
        secret.metadata.uid = Some(format!("mem-uid-{rev}"));
        guard.secrets.insert(key, secret.clone());
        Ok(secret)
    }

    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError> {
        Ok(!self
            .state
            .read()
            .await
            .absent_namespaces
            .contains(namespace))
    }
}

#[async_trait]
impl<B: SecretBackend> SecretStateStore for B {
    async fn get_or_create_session_key(&self) -> Result<[u8; SESSION_KEY_LEN], KubeError> {
        get_or_create_session_key_impl(self).await
    }

    async fn put_user_git_creds(
        &self,
        oidc_sub: &str,
        creds: &UserGitCredentials,
        encryption_key: &[u8; SESSION_KEY_LEN],
    ) -> Result<(), KubeError> {
        put_user_git_creds_impl(self, oidc_sub, creds, encryption_key).await
    }

    async fn get_user_git_creds(
        &self,
        oidc_sub: &str,
        encryption_key: &[u8; SESSION_KEY_LEN],
    ) -> Result<Option<UserGitCredentials>, KubeError> {
        get_user_git_creds_impl(self, oidc_sub, encryption_key).await
    }

    async fn store_tenant_age_key(
        &self,
        tenant_id: &str,
        age_key: &TenantAgeKey,
    ) -> Result<(), KubeError> {
        store_tenant_age_key_impl(self, tenant_id, age_key).await
    }

    async fn ensure_pulumi_passphrase(
        &self,
        namespace: &str,
    ) -> Result<PulumiPassphraseOutcome, KubeError> {
        ensure_pulumi_passphrase_impl(self, namespace).await
    }

    async fn has_pulumi_passphrase(&self, namespace: &str) -> Result<bool, KubeError> {
        has_pulumi_passphrase_impl(self, namespace).await
    }

    async fn get_tenant_age_key(&self, tenant_id: &str) -> Result<Option<TenantAgeKey>, KubeError> {
        get_tenant_age_key_impl(self, tenant_id).await
    }

    async fn has_s3_credentials(&self) -> Result<bool, KubeError> {
        has_s3_credentials_impl(self).await
    }

    async fn ensure_s3_credentials_in_namespace(
        &self,
        target_namespace: &str,
    ) -> Result<bool, KubeError> {
        ensure_s3_credentials_in_namespace_impl(self, target_namespace).await
    }

    async fn ensure_github_token_in_namespace(
        &self,
        target_namespace: &str,
    ) -> Result<bool, KubeError> {
        ensure_secret_in_namespace_impl(
            self,
            target_namespace,
            GITHUB_TOKEN_SECRET_NAME,
            "github-token",
        )
        .await
    }

    async fn namespace_exists(&self, namespace: &str) -> Result<bool, KubeError> {
        SecretBackend::namespace_exists(self, namespace).await
    }

    async fn create_tenant_age_key_if_absent(
        &self,
        tenant_id: &str,
        age_key: &TenantAgeKey,
    ) -> Result<bool, KubeError> {
        let namespace = tenant_namespace(tenant_id)?;
        let desired = build_tenant_age_key_secret(tenant_id, age_key)?;
        let stored = self.create_secret_if_absent(&namespace, desired).await?;
        let created = stored
            .data
            .as_ref()
            .and_then(|d| d.get(AGE_PUBLIC_KEY_FIELD))
            .is_some_and(|v| v.0 == age_key.public_key.trim().as_bytes());
        Ok(created)
    }

    async fn get_tenant_gitops_credentials(
        &self,
        tenant_id: &str,
    ) -> Result<Option<Secret>, KubeError> {
        let namespace = tenant_namespace(tenant_id)?;
        self.get_secret(
            &namespace,
            crate::tenant_repo::TENANT_GITOPS_CREDENTIALS_SECRET_NAME,
        )
        .await
    }

    async fn put_tenant_gitops_credentials(
        &self,
        tenant_id: &str,
        secret: Secret,
    ) -> Result<(), KubeError> {
        let namespace = tenant_gitops_credentials_namespace(tenant_id, &secret)?;
        self.upsert_secret(&namespace, secret).await?;
        Ok(())
    }

    async fn create_tenant_gitops_credentials_if_absent(
        &self,
        tenant_id: &str,
        secret: Secret,
    ) -> Result<bool, KubeError> {
        let namespace = tenant_gitops_credentials_namespace(tenant_id, &secret)?;
        let credential_id = |s: &Secret| {
            s.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(crate::tenant_repo::CREDENTIAL_ID_ANNOTATION))
                .cloned()
        };
        let wanted = credential_id(&secret);
        let stored = self.create_secret_if_absent(&namespace, secret).await?;
        Ok(wanted.is_some() && credential_id(&stored) == wanted)
    }
}

/// Namespace of a `keel-gitops-credentials` write; refuses any other Secret.
fn tenant_gitops_credentials_namespace(
    tenant_id: &str,
    secret: &Secret,
) -> Result<String, KubeError> {
    let namespace = tenant_namespace(tenant_id)?;
    let meta = &secret.metadata;
    let kind_label = meta
        .labels
        .as_ref()
        .and_then(|l| l.get("keel.io/secret-kind"))
        .map(String::as_str);
    if meta.name.as_deref() != Some(crate::tenant_repo::TENANT_GITOPS_CREDENTIALS_SECRET_NAME)
        || meta.namespace.as_deref() != Some(namespace.as_str())
        || kind_label != Some(crate::tenant_repo::TENANT_GITOPS_CREDENTIALS_SECRET_KIND)
    {
        return Err(KubeError::InvalidInput(format!(
            "refusing to write a Secret other than `{namespace}/{}`",
            crate::tenant_repo::TENANT_GITOPS_CREDENTIALS_SECRET_NAME
        )));
    }
    Ok(namespace)
}

fn managed_labels(secret_kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "keel".to_string(),
        ),
        ("keel.io/secret-kind".to_string(), secret_kind.to_string()),
    ])
}

/// Constant-time byte comparison to prevent timing side-channels on token hashes.
fn parse_session_key_secret(secret: &Secret) -> Result<[u8; SESSION_KEY_LEN], KubeError> {
    let raw = secret
        .data
        .as_ref()
        .and_then(|d| d.get(SESSION_KEY_FIELD))
        .ok_or_else(|| KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: SESSION_KEY_SECRET_NAME.to_string(),
            reason: format!("missing `{SESSION_KEY_FIELD}` field"),
        })?;

    raw.0
        .as_slice()
        .try_into()
        .map_err(|_| KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: SESSION_KEY_SECRET_NAME.to_string(),
            reason: format!(
                "expected {SESSION_KEY_LEN}-byte session key, found {} bytes",
                raw.0.len()
            ),
        })
}

async fn get_or_create_session_key_impl<B: SecretBackend>(
    backend: &B,
) -> Result<[u8; SESSION_KEY_LEN], KubeError> {
    if let Some(existing) = backend
        .get_secret(KEEL_SYSTEM_NAMESPACE, SESSION_KEY_SECRET_NAME)
        .await?
    {
        return parse_session_key_secret(&existing);
    }

    let mut generated = [0u8; SESSION_KEY_LEN];
    rand::rng().fill_bytes(&mut generated);

    let data = BTreeMap::from([(
        SESSION_KEY_FIELD.to_string(),
        ByteString(generated.to_vec()),
    )]);
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(SESSION_KEY_SECRET_NAME.to_string()),
            namespace: Some(KEEL_SYSTEM_NAMESPACE.to_string()),
            labels: Some(managed_labels("session-key")),
            ..ObjectMeta::default()
        },
        type_: Some("keel.io/session-key".to_string()),
        data: Some(data),
        ..Secret::default()
    };

    let persisted = backend
        .create_secret_if_absent(KEEL_SYSTEM_NAMESPACE, secret)
        .await?;
    parse_session_key_secret(&persisted)
}

async fn put_user_git_creds_impl<B: SecretBackend>(
    backend: &B,
    oidc_sub: &str,
    creds: &UserGitCredentials,
    encryption_key: &[u8; SESSION_KEY_LEN],
) -> Result<(), KubeError> {
    let secret_name = user_git_creds_secret_name(oidc_sub)?;
    let aad = oidc_sub.trim().as_bytes();
    let plaintext = serde_json::to_vec(creds)?;

    let mut nonce_bytes = [0u8; AES_GCM_NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);

    let key: Key<Aes256Gcm> = (*encryption_key).into();
    let cipher = Aes256Gcm::new(&key);
    let nonce = Nonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad,
            },
        )
        .map_err(|_| KubeError::Crypto("AES-256-GCM encryption failed".to_string()))?;

    let data = BTreeMap::from([
        (
            ALGORITHM_FIELD.to_string(),
            ByteString(ALGORITHM_AES_256_GCM.to_vec()),
        ),
        (NONCE_FIELD.to_string(), ByteString(nonce_bytes.to_vec())),
        (CIPHERTEXT_FIELD.to_string(), ByteString(ciphertext)),
    ]);

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name),
            namespace: Some(KEEL_SYSTEM_NAMESPACE.to_string()),
            labels: Some(managed_labels("user-git-creds")),
            ..ObjectMeta::default()
        },
        type_: Some("keel.io/user-git-credentials".to_string()),
        data: Some(data),
        string_data: None,
        ..Secret::default()
    };

    backend.upsert_secret(KEEL_SYSTEM_NAMESPACE, secret).await?;
    Ok(())
}

async fn get_user_git_creds_impl<B: SecretBackend>(
    backend: &B,
    oidc_sub: &str,
    encryption_key: &[u8; SESSION_KEY_LEN],
) -> Result<Option<UserGitCredentials>, KubeError> {
    let secret_name = user_git_creds_secret_name(oidc_sub)?;
    let Some(secret) = backend
        .get_secret(KEEL_SYSTEM_NAMESPACE, &secret_name)
        .await?
    else {
        return Ok(None);
    };

    let data = secret
        .data
        .as_ref()
        .ok_or_else(|| KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: secret_name.clone(),
            reason: "Secret data map is empty".to_string(),
        })?;

    let algorithm = data
        .get(ALGORITHM_FIELD)
        .map(|b| b.0.as_slice())
        .unwrap_or_default();
    if algorithm != ALGORITHM_AES_256_GCM {
        return Err(KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: secret_name,
            reason: "unsupported or missing encryption algorithm".to_string(),
        });
    }

    let nonce_raw = data
        .get(NONCE_FIELD)
        .ok_or_else(|| KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: secret_name.clone(),
            reason: format!("missing `{NONCE_FIELD}` field"),
        })?;
    if nonce_raw.0.len() != AES_GCM_NONCE_LEN {
        return Err(KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: secret_name,
            reason: format!(
                "expected {AES_GCM_NONCE_LEN}-byte nonce, found {} bytes",
                nonce_raw.0.len()
            ),
        });
    }

    let ciphertext = data
        .get(CIPHERTEXT_FIELD)
        .ok_or_else(|| KubeError::CorruptedSecret {
            namespace: KEEL_SYSTEM_NAMESPACE.to_string(),
            name: secret_name.clone(),
            reason: format!("missing `{CIPHERTEXT_FIELD}` field"),
        })?;

    let aad = oidc_sub.trim().as_bytes();
    let key: Key<Aes256Gcm> = (*encryption_key).into();
    let cipher = Aes256Gcm::new(&key);
    let nonce = Nonce::try_from(nonce_raw.0.as_slice())
        .map_err(|_| KubeError::Crypto("Invalid AES-GCM nonce length".to_string()))?;
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &ciphertext.0,
                aad,
            },
        )
        .map_err(|_| {
            KubeError::Crypto(
                "AES-256-GCM decryption failed (invalid key, subject mismatch, or tampered payload)"
                    .to_string(),
            )
        })?;

    let creds: UserGitCredentials = serde_json::from_slice(&plaintext)?;
    Ok(Some(creds))
}

async fn store_tenant_age_key_impl<B: SecretBackend>(
    backend: &B,
    tenant_id: &str,
    age_key: &TenantAgeKey,
) -> Result<(), KubeError> {
    let namespace = tenant_namespace(tenant_id)?;
    let secret = build_tenant_age_key_secret(tenant_id, age_key)?;
    backend.upsert_secret(&namespace, secret).await?;
    Ok(())
}

/// Builds the `tenant-<id>/sops-age` Secret (Flux `age.agekey` plus `public-key`/`secret-key`).
pub fn build_tenant_age_key_secret(
    tenant_id: &str,
    age_key: &TenantAgeKey,
) -> Result<Secret, KubeError> {
    let namespace = tenant_namespace(tenant_id)?;
    let pub_key = age_key.public_key.trim();
    let sec_key = age_key.secret_key.trim();
    if !pub_key.starts_with("age1") {
        return Err(KubeError::InvalidInput(
            "tenant age public key must start with `age1`".to_string(),
        ));
    }
    if !sec_key.starts_with("AGE-SECRET-KEY-1") {
        return Err(KubeError::InvalidInput(
            "tenant age secret key must start with `AGE-SECRET-KEY-1`".to_string(),
        ));
    }

    let mut labels = managed_labels("tenant-sops-age");
    labels.insert("keel.io/tenant".to_string(), tenant_id.to_string());

    let data = BTreeMap::from([
        (
            AGE_SECRET_FILE_FIELD.to_string(),
            ByteString(age_key.to_sops_agekey_file().into_bytes()),
        ),
        (
            AGE_PUBLIC_KEY_FIELD.to_string(),
            ByteString(pub_key.as_bytes().to_vec()),
        ),
        (
            AGE_SECRET_KEY_FIELD.to_string(),
            ByteString(sec_key.as_bytes().to_vec()),
        ),
    ]);

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(TENANT_SOPS_AGE_SECRET_NAME.to_string()),
            namespace: Some(namespace.clone()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(data),
        ..Secret::default()
    };
    Ok(secret)
}

async fn get_tenant_age_key_impl<B: SecretBackend>(
    backend: &B,
    tenant_id: &str,
) -> Result<Option<TenantAgeKey>, KubeError> {
    let namespace = tenant_namespace(tenant_id)?;
    let Some(secret) = backend
        .get_secret(&namespace, TENANT_SOPS_AGE_SECRET_NAME)
        .await?
    else {
        return Ok(None);
    };

    let data = secret
        .data
        .as_ref()
        .ok_or_else(|| KubeError::CorruptedSecret {
            namespace: namespace.clone(),
            name: TENANT_SOPS_AGE_SECRET_NAME.to_string(),
            reason: "Secret data map is empty".to_string(),
        })?;

    if let (Some(pub_bytes), Some(sec_bytes)) = (
        data.get(AGE_PUBLIC_KEY_FIELD),
        data.get(AGE_SECRET_KEY_FIELD),
    ) {
        let public_key =
            String::from_utf8(pub_bytes.0.clone()).map_err(|_| KubeError::CorruptedSecret {
                namespace: namespace.clone(),
                name: TENANT_SOPS_AGE_SECRET_NAME.to_string(),
                reason: "invalid UTF-8 in `public-key`".to_string(),
            })?;
        let secret_key =
            String::from_utf8(sec_bytes.0.clone()).map_err(|_| KubeError::CorruptedSecret {
                namespace: namespace.clone(),
                name: TENANT_SOPS_AGE_SECRET_NAME.to_string(),
                reason: "invalid UTF-8 in `secret-key`".to_string(),
            })?;
        return Ok(Some(TenantAgeKey::new(public_key, secret_key)));
    }

    // Fallback: parse standard Flux `age.agekey` format (`# public key: age1...\nAGE-SECRET-KEY-1...`)
    let agekey_raw = data
        .get(AGE_SECRET_FILE_FIELD)
        .ok_or_else(|| KubeError::CorruptedSecret {
            namespace: namespace.clone(),
            name: TENANT_SOPS_AGE_SECRET_NAME.to_string(),
            reason: format!("missing `{AGE_SECRET_FILE_FIELD}` field"),
        })?;
    let content =
        String::from_utf8(agekey_raw.0.clone()).map_err(|_| KubeError::CorruptedSecret {
            namespace: namespace.clone(),
            name: TENANT_SOPS_AGE_SECRET_NAME.to_string(),
            reason: format!("invalid UTF-8 in `{AGE_SECRET_FILE_FIELD}`"),
        })?;

    let mut public_key = String::new();
    let mut secret_key = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# public key:") {
            public_key = rest.trim().to_string();
        } else if trimmed.starts_with("AGE-SECRET-KEY-1") {
            secret_key = trimmed.to_string();
        }
    }

    if secret_key.is_empty() {
        return Err(KubeError::CorruptedSecret {
            namespace,
            name: TENANT_SOPS_AGE_SECRET_NAME.to_string(),
            reason: "missing `AGE-SECRET-KEY-1` line in `age.agekey`".to_string(),
        });
    }

    Ok(Some(TenantAgeKey::new(public_key, secret_key)))
}

async fn has_s3_credentials_impl(backend: &impl SecretBackend) -> Result<bool, KubeError> {
    Ok(backend
        .get_secret(KEEL_SYSTEM_NAMESPACE, S3_CREDENTIALS_SECRET_NAME)
        .await?
        .is_some())
}

async fn ensure_s3_credentials_in_namespace_impl(
    backend: &impl SecretBackend,
    target_namespace: &str,
) -> Result<bool, KubeError> {
    if target_namespace.starts_with(TENANT_NAMESPACE_PREFIX) {
        return ensure_tenant_scoped_s3_credentials_impl(backend, target_namespace).await;
    }
    ensure_secret_in_namespace_impl(
        backend,
        target_namespace,
        S3_CREDENTIALS_SECRET_NAME,
        "s3-credentials",
    )
    .await
}

/// Tenant namespaces NEVER receive the control-plane (SeaweedFS admin) `keel-s3-credentials`.
///
/// Instead, the tenant-local scoped identity `keel-tenant-s3-credentials` (provisioned per tenant,
/// see `keel_kube::tenant::seaweedfs_configure_tenant_identity_script`) is mirrored under the
/// `keel-s3-credentials` name PKO `Stack.spec.envRefs` reference. Any other Secret under that
/// name (e.g. an admin copy) is overwritten. Returns `Ok(false)` when the tenant has no
/// scoped identity yet (fail closed).
pub(crate) async fn ensure_tenant_scoped_s3_credentials_impl(
    backend: &impl SecretBackend,
    tenant_ns: &str,
) -> Result<bool, KubeError> {
    let Some(scoped) = backend
        .get_secret(tenant_ns, TENANT_S3_CREDENTIALS_SECRET_NAME)
        .await?
    else {
        return Ok(false);
    };
    if scoped
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(TENANT_S3_IDENTITY_PENDING_ANNOTATION))
    {
        // Keys stored but not yet applied to SeaweedFS (provisioning interrupted): fail closed.
        return Ok(false);
    }
    let scoped_data = scoped.data.unwrap_or_default();
    let mut data = BTreeMap::new();
    for key in ["accessKey", "secretKey"] {
        match scoped_data.get(key) {
            Some(v) if !v.0.is_empty() => {
                data.insert(key.to_string(), v.clone());
            }
            _ => {
                return Err(KubeError::CorruptedSecret {
                    namespace: tenant_ns.to_string(),
                    name: TENANT_S3_CREDENTIALS_SECRET_NAME.to_string(),
                    reason: format!("missing `{key}`"),
                });
            }
        }
    }

    if let Some(existing) = backend
        .get_secret(tenant_ns, S3_CREDENTIALS_SECRET_NAME)
        .await?
        && existing.data.as_ref() == Some(&data)
    {
        return Ok(true);
    }

    let mirror = Secret {
        metadata: ObjectMeta {
            name: Some(S3_CREDENTIALS_SECRET_NAME.to_string()),
            namespace: Some(tenant_ns.to_string()),
            labels: Some(managed_labels(TENANT_S3_CREDENTIALS_SECRET_KIND)),
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(data),
        ..Secret::default()
    };
    backend.upsert_secret(tenant_ns, mirror).await?;
    Ok(true)
}

async fn ensure_secret_in_namespace_impl(
    backend: &impl SecretBackend,
    target_namespace: &str,
    secret_name: &str,
    secret_kind: &str,
) -> Result<bool, KubeError> {
    let Some(source) = backend
        .get_secret(KEEL_SYSTEM_NAMESPACE, secret_name)
        .await?
    else {
        return Ok(false);
    };

    if target_namespace == KEEL_SYSTEM_NAMESPACE {
        return Ok(true);
    }

    if backend
        .get_secret(target_namespace, secret_name)
        .await?
        .is_some()
    {
        return Ok(true);
    }

    let copy = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name.to_string()),
            namespace: Some(target_namespace.to_string()),
            labels: Some(managed_labels(secret_kind)),
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".to_string()),
        data: source.data,
        ..Secret::default()
    };

    backend
        .create_secret_if_absent(target_namespace, copy)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_key_is_created_once_and_reused() {
        let store = Arc::new(InMemorySecretStore::new());
        let key1 = store.get_or_create_session_key().await.unwrap();
        let key2 = store.get_or_create_session_key().await.unwrap();
        assert_eq!(key1, key2);
        assert_ne!(key1, [0u8; SESSION_KEY_LEN]);

        // Verify stored in `keel-system/session-key`
        let secret = store
            .inspect_raw_secret(KEEL_SYSTEM_NAMESPACE, SESSION_KEY_SECRET_NAME)
            .await
            .expect("session-key secret must exist");
        assert_eq!(secret.metadata.namespace.as_deref(), Some("keel-system"));
        assert_eq!(secret.metadata.name.as_deref(), Some("session-key"));
    }

    #[tokio::test]
    async fn user_git_creds_stored_as_aes_gcm_ciphertext_never_plaintext() {
        let store = InMemorySecretStore::new();
        let key = store.get_or_create_session_key().await.unwrap();
        let sub = "https://idp.example.com|user-alice-42";

        let raw_ssh = "-----BEGIN OPENSSH PRIVATE KEY-----\nsuper-secret-ed25519-key\n-----END OPENSSH PRIVATE KEY-----";
        let raw_pat = "ghp_SuperSecretPersonalAccessToken999999";

        let creds = UserGitCredentials::new(
            Some(raw_ssh.to_string()),
            Some(raw_pat.to_string()),
            Some("alice".to_string()),
        );

        // Verify Debug redacts secrets (NFR-SEC-05)
        let debug_str = format!("{creds:?}");
        assert!(!debug_str.contains(raw_ssh));
        assert!(!debug_str.contains(raw_pat));
        assert!(debug_str.contains("[REDACTED]"));

        store.put_user_git_creds(sub, &creds, &key).await.unwrap();

        let expected_secret_name = user_git_creds_secret_name(sub).unwrap();
        assert!(expected_secret_name.starts_with("user-git-creds-"));

        let stored_secret = store
            .inspect_raw_secret(KEEL_SYSTEM_NAMESPACE, &expected_secret_name)
            .await
            .expect("encrypted git credentials secret must exist");

        assert!(stored_secret.string_data.is_none());
        let data = stored_secret.data.as_ref().unwrap();
        assert_eq!(data.get("algorithm").unwrap().0, b"AES-256-GCM");
        assert_eq!(data.get("nonce").unwrap().0.len(), 12);

        // Ensure neither raw bytes nor JSON serialization of the K8s Secret leaks plaintext!
        let raw_cipher = &data.get("ciphertext").unwrap().0;
        assert!(
            !String::from_utf8_lossy(raw_cipher).contains("ghp_SuperSecretPersonalAccessToken")
        );
        assert!(!String::from_utf8_lossy(raw_cipher).contains("BEGIN OPENSSH PRIVATE KEY"));
        let secret_json = serde_json::to_string(&stored_secret).unwrap();
        assert!(!secret_json.contains(raw_pat));
        assert!(!secret_json.contains("super-secret-ed25519-key"));

        // Round-trip decrypt with valid key and matching OIDC subject
        let loaded = store
            .get_user_git_creds(sub, &key)
            .await
            .unwrap()
            .expect("credentials must round-trip");
        assert_eq!(loaded, creds);

        // Missing user returns Ok(None)
        assert!(
            store
                .get_user_git_creds("nonexistent-sub", &key)
                .await
                .unwrap()
                .is_none()
        );

        // Wrong key fails AES-GCM authentication
        let wrong_key = [7u8; SESSION_KEY_LEN];
        let err = store.get_user_git_creds(sub, &wrong_key).await.unwrap_err();
        assert!(matches!(err, KubeError::Crypto(_)));

        // Empty subject is rejected
        assert!(user_git_creds_secret_name("   ").is_err());
    }

    #[tokio::test]
    async fn user_git_creds_aad_prevents_cross_user_secret_swap() {
        let store = InMemorySecretStore::new();
        let key = store.get_or_create_session_key().await.unwrap();
        let creds = UserGitCredentials::new(None, Some("glpat-secret".into()), Some("bob".into()));

        store
            .put_user_git_creds("sub-bob", &creds, &key)
            .await
            .unwrap();

        // Simulate an attacker copying bob's Secret data into mallory's Secret name
        let bob_secret = store
            .inspect_raw_secret(
                KEEL_SYSTEM_NAMESPACE,
                &user_git_creds_secret_name("sub-bob").unwrap(),
            )
            .await
            .unwrap();
        let mut swapped = bob_secret;
        swapped.metadata.name = Some(user_git_creds_secret_name("sub-mallory").unwrap());
        store
            .upsert_secret(KEEL_SYSTEM_NAMESPACE, swapped)
            .await
            .unwrap();

        let res = store.get_user_git_creds("sub-mallory", &key).await;
        assert!(matches!(res, Err(KubeError::Crypto(_))));
    }

    #[tokio::test]
    async fn tenant_age_key_is_created_once_and_never_replaced() {
        let store = InMemorySecretStore::new();
        let first = TenantAgeKey::new("age1first", "AGE-SECRET-KEY-1FIRST");
        let second = TenantAgeKey::new("age1second", "AGE-SECRET-KEY-1SECOND");
        assert!(
            store
                .create_tenant_age_key_if_absent("acme", &first)
                .await
                .unwrap()
        );
        assert!(
            !store
                .create_tenant_age_key_if_absent("acme", &second)
                .await
                .unwrap()
        );
        let kept = store.get_tenant_age_key("acme").await.unwrap().unwrap();
        assert_eq!(kept.public_key, "age1first");

        // A missing namespace fails instead of being silently skipped.
        store.set_namespace_present("tenant-beta", false).await;
        assert!(
            !SecretStateStore::namespace_exists(&store, "tenant-beta")
                .await
                .unwrap()
        );
        assert!(
            store
                .create_tenant_age_key_if_absent("beta", &first)
                .await
                .is_err()
        );
        store.set_namespace_present("tenant-beta", true).await;
        assert!(
            SecretStateStore::namespace_exists(&store, "tenant-beta")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn tenant_gitops_credentials_put_refuses_other_secrets() {
        let store = InMemorySecretStore::new();
        let source = crate::TenantRepoSource::new(
            "acme",
            "https://forgejo.keel.local:8888/keel-tenants/infra-gitops-tenant-acme.git",
        )
        .unwrap();
        let credential = crate::TenantRepoCredential::HttpsToken {
            username: "keel-tenant-reader".to_string(),
            token: "tok".to_string(),
            ca_crt: None,
        };
        let secret = crate::tenant_gitops_credentials_secret(&source, &credential, "1").unwrap();
        assert!(
            store
                .get_tenant_gitops_credentials("acme")
                .await
                .unwrap()
                .is_none()
        );
        store
            .put_tenant_gitops_credentials("acme", secret.clone())
            .await
            .unwrap();
        assert!(
            store
                .get_tenant_gitops_credentials("acme")
                .await
                .unwrap()
                .is_some()
        );
        // Another tenant's namespace or another Secret kind is refused.
        assert!(
            store
                .put_tenant_gitops_credentials("beta", secret.clone())
                .await
                .is_err()
        );
        let mut foreign = secret.clone();
        foreign.metadata.labels = None;
        assert!(
            store
                .put_tenant_gitops_credentials("acme", foreign.clone())
                .await
                .is_err()
        );
        assert!(
            store
                .create_tenant_gitops_credentials_if_absent("acme", foreign)
                .await
                .is_err()
        );

        // Create-if-absent: the first writer wins, a later writer never overwrites it.
        let other = crate::tenant_gitops_credentials_secret(&source, &credential, "2").unwrap();
        assert!(
            !store
                .create_tenant_gitops_credentials_if_absent("acme", other.clone())
                .await
                .unwrap()
        );
        store
            .remove_secret("tenant-acme", "keel-gitops-credentials")
            .await;
        assert!(
            store
                .create_tenant_gitops_credentials_if_absent("acme", other)
                .await
                .unwrap()
        );
        assert!(
            !store
                .create_tenant_gitops_credentials_if_absent("acme", secret)
                .await
                .unwrap()
        );
        let kept = store
            .get_tenant_gitops_credentials("acme")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            kept.metadata.annotations.unwrap()[crate::tenant_repo::CREDENTIAL_ID_ANNOTATION],
            "2"
        );
    }

    #[tokio::test]
    async fn tenant_age_key_roundtrip_and_validation() {
        let store = InMemorySecretStore::new();
        let test_sec = format!(
            "AGE-SECRET-KEY-1{}",
            "GFPY0J9V5C8X7W4N6M2Q3R8T9Y0U1I2O3P4A5S6D7F8G9H0J1K2L3Z4X5C"
        );
        let age_key = TenantAgeKey::new(
            "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p",
            test_sec,
        );

        // Debug redacts private key
        let dbg = format!("{age_key:?}");
        assert!(!dbg.contains("AGE-SECRET-KEY-1"));
        assert!(dbg.contains("[REDACTED]"));

        store
            .store_tenant_age_key("payments", &age_key)
            .await
            .unwrap();

        // Stored strictly inside `tenant-payments/sops-age`
        let raw_secret = store
            .inspect_raw_secret("tenant-payments", TENANT_SOPS_AGE_SECRET_NAME)
            .await
            .expect("sops-age secret must exist in tenant-payments");
        assert_eq!(
            raw_secret.metadata.namespace.as_deref(),
            Some("tenant-payments")
        );
        assert_eq!(raw_secret.metadata.name.as_deref(), Some("sops-age"));
        assert!(raw_secret.data.as_ref().unwrap().contains_key("age.agekey"));

        let fetched = store
            .get_tenant_age_key("payments")
            .await
            .unwrap()
            .expect("tenant age key must be found");
        assert_eq!(fetched, age_key);

        // Fallback parsing when only `age.agekey` is populated (Flux-created secret)
        let flux_only_secret = Secret {
            metadata: ObjectMeta {
                name: Some(TENANT_SOPS_AGE_SECRET_NAME.to_string()),
                namespace: Some("tenant-legacy".to_string()),
                ..ObjectMeta::default()
            },
            data: Some(BTreeMap::from([(
                AGE_SECRET_FILE_FIELD.to_string(),
                ByteString(age_key.to_sops_agekey_file().into_bytes()),
            )])),
            ..Secret::default()
        };
        store
            .upsert_secret("tenant-legacy", flux_only_secret)
            .await
            .unwrap();
        let parsed_legacy = store
            .get_tenant_age_key("legacy")
            .await
            .unwrap()
            .expect("fallback parser must extract age keypair");
        assert_eq!(parsed_legacy, age_key);

        // Unknown tenant returns Ok(None)
        assert!(store.get_tenant_age_key("unknown").await.unwrap().is_none());

        // Invalid keys or invalid tenant IDs are rejected
        assert!(
            store
                .store_tenant_age_key("../flux-system", &age_key)
                .await
                .is_err()
        );
        assert!(
            store
                .store_tenant_age_key(
                    "payments",
                    &TenantAgeKey::new("invalid-pub", "AGE-SECRET-KEY-1VALID")
                )
                .await
                .is_err()
        );
        assert!(
            store
                .store_tenant_age_key(
                    "payments",
                    &TenantAgeKey::new("age1valid", "invalid-secret")
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn corrupted_secret_payloads_surface_structured_errors() {
        let store = InMemorySecretStore::new();

        // Corrupted session-key (wrong byte length)
        store
            .upsert_secret(
                KEEL_SYSTEM_NAMESPACE,
                Secret {
                    metadata: ObjectMeta {
                        name: Some(SESSION_KEY_SECRET_NAME.to_string()),
                        ..ObjectMeta::default()
                    },
                    data: Some(BTreeMap::from([(
                        SESSION_KEY_FIELD.to_string(),
                        ByteString(vec![1, 2, 3]),
                    )])),
                    ..Secret::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            store.get_or_create_session_key().await,
            Err(KubeError::CorruptedSecret { .. })
        ));

        // 3. Corrupted user-git-creds (bad algorithm & bad nonce length)
        let sub_secret = user_git_creds_secret_name("sub-corrupt").unwrap();
        store
            .upsert_secret(
                KEEL_SYSTEM_NAMESPACE,
                Secret {
                    metadata: ObjectMeta {
                        name: Some(sub_secret.clone()),
                        ..ObjectMeta::default()
                    },
                    data: Some(BTreeMap::from([(
                        ALGORITHM_FIELD.to_string(),
                        ByteString(b"PLAINTEXT".to_vec()),
                    )])),
                    ..Secret::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            store
                .get_user_git_creds("sub-corrupt", &[0u8; SESSION_KEY_LEN])
                .await,
            Err(KubeError::CorruptedSecret { .. })
        ));

        store
            .upsert_secret(
                KEEL_SYSTEM_NAMESPACE,
                Secret {
                    metadata: ObjectMeta {
                        name: Some(sub_secret),
                        ..ObjectMeta::default()
                    },
                    data: Some(BTreeMap::from([
                        (
                            ALGORITHM_FIELD.to_string(),
                            ByteString(ALGORITHM_AES_256_GCM.to_vec()),
                        ),
                        (NONCE_FIELD.to_string(), ByteString(vec![0u8; 4])),
                    ])),
                    ..Secret::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            store
                .get_user_git_creds("sub-corrupt", &[0u8; SESSION_KEY_LEN])
                .await,
            Err(KubeError::CorruptedSecret { .. })
        ));
    }

    fn s3_secret(name: &str, access: &[u8], secret: &[u8]) -> Secret {
        Secret {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..ObjectMeta::default()
            },
            data: Some(BTreeMap::from([
                ("accessKey".to_string(), ByteString(access.to_vec())),
                ("secretKey".to_string(), ByteString(secret.to_vec())),
            ])),
            ..Secret::default()
        }
    }

    #[tokio::test]
    async fn s3_credentials_propagation_copies_admin_only_to_control_plane_namespaces() {
        let store = InMemorySecretStore::new();
        assert!(!store.has_s3_credentials().await.unwrap());
        assert!(
            !store
                .ensure_s3_credentials_in_namespace(KEEL_PREVIEWS_NAMESPACE_FOR_TEST)
                .await
                .unwrap()
        );

        store
            .upsert_secret(
                KEEL_SYSTEM_NAMESPACE,
                s3_secret(S3_CREDENTIALS_SECRET_NAME, b"admin-key", b"admin-secret"),
            )
            .await
            .unwrap();

        assert!(store.has_s3_credentials().await.unwrap());
        assert!(
            store
                .ensure_s3_credentials_in_namespace(KEEL_PREVIEWS_NAMESPACE_FOR_TEST)
                .await
                .unwrap()
        );
        let copied = store
            .get_secret(KEEL_PREVIEWS_NAMESPACE_FOR_TEST, S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap()
            .expect("copied secret");
        assert_eq!(
            copied.data.unwrap().get("accessKey").unwrap().0,
            b"admin-key"
        );
    }

    #[tokio::test]
    async fn tenant_namespaces_never_receive_admin_s3_credentials() {
        let store = InMemorySecretStore::new();
        store
            .upsert_secret(
                KEEL_SYSTEM_NAMESPACE,
                s3_secret(S3_CREDENTIALS_SECRET_NAME, b"admin-key", b"admin-secret"),
            )
            .await
            .unwrap();

        // No scoped identity yet: fail closed, nothing copied.
        assert!(
            !store
                .ensure_s3_credentials_in_namespace("tenant-acme")
                .await
                .unwrap()
        );
        assert!(
            store
                .get_secret("tenant-acme", S3_CREDENTIALS_SECRET_NAME)
                .await
                .unwrap()
                .is_none()
        );

        // An admin copy under the same name is replaced by the scoped one.
        store
            .upsert_secret(
                "tenant-acme",
                s3_secret(S3_CREDENTIALS_SECRET_NAME, b"admin-key", b"admin-secret"),
            )
            .await
            .unwrap();
        store
            .upsert_secret(
                "tenant-acme",
                s3_secret(
                    TENANT_S3_CREDENTIALS_SECRET_NAME,
                    b"tenant-key",
                    b"tenant-secret",
                ),
            )
            .await
            .unwrap();
        assert!(
            store
                .ensure_s3_credentials_in_namespace("tenant-acme")
                .await
                .unwrap()
        );
        let mirrored = store
            .get_secret("tenant-acme", S3_CREDENTIALS_SECRET_NAME)
            .await
            .unwrap()
            .expect("mirrored secret");
        let labels = mirrored.metadata.labels.clone().unwrap();
        assert_eq!(
            labels.get("keel.io/secret-kind").map(String::as_str),
            Some(TENANT_S3_CREDENTIALS_SECRET_KIND)
        );
        let data = mirrored.data.unwrap();
        assert_eq!(data.get("accessKey").unwrap().0, b"tenant-key");
        assert_eq!(data.get("secretKey").unwrap().0, b"tenant-secret");

        // Idempotent.
        assert!(
            store
                .ensure_s3_credentials_in_namespace("tenant-acme")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn tenant_scoped_s3_credentials_missing_keys_are_rejected() {
        let store = InMemorySecretStore::new();
        let mut broken = s3_secret(TENANT_S3_CREDENTIALS_SECRET_NAME, b"k", b"");
        broken.data.as_mut().unwrap().remove("secretKey");
        store.upsert_secret("tenant-acme", broken).await.unwrap();
        assert!(matches!(
            store
                .ensure_s3_credentials_in_namespace("tenant-acme")
                .await,
            Err(KubeError::CorruptedSecret { .. })
        ));
    }

    const KEEL_PREVIEWS_NAMESPACE_FOR_TEST: &str = "keel-previews";

    #[tokio::test]
    async fn github_token_propagation_copies_from_keel_system() {
        let store = InMemorySecretStore::new();
        assert!(
            !store
                .ensure_github_token_in_namespace("tenant-acme")
                .await
                .unwrap()
        );

        store
            .upsert_secret(
                KEEL_SYSTEM_NAMESPACE,
                Secret {
                    metadata: ObjectMeta {
                        name: Some(GITHUB_TOKEN_SECRET_NAME.to_string()),
                        ..ObjectMeta::default()
                    },
                    data: Some(BTreeMap::from([(
                        "token".to_string(),
                        ByteString(b"ghp_test_token".to_vec()),
                    )])),
                    ..Secret::default()
                },
            )
            .await
            .unwrap();

        assert!(
            store
                .ensure_github_token_in_namespace("tenant-acme")
                .await
                .unwrap()
        );

        let copied = store
            .get_secret("tenant-acme", GITHUB_TOKEN_SECRET_NAME)
            .await
            .unwrap()
            .expect("copied secret");
        assert_eq!(
            copied.data.unwrap().get("token").unwrap().0,
            b"ghp_test_token"
        );
    }

    fn passphrase_of(secret: &Secret) -> String {
        String::from_utf8(
            secret.data.as_ref().unwrap()[PULUMI_PASSPHRASE_SECRET_KEY]
                .0
                .clone(),
        )
        .unwrap()
    }

    #[test]
    fn generated_pulumi_passphrases_are_256_bit_url_safe_and_unique() {
        let a = generate_pulumi_passphrase();
        let b = generate_pulumi_passphrase();
        assert_ne!(a, b);
        // 32 bytes base64url without padding = 43 chars (>= 256 bits of entropy).
        assert_eq!(a.len(), 43);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "not URL-safe"
        );
    }

    #[tokio::test]
    async fn pulumi_passphrase_is_created_once_and_never_rotated() {
        let store = InMemorySecretStore::new();
        assert!(!store.has_pulumi_passphrase("tenant-acme").await.unwrap());
        assert_eq!(
            store.ensure_pulumi_passphrase("tenant-acme").await.unwrap(),
            PulumiPassphraseOutcome::Created
        );
        assert!(store.has_pulumi_passphrase("tenant-acme").await.unwrap());
        let first = store
            .inspect_raw_secret("tenant-acme", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .unwrap();
        let pass = passphrase_of(&first);
        assert_eq!(pass.len(), 43);
        assert_eq!(
            first.metadata.labels.as_ref().unwrap()["keel.io/secret-kind"],
            PULUMI_PASSPHRASE_SECRET_KIND
        );
        // Never surfaced through Debug of the outcome.
        let outcome = store.ensure_pulumi_passphrase("tenant-acme").await.unwrap();
        assert_eq!(outcome, PulumiPassphraseOutcome::Reused);
        assert!(!format!("{outcome:?}").contains(&pass));
        let second = store
            .inspect_raw_secret("tenant-acme", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(passphrase_of(&second), pass, "passphrase must never rotate");

        // Namespaces are independent (tenant vs management).
        store.ensure_pulumi_passphrase("flux-system").await.unwrap();
        let mgmt = store
            .inspect_raw_secret("flux-system", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .unwrap();
        assert_ne!(passphrase_of(&mgmt), pass);
    }

    #[tokio::test]
    async fn pre_existing_passphrase_is_reused_and_empty_one_fails_closed() {
        let store = InMemorySecretStore::new();
        store
            .upsert_secret(
                "tenant-acme",
                build_pulumi_passphrase_secret("tenant-acme", "operator-provided"),
            )
            .await
            .unwrap();
        assert_eq!(
            store.ensure_pulumi_passphrase("tenant-acme").await.unwrap(),
            PulumiPassphraseOutcome::Reused
        );
        let kept = store
            .inspect_raw_secret("tenant-acme", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(passphrase_of(&kept), "operator-provided");

        store
            .upsert_secret(
                "tenant-bad",
                build_pulumi_passphrase_secret("tenant-bad", ""),
            )
            .await
            .unwrap();
        assert!(!store.has_pulumi_passphrase("tenant-bad").await.unwrap());
        assert!(matches!(
            store.ensure_pulumi_passphrase("tenant-bad").await,
            Err(KubeError::CorruptedSecret { .. })
        ));
        // The empty value is left untouched (no silent regeneration).
        let bad = store
            .inspect_raw_secret("tenant-bad", PULUMI_PASSPHRASE_SECRET_NAME)
            .await
            .unwrap();
        assert_eq!(passphrase_of(&bad), "");
    }
}
