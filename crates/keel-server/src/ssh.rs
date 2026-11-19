//! Real `russh` WebSocket-to-SSH PTY bridge (`SDD.md` §7.3).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use keel_core::StackSshInfoResponse;
use russh::keys::{PrivateKeyWithHashAlg, decode_secret_key};
use russh::{ChannelMsg, client};
use serde::{Deserialize, Serialize};

use crate::routes::{clean_candidate_ssh_host, clean_candidate_ssh_user};

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
pub fn is_ssh_supported_infra(provider: &str, stack_name: &str) -> bool {
    let p = provider.trim().to_ascii_lowercase();
    if NON_SSH_PROVIDERS.contains(&p.as_str()) {
        return false;
    }
    let name = stack_name.trim().to_ascii_lowercase();
    if name == "management-cluster-core" && p.is_empty() {
        return false;
    }
    if matches!(name.as_str(), "postgres-managed" | "canary-gateway-config") {
        return false;
    }
    true
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
    Connected {
        host: String,
        user: String,
        port: u16,
        message: String,
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

struct SshClientHandler;

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Bridges an authenticated WebSocket client to a real `russh` SSH PTY shell session.
pub(crate) async fn handle_stack_ssh_ws(
    mut socket: WebSocket,
    ssh_info: StackSshInfoResponse,
    saved_ssh_key: Option<String>,
    user_subject: String,
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

    let mut active_user = ssh_info.ssh_user.clone();
    let mut active_port = ssh_info.ssh_port;
    let mut active_host = ssh_info.ssh_host.clone();
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
                        if let Some(h) = host.as_deref() {
                            if let Some(cleaned) = clean_candidate_ssh_host(h) {
                                active_host = cleaned;
                            } else {
                                let _ = send_ws_json(
                                    &mut socket,
                                    &SshWsServerMessage::Error {
                                        message: "Invalid SSH host".to_string(),
                                    },
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
                        if let Some(p) = port.filter(|p| *p > 0) {
                            active_port = p;
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

    let mut session = match tokio::time::timeout(
        Duration::from_secs(15),
        client::connect(
            config,
            (active_host.as_str(), active_port),
            SshClientHandler,
        ),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            let _ = send_ws_json(
                &mut socket,
                &SshWsServerMessage::Error {
                    message: format!(
                        "SSH connection to {}@{}:{} failed: {err}",
                        active_user, active_host, active_port
                    ),
                },
            )
            .await;
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

    let _ = send_ws_json(
        &mut socket,
        &SshWsServerMessage::Connected {
            host: active_host.clone(),
            user: active_user.clone(),
            port: active_port,
            message: format!("Connected to {active_user}@{active_host}:{active_port}"),
        },
    )
    .await;

    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        let text = String::from_utf8_lossy(&data).into_owned();
                        if send_ws_json(&mut socket, &SshWsServerMessage::Output { data: text }).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let text = String::from_utf8_lossy(&data).into_owned();
                        if send_ws_json(&mut socket, &SshWsServerMessage::Output { data: text }).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
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

/// Establishes a real `russh` SSH connection, authenticates with the given PEM private key,
/// requests a PTY, and opens an interactive shell channel.
#[cfg(test)]
async fn connect_and_open_pty_shell(
    host: &str,
    port: u16,
    user: &str,
    key_pem: &str,
    cols: u32,
    rows: u32,
) -> Result<
    (
        client::Handle<SshClientHandler>,
        russh::Channel<client::Msg>,
    ),
    String,
> {
    let key_pair = Arc::new(
        decode_secret_key(key_pem, None)
            .map_err(|e| format!("Failed to parse SSH private key: {e}"))?,
    );
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let mut session = client::connect(config, (host, port), SshClientHandler)
        .await
        .map_err(|e| format!("SSH connect failed: {e}"))?;
    let best_hash = session
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    let auth_res = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key_pair, best_hash))
        .await
        .map_err(|e| format!("SSH auth error: {e}"))?;
    if !auth_res.success() {
        return Err("SSH public-key authentication rejected".to_string());
    }
    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| format!("Failed to open session channel: {e}"))?;
    channel
        .request_pty(false, "dumb", cols, rows, 0, 0, &[])
        .await
        .map_err(|e| format!("Failed to request PTY: {e}"))?;
    channel
        .request_shell(false)
        .await
        .map_err(|e| format!("Failed to request shell: {e}"))?;
    Ok((session, channel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey};
    use russh::server::{self, Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use tokio::net::TcpListener;

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

    #[tokio::test]
    async fn russh_pty_session_authenticates_and_streams_arbitrary_commands() {
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut srv = TestSshServer {
            expected_pubkey: client_key.public_key().clone(),
        };
        tokio::spawn(async move {
            if let Ok((stream, addr)) = listener.accept().await {
                let handler = server::Server::new_client(&mut srv, Some(addr));
                let _ = server::run_stream(server_cfg, stream, handler).await;
            }
        });

        let (session, mut channel) =
            connect_and_open_pty_shell("127.0.0.1", port, "ubuntu", &client_pem, 80, 24)
                .await
                .expect("connect and open PTY shell");

        // Expect initial prompt from shell_request
        let first = channel.wait().await.expect("initial prompt");
        match first {
            ChannelMsg::Data { data } => {
                assert!(String::from_utf8_lossy(&data).contains("ubuntu@test-vm:~$"));
            }
            other => panic!("unexpected channel msg: {other:?}"),
        }

        // Send arbitrary commands that previously failed in the simulated shell
        for cmd in [
            "touch /tmp/a\n",
            "cat /etc/hosts\n",
            "ip addr\n",
            "which sh\n",
        ] {
            channel.data(cmd.as_bytes()).await.unwrap();
            let msg = channel.wait().await.expect("command output");
            match msg {
                ChannelMsg::Data { data } => {
                    let out = String::from_utf8_lossy(&data);
                    assert!(out.contains(&format!("exec:{}", cmd.trim())));
                }
                other => panic!("unexpected channel msg: {other:?}"),
            }
        }

        channel.window_change(120, 40, 0, 0).await.unwrap();
        channel.eof().await.unwrap();
        session
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await
            .unwrap();
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
                async move {
                    ws.on_upgrade(move |socket| {
                        handle_stack_ssh_ws(socket, info, saved_key, "tester".to_string())
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
}
