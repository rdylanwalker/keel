//! Real `russh` WebSocket-to-SSH PTY bridge (`SDD.md` §7.3).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use keel_core::models::{is_private_or_internal_ip, is_unresolved_ssh_target};
use keel_core::{
    StackSshInfoResponse, clean_candidate_ssh_host, clean_candidate_ssh_user,
    is_private_or_internal_ssh_host,
};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, decode_secret_key};
use russh::{ChannelMsg, client};
use serde::{Deserialize, Serialize};

/// Providers that do not expose an SSH-accessible VM/node endpoint.
const NON_SSH_PROVIDERS: &[&str] = &[
    "kind",
    "kubernetes",
    "k8s",
    "helm",
    "manifest",
    "kustomize",
    "flux",
    "pulumi-k8s",
    "docker",
    "local",
    "cloudflare",
    "kubeconfig",
];

/// Returns `true` if the given infrastructure stack/provider supports SSH access.
///
/// Stacks whose blueprint declares a non-VM `metadata.resourceKind` (stamped as the
/// `keel.io/resource-kind` label) are already refused by `resolve_stack_ssh_info`; this only
/// screens providers that never expose an SSH endpoint.
pub fn is_ssh_supported_infra(provider: &str, stack_name: &str) -> bool {
    let p = provider.trim().to_ascii_lowercase();
    if NON_SSH_PROVIDERS.contains(&p.as_str()) {
        return false;
    }
    !(p.is_empty()
        && stack_name
            .trim()
            .eq_ignore_ascii_case("management-cluster-core"))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum SshWsClientMessage {
    #[serde(rename_all = "camelCase")]
    Connect {
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        private_key: Option<String>,
    },
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Disconnect,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum SshWsServerMessage {
    #[serde(rename_all = "camelCase")]
    Connected {
        host: String,
        user: String,
        port: u16,
        message: String,
        /// `SHA256:<base64>` fingerprint of the host key presented by the SSH server.
        host_key_fingerprint: Option<String>,
        /// `true` when the fingerprint matched an expected host key from the stack outputs;
        /// `false` means the key was accepted unverified and the UI should display it.
        host_key_verified: bool,
    },
    Output {
        data: String,
    },
    Error {
        message: String,
    },
    Disconnected,
}

async fn send_ws_json(socket: &mut WebSocket, msg: &SshWsServerMessage) -> Result<(), axum::Error> {
    if let Ok(json_str) = serde_json::to_string(msg) {
        socket.send(Message::Text(json_str.into())).await?;
    }
    Ok(())
}

/// Per-session security policy computed by the authenticated HTTP layer
/// (`routes::workloads::stack_ssh_ws`).
#[derive(Debug, Clone, Default)]
pub(crate) struct SshSessionPolicy {
    /// Raw expected host key(s) from stack outputs / GitOps config (`sshHostKeyFingerprint`,
    /// `sshHostKey`, ...): `SHA256:<base64>` fingerprints and/or OpenSSH public key lines,
    /// separated by newlines or commas. When present, the server key **must** match.
    pub expected_host_key: Option<String>,
    /// Whether the caller may supply an SSH target when the stack has no resolved endpoint.
    /// Granted only to tenant administrators; tenant members are restricted to stack-resolved
    /// targets.
    pub allow_unresolved_target_override: bool,
}

/// Final SSH connection target selected for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectTarget {
    pub host: String,
    pub port: u16,
    /// `true` when the target came from stack outputs / GitOps manifests (trusted: may live on
    /// private networks, e.g. Proxmox/kind VMs). `false` for client-supplied targets, which must
    /// resolve exclusively to public addresses.
    pub trusted: bool,
}

/// Selects the SSH target for a session.
///
/// Design: stack-resolved targets (Pulumi outputs / `values.yaml` / `stack.yaml`) are trusted,
/// because they are produced by the tenant's reviewed GitOps/PKO pipeline and commonly point at
/// RFC 1918 VMs. Client-supplied `host`/`port` values are therefore only accepted when they are
/// identical to the resolved target (the UI always echoes them back). Only if the stack has **no**
/// resolved endpoint may a tenant administrator supply one, and that host must not be a private,
/// loopback, link-local, CGNAT, ULA, or cluster-internal name/address (re-checked after DNS
/// resolution by [`pick_vetted_socket_addr`]).
pub(crate) fn select_connect_target(
    info: &StackSshInfoResponse,
    req_host: Option<&str>,
    req_port: Option<u16>,
    allow_unresolved_override: bool,
) -> Result<ConnectTarget, String> {
    let req_host = match req_host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => Some(clean_candidate_ssh_host(h).ok_or_else(|| "Invalid SSH host".to_string())?),
        None => None,
    };
    let req_port = req_port.filter(|p| *p > 0);

    if !is_unresolved_ssh_target(info) {
        if let Some(h) = req_host.as_deref()
            && !h.eq_ignore_ascii_case(&info.ssh_host)
        {
            return Err(
                "Refusing SSH host override: this stack's SSH target is pinned to its resolved endpoint"
                    .to_string(),
            );
        }
        if let Some(p) = req_port
            && p != info.ssh_port
        {
            return Err(
                "Refusing SSH port override: this stack's SSH target is pinned to its resolved endpoint"
                    .to_string(),
            );
        }
        return Ok(ConnectTarget {
            host: info.ssh_host.clone(),
            port: info.ssh_port,
            trusted: true,
        });
    }

    if !allow_unresolved_override {
        return Err(
            "SSH target for this stack could not be resolved from its outputs; only tenant administrators may supply a target host"
                .to_string(),
        );
    }
    let Some(host) = req_host else {
        return Err(
            "SSH target for this stack could not be resolved from its outputs; supply a public target host"
                .to_string(),
        );
    };
    if is_private_or_internal_ssh_host(&host) {
        return Err(
            "Refusing SSH host override targeting private, loopback, or cluster-internal address"
                .to_string(),
        );
    }
    Ok(ConnectTarget {
        host,
        port: req_port.unwrap_or(22),
        trusted: false,
    })
}

/// Picks the socket address to connect to from a DNS resolution result. For untrusted
/// (client-supplied) targets **every** resolved address must be public, which defeats DNS
/// rebinding: the vetted `SocketAddr` itself (not the hostname) is then passed to `connect`.
pub(crate) fn pick_vetted_socket_addr(
    addrs: &[SocketAddr],
    trusted: bool,
) -> Result<SocketAddr, String> {
    let Some(first) = addrs.first() else {
        return Err("SSH target hostname did not resolve to any address".to_string());
    };
    if !trusted && addrs.iter().any(|a| is_private_or_internal_ip(a.ip())) {
        return Err(
            "Refusing SSH target: hostname resolves to a private, loopback, or cluster-internal address"
                .to_string(),
        );
    }
    Ok(*first)
}

async fn resolve_vetted_socket_addr(target: &ConnectTarget) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((target.host.as_str(), target.port)),
    )
    .await
    .map_err(|_| format!("DNS resolution for SSH host '{}' timed out", target.host))?
    .map_err(|e| format!("DNS resolution for SSH host '{}' failed: {e}", target.host))?
    .collect();
    pick_vetted_socket_addr(&addrs, target.trusted)
}

/// Parses expected host key material into normalized `SHA256:<base64-nopad>` fingerprints.
///
/// Accepts a list separated by newlines or commas whose entries are either fingerprints
/// (`SHA256:...`) or OpenSSH public keys (`ssh-ed25519 AAAA... [comment]`, optionally prefixed by
/// a `known_hosts` host field). Any unparsable entry fails closed so a misconfigured pin never
/// silently degrades to unverified connections.
pub(crate) fn parse_expected_host_fingerprints(raw: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for entry in raw
        .split(['\n', ','])
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        if let Some(rest) = entry.strip_prefix("SHA256:") {
            let b64 = rest.trim().trim_end_matches('=');
            if b64.is_empty()
                || !b64
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
            {
                return Err("Invalid expected SSH host key fingerprint".to_string());
            }
            out.push(format!("SHA256:{b64}"));
            continue;
        }
        let without_host = entry
            .split_once(char::is_whitespace)
            .map(|(_, r)| r.trim())
            .unwrap_or("");
        let parsed = PublicKey::from_openssh(entry)
            .or_else(|_| PublicKey::from_openssh(without_host))
            .map_err(|_| {
                "Unsupported expected SSH host key format (expected `SHA256:<base64>` or an OpenSSH public key)"
                    .to_string()
            })?;
        out.push(parsed.fingerprint(HashAlg::Sha256).to_string());
    }
    if out.is_empty() {
        return Err("Expected SSH host key is empty".to_string());
    }
    Ok(out)
}

/// Computes the `SHA256:<base64>` fingerprint of the key presented by the SSH server.
fn server_key_fingerprint(key: &russh::keys::PublicKeyOrCertificate) -> String {
    match key {
        russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => {
            key.fingerprint(HashAlg::Sha256).to_string()
        }
        russh::keys::PublicKeyOrCertificate::Certificate(cert) => {
            cert.public_key().fingerprint(HashAlg::Sha256).to_string()
        }
    }
}

/// Returns `true` when `observed` is acceptable: no pin configured, or it matches a pinned key.
pub(crate) fn host_key_matches(expected: &[String], observed: &str) -> bool {
    expected.is_empty() || expected.iter().any(|e| e == observed)
}

struct SshClientHandler {
    expected_fingerprints: Vec<String>,
    observed_fingerprint: Arc<Mutex<Option<String>>>,
}

impl SshClientHandler {
    fn new(expected_fingerprints: Vec<String>) -> (Self, Arc<Mutex<Option<String>>>) {
        let observed = Arc::new(Mutex::new(None));
        (
            Self {
                expected_fingerprints,
                observed_fingerprint: observed.clone(),
            },
            observed,
        )
    }
}

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_key_fingerprint(server_public_key);
        let ok = host_key_matches(&self.expected_fingerprints, &fingerprint);
        if let Ok(mut slot) = self.observed_fingerprint.lock() {
            *slot = Some(fingerprint);
        }
        Ok(ok)
    }
}

/// Bridges an authenticated WebSocket client to a real `russh` SSH PTY shell session.
pub(crate) async fn handle_stack_ssh_ws(
    mut socket: WebSocket,
    ssh_info: StackSshInfoResponse,
    saved_ssh_key: Option<String>,
    user_subject: String,
    policy: SshSessionPolicy,
) {
    if !is_ssh_supported_infra(&ssh_info.provider, &ssh_info.stack_name) {
        let _ = send_ws_json(
            &mut socket,
            &SshWsServerMessage::Error {
                message: format!(
                    "SSH is not supported for '{}' infrastructure (provider: '{}')",
                    ssh_info.stack_name, ssh_info.provider
                ),
            },
        )
        .await;
        let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
        return;
    }

    let expected_fingerprints = match policy.expected_host_key.as_deref() {
        Some(raw) => match parse_expected_host_fingerprints(raw) {
            Ok(fps) => fps,
            Err(message) => {
                let _ = send_ws_json(&mut socket, &SshWsServerMessage::Error { message }).await;
                let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
                return;
            }
        },
        None => Vec::new(),
    };

    let mut active_user = ssh_info.ssh_user.clone();
    let active_port: u16;
    let active_host: String;
    let target_trusted: bool;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let ephemeral_key: Option<String>;

    // Wait for the initial Connect frame
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<SshWsClientMessage>(&text) {
                    Ok(SshWsClientMessage::Connect {
                        cols: req_cols,
                        rows: req_rows,
                        host,
                        user,
                        port,
                        private_key,
                    }) => {
                        match select_connect_target(
                            &ssh_info,
                            host.as_deref(),
                            port,
                            policy.allow_unresolved_target_override,
                        ) {
                            Ok(target) => {
                                active_host = target.host;
                                active_port = target.port;
                                target_trusted = target.trusted;
                            }
                            Err(message) => {
                                let _ = send_ws_json(
                                    &mut socket,
                                    &SshWsServerMessage::Error { message },
                                )
                                .await;
                                let _ =
                                    send_ws_json(&mut socket, &SshWsServerMessage::Disconnected)
                                        .await;
                                return;
                            }
                        }
                        if let Some(u) = user.as_deref() {
                            if let Some(cleaned) = clean_candidate_ssh_user(u) {
                                active_user = cleaned;
                            } else {
                                let _ = send_ws_json(
                                    &mut socket,
                                    &SshWsServerMessage::Error {
                                        message: "Invalid SSH username".to_string(),
                                    },
                                )
                                .await;
                                let _ =
                                    send_ws_json(&mut socket, &SshWsServerMessage::Disconnected)
                                        .await;
                                return;
                            }
                        }
                        if let Some(c) = req_cols.filter(|c| *c > 0) {
                            cols = c;
                        }
                        if let Some(r) = req_rows.filter(|r| *r > 0) {
                            rows = r;
                        }
                        ephemeral_key = private_key
                            .map(|k| k.trim().to_string())
                            .filter(|k| !k.is_empty());
                        break;
                    }
                    Ok(SshWsClientMessage::Disconnect) => {
                        let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
                        return;
                    }
                    Ok(_) => {
                        let _ = send_ws_json(
                            &mut socket,
                            &SshWsServerMessage::Error {
                                message: "Expected initial 'connect' frame".to_string(),
                            },
                        )
                        .await;
                    }
                    Err(err) => {
                        let _ = send_ws_json(
                            &mut socket,
                            &SshWsServerMessage::Error {
                                message: format!("Invalid WebSocket SSH frame: {err}"),
                            },
                        )
                        .await;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return,
            _ => {}
        }
    }

    let Some(key_pem) = ephemeral_key.or(saved_ssh_key) else {
        let _ = send_ws_json(
            &mut socket,
            &SshWsServerMessage::Error {
                message: "No SSH private key available. Provide an ephemeral private key or save one in your profile.".to_string(),
            },
        )
        .await;
        let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
        return;
    };

    let key_pair = match decode_secret_key(&key_pem, None) {
        Ok(k) => Arc::new(k),
        Err(err) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!("Failed to parse SSH private key: {err}"),
                },
            )
            .await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
    };

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let target = ConnectTarget {
        host: active_host.clone(),
        port: active_port,
        trusted: target_trusted,
    };
    let socket_addr = match resolve_vetted_socket_addr(&target).await {
        Ok(addr) => addr,
        Err(message) => {
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Error { message }).await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
    };

    let (handler, observed_fingerprint) = SshClientHandler::new(expected_fingerprints.clone());
    let mut session = match tokio::time::timeout(
        Duration::from_secs(15),
        client::connect(config, socket_addr, handler),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            let observed = observed_fingerprint.lock().ok().and_then(|g| g.clone());
            let message = match observed {
                Some(fp) if !host_key_matches(&expected_fingerprints, &fp) => {
                    tracing::warn!(
                        target: "keel::audit",
                        user = %user_subject,
                        tenant = %ssh_info.tenant,
                        stack = %ssh_info.stack_name,
                        ssh_host = %active_host,
                        presented = %fp,
                        "SSH host key verification failed"
                    );
                    format!(
                        "SSH host key verification failed for {}:{}: server presented {fp}, expected {}",
                        active_host,
                        active_port,
                        expected_fingerprints.join(", ")
                    )
                }
                _ => format!(
                    "SSH connection to {}@{}:{} failed: {err}",
                    active_user, active_host, active_port
                ),
            };
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Error { message }).await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
        Err(_) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!(
                        "SSH connection to {}@{}:{} timed out",
                        active_user, active_host, active_port
                    ),
                },
            )
            .await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
    };

    let best_hash = session
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    let auth_res = session
        .authenticate_publickey(
            &active_user,
            PrivateKeyWithHashAlg::new(key_pair, best_hash),
        )
        .await;

    match auth_res {
        Ok(res) if res.success() => {}
        Ok(_) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!(
                        "SSH public-key authentication rejected for {}@{}",
                        active_user, active_host
                    ),
                },
            )
            .await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
        Err(err) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!("SSH authentication error: {err}"),
                },
            )
            .await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
    }

    let mut channel = match session.channel_open_session().await {
        Ok(ch) => ch,
        Err(err) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!("Failed to open SSH session channel: {err}"),
                },
            )
            .await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
    };

    if let Err(err) = channel
        .request_pty(false, "dumb", cols as u32, rows as u32, 0, 0, &[])
        .await
    {
        let _ = send_ws_json(
            &mut socket,
            &SshWsServerMessage::Error {
                message: format!("Failed to allocate SSH PTY: {err}"),
            },
        )
        .await;
        let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
        return;
    }

    if let Err(err) = channel.request_shell(false).await {
        let _ = send_ws_json(
            &mut socket,
            &SshWsServerMessage::Error {
                message: format!("Failed to start remote shell: {err}"),
            },
        )
        .await;
        let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
        return;
    }

    let host_key_fingerprint = observed_fingerprint.lock().ok().and_then(|g| g.clone());
    let host_key_verified = !expected_fingerprints.is_empty() && host_key_fingerprint.is_some();
    tracing::info!(
        target: "keel::audit",
        user = %user_subject,
        tenant = %ssh_info.tenant,
        stack = %ssh_info.stack_name,
        ssh_host = %active_host,
        ssh_port = active_port,
        host_key_fingerprint = host_key_fingerprint.as_deref().unwrap_or("unknown"),
        host_key_verified,
        "WebSocket SSH session established"
    );
    let verification_note = match (&host_key_fingerprint, host_key_verified) {
        (Some(fp), true) => format!(" (host key {fp} verified)"),
        (Some(fp), false) => format!(
            " (host key {fp} NOT verified: no expected host key published in stack outputs)"
        ),
        (None, _) => String::new(),
    };
    let _ = send_ws_json(
        &mut socket,
        &SshWsServerMessage::Connected {
            host: active_host.clone(),
            user: active_user.clone(),
            port: active_port,
            message: format!(
                "Connected to {active_user}@{active_host}:{active_port}{verification_note}"
            ),
            host_key_fingerprint,
            host_key_verified,
        },
    )
    .await;

    // SSH channel chunks can split multi-byte UTF-8 sequences; decode stdout and stderr streams
    // incrementally so characters straddling a chunk boundary are not replaced with U+FFFD.
    let mut stdout_decoder = Utf8StreamDecoder::default();
    let mut stderr_decoder = Utf8StreamDecoder::default();
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        let text = stdout_decoder.decode(&data);
                        if text.is_empty() {
                            continue;
                        }
                        if send_ws_json(&mut socket, &SshWsServerMessage::Output { data: text }).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let text = stderr_decoder.decode(&data);
                        if text.is_empty() {
                            continue;
                        }
                        if send_ws_json(&mut socket, &SshWsServerMessage::Output { data: text }).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        let tail = format!("{}{}", stdout_decoder.finish(), stderr_decoder.finish());
                        if !tail.is_empty() {
                            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Output { data: tail }).await;
                        }
                        let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
                        break;
                    }
                    _ => {}
                }
            }
            ws_msg = socket.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<SshWsClientMessage>(&text) {
                            Ok(SshWsClientMessage::Input { data }) => {
                                if channel.data(data.as_bytes()).await.is_err() {
                                    let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
                                    break;
                                }
                            }
                            Ok(SshWsClientMessage::Resize { cols, rows }) => {
                                let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                            }
                            Ok(SshWsClientMessage::Disconnect) => {
                                tracing::info!(
                                    target: "keel::audit",
                                    user = %user_subject,
                                    tenant = %ssh_info.tenant,
                                    stack = %ssh_info.stack_name,
                                    "WebSocket SSH session disconnected by client"
                                );
                                let _ = channel.eof().await;
                                let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
                                break;
                            }
                            Ok(SshWsClientMessage::Connect { .. }) => {}
                            Err(err) => {
                                let _ = send_ws_json(
                                    &mut socket,
                                    &SshWsServerMessage::Error {
                                        message: format!("Invalid WebSocket SSH frame: {err}"),
                                    },
                                )
                                .await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = channel.eof().await;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
}

/// Incremental UTF-8 decoder for byte streams delivered in arbitrary chunks.
///
/// Incomplete trailing sequences (at most 3 bytes) are carried over to the next chunk; genuinely
/// invalid bytes are replaced with U+FFFD exactly like [`String::from_utf8_lossy`].
#[derive(Debug, Default)]
pub(crate) struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// Decodes `chunk` (prefixed with any carried-over bytes), returning all complete characters.
    pub(crate) fn decode(&mut self, chunk: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(chunk);
        let mut out = String::with_capacity(buf.len());
        let mut rest: &[u8] = &buf;
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    // `valid_up_to` guarantees this prefix is valid UTF-8.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match err.error_len() {
                        Some(len) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[len..];
                        }
                        None => {
                            // Truncated sequence at the end of the chunk: wait for more bytes.
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// Flushes any incomplete trailing bytes (lossily) at end of stream.
    pub(crate) fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&pending).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey};
    use russh::server::{self, Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use tokio::net::TcpListener;

    #[test]
    fn utf8_stream_decoder_carries_split_multibyte_chars_across_chunks() {
        let text = "héllo → 世界 🚀";
        let bytes = text.as_bytes();
        // Split at every possible boundary, including inside 2-, 3- and 4-byte sequences.
        for split in 0..=bytes.len() {
            let mut dec = Utf8StreamDecoder::default();
            let mut out = dec.decode(&bytes[..split]);
            out.push_str(&dec.decode(&bytes[split..]));
            out.push_str(&dec.finish());
            assert_eq!(out, text, "split at {split}");
        }
        // Byte-at-a-time delivery of a 4-byte emoji emits nothing until complete.
        let mut dec = Utf8StreamDecoder::default();
        let rocket = "🚀".as_bytes();
        assert_eq!(dec.decode(&rocket[..1]), "");
        assert_eq!(dec.decode(&rocket[1..2]), "");
        assert_eq!(dec.decode(&rocket[2..3]), "");
        assert_eq!(dec.decode(&rocket[3..]), "🚀");
        assert_eq!(dec.finish(), "");
    }

    #[test]
    fn utf8_stream_decoder_replaces_invalid_bytes_and_flushes_truncated_tail() {
        let mut dec = Utf8StreamDecoder::default();
        assert_eq!(dec.decode(b"a\xffb"), "a\u{FFFD}b");
        // Truncated 3-byte sequence is held, then flushed lossily at end of stream.
        assert_eq!(dec.decode(&[b'x', 0xE4, 0xB8]), "x");
        assert_eq!(dec.finish(), "\u{FFFD}");
        // Invalid continuation after a held lead byte is replaced, not swallowed.
        assert_eq!(dec.decode(&[0xE4]), "");
        assert_eq!(dec.decode(b"z"), "\u{FFFD}z");
    }

    #[test]
    fn ssh_supported_infra_gates_kind_and_non_vm_stacks() {
        assert!(is_ssh_supported_infra("proxmox", "pve-workload-prod"));
        assert!(is_ssh_supported_infra("aws", "aws-us-east-1-cluster"));
        assert!(is_ssh_supported_infra("gcp", "gcp-vm-01"));
        assert!(is_ssh_supported_infra("azure", "azure-vm-01"));
        assert!(is_ssh_supported_infra("hetzner", "hetzner-fsn1-nodes"));

        assert!(!is_ssh_supported_infra("kind", "management-cluster-core"));
        assert!(!is_ssh_supported_infra("kind", "e2e-kind-cluster"));
        assert!(!is_ssh_supported_infra("", "management-cluster-core"));
        assert!(!is_ssh_supported_infra("kubernetes", "postgres-managed"));
        assert!(!is_ssh_supported_infra("helm", "podinfo"));
        assert!(!is_ssh_supported_infra("manifest", "http-echo"));
        assert!(!is_ssh_supported_infra(
            "kustomize",
            "canary-gateway-config"
        ));
        assert!(!is_ssh_supported_infra("kubeconfig", "external-cluster"));
    }

    #[derive(Clone)]
    struct TestSshServer {
        expected_pubkey: russh::keys::PublicKey,
    }

    impl server::Server for TestSshServer {
        type Handler = TestSshServerHandler;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            TestSshServerHandler {
                expected_pubkey: self.expected_pubkey.clone(),
            }
        }
    }

    struct TestSshServerHandler {
        expected_pubkey: russh::keys::PublicKey,
    }

    impl server::Handler for TestSshServerHandler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            user: &str,
            public_key: russh::server::VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            if user == "ubuntu" && public_key.public_key == &self.expected_pubkey {
                Ok(Auth::Accept)
            } else {
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            handle: server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            handle.accept().await;
            Ok(())
        }

        async fn pty_request(
            &mut self,
            channel: ChannelId,
            _term: &str,
            _col_width: u32,
            _row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            session.data(channel, "ubuntu@test-vm:~$ ")?;
            Ok(())
        }

        async fn window_change_request(
            &mut self,
            _channel: ChannelId,
            _col_width: u32,
            _row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn data(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            let text = String::from_utf8_lossy(data);
            let reply = format!("exec:{}\n", text.trim());
            session.data(channel, reply)?;
            Ok(())
        }
    }

    struct MiniWsClient {
        stream: tokio::net::TcpStream,
    }

    impl MiniWsClient {
        async fn connect(addr: std::net::SocketAddr, path: &str) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                buf.push(byte[0]);
            }
            let hdr = String::from_utf8_lossy(&buf);
            assert!(
                hdr.starts_with("HTTP/1.1 101"),
                "unexpected ws handshake: {hdr}"
            );
            Self { stream }
        }

        async fn send_text(&mut self, text: &str) {
            use tokio::io::AsyncWriteExt;
            let payload = text.as_bytes();
            let mut frame = Vec::new();
            frame.push(0x81); // FIN + text
            let len = payload.len();
            if len < 126 {
                frame.push(0x80 | (len as u8));
            } else {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            let mask = [0x12, 0x34, 0x56, 0x78];
            frame.extend_from_slice(&mask);
            for (i, b) in payload.iter().enumerate() {
                frame.push(b ^ mask[i % 4]);
            }
            self.stream.write_all(&frame).await.unwrap();
        }

        async fn recv_text(&mut self) -> String {
            use tokio::io::AsyncReadExt;
            loop {
                let mut hdr = [0u8; 2];
                self.stream.read_exact(&mut hdr).await.unwrap();
                let opcode = hdr[0] & 0x0f;
                let masked = (hdr[1] & 0x80) != 0;
                let mut len = (hdr[1] & 0x7f) as usize;
                if len == 126 {
                    let mut ext = [0u8; 2];
                    self.stream.read_exact(&mut ext).await.unwrap();
                    len = u16::from_be_bytes(ext) as usize;
                } else if len == 127 {
                    let mut ext = [0u8; 8];
                    self.stream.read_exact(&mut ext).await.unwrap();
                    len = u64::from_be_bytes(ext) as usize;
                }
                let mask = if masked {
                    let mut m = [0u8; 4];
                    self.stream.read_exact(&mut m).await.unwrap();
                    Some(m)
                } else {
                    None
                };
                let mut payload = vec![0u8; len];
                self.stream.read_exact(&mut payload).await.unwrap();
                if let Some(m) = mask {
                    for (i, b) in payload.iter_mut().enumerate() {
                        *b ^= m[i % 4];
                    }
                }
                if opcode == 0x8 {
                    panic!("unexpected close frame");
                }
                if opcode == 0x1 {
                    return String::from_utf8(payload).unwrap();
                }
            }
        }
    }

    fn test_ssh_info(
        provider: &str,
        stack_name: &str,
        host: &str,
        port: u16,
    ) -> StackSshInfoResponse {
        StackSshInfoResponse {
            tenant: "default".to_string(),
            namespace: "tenant-default".to_string(),
            stack_name: stack_name.to_string(),
            provider: provider.to_string(),
            ssh_host: host.to_string(),
            ssh_user: "ubuntu".to_string(),
            ssh_port: port,
            has_saved_ssh_key: false,
            ssh_command: format!("ssh ubuntu@{host} -p {port}"),
            cli_command: format!("keel infra ssh {stack_name}"),
            cli_exec_command: format!("keel infra ssh {stack_name} --"),
        }
    }

    async fn spawn_ws_handler(
        info: StackSshInfoResponse,
        saved_key: Option<String>,
    ) -> std::net::SocketAddr {
        spawn_ws_handler_with_policy(info, saved_key, SshSessionPolicy::default()).await
    }

    async fn spawn_ws_handler_with_policy(
        info: StackSshInfoResponse,
        saved_key: Option<String>,
        policy: SshSessionPolicy,
    ) -> std::net::SocketAddr {
        use axum::Router;
        use axum::extract::ws::WebSocketUpgrade;
        use axum::routing::get;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let info = info.clone();
                let saved_key = saved_key.clone();
                let policy = policy.clone();
                async move {
                    ws.on_upgrade(move |socket| {
                        handle_stack_ssh_ws(socket, info, saved_key, "tester".to_string(), policy)
                    })
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_error_gates_and_full_pty_bridge() {
        // 1. Unsupported infra returns Error + Disconnected
        let addr = spawn_ws_handler(test_ssh_info("kind", "e2e-kind", "127.0.0.1", 22), None).await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        let msg1 = ws.recv_text().await;
        assert!(msg1.contains("SSH is not supported"));
        assert!(ws.recv_text().await.contains("disconnected"));

        // 2. Missing SSH key returns Error + Disconnected
        let addr =
            spawn_ws_handler(test_ssh_info("proxmox", "pve-vm", "127.0.0.1", 22), None).await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect","cols":80,"rows":24}"#)
            .await;
        assert!(
            ws.recv_text()
                .await
                .contains("No SSH private key available")
        );
        assert!(ws.recv_text().await.contains("disconnected"));

        // 3. Invalid host / user / PEM returns Error + Disconnected
        let addr = spawn_ws_handler(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", 22),
            Some("not-a-pem-key".to_string()),
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect","host":"bad host!!"}"#)
            .await;
        assert!(ws.recv_text().await.contains("Invalid SSH host"));
        assert!(ws.recv_text().await.contains("disconnected"));

        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect","host":"169.254.169.254"}"#)
            .await;
        assert!(ws.recv_text().await.contains("Refusing SSH host override"));
        assert!(ws.recv_text().await.contains("disconnected"));

        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect","user":"bad user!!"}"#)
            .await;
        assert!(ws.recv_text().await.contains("Invalid SSH username"));
        assert!(ws.recv_text().await.contains("disconnected"));

        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect"}"#).await;
        assert!(
            ws.recv_text()
                .await
                .contains("Failed to parse SSH private key")
        );
        assert!(ws.recv_text().await.contains("disconnected"));

        // 4. Full WebSocket-to-SSH PTY session against TestSshServer
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let client_pem = client_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap()
            .to_string();

        let server_cfg = Arc::new(server::Config {
            keys: vec![host_key],
            ..Default::default()
        });
        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_port = ssh_listener.local_addr().unwrap().port();
        let mut srv = TestSshServer {
            expected_pubkey: client_key.public_key().clone(),
        };
        tokio::spawn(async move {
            if let Ok((stream, addr)) = ssh_listener.accept().await {
                let handler = server::Server::new_client(&mut srv, Some(addr));
                let _ = server::run_stream(server_cfg, stream, handler).await;
            }
        });

        let addr = spawn_ws_handler(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", ssh_port),
            None,
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;

        // Invalid frame and non-connect frame before connect
        ws.send_text("not-json").await;
        assert!(ws.recv_text().await.contains("Invalid WebSocket SSH frame"));
        ws.send_text(r#"{"type":"input","data":"ls\n"}"#).await;
        assert!(
            ws.recv_text()
                .await
                .contains("Expected initial 'connect' frame")
        );

        // Connect with ephemeral private_key
        let connect_msg = serde_json::json!({
            "type": "connect",
            "cols": 100,
            "rows": 30,
            "host": "127.0.0.1",
            "user": "ubuntu",
            "port": ssh_port,
            "privateKey": client_pem
        })
        .to_string();
        ws.send_text(&connect_msg).await;

        let connected = ws.recv_text().await;
        assert!(connected.contains("connected"));
        assert!(connected.contains("\"hostKeyFingerprint\":\"SHA256:"));
        assert!(connected.contains("\"hostKeyVerified\":false"));
        let prompt = ws.recv_text().await;
        assert!(prompt.contains("ubuntu@test-vm:~$"));

        // Send Input, Resize, and Disconnect
        ws.send_text(r#"{"type":"input","data":"whoami\n"}"#).await;
        let out = ws.recv_text().await;
        assert!(out.contains("exec:whoami"));

        ws.send_text(r#"{"type":"resize","cols":120,"rows":40}"#)
            .await;
        ws.send_text(r#"{"type":"disconnect"}"#).await;
        let disc = ws.recv_text().await;
        assert!(disc.contains("disconnected"));
    }

    #[test]
    fn select_connect_target_pins_resolved_targets() {
        let info = test_ssh_info("proxmox", "pve-vm", "192.168.10.25", 22);
        // Resolved private target is trusted; echoing it back is fine.
        let t = select_connect_target(&info, Some("192.168.10.25"), Some(22), false).unwrap();
        assert_eq!(
            t,
            ConnectTarget {
                host: "192.168.10.25".to_string(),
                port: 22,
                trusted: true
            }
        );
        let t = select_connect_target(&info, None, None, false).unwrap();
        assert!(t.trusted);
        // Overrides that differ are refused, even for admins and even for public hosts.
        let err = select_connect_target(&info, Some("203.0.113.5"), None, true).unwrap_err();
        assert!(err.contains("Refusing SSH host override"));
        let err = select_connect_target(&info, None, Some(2222), true).unwrap_err();
        assert!(err.contains("Refusing SSH port override"));
        let err = select_connect_target(&info, Some("bad host!!"), None, true).unwrap_err();
        assert_eq!(err, "Invalid SSH host");
    }

    #[test]
    fn select_connect_target_unresolved_requires_admin_and_public_host() {
        let info = test_ssh_info("proxmox", "pve-vm", "pve-vm.default.local", 22);
        let err = select_connect_target(&info, Some("203.0.113.5"), None, false).unwrap_err();
        assert!(err.contains("only tenant administrators"));
        let err = select_connect_target(&info, None, None, true).unwrap_err();
        assert!(err.contains("supply a public target host"));
        for internal in [
            "10.0.0.5",
            "169.254.169.254",
            "127.0.0.1",
            "kubernetes.default.svc",
            "fd00::1",
            "100.64.1.1",
        ] {
            let err = select_connect_target(&info, Some(internal), None, true).unwrap_err();
            assert!(err.contains("Refusing SSH host override"), "{internal}");
        }
        let t = select_connect_target(&info, Some("203.0.113.5"), Some(2222), true).unwrap();
        assert_eq!(
            t,
            ConnectTarget {
                host: "203.0.113.5".to_string(),
                port: 2222,
                trusted: false
            }
        );
    }

    #[test]
    fn pick_vetted_socket_addr_blocks_rebinding_for_untrusted_targets() {
        let public: SocketAddr = "203.0.113.5:22".parse().unwrap();
        let private: SocketAddr = "10.0.0.5:22".parse().unwrap();
        let metadata: SocketAddr = "169.254.169.254:22".parse().unwrap();
        assert_eq!(pick_vetted_socket_addr(&[public], false).unwrap(), public);
        assert!(pick_vetted_socket_addr(&[public, private], false).is_err());
        assert!(pick_vetted_socket_addr(&[metadata], false).is_err());
        assert_eq!(pick_vetted_socket_addr(&[private], true).unwrap(), private);
        assert!(pick_vetted_socket_addr(&[], true).is_err());
    }

    #[test]
    fn parse_expected_host_fingerprints_accepts_fingerprints_and_public_keys() {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let pubkey = key.public_key().to_openssh().unwrap();
        let fp = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        assert!(fp.starts_with("SHA256:"));

        assert_eq!(
            parse_expected_host_fingerprints(&pubkey).unwrap(),
            vec![fp.clone()]
        );
        assert_eq!(
            parse_expected_host_fingerprints(&format!("vm.example.com {pubkey}")).unwrap(),
            vec![fp.clone()]
        );
        assert_eq!(
            parse_expected_host_fingerprints(&format!("{fp}=, SHA256:abc+/9\n")).unwrap(),
            vec![fp.clone(), "SHA256:abc+/9".to_string()]
        );
        assert!(parse_expected_host_fingerprints("SHA256:").is_err());
        assert!(parse_expected_host_fingerprints("SHA256:not valid!").is_err());
        assert!(parse_expected_host_fingerprints("garbage").is_err());
        assert!(parse_expected_host_fingerprints(" , ").is_err());

        assert!(host_key_matches(&[], &fp));
        assert!(host_key_matches(std::slice::from_ref(&fp), &fp));
        assert!(!host_key_matches(&["SHA256:other".to_string()], &fp));
    }

    async fn spawn_test_ssh_server(
        host_key: PrivateKey,
        client_pubkey: russh::keys::PublicKey,
    ) -> u16 {
        let server_cfg = Arc::new(server::Config {
            keys: vec![host_key],
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut srv = TestSshServer {
            expected_pubkey: client_pubkey,
        };
        tokio::spawn(async move {
            if let Ok((stream, addr)) = listener.accept().await {
                let handler = server::Server::new_client(&mut srv, Some(addr));
                let _ = server::run_stream(server_cfg, stream, handler).await;
            }
        });
        port
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_enforces_pinned_host_key() {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let host_fp = host_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        let other_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let other_pub = other_key.public_key().to_openssh().unwrap();
        let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let client_pem = client_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap()
            .to_string();

        // Mismatched pin -> refused with a host key verification error.
        let port = spawn_test_ssh_server(host_key.clone(), client_key.public_key().clone()).await;
        let addr = spawn_ws_handler_with_policy(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", port),
            Some(client_pem.clone()),
            SshSessionPolicy {
                expected_host_key: Some(other_pub),
                allow_unresolved_target_override: false,
            },
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect"}"#).await;
        let err = ws.recv_text().await;
        assert!(err.contains("host key verification failed"), "{err}");
        assert!(err.contains(&host_fp));
        assert!(ws.recv_text().await.contains("disconnected"));

        // Matching pin -> connected and reported as verified.
        let port = spawn_test_ssh_server(host_key, client_key.public_key().clone()).await;
        let addr = spawn_ws_handler_with_policy(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", port),
            Some(client_pem),
            SshSessionPolicy {
                expected_host_key: Some(host_fp.clone()),
                allow_unresolved_target_override: false,
            },
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect"}"#).await;
        let connected = ws.recv_text().await;
        assert!(
            connected.contains("\"hostKeyVerified\":true"),
            "{connected}"
        );
        assert!(connected.contains(&host_fp));

        // Invalid pin configuration fails closed before connecting.
        let addr = spawn_ws_handler_with_policy(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", 22),
            None,
            SshSessionPolicy {
                expected_host_key: Some("garbage".to_string()),
                allow_unresolved_target_override: false,
            },
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        assert!(
            ws.recv_text()
                .await
                .contains("Unsupported expected SSH host key format")
        );

        // Unresolved target: tenant members cannot supply their own host.
        let addr = spawn_ws_handler(
            test_ssh_info("proxmox", "pve-vm", "pve-vm.default.local", 22),
            None,
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect","host":"203.0.113.5"}"#)
            .await;
        assert!(ws.recv_text().await.contains("only tenant administrators"));
    }
}
