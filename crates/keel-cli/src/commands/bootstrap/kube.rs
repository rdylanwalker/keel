//! `kubectl` helpers used by Stage 0 bootstrap (explicit argv, never a shell — `NFR-SEC-06`).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use keel_core::KEEL_SYSTEM_NAMESPACE;
use reqwest::Client;
use tokio::io::AsyncWriteExt;

use crate::commands::CliError;

/// Target cluster coordinates for `kubectl` invocations (`--kubeconfig` / `--context`).
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

fn pick_local_port(preferred: u16) -> Result<u16, CliError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", preferred))
        .or_else(|_| std::net::TcpListener::bind("127.0.0.1:0"))?;
    Ok(listener.local_addr()?.port())
}

/// Spawns `kubectl -n <ns> port-forward <resource> <local>:<remote>` and waits until the local
/// TCP port accepts connections.
pub(crate) async fn start_port_forward(
    target: KubeTarget<'_>,
    namespace: &str,
    resource: &str,
    preferred_local_port: u16,
    remote_port: u16,
) -> Result<TempPortForward, CliError> {
    let local_port = pick_local_port(preferred_local_port)?;
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
        3000,
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
        8888,
        8888,
    )
    .await
}
