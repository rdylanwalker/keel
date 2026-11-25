//! `kubectl` helpers used by Stage 0 bootstrap (explicit argv, never a shell — `NFR-SEC-06`).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use keel_core::KEEL_SYSTEM_NAMESPACE;
use reqwest::Client;
use tokio::io::AsyncWriteExt;

use crate::commands::CliError;

/// Target cluster coordinates for `kubectl` invocations (`--kubeconfig` / `--context`).
/// Verb for [`KubeTarget::write_secret`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretWrite {
    /// `kubectl create` (create-if-absent).
    Create,
    /// `kubectl replace` (overwrite an existing object).
    Replace,
}

impl SecretWrite {
    /// The `kubectl` subcommand.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Replace => "replace",
        }
    }
}

/// Whether `kubectl` stderr reports an `AlreadyExists` API error.
pub(crate) fn is_already_exists(stderr: &str) -> bool {
    stderr.contains("AlreadyExists") || stderr.contains("already exists")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KubeTarget<'a> {
    pub context: Option<&'a str>,
    pub kubeconfig: Option<&'a Path>,
}

impl<'a> KubeTarget<'a> {
    pub(crate) fn new(context: Option<&'a str>, kubeconfig: Option<&'a Path>) -> Self {
        Self {
            context,
            kubeconfig,
        }
    }

    /// Builds a `kubectl` command pre-populated with `--kubeconfig`/`--context`.
    pub(crate) fn cmd(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("kubectl");
        if let Some(kc) = self.kubeconfig {
            cmd.arg("--kubeconfig").arg(kc);
        }
        if let Some(ctx) = self.context {
            cmd.arg("--context").arg(ctx);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Runs `kubectl <args>` and returns stdout on success.
    pub(crate) async fn output(&self, args: &[&str]) -> Result<String, CliError> {
        let out = self.cmd().args(args).output().await.map_err(|e| {
            CliError::Execution(format!(
                "failed to invoke `kubectl` (is it installed and in PATH?): {e}"
            ))
        })?;
        if !out.status.success() {
            let verb = args
                .iter()
                .find(|a| !a.starts_with('-') && **a != KEEL_SYSTEM_NAMESPACE)
                .copied()
                .unwrap_or("command");
            return Err(CliError::Execution(format!(
                "`kubectl {verb}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs `kubectl <args>` feeding `stdin_data` on stdin (used for secrets so they never reach argv).
    pub(crate) async fn output_with_stdin(
        &self,
        args: &[&str],
        stdin_data: &[u8],
    ) -> Result<std::process::Output, CliError> {
        let mut cmd = self.cmd();
        cmd.args(args).stdin(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            CliError::Execution(format!(
                "failed to invoke `kubectl` (is it installed and in PATH?): {e}"
            ))
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_data).await?;
            stdin.shutdown().await?;
        }
        Ok(child.wait_with_output().await?)
    }

    /// `kubectl apply -f -` with the manifest supplied on stdin.
    pub(crate) async fn apply(&self, manifest_yaml: &str) -> Result<(), CliError> {
        self.apply_as(manifest_yaml, None).await
    }

    /// `kubectl apply -f -` (manifest on stdin), impersonating `as_user` (`--as`) when set.
    pub(crate) async fn apply_as(
        &self,
        manifest_yaml: &str,
        as_user: Option<&str>,
    ) -> Result<(), CliError> {
        let as_arg = as_user.map(|u| format!("--as={u}"));
        let mut args = vec!["apply", "-f", "-"];
        if let Some(arg) = as_arg.as_deref() {
            args.push(arg);
        }
        let output = self
            .output_with_stdin(&args, manifest_yaml.as_bytes())
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CliError::Execution(format!(
                "`kubectl apply` failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Writes a manifest holding secret material without `kubectl apply`, which would copy the
    /// whole payload into the `kubectl.kubernetes.io/last-applied-configuration` annotation:
    /// `kubectl create` (create-if-absent: returns `false` when the object already exists) or
    /// `kubectl replace` (`replace = true`), impersonating `as_user` when set.
    pub(crate) async fn write_secret_manifest(
        &self,
        manifest_yaml: &str,
        replace: bool,
        as_user: Option<&str>,
    ) -> Result<bool, CliError> {
        let verb = if replace {
            SecretWrite::Replace
        } else {
            SecretWrite::Create
        };
        Ok(self
            .write_secret(manifest_yaml.as_bytes(), verb, None, as_user, false)
            .await?
            .is_some())
    }

    /// `kubectl [-n <namespace>] create|replace -f - [-o json] [--as=<user>]` with the secret
    /// manifest on stdin only (never argv, never `apply`). Returns the command's stdout (the stored
    /// object when `output_json`), or `None` when `create` hit an existing object.
    pub(crate) async fn write_secret(
        &self,
        manifest: &[u8],
        verb: SecretWrite,
        namespace: Option<&str>,
        as_user: Option<&str>,
        output_json: bool,
    ) -> Result<Option<Vec<u8>>, CliError> {
        let as_arg = as_user.map(|u| format!("--as={u}"));
        let verb_str = verb.as_str();
        let mut args = Vec::new();
        if let Some(ns) = namespace {
            args.extend(["-n", ns]);
        }
        args.extend([verb_str, "-f", "-"]);
        if output_json {
            args.extend(["-o", "json"]);
        }
        if let Some(arg) = as_arg.as_deref() {
            args.push(arg);
        }
        let output = self.output_with_stdin(&args, manifest).await?;
        if output.status.success() {
            return Ok(Some(output.stdout));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if verb == SecretWrite::Create && is_already_exists(&stderr) {
            return Ok(None);
        }
        Err(CliError::Execution(format!(
            "`kubectl {verb_str}` failed: {}",
            stderr.trim()
        )))
    }

    /// Reads and base64-decodes `data.<key>` of `namespace/secret_name`, or `None`.
    pub(crate) async fn read_secret_key(
        &self,
        namespace: &str,
        secret_name: &str,
        key: &str,
    ) -> Option<String> {
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return None;
        }
        let tmpl = format!("go-template={{{{index .data \"{key}\" | base64decode}}}}");
        let out = self
            .cmd()
            .args(["-n", namespace, "get", "secret", secret_name, "-o", &tmpl])
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if val.is_empty() || val == "<no value>" {
            None
        } else {
            Some(val)
        }
    }
}

/// RAII guard for a background `kubectl port-forward` child process.
pub(crate) struct TempPortForward {
    pub(crate) local_port: u16,
    child: tokio::process::Child,
}

impl Drop for TempPortForward {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Which host port a `kubectl port-forward` listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalPort {
    /// Prefer this port, falling back (with a warning) to a random free port when it is taken.
    Preferred(u16),
    /// Exactly this port (e.g. `8888`, baked into the OIDC issuer URL); taken means an error.
    Required(u16),
}

/// Explains a busy host port: usually an orphaned `kubectl port-forward` from an earlier
/// `keel serve`/`keel bootstrap` that was killed without cleaning up its children.
pub(crate) fn port_in_use_hint(port: u16) -> String {
    format!(
        "host port 127.0.0.1:{port} is already in use, most likely by an orphaned `kubectl \
         port-forward` left behind by an earlier `keel serve` (check with `ss -ltnp \
         'sport = :{port}'` or `lsof -i :{port}`; stop it with `pkill -f \
         'kubectl.*port-forward.*{port}:'`)"
    )
}

fn pick_local_port(port: LocalPort) -> Result<u16, CliError> {
    let preferred = match port {
        LocalPort::Preferred(p) | LocalPort::Required(p) => p,
    };
    let listener = match std::net::TcpListener::bind(("127.0.0.1", preferred)) {
        Ok(listener) => listener,
        Err(_) if matches!(port, LocalPort::Required(_)) => {
            return Err(CliError::Execution(port_in_use_hint(preferred)));
        }
        Err(_) => {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            tracing::warn!(
                fallback_port = listener.local_addr()?.port(),
                "{}; using a random free port instead",
                port_in_use_hint(preferred)
            );
            listener
        }
    };
    Ok(listener.local_addr()?.port())
}

/// Spawns `kubectl -n <ns> port-forward <resource> <local>:<remote>` and waits until the local
/// TCP port accepts connections.
pub(crate) async fn start_port_forward(
    target: KubeTarget<'_>,
    namespace: &str,
    resource: &str,
    local_port: LocalPort,
    remote_port: u16,
) -> Result<TempPortForward, CliError> {
    let local_port = pick_local_port(local_port)?;
    let mut cmd = target.cmd();
    cmd.args([
        "-n",
        namespace,
        "port-forward",
        resource,
        &format!("{local_port}:{remote_port}"),
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| CliError::Execution(format!("failed to spawn kubectl port-forward: {e}")))?;
    let pf = TempPortForward { local_port, child };
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .is_ok()
        {
            return Ok(pf);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(CliError::Execution(format!(
        "timed out waiting for port-forward to {namespace}/{resource} on 127.0.0.1:{local_port}"
    )))
}

pub(crate) async fn start_forgejo_port_forward(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<TempPortForward, CliError> {
    let pf = start_port_forward(
        KubeTarget::new(context, kubeconfig),
        KEEL_SYSTEM_NAMESPACE,
        "svc/forgejo-http",
        LocalPort::Preferred(3000),
        3000,
    )
    .await?;
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| CliError::Execution(e.to_string()))?;
    let health_url = format!("http://127.0.0.1:{}/api/v1/version", pf.local_port);
    for _ in 0..40 {
        if let Ok(resp) = client.get(&health_url).send().await
            && resp.status().is_success()
        {
            return Ok(pf);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(CliError::Execution(format!(
        "timed out waiting for Forgejo API on 127.0.0.1:{}",
        pf.local_port
    )))
}

/// Resolves the Envoy Gateway proxy Service generated for the `keel-ingress` Gateway.
pub(crate) async fn find_gateway_service(target: KubeTarget<'_>) -> Option<String> {
    let out = target
        .output(&[
            "-n",
            KEEL_SYSTEM_NAMESPACE,
            "get",
            "svc",
            "-l",
            "gateway.envoyproxy.io/owning-gateway-name=keel-ingress",
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])
        .await
        .ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then_some(name)
}

pub(crate) async fn start_gateway_port_forward(
    context: Option<&str>,
    kubeconfig: Option<&Path>,
) -> Result<TempPortForward, CliError> {
    let target = KubeTarget::new(context, kubeconfig);
    let svc_name = find_gateway_service(target).await.ok_or_else(|| {
        CliError::Execution(
            "Envoy Gateway service for keel-ingress not found in keel-system".to_string(),
        )
    })?;
    start_port_forward(
        target,
        KEEL_SYSTEM_NAMESPACE,
        &format!("svc/{svc_name}"),
        // The OIDC issuer (`https://idp.keel.local:8888/...`) names this port.
        LocalPort::Required(8888),
        8888,
    )
    .await
}

#[cfg(test)]
mod local_port_tests {
    use super::*;

    /// A taken required port (8888, named by the OIDC issuer) is reported with the orphaned
    /// port-forward hint instead of silently moving to a random port; a taken preferred port
    /// falls back to a free one.
    #[test]
    fn busy_ports_are_reported_or_replaced() {
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();

        let err = pick_local_port(LocalPort::Required(port))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("127.0.0.1:{port} is already in use")),
            "{err}"
        );
        assert!(err.contains("orphaned `kubectl port-forward`"), "{err}");
        assert!(err.contains(&format!("port-forward.*{port}:")), "{err}");

        let fallback = pick_local_port(LocalPort::Preferred(port)).unwrap();
        assert_ne!(fallback, port);

        drop(busy);
        // A child forked by another test thread while `busy` was open keeps the listening socket
        // until it `exec`s (O_CLOEXEC), so the port can stay bound for a moment after the drop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reacquired = loop {
            match pick_local_port(LocalPort::Required(port)) {
                Ok(p) => break p,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("port {port} still busy after drop: {err}"),
            }
        };
        assert_eq!(reacquired, port);
    }
}

#[cfg(test)]
mod already_exists_tests {
    use super::*;

    #[test]
    fn detects_already_exists_errors() {
        assert!(is_already_exists(
            "Error from server (AlreadyExists): secrets \"s\" already exists"
        ));
        assert!(!is_already_exists("Error from server (Forbidden): nope"));
        assert_eq!(SecretWrite::Create.as_str(), "create");
        assert_eq!(SecretWrite::Replace.as_str(), "replace");
    }
}
