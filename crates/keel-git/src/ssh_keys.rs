//! SSH key helpers for per-repository deploy keys (GitHub, Forgejo SSH fallback, `SDD.md` §3.2):
//! ed25519 key generation, SHA-256 fingerprints, `known_hosts` lines and SSH clone URLs.
//!
//! Private keys are held in [`Zeroizing`] buffers and never appear in `Debug` output.

use std::fmt;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use zeroize::Zeroizing;

use crate::forge::ForgeError;

/// GitHub's SSH-over-HTTPS-port endpoint, for networks that block outbound port 22 (D6).
/// Its host keys are the same as `github.com`'s (`GET /meta` `ssh_keys`).
pub const GITHUB_SSH_FALLBACK_HOST: &str = "ssh.github.com";
/// Port of [`GITHUB_SSH_FALLBACK_HOST`].
pub const GITHUB_SSH_FALLBACK_PORT: u16 = 443;

/// A freshly generated ed25519 key pair in OpenSSH format. `Debug` never prints the private key.
pub struct GeneratedSshKeyPair {
    /// OpenSSH private key (`-----BEGIN OPENSSH PRIVATE KEY-----`, LF line endings, unencrypted).
    pub private_key_openssh: Zeroizing<String>,
    /// One-line OpenSSH public key (`ssh-ed25519 <base64> <comment>`).
    pub public_key_openssh: String,
    /// `SHA256:<base64>` fingerprint of the public key.
    pub fingerprint_sha256: String,
}

impl fmt::Debug for GeneratedSshKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedSshKeyPair")
            .field("private_key_openssh", &"[REDACTED]")
            .field("public_key_openssh", &self.public_key_openssh)
            .field("fingerprint_sha256", &self.fingerprint_sha256)
            .finish()
    }
}

fn ssh_err(context: &str, err: impl fmt::Display) -> ForgeError {
    ForgeError::InvalidInput(format!("{context}: {err}"))
}

/// Generates a new ed25519 key pair with `comment` (e.g. `keel-tenant-acme`). The comment must
/// be a single printable line.
pub fn generate_ed25519_key_pair(comment: &str) -> Result<GeneratedSshKeyPair, ForgeError> {
    if comment.chars().any(char::is_control) {
        return Err(ForgeError::InvalidInput(
            "SSH key comment must not contain control characters".to_string(),
        ));
    }
    let mut key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .map_err(|e| ssh_err("generating an ed25519 key", e))?;
    key.set_comment(comment.trim());
    let private_key_openssh = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| ssh_err("encoding the private key", e))?;
    let public = key.public_key();
    let public_key_openssh = public
        .to_openssh()
        .map_err(|e| ssh_err("encoding the public key", e))?;
    Ok(GeneratedSshKeyPair {
        private_key_openssh,
        public_key_openssh,
        fingerprint_sha256: public.fingerprint(HashAlg::Sha256).to_string(),
    })
}

fn parse_public_key(public_key: &str) -> Result<PublicKey, ForgeError> {
    let line = public_key.trim();
    if line.is_empty() || line.contains('\n') || line.contains('\r') {
        return Err(ForgeError::InvalidInput(
            "SSH public key must be a single OpenSSH line ('<type> <base64> [comment]')"
                .to_string(),
        ));
    }
    PublicKey::from_openssh(line).map_err(|e| ssh_err("invalid OpenSSH public key", e))
}

/// `SHA256:<base64>` fingerprint (as printed by `ssh-keygen -lf`) of a one-line OpenSSH public
/// key. The comment does not affect the fingerprint.
pub fn ssh_public_key_fingerprint(public_key: &str) -> Result<String, ForgeError> {
    Ok(parse_public_key(public_key)?
        .fingerprint(HashAlg::Sha256)
        .to_string())
}

/// `<type> <base64>` of a one-line OpenSSH public key, without its comment.
pub fn normalize_ssh_public_key(public_key: &str) -> Result<String, ForgeError> {
    let mut key = parse_public_key(public_key)?;
    key.set_comment("");
    key.to_openssh()
        .map(|s| s.trim().to_string())
        .map_err(|e| ssh_err("encoding the public key", e))
}

/// Validates a host name or IP literal for `known_hosts` / SSH URLs: no whitespace, patterns
/// (`*`, `?`, `!`), lists (`,`), hashed entries (`|`), brackets or user info.
fn validate_ssh_host(host: &str) -> Result<&str, ForgeError> {
    let host = host.trim();
    let valid = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'));
    if valid {
        Ok(host)
    } else {
        Err(ForgeError::InvalidInput(format!(
            "invalid SSH host '{host}'"
        )))
    }
}

/// Host field of a `known_hosts` entry / OpenSSH host spec: the bare host on port 22,
/// `[host]:port` otherwise.
fn known_hosts_host(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Builds one `known_hosts` line (`<host> <type> <base64>`) for `host_public_key` (an OpenSSH
/// public key line; any comment is dropped). Port 22 uses the bare host, other ports
/// `[host]:port`, matching how OpenSSH (and Flux/PKO through `known_hosts`) look hosts up.
pub fn known_hosts_line(
    host: &str,
    port: u16,
    host_public_key: &str,
) -> Result<String, ForgeError> {
    if port == 0 {
        return Err(ForgeError::InvalidInput(
            "SSH port must not be 0".to_string(),
        ));
    }
    let host = validate_ssh_host(host)?;
    let key = normalize_ssh_public_key(host_public_key)?;
    Ok(format!("{} {key}", known_hosts_host(host, port)))
}

/// Builds a `known_hosts` file (one line per host key, trailing newline).
pub fn known_hosts_file(
    host: &str,
    port: u16,
    host_public_keys: &[&str],
) -> Result<String, ForgeError> {
    if host_public_keys.is_empty() {
        return Err(ForgeError::InvalidInput(
            "at least one SSH host key is required (never trust on first use)".to_string(),
        ));
    }
    let mut out = String::new();
    for key in host_public_keys {
        out.push_str(&known_hosts_line(host, port, key)?);
        out.push('\n');
    }
    Ok(out)
}

/// Builds an `ssh://git@<host>[:port]/<owner>/<repo>.git` clone URL. Keel builds SSH URLs
/// itself because forges report `ssh_url` from their own configuration (Forgejo's can point at
/// an unreachable `SSH_DOMAIN`).
pub fn ssh_clone_url(host: &str, port: u16, owner: &str, repo: &str) -> Result<String, ForgeError> {
    if port == 0 {
        return Err(ForgeError::InvalidInput(
            "SSH port must not be 0".to_string(),
        ));
    }
    let host = validate_ssh_host(host)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let owner = crate::remote_repo::namespace_path(owner, "owner")?;
    let repo =
        crate::remote_repo::single_segment(repo.strip_suffix(".git").unwrap_or(repo), "repo")?;
    let authority = if port == 22 {
        host
    } else {
        format!("{host}:{port}")
    };
    Ok(format!("ssh://git@{authority}/{owner}/{repo}.git"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    #[test]
    fn generated_key_pair_round_trips_and_is_redacted() {
        let pair = generate_ed25519_key_pair("keel-tenant-acme").unwrap();
        assert!(
            pair.private_key_openssh
                .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n")
        );
        assert!(!pair.private_key_openssh.contains('\r'));
        assert!(
            pair.public_key_openssh
                .starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5")
        );
        assert!(pair.public_key_openssh.ends_with(" keel-tenant-acme"));
        assert!(pair.fingerprint_sha256.starts_with("SHA256:"));
        assert_eq!(
            ssh_public_key_fingerprint(&pair.public_key_openssh).unwrap(),
            pair.fingerprint_sha256
        );
        // The private key parses and derives the same public key.
        let parsed = PrivateKey::from_openssh(pair.private_key_openssh.as_bytes()).unwrap();
        assert_eq!(
            parsed.public_key().fingerprint(HashAlg::Sha256).to_string(),
            pair.fingerprint_sha256
        );
        let debug = format!("{pair:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("PRIVATE KEY"));

        let other = generate_ed25519_key_pair("").unwrap();
        assert_ne!(other.fingerprint_sha256, pair.fingerprint_sha256);
        assert!(generate_ed25519_key_pair("bad\ncomment").is_err());
    }

    #[test]
    fn fingerprint_ignores_comment_and_matches_ssh_keygen() {
        // `ssh-keygen -lf` of HOST_KEY (GitHub's published ed25519 host key).
        let expected = "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU";
        assert_eq!(ssh_public_key_fingerprint(HOST_KEY).unwrap(), expected);
        assert_eq!(
            ssh_public_key_fingerprint(&format!("  {HOST_KEY} some comment \n")).unwrap(),
            expected
        );
        assert_eq!(
            normalize_ssh_public_key(&format!("{HOST_KEY} c")).unwrap(),
            HOST_KEY
        );
        for bad in ["", "ssh-ed25519", "ssh-ed25519 !!!", "not a key", "a\nb"] {
            assert!(ssh_public_key_fingerprint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn known_hosts_lines_use_bare_host_on_22_and_brackets_otherwise() {
        assert_eq!(
            known_hosts_line("github.com", 22, &format!("{HOST_KEY} comment")).unwrap(),
            format!("github.com {HOST_KEY}")
        );
        assert_eq!(
            known_hosts_line(GITHUB_SSH_FALLBACK_HOST, GITHUB_SSH_FALLBACK_PORT, HOST_KEY).unwrap(),
            format!("[ssh.github.com]:443 {HOST_KEY}")
        );
        assert_eq!(
            known_hosts_line("forgejo-ssh.keel-system.svc.cluster.local", 2222, HOST_KEY).unwrap(),
            format!("[forgejo-ssh.keel-system.svc.cluster.local]:2222 {HOST_KEY}")
        );
        for host in [
            "",
            "*.example.com",
            "a,b",
            "|1|hash",
            "-oProxy",
            "a b",
            "[x]",
            "u@h",
        ] {
            assert!(known_hosts_line(host, 22, HOST_KEY).is_err(), "{host}");
        }
        assert!(known_hosts_line("h", 0, HOST_KEY).is_err());
        assert!(known_hosts_line("h", 22, "garbage").is_err());

        let file = known_hosts_file("h", 22, &[HOST_KEY, HOST_KEY]).unwrap();
        assert_eq!(file.lines().count(), 2);
        assert!(file.ends_with('\n'));
        assert!(known_hosts_file("h", 22, &[]).is_err());
    }

    #[test]
    fn ssh_clone_urls_are_built_from_host_and_port() {
        assert_eq!(
            ssh_clone_url("github.com", 22, "acme", "tenant-a").unwrap(),
            "ssh://git@github.com/acme/tenant-a.git"
        );
        assert_eq!(
            ssh_clone_url("ssh.github.com", 443, "acme", "tenant-a.git").unwrap(),
            "ssh://git@ssh.github.com:443/acme/tenant-a.git"
        );
        assert_eq!(
            ssh_clone_url("::1", 2222, "o", "r").unwrap(),
            "ssh://git@[::1]:2222/o/r.git"
        );
        assert!(ssh_clone_url("h", 0, "o", "r").is_err());
        assert!(ssh_clone_url("h", 22, "..", "r").is_err());
        assert!(ssh_clone_url("h h", 22, "o", "r").is_err());
        assert_eq!(
            ssh_clone_url("gitlab.example.com", 22, "team/tenants", "tenant-a").unwrap(),
            "ssh://git@gitlab.example.com/team/tenants/tenant-a.git"
        );
        for (owner, repo) in [
            ("%2e%2e", "r"),
            ("o", ".%2E"),
            ("o@evil", "r"),
            ("o", "r r"),
            ("o", "a%2fb"),
            ("o", "."),
            ("team//x", "r"),
            ("/o", "r"),
        ] {
            assert!(
                ssh_clone_url("h", 22, owner, repo).is_err(),
                "{owner}/{repo}"
            );
        }
    }
}
