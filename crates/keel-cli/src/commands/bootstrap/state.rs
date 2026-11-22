//! Durable Stage 0 Pulumi state: per-cluster local backend under the user config dir, generated
//! passphrase (local `0600` file + `keel-system` Secret), escrow to the in-cluster S3 bucket
//! (`s3://pulumi-state/stage0/<cluster>`, versioned) (`NFR-REL-01`).
//!
//! Every `pulumi` invocation uses explicit argv; backend URLs, S3 credentials and passphrases are
//! passed through environment variables and secret config values through stdin — never argv
//! (`NFR-SEC-05`, `NFR-SEC-06`). The user's global `pulumi login` is never modified.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use keel_core::{
    KEEL_SYSTEM_NAMESPACE, STAGE0_BACKEND_DIR, STAGE0_PASSPHRASE_FILE, Stage0Passphrase,
    Stage0PassphraseSource, ensure_private_dir, resolve_stage0_passphrase_in,
    stage0_cluster_dir_in, stage0_program_dir_in, stage0_state_root, write_private_file,
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::kube::KubeTarget;
use super::s3::{
    DEFAULT_S3_REGION, S3Client, S3Credentials, S3Endpoint, ensure_seaweedfs_pulumi_bucket,
    read_seaweedfs_admin_credentials, s3_backend_url, start_seaweedfs_s3_port_forward,
};
use crate::commands::CliError;

/// Secret (in `keel-system`) holding the Stage 0 Pulumi passphrase.
pub const STAGE0_PASSPHRASE_SECRET_NAME: &str = "keel-stage0-pulumi-passphrase";

/// Embedded Stage 0 control-plane programs (`deploy/bootstrap/<program>/Pulumi.yaml`).
pub(crate) const CONTROL_PLANE_PULUMI_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/control-plane/Pulumi.yaml");
pub(crate) const CONTROL_PLANE_FORGEJO_PULUMI_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/control-plane-forgejo/Pulumi.yaml");
pub(crate) const CONTROL_PLANE_RAUTHY_PULUMI_YAML: &str =
    include_str!("../../../../../deploy/bootstrap/control-plane-rauthy/Pulumi.yaml");

/// Returns the embedded Pulumi program for a Stage 0 program name, if Keel ships one.
pub(crate) fn embedded_program_yaml(program: &str) -> Option<String> {
    match program {
        "control-plane" => Some(CONTROL_PLANE_PULUMI_YAML.to_string()),
        "control-plane-forgejo" => Some(CONTROL_PLANE_FORGEJO_PULUMI_YAML.to_string()),
        "control-plane-rauthy" => Some(CONTROL_PLANE_RAUTHY_PULUMI_YAML.to_string()),
        other => other
            .parse::<keel_core::BootstrapTargetKind>()
            .ok()
            .and_then(|kind| {
                keel_core::PulumiBootstrapTarget::new(kind)
                    .embedded_pulumi_yaml()
                    .map(str::to_string)
            }),
    }
}

/// Stage 0 stack name for a program and cluster (`stage0-<program>-<cluster>`).
pub(crate) fn stage0_stack_name(program: &str, cluster_name: &str) -> String {
    format!("stage0-{program}-{cluster_name}")
}

/// Inverse of [`stage0_stack_name`].
pub(crate) fn program_from_stack_name(stack: &str, cluster_name: &str) -> Option<String> {
    stack
        .strip_prefix("stage0-")?
        .strip_suffix(&format!("-{cluster_name}"))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// S3 key prefix of a cluster's Stage 0 escrow.
pub(crate) fn stage0_s3_prefix(cluster_name: &str) -> String {
    format!("stage0/{cluster_name}")
}

/// Per-cluster durable Stage 0 context (paths + resolved passphrase).
pub(crate) struct Stage0Context {
    pub cluster_name: String,
    pub cluster_dir: PathBuf,
    pub backend_dir: PathBuf,
    pub passphrase: Stage0Passphrase,
    /// `true` when this run created a new passphrase that must be escrowed.
    pub passphrase_newly_created: bool,
}

impl Stage0Context {
    /// Creates the per-cluster Stage 0 directory and resolves the passphrase.
    /// `passphrase_preexisted` must be sampled before any Stage 0 Pulumi run in this invocation.
    pub(crate) fn prepare(
        cluster_name: &str,
        passphrase_preexisted: bool,
    ) -> Result<Self, CliError> {
        Self::prepare_in(&stage0_state_root(), cluster_name, passphrase_preexisted)
    }

    pub(crate) fn prepare_in(
        root: &Path,
        cluster_name: &str,
        passphrase_preexisted: bool,
    ) -> Result<Self, CliError> {
        let cluster_dir = stage0_cluster_dir_in(root, cluster_name);
        ensure_private_dir(&cluster_dir)?;
        let passphrase = resolve_stage0_passphrase_in(root, cluster_name)?;
        let passphrase_newly_created = passphrase.was_generated()
            || (!passphrase_preexisted
                && matches!(passphrase.source, Stage0PassphraseSource::LocalFile(_)));
        Ok(Self {
            cluster_name: cluster_name.to_string(),
            backend_dir: cluster_dir.join(STAGE0_BACKEND_DIR),
            cluster_dir,
            passphrase,
            passphrase_newly_created,
        })
    }

    /// Path of the local passphrase file (whether or not it exists).
    pub(crate) fn passphrase_file(&self) -> PathBuf {
        self.cluster_dir.join(STAGE0_PASSPHRASE_FILE)
    }

    pub(crate) fn local_backend_url(&self) -> String {
        format!("file://{}", self.backend_dir.display())
    }

    /// Materializes `<cluster_dir>/programs/<program>/Pulumi.yaml` from the repository checkout
    /// (when run from it) or the embedded copy.
    pub(crate) async fn materialize_program(
        &self,
        program: &str,
        embedded_yaml: &str,
    ) -> Result<PathBuf, CliError> {
        let program_dir = stage0_program_dir_in(&self.cluster_dir, program);
        ensure_private_dir(&program_dir)?;
        let rel = PathBuf::from("deploy").join("bootstrap").join(program);
        let candidates = [
            rel.join("Pulumi.yaml"),
            PathBuf::from("..").join(&rel).join("Pulumi.yaml"),
        ];
        let mut yaml = embedded_yaml.to_string();
        for candidate in candidates {
            if candidate.is_file() {
                yaml = tokio::fs::read_to_string(&candidate).await?;
                break;
            }
        }
        tokio::fs::write(program_dir.join("Pulumi.yaml"), yaml).await?;
        Ok(program_dir)
    }

    /// One-time operator instruction to escrow a newly generated passphrase.
    pub(crate) fn escrow_notice(&self) -> Option<String> {
        if !self.passphrase_newly_created {
            return None;
        }
        Some(format!(
            "IMPORTANT: a new Stage 0 Pulumi passphrase was generated for cluster '{}'.\n\
             \x20 Local copy (0600): {}\n\
             \x20 In-cluster copy:   {KEEL_SYSTEM_NAMESPACE}/{STAGE0_PASSPHRASE_SECRET_NAME}\n\
             \x20 Escrow it NOW in your password manager / vault: `keel bootstrap recover` cannot\n\
             \x20 decrypt the S3 state backup without it if this workstation and the cluster are lost.\n\
             \x20 (This notice is shown only once.)",
            self.cluster_name,
            self.passphrase_file().display()
        ))
    }
}

/// Thin `pulumi` runner with explicit env (backend URL, passphrase, optional S3 credentials).
pub(crate) struct PulumiRunner {
    pub program_dir: PathBuf,
    pub backend_url: String,
    passphrase: zeroize::Zeroizing<String>,
    extra_env: Vec<(&'static str, String)>,
}

impl PulumiRunner {
    pub(crate) fn new(
        program_dir: impl Into<PathBuf>,
        backend_url: impl Into<String>,
        passphrase: &str,
    ) -> Self {
        Self {
            program_dir: program_dir.into(),
            backend_url: backend_url.into(),
            passphrase: zeroize::Zeroizing::new(passphrase.to_string()),
            extra_env: Vec::new(),
        }
    }

    pub(crate) fn with_s3_credentials(mut self, creds: &S3Credentials) -> Self {
        self.extra_env.extend(creds.env_pairs());
        self
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("pulumi");
        cmd.args(args)
            .current_dir(&self.program_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PULUMI_SKIP_UPDATE_CHECK", "true")
            .env("PULUMI_BACKEND_URL", &self.backend_url)
            .env("PULUMI_CONFIG_PASSPHRASE", self.passphrase.as_str())
            .env_remove("PULUMI_CONFIG_PASSPHRASE_FILE");
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd
    }

    fn failure(args: &[&str], output: &std::process::Output) -> CliError {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        let sub: Vec<&str> = args.iter().take(2).copied().collect();
        CliError::Execution(format!("`pulumi {}` failed: {detail}", sub.join(" ")))
    }

    /// Runs `pulumi <args>` with stdin closed and returns stdout.
    pub(crate) async fn run(&self, args: &[&str]) -> Result<String, CliError> {
        let output = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| {
                CliError::Execution(format!(
                    "failed to invoke `pulumi` (is it installed and in PATH?): {e}"
                ))
            })?;
        if !output.status.success() {
            return Err(Self::failure(args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `pulumi <args>` piping `stdin_data` (used for secret config values).
    pub(crate) async fn run_with_stdin(
        &self,
        args: &[&str],
        stdin_data: &[u8],
    ) -> Result<String, CliError> {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| {
                CliError::Execution(format!(
                    "failed to invoke `pulumi` (is it installed and in PATH?): {e}"
                ))
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_data).await?;
            stdin.shutdown().await?;
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(Self::failure(args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `pulumi stack select <stack> [--create]`.
    pub(crate) async fn select_stack(&self, stack: &str, create: bool) -> Result<(), CliError> {
        let mut args = vec!["stack", "select", "--non-interactive"];
        if create {
            args.push("--create");
        }
        args.push(stack);
        self.run(&args).await.map(|_| ())
    }

    /// Sets plaintext config via argv (non-secret values only) and secret config via stdin.
    pub(crate) async fn set_config(
        &self,
        stack: &str,
        pairs: &[(&str, &str, bool)],
    ) -> Result<(), CliError> {
        let plain: Vec<String> = pairs
            .iter()
            .filter(|(_, _, secret)| !secret)
            .flat_map(|(k, v, _)| ["--plaintext".to_string(), format!("{k}={v}")])
            .collect();
        if !plain.is_empty() {
            let mut args: Vec<&str> = vec!["config", "set-all", "--stack", stack];
            args.extend(plain.iter().map(String::as_str));
            self.run(&args).await?;
        }
        for (key, value, _) in pairs.iter().filter(|(_, _, secret)| *secret) {
            self.run_with_stdin(
                &[
                    "config",
                    "set",
                    "--secret",
                    "--non-interactive",
                    "--stack",
                    stack,
                    key,
                ],
                value.as_bytes(),
            )
            .await?;
        }
        Ok(())
    }

    /// `pulumi stack export --stack <stack> --file <file>` (file restricted to `0600`).
    pub(crate) async fn export(&self, stack: &str, file: &Path) -> Result<(), CliError> {
        if let Some(parent) = file.parent() {
            ensure_private_dir(parent)?;
        }
        // Pre-create with 0600 so the export never exists world-readable.
        write_private_file(file, b"")?;
        let file_str = file.to_string_lossy().to_string();
        self.run(&["stack", "export", "--stack", stack, "--file", &file_str])
            .await
            .map(|_| ())
    }

    /// Ensures `stack` exists (init with passphrase provider if missing) and imports `file`.
    pub(crate) async fn init_and_import(&self, stack: &str, file: &Path) -> Result<(), CliError> {
        if let Err(err) = self
            .run(&[
                "stack",
                "init",
                "--non-interactive",
                "--secrets-provider",
                "passphrase",
                stack,
            ])
            .await
            && !err.to_string().contains("already exists")
        {
            return Err(err);
        }
        let file_str = file.to_string_lossy().to_string();
        self.run(&[
            "stack",
            "import",
            "--non-interactive",
            "--stack",
            stack,
            "--file",
            &file_str,
        ])
        .await
        .map(|_| ())
    }
}

/// Runs a Stage 0 control-plane Pulumi program against the durable local backend.
pub(crate) async fn run_pulumi_control_plane_stack(
    ctx: &Stage0Context,
    program_name: &str,
    embedded_yaml: &str,
    config_pairs: &[(&str, &str, bool)],
) -> Result<(), CliError> {
    ensure_private_dir(&ctx.backend_dir)?;
    let program_dir = ctx.materialize_program(program_name, embedded_yaml).await?;
    let stack = stage0_stack_name(program_name, &ctx.cluster_name);
    let runner = PulumiRunner::new(
        program_dir,
        ctx.local_backend_url(),
        ctx.passphrase.expose(),
    );
    runner.select_stack(&stack, true).await?;
    runner.set_config(&stack, config_pairs).await?;
    runner
        .run(&["up", "--yes", "--non-interactive", "--stack", &stack])
        .await?;
    Ok(())
}

/// Stores the passphrase in `keel-system/keel-stage0-pulumi-passphrase` (stdin apply).
pub(crate) async fn store_passphrase_secret(
    target: KubeTarget<'_>,
    ctx: &Stage0Context,
) -> Result<(), CliError> {
    let doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": STAGE0_PASSPHRASE_SECRET_NAME,
            "namespace": KEEL_SYSTEM_NAMESPACE,
            "labels": {
                "app.kubernetes.io/managed-by": "keel",
                "keel.io/secret-kind": "stage0-pulumi-passphrase",
                "keel.io/cluster": ctx.cluster_name,
            },
        },
        "type": "Opaque",
        "stringData": { "passphrase": ctx.passphrase.expose() },
    });
    target.apply(&serde_yml::to_string(&doc)?).await
}

/// Ensures the Pulumi state passphrase Secret (`<namespace>/keel-pulumi-passphrase`, key
/// `passphrase`) PKO `Stack` workspaces in `namespace` source `PULUMI_CONFIG_PASSPHRASE` from.
///
/// An existing passphrase is always reused (never rotated). Otherwise a fresh 256-bit URL-safe
/// passphrase is created with `kubectl create` (stdin, never argv), which never overwrites a
/// concurrently created or pre-existing Secret. Returns `true` when a new passphrase was created.
pub(crate) async fn ensure_pulumi_passphrase_secret(
    target: KubeTarget<'_>,
    namespace: &str,
) -> Result<bool, CliError> {
    let name = keel_kube::PULUMI_PASSPHRASE_SECRET_NAME;
    let key = keel_kube::PULUMI_PASSPHRASE_SECRET_KEY;
    if read_pulumi_passphrase(target, namespace).await.is_some() {
        return Ok(false);
    }
    let passphrase = zeroize::Zeroizing::new(keel_kube::generate_pulumi_passphrase());
    let secret = keel_kube::build_pulumi_passphrase_secret(namespace, &passphrase);
    let manifest = zeroize::Zeroizing::new(serde_json::to_vec(&secret)?);
    let output = target
        .output_with_stdin(&["create", "-f", "-"], &manifest)
        .await?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("AlreadyExists") || stderr.contains("already exists") {
        if read_pulumi_passphrase(target, namespace).await.is_some() {
            return Ok(false);
        }
        return Err(CliError::Execution(format!(
            "`{namespace}/{name}` exists but has no usable `{key}`; restore the original \
             passphrase (it encrypts existing Pulumi state) instead of replacing it"
        )));
    }
    Err(CliError::Execution(format!(
        "failed to create `{namespace}/{name}`: {}",
        stderr.trim()
    )))
}

/// Reads the Pulumi state passphrase from `<namespace>/keel-pulumi-passphrase` (non-empty only).
pub(crate) async fn read_pulumi_passphrase(
    target: KubeTarget<'_>,
    namespace: &str,
) -> Option<zeroize::Zeroizing<String>> {
    target
        .read_secret_key(
            namespace,
            keel_kube::PULUMI_PASSPHRASE_SECRET_NAME,
            keel_kube::PULUMI_PASSPHRASE_SECRET_KEY,
        )
        .await
        .filter(|p| !p.is_empty())
        .map(zeroize::Zeroizing::new)
}

/// Lists Stage 0 programs with local state for this cluster (in a stable order).
pub(crate) async fn local_stage0_programs(ctx: &Stage0Context) -> Vec<String> {
    let mut programs = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(ctx.cluster_dir.join("programs")).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                let stack = stage0_stack_name(name, &ctx.cluster_name);
                if entry.path().join(format!("Pulumi.{stack}.yaml")).is_file() {
                    programs.push(name.to_string());
                }
            }
        }
    }
    programs.sort();
    programs
}

/// Result of escrowing Stage 0 state to S3.
#[derive(Debug, Default, Clone)]
pub(crate) struct Stage0SyncReport {
    pub backend_url: String,
    pub stacks: Vec<String>,
    pub versioning_enabled: bool,
    pub snapshots: Vec<String>,
}

/// Escrows every local Stage 0 stack to `s3://pulumi-state/stage0/<cluster>` through a temporary
/// port-forward to SeaweedFS: enables bucket versioning (falling back to timestamped snapshots),
/// imports each stack into the S3 backend, and uploads the encrypted stack config files and
/// program sources needed by `keel bootstrap recover`.
pub(crate) async fn sync_stage0_state_to_s3(
    target: KubeTarget<'_>,
    ctx: &Stage0Context,
) -> Result<Stage0SyncReport, CliError> {
    let bucket = crate::cli::DEFAULT_S3_BUCKET;
    ensure_seaweedfs_pulumi_bucket(target).await?;
    let creds = read_seaweedfs_admin_credentials(target).await?;
    let pf = start_seaweedfs_s3_port_forward(target).await?;
    let endpoint = S3Endpoint::parse(
        &format!("http://127.0.0.1:{}", pf.local_port),
        "S3 endpoint",
    )?;
    let client = S3Client::new(endpoint.clone(), DEFAULT_S3_REGION, creds.clone())?;

    let versioning_enabled = match client.enable_bucket_versioning(bucket).await {
        Ok(()) => client
            .bucket_versioning_enabled(bucket)
            .await
            .unwrap_or(false),
        Err(err) => {
            tracing::warn!(error = %err, "S3 bucket versioning unavailable; falling back to timestamped Stage 0 snapshots");
            false
        }
    };

    let prefix = stage0_s3_prefix(&ctx.cluster_name);
    let s3_url = s3_backend_url(bucket, Some(&prefix), &endpoint, DEFAULT_S3_REGION)?;
    let mut report = Stage0SyncReport {
        backend_url: s3_url.clone(),
        versioning_enabled,
        ..Default::default()
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let programs = local_stage0_programs(ctx).await;
    let mut manifest_stacks = Vec::new();
    for program in &programs {
        let stack = stage0_stack_name(program, &ctx.cluster_name);
        let program_dir = stage0_program_dir_in(&ctx.cluster_dir, program);
        let local = PulumiRunner::new(
            &program_dir,
            ctx.local_backend_url(),
            ctx.passphrase.expose(),
        );
        let export_file = ctx
            .cluster_dir
            .join("exports")
            .join(format!("{stack}.json"));
        local.export(&stack, &export_file).await?;

        let remote = PulumiRunner::new(&program_dir, s3_url.clone(), ctx.passphrase.expose())
            .with_s3_credentials(&creds);
        remote.init_and_import(&stack, &export_file).await?;

        for file in ["Pulumi.yaml".to_string(), format!("Pulumi.{stack}.yaml")] {
            let data = tokio::fs::read(program_dir.join(&file)).await?;
            client
                .put_object(bucket, &format!("{prefix}/config/{program}/{file}"), data)
                .await?;
        }
        if !versioning_enabled {
            let key = format!("{prefix}/snapshots/{stack}/{ts}.json");
            client
                .put_object(bucket, &key, tokio::fs::read(&export_file).await?)
                .await?;
            report.snapshots.push(key);
        }
        manifest_stacks.push(json!({ "program": program, "stack": stack }));
        report.stacks.push(stack);
    }
    let manifest = json!({
        "cluster": ctx.cluster_name,
        "stacks": manifest_stacks,
        "updatedAt": ts,
        "versioning": versioning_enabled,
        "keelVersion": env!("CARGO_PKG_VERSION"),
    });
    client
        .put_object(
            bucket,
            &format!("{prefix}/manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )
        .await?;
    drop(pf);
    Ok(report)
}

/// Converts a DIY checkpoint file (`.pulumi/stacks/<project>/<stack>.json`) into the
/// `pulumi stack import` format.
pub(crate) fn checkpoint_to_export(raw: &[u8]) -> Result<Vec<u8>, CliError> {
    let parsed: Value = serde_json::from_slice(raw)?;
    if parsed.get("deployment").is_some() {
        return Ok(raw.to_vec());
    }
    let latest = parsed
        .pointer("/checkpoint/latest")
        .cloned()
        .ok_or_else(|| {
            CliError::Execution(
                "state object is not a Pulumi checkpoint (missing checkpoint.latest)".to_string(),
            )
        })?;
    let version = parsed.get("version").cloned().unwrap_or(json!(3));
    Ok(serde_json::to_vec(
        &json!({ "version": version, "deployment": latest }),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_naming_roundtrip_and_programs() {
        assert_eq!(
            stage0_stack_name("control-plane", "keel-mgmt"),
            "stage0-control-plane-keel-mgmt"
        );
        assert_eq!(
            program_from_stack_name("stage0-control-plane-rauthy-keel-mgmt", "keel-mgmt")
                .as_deref(),
            Some("control-plane-rauthy")
        );
        assert_eq!(
            program_from_stack_name("management-cluster-core", "keel-mgmt"),
            None
        );
        for program in [
            "control-plane",
            "control-plane-forgejo",
            "control-plane-rauthy",
            "kind",
            "aws",
        ] {
            assert!(embedded_program_yaml(program).is_some(), "{program}");
        }
        assert!(embedded_program_yaml("unknown").is_none());
        assert_eq!(stage0_s3_prefix("keel-mgmt"), "stage0/keel-mgmt");
    }

    #[test]
    fn pulumi_passphrase_secret_manifest_is_a_complete_kubectl_document() {
        let secret = keel_kube::build_pulumi_passphrase_secret("flux-system", "p4ss");
        let v: Value = serde_json::to_value(&secret).unwrap();
        assert_eq!(v["apiVersion"], "v1");
        assert_eq!(v["kind"], "Secret");
        assert_eq!(
            v["metadata"]["name"],
            keel_kube::PULUMI_PASSPHRASE_SECRET_NAME
        );
        assert_eq!(v["metadata"]["namespace"], "flux-system");
        // `data` is base64 (`p4ss`), never the raw value.
        assert_eq!(
            v["data"][keel_kube::PULUMI_PASSPHRASE_SECRET_KEY],
            "cDRzcw=="
        );
    }

    #[test]
    fn checkpoint_conversion() {
        let cp = br#"{"version":3,"checkpoint":{"stack":"s","latest":{"manifest":{}}}}"#;
        let v: Value = serde_json::from_slice(&checkpoint_to_export(cp).unwrap()).unwrap();
        assert_eq!(v["deployment"]["manifest"], json!({}));
        let exp = br#"{"version":3,"deployment":{"x":1}}"#;
        assert_eq!(checkpoint_to_export(exp).unwrap(), exp.to_vec());
        assert!(checkpoint_to_export(br#"{"version":3}"#).is_err());
    }

    #[test]
    fn stage0_context_prepare_generates_and_flags_new_passphrase() {
        if std::env::var_os("PULUMI_CONFIG_PASSPHRASE").is_some() {
            eprintln!("SKIPPED: PULUMI_CONFIG_PASSPHRASE set");
            return;
        }
        let root = std::env::temp_dir().join(format!("keel-cli-stage0-{}", uuid::Uuid::new_v4()));
        let ctx = Stage0Context::prepare_in(&root, "ctx-test", false).unwrap();
        assert!(ctx.passphrase_newly_created);
        let notice = ctx.escrow_notice().unwrap();
        assert!(notice.contains(STAGE0_PASSPHRASE_SECRET_NAME));
        assert!(!notice.contains(ctx.passphrase.expose()));
        assert!(ctx.local_backend_url().starts_with("file://"));
        let again = Stage0Context::prepare_in(&root, "ctx-test", true).unwrap();
        assert!(!again.passphrase_newly_created);
        assert!(again.escrow_notice().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
