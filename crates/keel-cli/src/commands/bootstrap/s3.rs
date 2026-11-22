//! In-cluster S3 (SeaweedFS) helpers for Stage 0: admin credential sync, bucket creation and
//! versioning, tenant identities, Pulumi DIY backend URLs, and a minimal AWS SigV4 client used to
//! enable bucket versioning and store Stage 0 config/snapshot objects (`NFR-REL-01`).
//!
//! Credentials are only ever passed via request signatures, environment variables, or `stdin`
//! (`weed shell`); never via process argv (`NFR-SEC-05`, `NFR-SEC-06`).

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use keel_core::{
    FLUX_SYSTEM_NAMESPACE, KEEL_PREVIEWS_NAMESPACE, KEEL_SYSTEM_NAMESPACE, generate_admin_password,
};
use keel_kube::S3_CREDENTIALS_SECRET_NAME;
use keel_kube::tenant::{
    TENANT_S3_CREDENTIALS_SECRET_NAME, build_tenant_s3_credentials_secret,
    seaweedfs_configure_tenant_identity_script,
};
use keel_pko::IN_CLUSTER_S3_BACKEND_URL;
use serde_json::json;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::kube::{KubeTarget, TempPortForward, start_port_forward};
use crate::commands::CliError;

/// Default S3 region used for SeaweedFS (matches the in-cluster PKO backend URL).
pub const DEFAULT_S3_REGION: &str = "us-east-1";
/// SeaweedFS chart-generated admin credential Secret in `keel-system`.
pub(crate) const SEAWEEDFS_S3_SECRET: &str = "s3-state-backend-seaweedfs-s3-secret";
/// SeaweedFS S3 gateway Service and port.
pub(crate) const SEAWEEDFS_S3_SERVICE: &str = "svc/s3-state-backend-seaweedfs-s3";
pub(crate) const SEAWEEDFS_S3_PORT: u16 = 8333;

/// S3 access key pair (secret zeroized on drop, redacted in `Debug`).
#[derive(Clone)]
pub struct S3Credentials {
    pub access_key: String,
    pub secret_key: Zeroizing<String>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"[REDACTED]")
            .finish()
    }
}

impl S3Credentials {
    /// Reads `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` from the environment.
    pub fn from_env() -> Option<Self> {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        Some(Self {
            access_key,
            secret_key: Zeroizing::new(secret_key),
        })
    }

    /// Environment variables to hand to `pulumi` for the DIY S3 backend.
    pub fn env_pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("AWS_ACCESS_KEY_ID", self.access_key.clone()),
            (
                "AWS_SECRET_ACCESS_KEY",
                self.secret_key.as_str().to_string(),
            ),
        ]
    }
}

/// Parsed S3 endpoint (`http(s)://host[:port]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Endpoint {
    pub https: bool,
    /// `host[:port]` as used in the `Host` header and Pulumi `endpoint=` query parameter.
    pub authority: String,
}

impl S3Endpoint {
    /// Parses and validates an S3 endpoint URL. `flag` names the CLI flag for error messages.
    pub fn parse(raw: &str, flag: &str) -> Result<Self, CliError> {
        let invalid = || {
            CliError::Validation(format!(
                "invalid {flag} '{}': expected an http:// or https:// URL such as `http://127.0.0.1:8333` (no path, query, or credentials)",
                raw.trim()
            ))
        };
        let parsed = url::Url::parse(raw.trim()).map_err(|_| invalid())?;
        let https = match parsed.scheme() {
            "http" => false,
            "https" => true,
            _ => return Err(invalid()),
        };
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !(parsed.path().is_empty() || parsed.path() == "/")
        {
            return Err(invalid());
        }
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(invalid)?;
        let authority = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        Ok(Self { https, authority })
    }

    pub fn base_url(&self) -> String {
        format!(
            "{}://{}",
            if self.https { "https" } else { "http" },
            self.authority
        )
    }
}

/// Builds a Pulumi DIY S3 backend URL with the same query parameters as the live in-cluster
/// Stack backend (`region`, `s3ForcePathStyle`, `disableSSL` for plain HTTP).
pub fn s3_backend_url(
    bucket: &str,
    prefix: Option<&str>,
    endpoint: &S3Endpoint,
    region: &str,
) -> Result<String, CliError> {
    keel_core::validate_dns_label(bucket, "bucket")?;
    if region.is_empty()
        || !region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(CliError::Validation(format!(
            "invalid S3 region '{region}'"
        )));
    }
    let mut url = format!("s3://{bucket}");
    if let Some(prefix) = prefix
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
    {
        if !is_safe_object_key(prefix) {
            return Err(CliError::Validation(format!(
                "invalid S3 state prefix '{prefix}'"
            )));
        }
        url.push('/');
        url.push_str(prefix);
    }
    url.push_str(&format!(
        "?endpoint={}&region={region}&s3ForcePathStyle=true",
        endpoint.authority
    ));
    if !endpoint.https {
        url.push_str("&disableSSL=true");
    }
    Ok(url)
}

/// Accepts only `[A-Za-z0-9._-/]` object keys without `..` or empty segments.
pub(crate) fn is_safe_object_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        && key
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

// ============================================================================
// Minimal SigV4 S3 client
// ============================================================================

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Minimal path-style S3 client (SigV4) for SeaweedFS / any S3-compatible endpoint.
pub struct S3Client {
    endpoint: S3Endpoint,
    region: String,
    creds: S3Credentials,
    http: reqwest::Client,
}

/// One object version entry from `ListObjectVersions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ObjectVersion {
    pub key: String,
    pub version_id: String,
    pub is_latest: bool,
    pub last_modified: String,
}

impl S3Client {
    pub fn new(endpoint: S3Endpoint, region: &str, creds: S3Credentials) -> Result<Self, CliError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("keel-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            endpoint,
            region: region.to_string(),
            creds,
            http,
        })
    }

    /// Signs and sends a request. `query` pairs must be unencoded.
    async fn send(
        &self,
        method: reqwest::Method,
        bucket: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<reqwest::Response, CliError> {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let canonical_uri = match key {
            Some(k) => format!("/{}/{}", uri_encode(bucket, true), uri_encode(k, false)),
            None => format!("/{}", uri_encode(bucket, true)),
        };
        let mut q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
            .collect();
        q.sort();
        let canonical_query = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let payload_hash = sha256_hex(&body);
        let host = &self.endpoint.authority;
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            method.as_str()
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_secret = Zeroizing::new(format!("AWS4{}", self.creds.secret_key.as_str()));
        let k_date = hmac(k_secret.as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.creds.access_key
        );
        let mut url = format!("{}{canonical_uri}", self.endpoint.base_url());
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }
        let resp = self
            .http
            .request(method, &url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                CliError::Execution(format!(
                    "cannot reach S3 endpoint {}: {}",
                    self.endpoint.base_url(),
                    e.without_url()
                ))
            })?;
        Ok(resp)
    }

    async fn expect_ok(resp: reqwest::Response, what: &str) -> Result<Vec<u8>, CliError> {
        let status = resp.status();
        let body = resp.bytes().await.unwrap_or_default().to_vec();
        if !status.is_success() {
            let snippet: String = String::from_utf8_lossy(&body).chars().take(300).collect();
            return Err(CliError::Execution(format!(
                "S3 {what} failed (HTTP {status}): {snippet}"
            )));
        }
        Ok(body)
    }

    /// `PUT /<bucket>?versioning` with `Status=Enabled`.
    pub async fn enable_bucket_versioning(&self, bucket: &str) -> Result<(), CliError> {
        let body = br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#.to_vec();
        let resp = self
            .send(
                reqwest::Method::PUT,
                bucket,
                None,
                &[("versioning", "")],
                body,
            )
            .await?;
        Self::expect_ok(resp, "PutBucketVersioning").await?;
        Ok(())
    }

    /// `GET /<bucket>?versioning`; returns `true` when `Status` is `Enabled`.
    pub async fn bucket_versioning_enabled(&self, bucket: &str) -> Result<bool, CliError> {
        let resp = self
            .send(
                reqwest::Method::GET,
                bucket,
                None,
                &[("versioning", "")],
                Vec::new(),
            )
            .await?;
        let body = Self::expect_ok(resp, "GetBucketVersioning").await?;
        Ok(String::from_utf8_lossy(&body).contains("<Status>Enabled</Status>"))
    }

    pub async fn put_object(&self, bucket: &str, key: &str, data: Vec<u8>) -> Result<(), CliError> {
        if !is_safe_object_key(key) {
            return Err(CliError::Validation(format!(
                "invalid S3 object key '{key}'"
            )));
        }
        let resp = self
            .send(reqwest::Method::PUT, bucket, Some(key), &[], data)
            .await?;
        Self::expect_ok(resp, "PutObject").await?;
        Ok(())
    }

    /// Returns `Ok(None)` when the object (or version) does not exist.
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<Option<Vec<u8>>, CliError> {
        if !is_safe_object_key(key) {
            return Err(CliError::Validation(format!(
                "invalid S3 object key '{key}'"
            )));
        }
        let query: Vec<(&str, &str)> = version_id.map(|v| ("versionId", v)).into_iter().collect();
        let resp = self
            .send(reqwest::Method::GET, bucket, Some(key), &query, Vec::new())
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::expect_ok(resp, "GetObject").await?))
    }

    /// `GET /<bucket>?versions&prefix=<prefix>` (first page).
    pub async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3ObjectVersion>, CliError> {
        let resp = self
            .send(
                reqwest::Method::GET,
                bucket,
                None,
                &[("versions", ""), ("prefix", prefix)],
                Vec::new(),
            )
            .await?;
        let body = Self::expect_ok(resp, "ListObjectVersions").await?;
        Ok(parse_list_object_versions(&String::from_utf8_lossy(&body)))
    }
}

fn xml_tag<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(&block[start..end])
}

pub(crate) fn parse_list_object_versions(xml: &str) -> Vec<S3ObjectVersion> {
    xml.split("<Version>")
        .skip(1)
        .filter_map(|chunk| {
            let block = chunk.split("</Version>").next()?;
            Some(S3ObjectVersion {
                key: xml_tag(block, "Key")?.to_string(),
                version_id: xml_tag(block, "VersionId").unwrap_or("null").to_string(),
                is_latest: xml_tag(block, "IsLatest") == Some("true"),
                last_modified: xml_tag(block, "LastModified")
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

// ============================================================================
// SeaweedFS (in-cluster) helpers
// ============================================================================

/// Reads the SeaweedFS admin credentials generated by the chart.
pub(crate) async fn read_seaweedfs_admin_credentials(
    target: KubeTarget<'_>,
) -> Result<S3Credentials, CliError> {
    let access_key = target
        .read_secret_key(
            KEEL_SYSTEM_NAMESPACE,
            SEAWEEDFS_S3_SECRET,
            "admin_access_key_id",
        )
        .await
        .ok_or_else(|| {
            CliError::Execution(format!(
                "missing `admin_access_key_id` in `{KEEL_SYSTEM_NAMESPACE}/{SEAWEEDFS_S3_SECRET}`"
            ))
        })?;
    let secret_key = target
        .read_secret_key(
            KEEL_SYSTEM_NAMESPACE,
            SEAWEEDFS_S3_SECRET,
            "admin_secret_access_key",
        )
        .await
        .ok_or_else(|| {
            CliError::Execution(format!(
                "missing `admin_secret_access_key` in `{KEEL_SYSTEM_NAMESPACE}/{SEAWEEDFS_S3_SECRET}`"
            ))
        })?;
    Ok(S3Credentials {
        access_key,
        secret_key: Zeroizing::new(secret_key),
    })
}

/// Mirrors the SeaweedFS admin credentials into control-plane namespaces only (never tenants).
pub(crate) async fn sync_in_cluster_s3_credentials_from_seaweedfs(
    target: KubeTarget<'_>,
) -> Result<(), CliError> {
    // The `tenant-default` baseline is applied by the caller before the Kyverno bundle
    // (`ensure_tenant_baseline`), never here.
    let creds = read_seaweedfs_admin_credentials(target).await?;

    for ns in [
        KEEL_SYSTEM_NAMESPACE,
        FLUX_SYSTEM_NAMESPACE,
        KEEL_PREVIEWS_NAMESPACE,
    ] {
        let secret_doc = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": S3_CREDENTIALS_SECRET_NAME,
                "namespace": ns,
                "labels": {
                    "app.kubernetes.io/managed-by": "keel",
                    "keel.io/secret-kind": "s3-credentials",
                },
            },
            "type": "Opaque",
            "stringData": {
                "accessKey": creds.access_key,
                "secretKey": creds.secret_key.as_str(),
            },
        });
        target.apply(&serde_yml::to_string(&secret_doc)?).await?;
    }
    Ok(())
}

/// Runs a `weed shell` script inside the SeaweedFS filer pod via **stdin** (keys never in argv).
pub(crate) async fn run_seaweedfs_shell_script(
    target: KubeTarget<'_>,
    script: &str,
) -> Result<String, CliError> {
    let out = target
        .output_with_stdin(
            &[
                "-n",
                KEEL_SYSTEM_NAMESPACE,
                "exec",
                "-i",
                "sts/s3-state-backend-seaweedfs-filer",
                "--",
                "weed",
                "shell",
                "-master=s3-state-backend-seaweedfs-master:9333",
            ],
            script.as_bytes(),
        )
        .await?;
    if !out.status.success() {
        // stdout/stderr may echo the script (including keys); never surface them verbatim.
        return Err(CliError::Execution(format!(
            "`weed shell` exited with {} inside the SeaweedFS filer pod",
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Creates the `pulumi-state` bucket (idempotent).
pub(crate) async fn ensure_seaweedfs_pulumi_bucket(target: KubeTarget<'_>) -> Result<(), CliError> {
    match run_seaweedfs_shell_script(
        target,
        &format!("s3.bucket.create -name {}\n", crate::cli::DEFAULT_S3_BUCKET),
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(err) => {
            // `weed shell` returns non-zero when the bucket already exists; verify by listing.
            let listing = run_seaweedfs_shell_script(target, "s3.bucket.list\n")
                .await
                .unwrap_or_default();
            if listing.contains(crate::cli::DEFAULT_S3_BUCKET) {
                Ok(())
            } else {
                Err(CliError::Execution(format!(
                    "failed to create `{}` bucket in SeaweedFS: {err}",
                    crate::cli::DEFAULT_S3_BUCKET
                )))
            }
        }
    }
}

/// Port-forwards the in-cluster SeaweedFS S3 gateway to `127.0.0.1:<port>`.
pub(crate) async fn start_seaweedfs_s3_port_forward(
    target: KubeTarget<'_>,
) -> Result<TempPortForward, CliError> {
    start_port_forward(
        target,
        KEEL_SYSTEM_NAMESPACE,
        SEAWEEDFS_S3_SERVICE,
        18333,
        SEAWEEDFS_S3_PORT,
    )
    .await
}

/// Provisions a least-privilege SeaweedFS identity for `tenant-<id>` scoped to
/// `<bucket>/tenants/<id>/*` (idempotent: existing keys are reused), plus the tenant's Pulumi
/// state passphrase (`tenant-<id>/keel-pulumi-passphrase`, created once, never rotated).
pub(crate) async fn provision_tenant_s3_identity(
    target: KubeTarget<'_>,
    tenant_id: &str,
) -> Result<(), CliError> {
    let tenant_ns = format!("tenant-{tenant_id}");
    super::state::ensure_pulumi_passphrase_secret(target, &tenant_ns).await?;
    let existing_access = target
        .read_secret_key(&tenant_ns, TENANT_S3_CREDENTIALS_SECRET_NAME, "accessKey")
        .await;
    let existing_secret = target
        .read_secret_key(&tenant_ns, TENANT_S3_CREDENTIALS_SECRET_NAME, "secretKey")
        .await;
    let (access_key, secret_key) = match (existing_access, existing_secret) {
        (Some(a), Some(s)) => (a, Zeroizing::new(s)),
        _ => (
            format!("KEEL{}", generate_admin_password()).to_uppercase(),
            Zeroizing::new(format!(
                "{}{}",
                generate_admin_password(),
                generate_admin_password()
            )),
        ),
    };

    let script = Zeroizing::new(seaweedfs_configure_tenant_identity_script(
        crate::cli::DEFAULT_S3_BUCKET,
        tenant_id,
        &access_key,
        &secret_key,
    )?);
    run_seaweedfs_shell_script(target, &script).await?;

    let scoped = build_tenant_s3_credentials_secret(
        tenant_id,
        &access_key,
        &secret_key,
        IN_CLUSTER_S3_BACKEND_URL,
    )?;
    target.apply(&serde_yml::to_string(&scoped)?).await?;

    let mirror = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": S3_CREDENTIALS_SECRET_NAME,
            "namespace": tenant_ns,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/secret-kind": keel_kube::tenant::TENANT_S3_CREDENTIALS_SECRET_KIND,
                "keel.io/tenant": tenant_id,
            },
        },
        "type": "Opaque",
        "stringData": {
            "accessKey": access_key,
            "secretKey": secret_key.as_str(),
        },
    });
    target.apply(&serde_yml::to_string(&mirror)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing_and_backend_url_parity_with_in_cluster_stack() {
        let ep = S3Endpoint::parse("http://127.0.0.1:18333", "--s3-endpoint").unwrap();
        assert!(!ep.https);
        assert_eq!(ep.authority, "127.0.0.1:18333");
        let url =
            s3_backend_url("pulumi-state", Some("stage0/keel-mgmt"), &ep, "us-east-1").unwrap();
        assert_eq!(
            url,
            "s3://pulumi-state/stage0/keel-mgmt?endpoint=127.0.0.1:18333&region=us-east-1&s3ForcePathStyle=true&disableSSL=true"
        );
        // Same query parameters as the live in-cluster Stack backend.
        for param in [
            "region=us-east-1",
            "s3ForcePathStyle=true",
            "disableSSL=true",
        ] {
            assert!(IN_CLUSTER_S3_BACKEND_URL.contains(param));
        }

        let tls = S3Endpoint::parse("https://s3.example.com", "--s3-endpoint").unwrap();
        let url = s3_backend_url("pulumi-state", None, &tls, "eu-west-1").unwrap();
        assert_eq!(
            url,
            "s3://pulumi-state?endpoint=s3.example.com&region=eu-west-1&s3ForcePathStyle=true"
        );

        for bad in [
            "ftp://s3.local",
            "s3.local:9000",
            "http://user:pw@s3.local",
            "http://s3.local/bucket",
            "http://s3.local?x=1",
        ] {
            let err = S3Endpoint::parse(bad, "--s3-endpoint")
                .unwrap_err()
                .to_string();
            assert!(err.contains("--s3-endpoint"), "{err}");
            assert!(!err.contains("server URL"), "{err}");
        }
        assert!(s3_backend_url("pulumi-state", Some("../x"), &ep, "us-east-1").is_err());
        assert!(s3_backend_url("pulumi-state", None, &ep, "us east").is_err());
    }

    #[test]
    fn sigv4_primitives_match_aws_reference_vectors() {
        // AWS SigV4 documentation example signing key derivation.
        let k_date = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20120215");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"iam");
        let k_signing = hmac(&k_service, b"aws4_request");
        assert_eq!(
            hex::encode(k_signing),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
        assert_eq!(uri_encode("a b/c~", false), "a%20b/c~");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert!(is_safe_object_key("stage0/keel-mgmt/config/Pulumi.x.yaml"));
        assert!(!is_safe_object_key("stage0/../x"));
        assert!(!is_safe_object_key("/abs"));
        assert!(!is_safe_object_key("a//b"));
    }

    #[test]
    fn list_object_versions_parser() {
        let xml = r#"<ListVersionsResult><Version><Key>stage0/a.json</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest><LastModified>2026-11-20T10:00:00Z</LastModified></Version><Version><Key>stage0/a.json</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest><LastModified>2026-11-19T10:00:00Z</LastModified></Version></ListVersionsResult>"#;
        let v = parse_list_object_versions(xml);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].version_id, "v2");
        assert!(v[0].is_latest);
        assert!(!v[1].is_latest);
    }

    #[test]
    fn credentials_debug_is_redacted() {
        let c = S3Credentials {
            access_key: "AK".into(),
            secret_key: Zeroizing::new("very-secret".into()),
        };
        assert!(!format!("{c:?}").contains("very-secret"));
    }
}
