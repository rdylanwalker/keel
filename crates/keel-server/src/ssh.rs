//! Real `russh` WebSocket-to-SSH PTY bridge (`SDD.md` §7.3).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::StreamExt;
use keel_core::{
    AuthPhase, StackSshInfoResponse, clean_candidate_ssh_host, clean_candidate_ssh_user,
    is_forbidden_ssh_proxy_host, is_forbidden_ssh_proxy_ip, is_private_or_internal_ssh_host,
};
use keel_core::{is_private_or_internal_ip, is_unresolved_ssh_target};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, decode_secret_key};
use russh::{ChannelMsg, client};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// Largest WebSocket message (and frame) the SSH bridge accepts: 64 KiB. Bigger frames (an
/// oversized paste or a flood) fail the WebSocket read and end the session.
pub(crate) const SSH_WS_MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// How long a new browser terminal may take to send its initial `connect` frame.
pub(crate) const SSH_CONNECT_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// A session with no `input` or `resize` frame for this long is closed (terminal output does not
/// count, and SSH keepalives would otherwise keep an abandoned tab open forever).
pub(crate) const SSH_INPUT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Absolute cap on a browser SSH session: never longer than the `keel_session` lifetime.
pub(crate) const SSH_MAX_SESSION_DURATION: Duration =
    Duration::from_secs(crate::auth::SESSION_TTL_SECONDS as u64);
/// Concurrent browser SSH sessions allowed per user (OIDC `sub`) on one keel-server replica.
pub(crate) const SSH_MAX_SESSIONS_PER_USER: usize = 4;
/// How often an open session re-checks the server's auth phase.
pub(crate) const SSH_AUTH_PHASE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Bound on SSH setup after the handshake (authentication, channel open, PTY and shell).
const SSH_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Time limits of a browser SSH session. Production uses [`Default`] (the `SSH_*` constants);
/// tests shorten them.
#[derive(Debug, Clone)]
pub(crate) struct SshSessionLimits {
    /// [`SSH_CONNECT_FRAME_TIMEOUT`].
    pub connect_frame_timeout: Duration,
    /// [`SSH_INPUT_IDLE_TIMEOUT`].
    pub idle_timeout: Duration,
    /// [`SSH_MAX_SESSION_DURATION`].
    pub max_duration: Duration,
    /// [`SSH_AUTH_PHASE_POLL_INTERVAL`].
    pub phase_poll_interval: Duration,
}

impl Default for SshSessionLimits {
    fn default() -> Self {
        Self {
            connect_frame_timeout: SSH_CONNECT_FRAME_TIMEOUT,
            idle_timeout: SSH_INPUT_IDLE_TIMEOUT,
            max_duration: SSH_MAX_SESSION_DURATION,
            phase_poll_interval: SSH_AUTH_PHASE_POLL_INTERVAL,
        }
    }
}

/// The auth phase a session was issued under and the server's live phase. A change (e.g. leaving
/// break-glass) invalidates every session, so open SSH sessions close too.
#[derive(Debug, Clone)]
pub(crate) struct AuthPhaseWatch {
    pub issued: AuthPhase,
    pub current: Arc<tokio::sync::RwLock<AuthPhase>>,
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
    /// `exp` (Unix seconds) of the `keel_session` that opened the WebSocket: the SSH session
    /// closes then. Logout only clears cookies (sessions are stateless and cannot be revoked
    /// server-side), so this is also what bounds a session that outlives a logout.
    pub session_expires_at: Option<i64>,
    /// Closes the session when the server's auth phase changes.
    pub auth_phase: Option<AuthPhaseWatch>,
    pub limits: SshSessionLimits,
    /// Tests only: lets stack-resolved targets reach loopback, where the test SSH server listens.
    #[cfg(test)]
    pub allow_host_local_targets: bool,
}

impl SshSessionPolicy {
    fn host_local_targets_allowed(&self) -> bool {
        #[cfg(test)]
        {
            self.allow_host_local_targets
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// The instant the session must end and why: the earlier of the `keel_session` expiry and
    /// `started + limits.max_duration`.
    fn deadline(&self, started: Instant) -> (Instant, &'static str) {
        let cap = started + self.limits.max_duration;
        let Some(exp) = self.session_expires_at else {
            return (cap, MAX_DURATION_MESSAGE);
        };
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let remaining_ms = exp.saturating_mul(1000).saturating_sub(now_ms).max(0);
        let expiry = started + Duration::from_millis(remaining_ms.unsigned_abs());
        if expiry <= cap {
            (expiry, SESSION_EXPIRED_MESSAGE)
        } else {
            (cap, MAX_DURATION_MESSAGE)
        }
    }
}

const SESSION_EXPIRED_MESSAGE: &str =
    "SSH session closed: your Keel login session expired. Log in again to reconnect.";
const MAX_DURATION_MESSAGE: &str =
    "SSH session closed: it reached the maximum browser terminal duration. Reconnect to continue.";
const AUTH_PHASE_CHANGED_MESSAGE: &str =
    "SSH session closed: the Keel authentication mode changed. Log in again to reconnect.";

/// Counts open browser SSH sessions per user and caps them at `max_per_user`
/// ([`SSH_MAX_SESSIONS_PER_USER`]). In-memory and per replica, like the sessions themselves.
#[derive(Debug)]
pub(crate) struct SshSessionRegistry {
    max_per_user: usize,
    active: Mutex<HashMap<String, usize>>,
}

impl Default for SshSessionRegistry {
    fn default() -> Self {
        Self::new(SSH_MAX_SESSIONS_PER_USER)
    }
}

impl SshSessionRegistry {
    pub(crate) fn new(max_per_user: usize) -> Self {
        Self {
            max_per_user,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Reserves a session slot for `user`, or `None` when the user is at the limit. The slot is
    /// released when the returned guard drops (the session ends, or the upgrade never happens).
    pub(crate) fn try_acquire(self: &Arc<Self>, user: &str) -> Option<SshSessionSlot> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let count = active.entry(user.to_string()).or_insert(0);
        if *count >= self.max_per_user {
            return None;
        }
        *count += 1;
        Some(SshSessionSlot {
            registry: Arc::clone(self),
            user: user.to_string(),
        })
    }

    /// Open sessions of `user`.
    #[cfg(test)]
    pub(crate) fn active_sessions(&self, user: &str) -> usize {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.get(user).copied().unwrap_or(0)
    }
}

/// RAII slot from [`SshSessionRegistry::try_acquire`].
#[derive(Debug)]
pub(crate) struct SshSessionSlot {
    registry: Arc<SshSessionRegistry>,
    user: String,
}

impl Drop for SshSessionSlot {
    fn drop(&mut self) {
        let mut active = self
            .registry
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = active.get_mut(&self.user) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                active.remove(&self.user);
            }
        }
    }
}

/// Upgrades `GET /api/v1/stacks/{ns}/{name}/ssh/ws` to the SSH bridge: caps WebSocket messages
/// and frames at [`SSH_WS_MAX_MESSAGE_BYTES`] and takes one of the user's session slots. A user
/// at the limit still gets the upgrade, but only to receive an error frame and a close, because
/// browsers cannot read the body of a refused upgrade.
pub(crate) fn upgrade_ssh_ws(
    ws: WebSocketUpgrade,
    sessions: &Arc<SshSessionRegistry>,
    ssh_info: StackSshInfoResponse,
    saved_ssh_key: Option<String>,
    user_subject: String,
    policy: SshSessionPolicy,
) -> Response {
    let ws = ws
        .max_message_size(SSH_WS_MAX_MESSAGE_BYTES)
        .max_frame_size(SSH_WS_MAX_MESSAGE_BYTES);
    match sessions.try_acquire(&user_subject) {
        Some(slot) => ws.on_upgrade(move |socket| async move {
            handle_stack_ssh_ws(socket, ssh_info, saved_ssh_key, user_subject, policy).await;
            drop(slot);
        }),
        None => {
            let max = sessions.max_per_user;
            tracing::warn!(
                target: "keel::audit",
                user = %user_subject,
                tenant = %ssh_info.tenant,
                stack = %ssh_info.stack_name,
                max_sessions = max,
                "WebSocket SSH session refused: concurrent session limit reached"
            );
            ws.on_upgrade(move |mut socket| async move {
                close_with_error(
                    &mut socket,
                    format!(
                        "Too many open SSH sessions: at most {max} browser terminals per user. Close another terminal and try again."
                    ),
                )
                .await;
            })
        }
    }
}

/// Sends an error frame, `disconnected` and a WebSocket close.
async fn close_with_error(socket: &mut WebSocket, message: String) {
    let _ = send_ws_json(socket, &SshWsServerMessage::Error { message }).await;
    let _ = send_ws_json(socket, &SshWsServerMessage::Disconnected).await;
    let _ = socket.send(Message::Close(None)).await;
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

/// Picks the socket address to connect to from a DNS resolution result; **every** resolved
/// address is vetted, which defeats DNS rebinding (the vetted `SocketAddr` itself, not the
/// hostname, is then passed to `connect`). Untrusted (client-supplied) targets must be public
/// ([`is_private_or_internal_ip`]). Trusted (stack-resolved) targets may be private VMs but never
/// loopback, link-local/cloud metadata, multicast or unspecified ([`is_forbidden_ssh_proxy_ip`]),
/// so a forged or mistaken stack output cannot point the proxy at keel-server or its node.
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
    if trusted && addrs.iter().any(|a| is_forbidden_ssh_proxy_ip(a.ip())) {
        return Err(
            "Refusing SSH target: it resolves to a loopback, link-local (cloud metadata), multicast or unspecified address"
                .to_string(),
        );
    }
    Ok(*first)
}

async fn resolve_vetted_socket_addr(
    target: &ConnectTarget,
    host_local_allowed: bool,
) -> Result<SocketAddr, String> {
    if target.trusted && !host_local_allowed && is_forbidden_ssh_proxy_host(&target.host) {
        return Err(format!(
            "Refusing SSH target '{}': loopback, link-local (cloud metadata), cluster-internal (`*.svc`, `*.cluster.local`) and single-label hosts are never reachable through the browser terminal",
            target.host
        ));
    }
    let literal = target
        .host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .ok();
    let addrs: Vec<SocketAddr> = match literal {
        Some(ip) => vec![SocketAddr::new(ip, target.port)],
        None => {
            // Resolve as an absolute name, so the pod's cluster search domains never turn a stack
            // output into `<name>.<ns>.svc.cluster.local`.
            let absolute = format!("{}.", target.host.trim_end_matches('.'));
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((absolute.as_str(), target.port)),
            )
            .await
            .map_err(|_| format!("DNS resolution for SSH host '{}' timed out", target.host))?
            .map_err(|e| format!("DNS resolution for SSH host '{}' failed: {e}", target.host))?
            .collect()
        }
    };
    if target.trusted && host_local_allowed {
        return addrs
            .first()
            .copied()
            .ok_or_else(|| "SSH target hostname did not resolve to any address".to_string());
    }
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
///
/// The session ends, with an error frame and a WebSocket close, when the `keel_session` that
/// opened it expires or `limits.max_duration` passes, when the server's auth phase changes, when
/// no `input`/`resize` frame arrives for `limits.idle_timeout`, or when the client does not send
/// its `connect` frame within `limits.connect_frame_timeout`.
pub(crate) async fn handle_stack_ssh_ws(
    mut socket: WebSocket,
    ssh_info: StackSshInfoResponse,
    saved_ssh_key: Option<String>,
    user_subject: String,
    policy: SshSessionPolicy,
) {
    let (deadline, deadline_message) = policy.deadline(Instant::now());

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

    // Wait for the initial Connect frame, bounded by the connect timeout and the session deadline.
    let connect_deadline = deadline.min(Instant::now() + policy.limits.connect_frame_timeout);
    loop {
        let next = match tokio::time::timeout_at(connect_deadline, socket.next()).await {
            Ok(next) => next,
            Err(_) => {
                let message = if connect_deadline >= deadline {
                    deadline_message.to_string()
                } else {
                    "Timed out waiting for the SSH 'connect' frame".to_string()
                };
                close_with_error(&mut socket, message).await;
                return;
            }
        };
        match next {
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
            // Oversized (> SSH_WS_MAX_MESSAGE_BYTES) or malformed frames end the connection.
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
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
    let socket_addr =
        match resolve_vetted_socket_addr(&target, policy.host_local_targets_allowed()).await {
            Ok(addr) => addr,
            Err(message) => {
                tracing::warn!(
                    target: "keel::audit",
                    user = %user_subject,
                    tenant = %ssh_info.tenant,
                    stack = %ssh_info.stack_name,
                    ssh_host = %active_host,
                    trusted = target_trusted,
                    "WebSocket SSH target refused"
                );
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

    let mut channel = match tokio::time::timeout(
        SSH_SETUP_TIMEOUT,
        open_shell(
            &mut session,
            &active_user,
            &active_host,
            key_pair,
            cols,
            rows,
        ),
    )
    .await
    {
        Ok(Ok(channel)) => channel,
        Ok(Err(message)) => {
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Error { message }).await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
        Err(_) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!("SSH session setup for {active_user}@{active_host} timed out"),
                },
            )
            .await;
            let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
            return;
        }
    };

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
    let mut idle_deadline = Instant::now() + policy.limits.idle_timeout;
    let mut phase_check = tokio::time::interval(policy.limits.phase_poll_interval);
    phase_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let audit_close = |reason: &str| {
        tracing::info!(
            target: "keel::audit",
            user = %user_subject,
            tenant = %ssh_info.tenant,
            stack = %ssh_info.stack_name,
            reason,
            "WebSocket SSH session closed by server"
        );
    };
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
                                idle_deadline = Instant::now() + policy.limits.idle_timeout;
                                if channel.data(data.as_bytes()).await.is_err() {
                                    let _ = send_ws_json(&mut socket, &SshWsServerMessage::Disconnected).await;
                                    break;
                                }
                            }
                            Ok(SshWsClientMessage::Resize { cols, rows }) => {
                                idle_deadline = Instant::now() + policy.limits.idle_timeout;
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
                    // Oversized (> SSH_WS_MAX_MESSAGE_BYTES) or malformed frames end the session.
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        let _ = channel.eof().await;
                        break;
                    }
                    _ => {}
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                audit_close("deadline");
                let _ = channel.eof().await;
                close_with_error(&mut socket, deadline_message.to_string()).await;
                break;
            }
            () = tokio::time::sleep_until(idle_deadline) => {
                audit_close("idle");
                let _ = channel.eof().await;
                close_with_error(
                    &mut socket,
                    format!(
                        "SSH session closed after {} without input. Reconnect to continue.",
                        format_duration(policy.limits.idle_timeout)
                    ),
                )
                .await;
                break;
            }
            _ = phase_check.tick(), if policy.auth_phase.is_some() => {
                let changed = match &policy.auth_phase {
                    Some(watch) => *watch.current.read().await != watch.issued,
                    None => false,
                };
                if changed {
                    audit_close("auth-phase-changed");
                    let _ = channel.eof().await;
                    close_with_error(&mut socket, AUTH_PHASE_CHANGED_MESSAGE.to_string()).await;
                    break;
                }
            }
        }
    }

    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
}

/// Authenticates with `key_pair`, then opens a session channel with a PTY and a shell.
async fn open_shell(
    session: &mut client::Handle<SshClientHandler>,
    user: &str,
    host: &str,
    key_pair: Arc<russh::keys::PrivateKey>,
    cols: u16,
    rows: u16,
) -> Result<russh::Channel<client::Msg>, String> {
    let best_hash = session
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    match session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key_pair, best_hash))
        .await
    {
        Ok(res) if res.success() => {}
        Ok(_) => {
            return Err(format!(
                "SSH public-key authentication rejected for {user}@{host}"
            ));
        }
        Err(err) => return Err(format!("SSH authentication error: {err}")),
    }
    let channel = session
        .channel_open_session()
        .await
        .map_err(|err| format!("Failed to open SSH session channel: {err}"))?;
    channel
        .request_pty(false, "dumb", cols as u32, rows as u32, 0, 0, &[])
        .await
        .map_err(|err| format!("Failed to allocate SSH PTY: {err}"))?;
    channel
        .request_shell(false)
        .await
        .map_err(|err| format!("Failed to start remote shell: {err}"))?;
    Ok(channel)
}

/// `15 minutes`, `90 seconds`, ... for user-facing messages.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        let mins = secs / 60;
        format!("{mins} minute{}", if mins == 1 { "" } else { "s" })
    } else if secs >= 1 {
        format!("{secs} second{}", if secs == 1 { "" } else { "s" })
    } else {
        format!("{} ms", d.as_millis())
    }
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
            } else if len <= usize::from(u16::MAX) {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            } else {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
            let mask = [0x12, 0x34, 0x56, 0x78];
            frame.extend_from_slice(&mask);
            for (i, b) in payload.iter().enumerate() {
                frame.push(b ^ mask[i % 4]);
            }
            // The server may already have closed the connection (oversized frames).
            let _ = self.stream.write_all(&frame).await;
        }

        /// Next text frame, or `None` once the server sends a close frame or the stream ends.
        async fn recv_text_or_end(&mut self) -> Option<String> {
            use tokio::io::AsyncReadExt;
            loop {
                let mut hdr = [0u8; 2];
                self.stream.read_exact(&mut hdr).await.ok()?;
                let opcode = hdr[0] & 0x0f;
                let mut len = (hdr[1] & 0x7f) as usize;
                if len == 126 {
                    let mut ext = [0u8; 2];
                    self.stream.read_exact(&mut ext).await.ok()?;
                    len = u16::from_be_bytes(ext) as usize;
                } else if len == 127 {
                    let mut ext = [0u8; 8];
                    self.stream.read_exact(&mut ext).await.ok()?;
                    len = u64::from_be_bytes(ext) as usize;
                }
                let mut payload = vec![0u8; len];
                self.stream.read_exact(&mut payload).await.ok()?;
                match opcode {
                    0x8 => return None,
                    0x1 => return Some(String::from_utf8(payload).unwrap()),
                    _ => {}
                }
            }
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

    /// Default policy, except that stack-resolved targets may be the loopback test SSH server.
    fn loopback_policy() -> SshSessionPolicy {
        SshSessionPolicy {
            allow_host_local_targets: true,
            ..Default::default()
        }
    }

    async fn spawn_ws_handler(
        info: StackSshInfoResponse,
        saved_key: Option<String>,
    ) -> std::net::SocketAddr {
        spawn_ws_handler_with_policy(info, saved_key, loopback_policy()).await
    }

    async fn spawn_ws_handler_with_policy(
        info: StackSshInfoResponse,
        saved_key: Option<String>,
        policy: SshSessionPolicy,
    ) -> std::net::SocketAddr {
        spawn_ws_handler_with_registry(info, saved_key, policy, Arc::default()).await
    }

    /// Serves the SSH bridge at `/ws` exactly like `routes::workloads::stack_ssh_ws` does after
    /// authentication (message caps and per-user slots from `upgrade_ssh_ws`), as user `tester`.
    async fn spawn_ws_handler_with_registry(
        info: StackSshInfoResponse,
        saved_key: Option<String>,
        policy: SshSessionPolicy,
        sessions: Arc<SshSessionRegistry>,
    ) -> std::net::SocketAddr {
        use axum::Router;
        use axum::routing::get;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let info = info.clone();
                let saved_key = saved_key.clone();
                let policy = policy.clone();
                let sessions = sessions.clone();
                async move {
                    upgrade_ssh_ws(ws, &sessions, info, saved_key, "tester".to_string(), policy)
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
        // Trusted (stack-resolved) targets may be private VMs but never host-local addresses,
        // even when only one of the resolved addresses is.
        for forbidden in [
            "127.0.0.1:22",
            "169.254.169.254:22",
            "0.0.0.0:22",
            "[::1]:22",
            "[fd00:ec2::254]:22",
            "[fe80::1]:22",
            "[::ffff:169.254.169.254]:22",
        ] {
            let addr: SocketAddr = forbidden.parse().unwrap();
            let err = pick_vetted_socket_addr(&[addr], true).unwrap_err();
            assert!(err.contains("loopback, link-local"), "{forbidden}");
            assert!(pick_vetted_socket_addr(&[private, addr], true).is_err());
        }
        let cgnat: SocketAddr = "100.64.0.9:22".parse().unwrap();
        assert_eq!(pick_vetted_socket_addr(&[cgnat], true).unwrap(), cgnat);
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
                allow_host_local_targets: true,
                ..Default::default()
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
                allow_host_local_targets: true,
                ..Default::default()
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
                allow_host_local_targets: true,
                ..Default::default()
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

    fn client_keypair() -> (PrivateKey, String) {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let pem = key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap()
            .to_string();
        (key, pem)
    }

    /// Next text frame within 10 s (`None`: closed). Guards tests against hanging forever.
    async fn recv_within(ws: &mut MiniWsClient) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(10), ws.recv_text_or_end())
            .await
            .expect("no WebSocket frame within 10s")
    }

    /// Opens a browser SSH session against a fresh loopback test SSH server under `policy` and
    /// consumes the `connected` frame and the shell prompt.
    async fn connected_session(mut policy: SshSessionPolicy) -> MiniWsClient {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let (client_key, client_pem) = client_keypair();
        let port = spawn_test_ssh_server(host_key, client_key.public_key().clone()).await;
        policy.allow_host_local_targets = true;
        let addr = spawn_ws_handler_with_policy(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", port),
            Some(client_pem),
            policy,
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        ws.send_text(r#"{"type":"connect"}"#).await;
        let connected = recv_within(&mut ws).await.unwrap();
        assert!(connected.contains("\"type\":\"connected\""), "{connected}");
        let prompt = recv_within(&mut ws).await.unwrap();
        assert!(prompt.contains("ubuntu@test-vm:~$"), "{prompt}");
        ws
    }

    /// Reads frames until an `error` frame, returning its JSON; asserts `disconnected` and a close
    /// follow it.
    async fn expect_closed_with_error(ws: &mut MiniWsClient) -> String {
        loop {
            let frame = recv_within(ws).await.expect("closed before an error frame");
            if frame.contains("\"type\":\"error\"") {
                assert!(
                    recv_within(ws)
                        .await
                        .unwrap()
                        .contains("\"type\":\"disconnected\"")
                );
                assert_eq!(
                    recv_within(ws).await,
                    None,
                    "expected a close after {frame}"
                );
                return frame;
            }
        }
    }

    fn unix_now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn limits(f: impl FnOnce(&mut SshSessionLimits)) -> SshSessionLimits {
        let mut l = SshSessionLimits::default();
        f(&mut l);
        l
    }

    #[test]
    fn session_limits_default_to_bounded_constants() {
        let l = SshSessionLimits::default();
        assert_eq!(l.connect_frame_timeout, Duration::from_secs(30));
        assert_eq!(l.idle_timeout, Duration::from_secs(15 * 60));
        assert!(
            l.max_duration <= Duration::from_secs(crate::auth::SESSION_TTL_SECONDS as u64),
            "a browser SSH session must never outlive the login session TTL"
        );
        assert_eq!(SSH_WS_MAX_MESSAGE_BYTES, 64 * 1024);
        assert_eq!(SSH_MAX_SESSIONS_PER_USER, 4);
    }

    #[test]
    fn session_deadline_is_the_earlier_of_login_expiry_and_max_duration() {
        let started = Instant::now();
        let policy = SshSessionPolicy {
            session_expires_at: Some(unix_now() + 120),
            ..Default::default()
        };
        let (at, why) = policy.deadline(started);
        assert_eq!(why, SESSION_EXPIRED_MESSAGE);
        assert!(at <= started + Duration::from_secs(120));
        assert!(at >= started + Duration::from_secs(118));

        let policy = SshSessionPolicy {
            session_expires_at: Some(unix_now() + 7200),
            ..Default::default()
        };
        let (at, why) = policy.deadline(started);
        assert_eq!(why, MAX_DURATION_MESSAGE);
        assert_eq!(at, started + SSH_MAX_SESSION_DURATION);

        // Already expired: the deadline is now.
        let policy = SshSessionPolicy {
            session_expires_at: Some(unix_now() - 10),
            ..Default::default()
        };
        assert_eq!(policy.deadline(started), (started, SESSION_EXPIRED_MESSAGE));
    }

    #[test]
    fn session_registry_caps_sessions_per_user_and_releases_on_drop() {
        let registry = Arc::new(SshSessionRegistry::new(2));
        let a1 = registry.try_acquire("alice").unwrap();
        let _a2 = registry.try_acquire("alice").unwrap();
        assert!(registry.try_acquire("alice").is_none());
        // Other users have their own budget.
        let b1 = registry.try_acquire("bob").unwrap();
        assert_eq!(registry.active_sessions("alice"), 2);
        drop(a1);
        assert_eq!(registry.active_sessions("alice"), 1);
        let _a3 = registry.try_acquire("alice").unwrap();
        drop(b1);
        assert_eq!(registry.active_sessions("bob"), 0);
        assert!(registry.active.lock().unwrap().get("bob").is_none());
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_refuses_trusted_host_local_targets() {
        let (_, client_pem) = client_keypair();
        for host in [
            "127.0.0.1",
            "169.254.169.254",
            "kubernetes.default.svc",
            "keel-server.keel-system.svc.cluster.local",
            "localhost",
        ] {
            // Stack-resolved (trusted) target, production policy.
            let addr = spawn_ws_handler_with_policy(
                test_ssh_info("proxmox", "pve-vm", host, 22),
                Some(client_pem.clone()),
                SshSessionPolicy::default(),
            )
            .await;
            let mut ws = MiniWsClient::connect(addr, "/ws").await;
            ws.send_text(r#"{"type":"connect"}"#).await;
            let err = recv_within(&mut ws).await.unwrap();
            assert!(err.contains("Refusing SSH target"), "{host}: {err}");
            assert!(recv_within(&mut ws).await.unwrap().contains("disconnected"));
        }
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_times_out_waiting_for_connect_frame() {
        let addr = spawn_ws_handler_with_policy(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", 22),
            None,
            SshSessionPolicy {
                limits: limits(|l| l.connect_frame_timeout = Duration::from_millis(150)),
                ..loopback_policy()
            },
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        let err = expect_closed_with_error(&mut ws).await;
        assert!(
            err.contains("Timed out waiting for the SSH 'connect' frame"),
            "{err}"
        );

        // A login session that is already expired never gets to connect either.
        let addr = spawn_ws_handler_with_policy(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", 22),
            None,
            SshSessionPolicy {
                session_expires_at: Some(unix_now() - 1),
                ..loopback_policy()
            },
        )
        .await;
        let mut ws = MiniWsClient::connect(addr, "/ws").await;
        let err = expect_closed_with_error(&mut ws).await;
        assert!(err.contains("login session expired"), "{err}");
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_closes_when_login_session_expires() {
        let mut ws = connected_session(SshSessionPolicy {
            // Whole seconds: at least one second left after connecting.
            session_expires_at: Some(unix_now() + 2),
            ..Default::default()
        })
        .await;
        let err = expect_closed_with_error(&mut ws).await;
        assert!(err.contains("login session expired"), "{err}");
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_enforces_max_duration() {
        let mut ws = connected_session(SshSessionPolicy {
            session_expires_at: Some(unix_now() + 3600),
            limits: limits(|l| l.max_duration = Duration::from_millis(400)),
            ..Default::default()
        })
        .await;
        // Typing does not extend the absolute cap.
        ws.send_text(r#"{"type":"input","data":"ls\n"}"#).await;
        let err = expect_closed_with_error(&mut ws).await;
        assert!(err.contains("maximum browser terminal duration"), "{err}");
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_closes_on_auth_phase_change() {
        let current = Arc::new(tokio::sync::RwLock::new(AuthPhase::BreakGlass));
        let mut ws = connected_session(SshSessionPolicy {
            auth_phase: Some(AuthPhaseWatch {
                issued: AuthPhase::BreakGlass,
                current: current.clone(),
            }),
            limits: limits(|l| l.phase_poll_interval = Duration::from_millis(20)),
            ..Default::default()
        })
        .await;
        // Unchanged phase: the session keeps working.
        ws.send_text(r#"{"type":"input","data":"whoami\n"}"#).await;
        assert!(recv_within(&mut ws).await.unwrap().contains("exec:whoami"));

        *current.write().await = AuthPhase::OidcActive;
        let err = expect_closed_with_error(&mut ws).await;
        assert!(err.contains("authentication mode changed"), "{err}");
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_closes_idle_sessions_and_input_resets_the_timer() {
        let idle = Duration::from_millis(600);
        let mut ws = connected_session(SshSessionPolicy {
            limits: limits(|l| l.idle_timeout = idle),
            ..Default::default()
        })
        .await;
        // Keep typing for longer than the idle timeout: the session stays open.
        for i in 0..4 {
            tokio::time::sleep(idle / 3).await;
            ws.send_text(&format!(r#"{{"type":"input","data":"cmd{i}\n"}}"#))
                .await;
            assert!(
                recv_within(&mut ws)
                    .await
                    .unwrap()
                    .contains(&format!("exec:cmd{i}"))
            );
        }
        let last_input = std::time::Instant::now();
        let err = expect_closed_with_error(&mut ws).await;
        assert!(err.contains("without input"), "{err}");
        assert!(last_input.elapsed() >= idle / 2, "closed too early");
    }

    #[tokio::test]
    async fn handle_stack_ssh_ws_rejects_oversized_frames() {
        let mut ws = connected_session(SshSessionPolicy::default()).await;
        let big = "x".repeat(SSH_WS_MAX_MESSAGE_BYTES + 1024);
        ws.send_text(&format!(r#"{{"type":"input","data":"{big}"}}"#))
            .await;
        // The frame is never forwarded to the SSH server; the session ends instead.
        while let Some(frame) = recv_within(&mut ws).await {
            assert!(!frame.contains("exec:"), "oversized input reached SSH");
        }
    }

    #[tokio::test]
    async fn upgrade_ssh_ws_limits_concurrent_sessions_per_user() {
        let sessions: Arc<SshSessionRegistry> = Arc::default();
        let addr = spawn_ws_handler_with_registry(
            test_ssh_info("proxmox", "pve-vm", "127.0.0.1", 22),
            None,
            loopback_policy(),
            sessions.clone(),
        )
        .await;
        // Four terminals waiting for their `connect` frame hold the user's four slots.
        let mut open = Vec::new();
        for _ in 0..SSH_MAX_SESSIONS_PER_USER {
            open.push(MiniWsClient::connect(addr, "/ws").await);
        }
        let mut fifth = MiniWsClient::connect(addr, "/ws").await;
        let err = expect_closed_with_error(&mut fifth).await;
        assert!(err.contains("Too many open SSH sessions"), "{err}");
        assert!(err.contains("at most 4"), "{err}");
        assert_eq!(
            sessions.active_sessions("tester"),
            SSH_MAX_SESSIONS_PER_USER
        );

        // Closing a terminal frees its slot.
        drop(open.pop());
        tokio::time::timeout(Duration::from_secs(10), async {
            while sessions.active_sessions("tester") >= SSH_MAX_SESSIONS_PER_USER {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("slot was not released");
        let mut again = MiniWsClient::connect(addr, "/ws").await;
        again.send_text(r#"{"type":"connect"}"#).await;
        assert!(
            recv_within(&mut again)
                .await
                .unwrap()
                .contains("No SSH private key available")
        );
    }
}
