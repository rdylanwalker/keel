//! Cryptographic primitives (`AES-256-GCM` envelope encryption, SHA-256 hashing,
//! bcrypt password hashing for Dex) and SOPS + `age` secrets management (`SDD.md` §6.3, `NFR-SEC-05`, `NFR-SEC-06`).

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::{CoreError, validate_dns_label};

/// AES-256 key length in bytes (256 bits).
pub const AES_256_KEY_LEN: usize = 32;

/// AES-GCM standard 96-bit nonce length in bytes.
pub const AES_GCM_NONCE_LEN: usize = 12;

/// AES-GCM authentication tag length in bytes.
pub const AES_GCM_TAG_LEN: usize = 16;

/// Computes the lowercase hexadecimal SHA-256 digest of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

/// Generates a cryptographically random 20-character alphanumeric admin password for Stage 0 Dex seeding.
pub fn generate_admin_password() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..20)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Hashes a plaintext password using bcrypt for Dex `staticPasswords` (`NFR-SEC-01`).
pub fn hash_dex_password_bcrypt(password: &str) -> Result<String, CoreError> {
    if password.trim().is_empty() {
        return Err(CoreError::Validation(
            "admin password must not be empty".to_string(),
        ));
    }
    bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| CoreError::Crypto(format!("bcrypt hash failed: {e}")))
}

/// `AES-256-GCM` envelope cipher for encrypting user SSH private keys and Forge PATs at rest (`NFR-SEC-05`).
///
/// Ciphertext format: `Base64(12-byte random nonce || ciphertext || 16-byte GCM tag)`.
#[derive(Clone)]
pub struct EnvelopeCrypto {
    key: [u8; AES_256_KEY_LEN],
}

impl fmt::Debug for EnvelopeCrypto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvelopeCrypto")
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl Drop for EnvelopeCrypto {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

impl EnvelopeCrypto {
    /// Creates an `EnvelopeCrypto` instance from a 32-byte key.
    pub fn new(key: [u8; AES_256_KEY_LEN]) -> Self {
        Self { key }
    }

    /// Creates an `EnvelopeCrypto` instance from a raw byte slice, validating 256-bit key length.
    pub fn from_slice(key_bytes: &[u8]) -> Result<Self, CoreError> {
        let key: [u8; AES_256_KEY_LEN] = key_bytes.try_into().map_err(|_| {
            CoreError::Crypto(format!(
                "Invalid AES-256-GCM key length: expected {} bytes, got {}",
                AES_256_KEY_LEN,
                key_bytes.len()
            ))
        })?;
        Ok(Self::new(key))
    }

    /// Creates an `EnvelopeCrypto` instance from a Base64-encoded 32-byte key.
    pub fn from_base64_key(b64_key: &str) -> Result<Self, CoreError> {
        let decoded = BASE64
            .decode(b64_key.trim())
            .map_err(|e| CoreError::Crypto(format!("Invalid Base64 envelope key: {e}")))?;
        Self::from_slice(&decoded)
    }

    /// Generates a fresh cryptographically random 256-bit key.
    pub fn generate_key() -> [u8; AES_256_KEY_LEN] {
        let mut key = [0u8; AES_256_KEY_LEN];
        rand::rng().fill(&mut key);
        key
    }

    /// Computes SHA-256 hex digest (associated helper convenience).
    pub fn sha256_hex(data: &[u8]) -> String {
        sha256_hex(data)
    }

    /// Encrypts `plaintext` with `AES-256-GCM` using a fresh 96-bit random nonce and returns Base64(`nonce || ciphertext`).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String, CoreError> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let mut nonce_bytes = [0u8; AES_GCM_NONCE_LEN];
        rand::rng().fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| CoreError::Crypto("AES-256-GCM encryption failed".to_string()))?;

        let mut combined = Vec::with_capacity(AES_GCM_NONCE_LEN + ciphertext.len());
        combined.extend_from_slice(&nonce_bytes);
        combined.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(combined))
    }

    /// Decrypts a Base64(`nonce || ciphertext`) payload produced by [`EnvelopeCrypto::encrypt`].
    pub fn decrypt(&self, encoded: &str) -> Result<Vec<u8>, CoreError> {
        let combined = BASE64
            .decode(encoded.trim())
            .map_err(|_| CoreError::Crypto("Invalid Base64 ciphertext payload".to_string()))?;

        if combined.len() < AES_GCM_NONCE_LEN + AES_GCM_TAG_LEN {
            return Err(CoreError::Crypto(
                "Ciphertext payload too short to contain AES-GCM nonce and authentication tag"
                    .to_string(),
            ));
        }

        let (nonce_bytes, ciphertext) = combined.split_at(AES_GCM_NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let nonce = Nonce::from_slice(nonce_bytes);

        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| CoreError::Crypto("AES-256-GCM decryption failed".to_string()))
    }

    /// Encrypts a UTF-8 credential string (SSH private key or PAT) into Base64(`nonce || ciphertext`).
    pub fn encrypt_str(&self, secret: &str) -> Result<String, CoreError> {
        self.encrypt(secret.as_bytes())
    }

    /// Decrypts a Base64(`nonce || ciphertext`) payload into a UTF-8 string.
    pub fn decrypt_string(&self, encoded: &str) -> Result<String, CoreError> {
        let bytes = self.decrypt(encoded)?;
        String::from_utf8(bytes)
            .map_err(|_| CoreError::Crypto("Decrypted payload is not valid UTF-8".to_string()))
    }
}

/// Generated `age` keypair with secret key redaction on `Debug` (`NFR-SEC-05`).
#[derive(Clone, PartialEq, Eq)]
pub struct AgeKeyPair {
    pub public_key: String,
    secret_key: String,
}

impl fmt::Debug for AgeKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgeKeyPair")
            .field("public_key", &self.public_key)
            .field("secret_key", &"[REDACTED]")
            .finish()
    }
}

impl AgeKeyPair {
    /// Constructs a validated `AgeKeyPair`.
    pub fn new(
        public_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let public_key = public_key.into().trim().to_string();
        let secret_key = secret_key.into().trim().to_string();
        validate_age_public_key(&public_key)?;
        if !secret_key.starts_with("AGE-SECRET-KEY-1")
            || secret_key.chars().any(char::is_whitespace)
        {
            return Err(CoreError::Validation(
                "Invalid age secret key; expected 'AGE-SECRET-KEY-1...'".to_string(),
            ));
        }
        Ok(Self {
            public_key,
            secret_key,
        })
    }

    /// Returns the raw `AGE-SECRET-KEY-1...` string for Kubernetes Secret injection or SOPS decryption.
    pub fn secret_key(&self) -> &str {
        &self.secret_key
    }

    /// Formats the keypair as a Flux-compatible `age.agekey` file (`flux-system/sops-age`).
    pub fn to_keyfile_contents(&self) -> String {
        format!("# public key: {}\n{}\n", self.public_key, self.secret_key)
    }
}

/// Single creation rule entry inside `.sops.yaml` (`SDD.md` §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SopsCreationRule {
    pub path_regex: String,
    pub age: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_regex: Option<String>,
}

/// Representation of the repository root `.sops.yaml` configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SopsConfig {
    pub creation_rules: Vec<SopsCreationRule>,
}

impl SopsConfig {
    /// Parses `.sops.yaml` content.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        Ok(serde_yml::from_str(yaml)?)
    }

    /// Serializes `.sops.yaml` content.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        Ok(serde_yml::to_string(self)?)
    }

    /// Inserts or updates a per-tenant rule (`^tenants/<tenant>/.*\.enc\.yaml$`) at the top of `creation_rules`
    /// so tenant-specific rules always match before any global fallback rule.
    pub fn upsert_tenant_rule(
        &mut self,
        tenant_id: &str,
        age_public_key: &str,
    ) -> Result<(), CoreError> {
        let rule = SopsManager::tenant_creation_rule(tenant_id, age_public_key)?;
        if let Some(existing) = self
            .creation_rules
            .iter_mut()
            .find(|r| r.path_regex == rule.path_regex)
        {
            existing.age = rule.age;
        } else {
            // Keep tenant rules ahead of catch-all cluster rules
            let insert_idx = self
                .creation_rules
                .iter()
                .position(|r| !r.path_regex.starts_with("^tenants/"))
                .unwrap_or(self.creation_rules.len());
            self.creation_rules.insert(insert_idx, rule);
        }
        Ok(())
    }
}

fn validate_age_public_key(age_public_key: &str) -> Result<(), CoreError> {
    let trimmed = age_public_key.trim();
    if !trimmed.starts_with("age1")
        || trimmed.len() < 10
        || trimmed.starts_with('-')
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(CoreError::Validation(
            "Invalid age public key; expected 'age1...' recipient without whitespace".to_string(),
        ));
    }
    Ok(())
}

/// Wrapper around `age-keygen` and `sops` CLI tools executing strictly via argument vectors (`NFR-SEC-06`).
#[derive(Debug, Clone)]
pub struct SopsManager {
    sops_bin: PathBuf,
    age_keygen_bin: PathBuf,
}

impl Default for SopsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SopsManager {
    /// Creates a `SopsManager` resolving `sops` and `age-keygen` from `PATH`.
    pub fn new() -> Self {
        Self {
            sops_bin: PathBuf::from("sops"),
            age_keygen_bin: PathBuf::from("age-keygen"),
        }
    }

    /// Creates a `SopsManager` with explicit paths to `sops` and `age-keygen`.
    pub fn with_binaries(sops_bin: impl Into<PathBuf>, age_keygen_bin: impl Into<PathBuf>) -> Self {
        Self {
            sops_bin: sops_bin.into(),
            age_keygen_bin: age_keygen_bin.into(),
        }
    }

    /// Builds a validated `.sops.yaml` creation rule scoped to `^tenants/<tenant>/.*\.enc\.yaml$`.
    pub fn tenant_creation_rule(
        tenant_id: &str,
        age_public_key: &str,
    ) -> Result<SopsCreationRule, CoreError> {
        validate_dns_label(tenant_id, "tenant_id")?;
        validate_age_public_key(age_public_key)?;
        Ok(SopsCreationRule {
            path_regex: format!(r"^tenants/{tenant_id}/.*\.enc\.yaml$"),
            age: age_public_key.trim().to_string(),
            encrypted_regex: None,
        })
    }

    /// Generates a complete `.sops.yaml` document for the given `(tenant_id, age_public_key)` pairs
    /// and optional cluster-level root `age` public key.
    pub fn generate_sops_yaml(
        tenants: &[(String, String)],
        root_age_public_key: Option<&str>,
    ) -> Result<String, CoreError> {
        let mut cfg = SopsConfig::default();
        for (tenant_id, pub_key) in tenants {
            cfg.upsert_tenant_rule(tenant_id, pub_key)?;
        }
        if let Some(root_pub) = root_age_public_key {
            validate_age_public_key(root_pub)?;
            cfg.creation_rules.push(SopsCreationRule {
                path_regex: r"^clusters/.*\.enc\.yaml$".to_string(),
                age: root_pub.trim().to_string(),
                encrypted_regex: None,
            });
        }
        cfg.to_yaml()
    }

    /// Builds explicit CLI argument vector for `sops --encrypt`.
    ///
    /// Note: all flags are placed before `target_file` as required by SOPS CLI parsing.
    pub fn build_encrypt_args(
        age_public_key: &str,
        target_file: &Path,
        sops_config: Option<&Path>,
    ) -> Result<Vec<String>, CoreError> {
        validate_age_public_key(age_public_key)?;
        let file_str = validate_cli_path(target_file, "target_file")?;

        let mut args = vec![
            "--encrypt".to_string(),
            "--age".to_string(),
            age_public_key.trim().to_string(),
            "--input-type".to_string(),
            "yaml".to_string(),
            "--output-type".to_string(),
            "yaml".to_string(),
        ];
        if let Some(cfg_path) = sops_config {
            let cfg_str = validate_cli_path(cfg_path, "sops_config")?;
            args.push("--config".to_string());
            args.push(cfg_str);
        }
        args.push(file_str);
        Ok(args)
    }

    /// Builds explicit CLI argument vector for `sops --decrypt`.
    pub fn build_decrypt_args(target_file: &Path) -> Result<Vec<String>, CoreError> {
        let file_str = validate_cli_path(target_file, "target_file")?;
        Ok(vec![
            "--decrypt".to_string(),
            "--input-type".to_string(),
            "yaml".to_string(),
            "--output-type".to_string(),
            "yaml".to_string(),
            file_str,
        ])
    }

    /// Generates a fresh `age` keypair by invoking `age-keygen` with an empty argument vector (no temp files).
    pub async fn generate_age_keypair(&self) -> Result<AgeKeyPair, CoreError> {
        let program_label = self.age_keygen_bin.display().to_string();
        let output = Command::new(&self.age_keygen_bin)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| CoreError::CommandFailed {
                program: program_label.clone(),
                message: e.to_string(),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CoreError::CommandFailed {
                program: program_label,
                message: format!("exit status {}: {}", output.status, stderr),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        parse_age_keygen_output(&stdout, &stderr)
    }

    /// Encrypts an on-disk YAML file via `sops --encrypt` and returns the SOPS-encrypted YAML output.
    pub async fn encrypt_file(
        &self,
        age_public_key: &str,
        target_file: &Path,
        sops_config: Option<&Path>,
    ) -> Result<String, CoreError> {
        let args = Self::build_encrypt_args(age_public_key, target_file, sops_config)?;
        self.run_sops(&args, None).await
    }

    /// Decrypts an on-disk SOPS-encrypted YAML file via `sops --decrypt`, passing the `age` secret key
    /// ephemerally via `SOPS_AGE_KEY` environment variable rather than CLI arguments.
    pub async fn decrypt_file(
        &self,
        age_secret_key: &str,
        target_file: &Path,
    ) -> Result<String, CoreError> {
        if !age_secret_key.trim().starts_with("AGE-SECRET-KEY-1") {
            return Err(CoreError::Validation(
                "Invalid age secret key for SOPS decryption".to_string(),
            ));
        }
        let args = Self::build_decrypt_args(target_file)?;
        self.run_sops(&args, Some(age_secret_key.trim())).await
    }

    /// Encrypts an in-memory YAML string using a restricted temporary file (`0o600` on Unix) cleaned up on drop.
    pub async fn encrypt_yaml(
        &self,
        age_public_key: &str,
        plaintext_yaml: &str,
    ) -> Result<String, CoreError> {
        let temp = ScopedTempFile::create(plaintext_yaml).await?;
        self.encrypt_file(age_public_key, temp.path(), None).await
    }

    /// Decrypts an in-memory SOPS-encrypted YAML string using `age_secret_key`.
    pub async fn decrypt_yaml(
        &self,
        age_secret_key: &str,
        encrypted_yaml: &str,
    ) -> Result<String, CoreError> {
        let temp = ScopedTempFile::create(encrypted_yaml).await?;
        self.decrypt_file(age_secret_key, temp.path()).await
    }

    async fn run_sops(
        &self,
        args: &[String],
        age_secret_key: Option<&str>,
    ) -> Result<String, CoreError> {
        let program_label = self.sops_bin.display().to_string();
        let mut cmd = Command::new(&self.sops_bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(secret_key) = age_secret_key {
            cmd.env("SOPS_AGE_KEY", secret_key);
        }

        let output = cmd.output().await.map_err(|e| CoreError::CommandFailed {
            program: program_label.clone(),
            message: e.to_string(),
        })?;

        if !output.status.success() {
            // Never include secret keys in error messages; only include SOPS status/stderr summary.
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(CoreError::CommandFailed {
                program: program_label,
                message: format!("exit status {}: {}", output.status, stderr),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn validate_cli_path(path: &Path, label: &str) -> Result<String, CoreError> {
    let s = path.to_string_lossy().into_owned();
    if s.trim().is_empty() || s.starts_with('-') {
        return Err(CoreError::Validation(format!(
            "{label} must be non-empty and must not start with '-'"
        )));
    }
    Ok(s)
}

fn parse_age_keygen_output(stdout: &str, stderr: &str) -> Result<AgeKeyPair, CoreError> {
    let mut public_key: Option<String> = None;
    let mut secret_key: Option<String> = None;

    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# public key:") {
            public_key = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Public key:") {
            public_key = Some(rest.trim().to_string());
        } else if trimmed.starts_with("AGE-SECRET-KEY-1") {
            secret_key = Some(trimmed.to_string());
        }
    }

    let pub_key = public_key.ok_or_else(|| {
        CoreError::Crypto("Failed to parse 'age1...' public key from age-keygen output".to_string())
    })?;
    let sec_key = secret_key.ok_or_else(|| {
        CoreError::Crypto(
            "Failed to parse 'AGE-SECRET-KEY-1...' secret key from age-keygen output".to_string(),
        )
    })?;

    AgeKeyPair::new(pub_key, sec_key)
}

struct ScopedTempFile {
    path: PathBuf,
}

impl ScopedTempFile {
    async fn create(contents: &str) -> Result<Self, CoreError> {
        let path = std::env::temp_dir().join(format!("keel-sops-{}.yaml", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, contents.as_bytes()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&path, perms).await?;
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_AGE_PUB: &str = "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p";
    const SAMPLE_AGE_SEC: &str =
        "AGE-SECRET-KEY-1GFPY0J9254PW3HYWW5AYYFG7ZQGVC7W3J2ELW8ZMRJ2KG5SFN9AQ5W2A0P";

    #[test]
    fn test_envelope_crypto_roundtrip_and_tamper_detection() {
        let key = EnvelopeCrypto::generate_key();
        let b64_key = BASE64.encode(key);
        let crypto = EnvelopeCrypto::from_base64_key(&b64_key).unwrap();

        // Debug redaction check (NFR-SEC-05)
        let dbg = format!("{crypto:?}");
        assert!(dbg.contains("[REDACTED]"));
        assert!(!dbg.contains(&b64_key));

        let ssh_key = "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret-ed25519-key\n-----END OPENSSH PRIVATE KEY-----";
        let encrypted_1 = crypto.encrypt_str(ssh_key).unwrap();
        let encrypted_2 = crypto.encrypt_str(ssh_key).unwrap();
        // Random 96-bit nonces ensure distinct ciphertexts for identical plaintexts
        assert_ne!(encrypted_1, encrypted_2);

        let decrypted = crypto.decrypt_string(&encrypted_1).unwrap();
        assert_eq!(decrypted, ssh_key);

        // Wrong key fails authentication
        let wrong_crypto = EnvelopeCrypto::new(EnvelopeCrypto::generate_key());
        assert!(wrong_crypto.decrypt(&encrypted_1).is_err());

        // Tampered ciphertext fails authentication
        let mut raw = BASE64.decode(&encrypted_1).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        let tampered = BASE64.encode(raw);
        assert!(crypto.decrypt(&tampered).is_err());

        // Too-short payload and invalid base64 rejected
        assert!(crypto.decrypt(&BASE64.encode([1u8; 10])).is_err());
        assert!(crypto.decrypt("!!!not-base64!!!").is_err());
        assert!(EnvelopeCrypto::from_slice(&[0u8; 16]).is_err());
        assert!(EnvelopeCrypto::from_base64_key("!!!").is_err());
    }

    #[test]
    fn test_sha256_and_dex_bcrypt_password_hashing() {
        let digest = EnvelopeCrypto::sha256_hex(b"keel-oidc");
        assert_eq!(digest.len(), 64);

        let password = generate_admin_password();
        assert_eq!(password.len(), 20);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));

        let hashed = hash_dex_password_bcrypt(&password).unwrap();
        assert!(hashed.starts_with("$2"));
        assert!(bcrypt::verify(&password, &hashed).unwrap());
        assert!(!bcrypt::verify("wrong-password", &hashed).unwrap());
        assert!(hash_dex_password_bcrypt("   ").is_err());
    }

    #[test]
    fn test_sops_yaml_per_tenant_rules_and_upsert() {
        let tenants = vec![
            ("alpha".to_string(), SAMPLE_AGE_PUB.to_string()),
            ("beta".to_string(), SAMPLE_AGE_PUB.to_string()),
        ];
        let yaml = SopsManager::generate_sops_yaml(&tenants, Some(SAMPLE_AGE_PUB)).unwrap();
        let mut parsed = SopsConfig::from_yaml(&yaml).unwrap();

        assert_eq!(parsed.creation_rules.len(), 3);
        assert_eq!(
            parsed.creation_rules[0].path_regex,
            r"^tenants/alpha/.*\.enc\.yaml$"
        );
        assert_eq!(
            parsed.creation_rules[1].path_regex,
            r"^tenants/beta/.*\.enc\.yaml$"
        );
        assert_eq!(
            parsed.creation_rules[2].path_regex,
            r"^clusters/.*\.enc\.yaml$"
        );

        // Upserting a new tenant places it BEFORE the cluster rule, and updating existing works in-place
        parsed.upsert_tenant_rule("gamma", SAMPLE_AGE_PUB).unwrap();
        parsed.upsert_tenant_rule("alpha", SAMPLE_AGE_PUB).unwrap();
        let reserialized = parsed.to_yaml().unwrap();
        assert!(reserialized.contains("tenants/gamma"));
        assert_eq!(parsed.creation_rules.len(), 4);
        assert_eq!(
            parsed.creation_rules[2].path_regex,
            r"^tenants/gamma/.*\.enc\.yaml$"
        );
        assert_eq!(
            parsed.creation_rules[3].path_regex,
            r"^clusters/.*\.enc\.yaml$"
        );

        // Reject unsafe tenant id or invalid age key
        assert!(SopsManager::tenant_creation_rule("../escape", SAMPLE_AGE_PUB).is_err());
        assert!(SopsManager::tenant_creation_rule("alpha", "bad-key").is_err());
    }

    #[test]
    fn test_sops_cli_arg_vector_construction_and_redaction() {
        let enc_args = SopsManager::build_encrypt_args(
            SAMPLE_AGE_PUB,
            Path::new("tenants/acme/secrets.enc.yaml"),
            Some(Path::new(".sops.yaml")),
        )
        .unwrap();
        assert_eq!(
            enc_args,
            vec![
                "--encrypt",
                "--age",
                SAMPLE_AGE_PUB,
                "--input-type",
                "yaml",
                "--output-type",
                "yaml",
                "--config",
                ".sops.yaml",
                "tenants/acme/secrets.enc.yaml",
            ]
        );

        let dec_args =
            SopsManager::build_decrypt_args(Path::new("tenants/acme/secrets.enc.yaml")).unwrap();
        assert_eq!(
            dec_args,
            vec![
                "--decrypt",
                "--input-type",
                "yaml",
                "--output-type",
                "yaml",
                "tenants/acme/secrets.enc.yaml",
            ]
        );

        // Flag injection paths rejected
        assert!(SopsManager::build_encrypt_args(SAMPLE_AGE_PUB, Path::new("-flag"), None).is_err());
        assert!(SopsManager::build_decrypt_args(Path::new("-flag")).is_err());

        // AgeKeyPair redaction & validation
        let kp = AgeKeyPair::new(SAMPLE_AGE_PUB, SAMPLE_AGE_SEC).unwrap();
        let dbg = format!("{kp:?}");
        assert!(dbg.contains(SAMPLE_AGE_PUB));
        assert!(dbg.contains("[REDACTED]"));
        assert!(!dbg.contains(SAMPLE_AGE_SEC));
        assert!(kp.to_keyfile_contents().contains(SAMPLE_AGE_SEC));
        assert!(AgeKeyPair::new(SAMPLE_AGE_PUB, "bad-secret").is_err());
        let _default_mgr = SopsManager::default();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_sops_manager_with_mock_binaries() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir =
            std::env::temp_dir().join(format!("keel-sops-mock-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        let mock_keygen = temp_dir.join("mock-age-keygen");
        let mock_sops = temp_dir.join("mock-sops");

        let keygen_script = format!(
            "#!/bin/sh\necho \"# created: 2026-11-14T00:00:00Z\"\necho \"# public key: {}\"\necho \"{}\"\n",
            SAMPLE_AGE_PUB, SAMPLE_AGE_SEC
        );
        let sops_script = "#!/bin/sh\nif [ \"$1\" = \"--encrypt\" ]; then\necho \"dbPassword: ENC[AES256_GCM,data:mock]\"\necho \"sops:\"\necho \"  version: 3.13.3\"\nexit 0\nfi\nif [ \"$1\" = \"--decrypt\" ] && [ -n \"$SOPS_AGE_KEY\" ]; then\necho \"dbPassword: super-secret-value\"\nexit 0\nfi\nexit 1\n";

        for (path, content) in [
            (&mock_keygen, keygen_script.as_str()),
            (&mock_sops, sops_script),
        ] {
            {
                use tokio::io::AsyncWriteExt;
                let mut f = tokio::fs::File::create(path).await.unwrap();
                f.write_all(content.as_bytes()).await.unwrap();
                f.sync_all().await.unwrap();
            }
            let mut perms = tokio::fs::metadata(path).await.unwrap().permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(path, perms).await.unwrap();
        }

        let manager = SopsManager::with_binaries(&mock_sops, &mock_keygen);
        let kp = manager.generate_age_keypair().await.unwrap();
        assert_eq!(kp.public_key, SAMPLE_AGE_PUB);
        assert_eq!(kp.secret_key(), SAMPLE_AGE_SEC);

        let encrypted = manager
            .encrypt_yaml(&kp.public_key, "dbPassword: super-secret-value\n")
            .await
            .unwrap();
        assert!(encrypted.contains("ENC[AES256_GCM"));

        let decrypted = manager
            .decrypt_yaml(kp.secret_key(), &encrypted)
            .await
            .unwrap();
        assert_eq!(decrypted.trim(), "dbPassword: super-secret-value");

        // Reject invalid secret key before invoking subprocess
        assert!(
            manager
                .decrypt_yaml("not-valid-key", &encrypted)
                .await
                .is_err()
        );

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_real_sops_and_age_roundtrip_if_installed() {
        let local_sops = PathBuf::from("/home/dylan/.local/bin/sops");
        let local_keygen = PathBuf::from("/home/dylan/.local/bin/age-keygen");
        if !(local_sops.exists() && local_keygen.exists()) {
            return;
        }

        let manager = SopsManager::with_binaries(local_sops, local_keygen);
        let keypair = manager.generate_age_keypair().await.unwrap();
        assert!(keypair.public_key.starts_with("age1"));
        assert!(keypair.secret_key().starts_with("AGE-SECRET-KEY-1"));

        let plaintext = "apiToken: s3cr3t-token-value\nnested:\n  password: p4ssw0rd\n";
        let encrypted = manager
            .encrypt_yaml(&keypair.public_key, plaintext)
            .await
            .unwrap();
        assert!(!encrypted.contains("s3cr3t-token-value"));
        assert!(!encrypted.contains("p4ssw0rd"));
        assert!(encrypted.contains("sops:"));

        let decrypted = manager
            .decrypt_yaml(keypair.secret_key(), &encrypted)
            .await
            .unwrap();
        let orig_val: serde_json::Value = serde_yml::from_str(plaintext).unwrap();
        let dec_val: serde_json::Value = serde_yml::from_str(&decrypted).unwrap();
        assert_eq!(orig_val, dec_val);
    }
}
