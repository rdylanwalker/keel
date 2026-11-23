//! Break-glass Stage 0 recovery from escrowed S3 state (`keel bootstrap recover`, `SDD.md` §2.2,
//! `NFR-REL-01`).
//!
//! `keel bootstrap init` escrows every Stage 0 stack to `s3://<bucket>/stage0/<cluster>` (versioned
//! bucket, or timestamped snapshots when versioning is unavailable) together with the encrypted
//! stack config files. Recovery re-materializes each program, optionally restores an older state
//! version / snapshot, then runs `pulumi refresh` and `pulumi up` against the S3 backend and
//! copies the result back into the local durable backend.
//!
//! All `pulumi` invocations use explicit argv; the backend URL, S3 credentials, and passphrase
//! travel only through environment variables, secret config through stdin (`NFR-SEC-06`).

use keel_core::{
    Stage0Passphrase, Stage0PassphraseSource, ensure_private_dir, stage0_cluster_dir_in,
    stage0_program_dir_in, stage0_state_root, validate_dns_label, write_private_file,
};
use serde_json::Value;

use crate::cli::BootstrapRecoverArgs;

use super::CliError;
use super::bootstrap::s3::{S3Client, S3Credentials, S3Endpoint, s3_backend_url};
use super::bootstrap::state::{
    PulumiRunner, checkpoint_to_export, embedded_program_yaml, program_from_stack_name,
    stage0_s3_prefix, stage0_stack_name,
};

fn validate_recover_args(args: &BootstrapRecoverArgs) -> Result<(S3Endpoint, String), CliError> {
    let endpoint = S3Endpoint::parse(&args.s3_endpoint, "--s3-endpoint")?;
    validate_dns_label(&args.bucket, "bucket")?;
    validate_dns_label(&args.cluster_name, "cluster-name")?;
    for stack in &args.stacks {
        super::validate_non_flag_input(stack, "stack")?;
        if program_from_stack_name(stack.trim(), &args.cluster_name).is_none() {
            return Err(CliError::Validation(format!(
                "--stack '{stack}' is not a Stage 0 stack of cluster '{}' (expected `stage0-<program>-{}`, e.g. `{}`)",
                args.cluster_name,
                args.cluster_name,
                stage0_stack_name("control-plane", &args.cluster_name)
            )));
        }
    }
    for (flag, v) in [
        ("state-version", &args.state_version),
        ("snapshot", &args.snapshot),
    ] {
        if let Some(v) = v
            && (v.is_empty()
                || !v
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        {
            return Err(CliError::Validation(format!("invalid --{flag} '{v}'")));
        }
    }
    let prefix = stage0_s3_prefix(&args.cluster_name);
    let url = s3_backend_url(&args.bucket, Some(&prefix), &endpoint, &args.region)?;
    Ok((endpoint, url))
}

/// Builds the explicit `pulumi` argv sequence for recovering `stacks` (zero shell, `NFR-SEC-06`).
/// The backend is selected through `PULUMI_BACKEND_URL`, so no `pulumi login` is needed.
fn plan_commands(args: &BootstrapRecoverArgs, stacks: &[String]) -> Vec<Vec<String>> {
    let v = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut cmds = Vec::new();
    for stack in stacks {
        cmds.push(v(&[
            "pulumi",
            "stack",
            "select",
            "--non-interactive",
            stack,
        ]));
        if args.state_version.is_some() || args.snapshot.is_some() {
            cmds.push(v(&[
                "pulumi",
                "stack",
                "import",
                "--non-interactive",
                "--stack",
                stack,
                "--file",
                "<restored-state.json>",
            ]));
        }
        if args.kubeconfig.is_some() && program_takes_kubeconfig(stack, &args.cluster_name) {
            cmds.push(v(&[
                "pulumi",
                "config",
                "set",
                "--secret",
                "--non-interactive",
                "--stack",
                stack,
                "kubeconfig",
                "<stdin>",
            ]));
        }
        if !args.skip_refresh {
            cmds.push(v(&[
                "pulumi",
                "refresh",
                "--yes",
                "--non-interactive",
                "--stack",
                stack,
            ]));
        }
        cmds.push(v(&[
            "pulumi",
            "up",
            "--yes",
            "--non-interactive",
            "--stack",
            stack,
        ]));
    }
    cmds
}

fn program_takes_kubeconfig(stack: &str, cluster: &str) -> bool {
    program_from_stack_name(stack, cluster).is_some_and(|p| p.starts_with("control-plane"))
}

fn resolve_passphrase(args: &BootstrapRecoverArgs) -> Result<Stage0Passphrase, CliError> {
    if let Some(file) = &args.passphrase_file {
        let raw = std::fs::read_to_string(file).map_err(|e| {
            CliError::Validation(format!(
                "cannot read --passphrase-file '{}': {e}",
                file.display()
            ))
        })?;
        return Ok(Stage0Passphrase::new(
            raw.trim_end_matches(['\r', '\n']),
            Stage0PassphraseSource::LocalFile(file.clone()),
        ));
    }
    if let Ok(env) = std::env::var("PULUMI_CONFIG_PASSPHRASE") {
        return Ok(Stage0Passphrase::new(
            env,
            Stage0PassphraseSource::Environment,
        ));
    }
    let local = stage0_cluster_dir_in(&stage0_state_root(), &args.cluster_name)
        .join(keel_core::STAGE0_PASSPHRASE_FILE);
    if let Ok(raw) = std::fs::read_to_string(&local) {
        return Ok(Stage0Passphrase::new(
            raw.trim_end_matches(['\r', '\n']),
            Stage0PassphraseSource::LocalFile(local),
        ));
    }
    Err(CliError::Validation(format!(
        "Stage 0 Pulumi passphrase not found. Provide it via PULUMI_CONFIG_PASSPHRASE or --passphrase-file \
         (escrowed copy, the in-cluster Secret keel-system/keel-stage0-pulumi-passphrase, or {})",
        local.display()
    )))
}

fn prerequisites_text(cluster: &str) -> String {
    format!(
        "Prerequisites:\n\
         \x20 - `pulumi` on PATH\n\
         \x20 - AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY: S3 keys for the state bucket (SeaweedFS admin keys:\n\
         \x20   fields `admin_access_key_id` / `admin_secret_access_key` of the Secret\n\
         \x20   keel-system/s3-state-backend-seaweedfs-s3-secret, or your escrowed copy)\n\
         \x20 - Passphrase: PULUMI_CONFIG_PASSPHRASE, --passphrase-file, or ~/.config/keel/stage0/{cluster}/pulumi-passphrase\n\
         \x20 - --kubeconfig/--context for the target cluster if it changed since bootstrap\n"
    )
}

/// The escrow manifest (`<prefix>/manifest.json`) listing the escrowed Stage 0 stacks.
async fn fetch_escrow_manifest(
    client: &S3Client,
    bucket: &str,
    prefix: &str,
) -> Result<Option<Value>, CliError> {
    Ok(client
        .get_object(bucket, &format!("{prefix}/manifest.json"), None)
        .await?
        .and_then(|raw| serde_json::from_slice(&raw).ok()))
}

/// Stacks a recovery without `--stack` re-runs: every escrowed control-plane stack, `control-plane`
/// first. The cluster-provisioning stack (kind/cloud) is only re-run on request.
fn recover_stacks(manifest: Option<&Value>, cluster: &str) -> Vec<String> {
    let Some(list) = manifest
        .and_then(|m| m.get("stacks"))
        .and_then(Value::as_array)
    else {
        return vec![stage0_stack_name("control-plane", cluster)];
    };
    let mut stacks: Vec<String> = list
        .iter()
        .filter_map(|e| e.get("stack").and_then(Value::as_str).map(str::to_string))
        .filter(|s| program_takes_kubeconfig(s, cluster))
        .collect();
    stacks.sort_by_key(|n| n != &stage0_stack_name("control-plane", cluster));
    stacks
}

/// Every control-plane stack `keel bootstrap init` can escrow (the dry run's list without S3
/// credentials to read the manifest).
fn candidate_control_plane_stacks(cluster: &str) -> Vec<String> {
    STAGE0_CONTROL_PLANE_PROGRAMS
        .iter()
        .map(|program| stage0_stack_name(program, cluster))
        .collect()
}

/// Control-plane Stage 0 programs Keel ships, in recovery order.
const STAGE0_CONTROL_PLANE_PROGRAMS: [&str; 3] = [
    "control-plane",
    "control-plane-forgejo",
    "control-plane-rauthy",
];

/// Stacks the dry run lists, with a note on where the list came from. Read-only and never
/// fatal: with S3 credentials it reads the same escrow manifest the real run recovers from, and
/// falls back to every candidate control-plane stack (naming the S3 error) when that read fails
/// (port-forward not up yet, unrelated AWS keys exported, ...).
async fn dry_run_stacks(
    args: &BootstrapRecoverArgs,
    endpoint: &S3Endpoint,
    prefix: &str,
    creds: Option<S3Credentials>,
) -> (Vec<String>, Option<String>) {
    if !args.stacks.is_empty() {
        return (
            args.stacks.iter().map(|s| s.trim().to_string()).collect(),
            None,
        );
    }
    let manifest_url = format!("s3://{}/{prefix}/manifest.json", args.bucket);
    let candidates = |why: String| {
        (
            candidate_control_plane_stacks(&args.cluster_name),
            Some(format!(
                "{why}; listing candidates: the real run recovers the control-plane stacks \
                 listed in {manifest_url} (forgejo/rauthy only when bootstrapped with them)"
            )),
        )
    };
    let Some(creds) = creds else {
        return candidates(
            "AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY not set (set them for the exact list)"
                .to_string(),
        );
    };
    let manifest = match S3Client::new(endpoint.clone(), &args.region, creds) {
        Ok(client) => fetch_escrow_manifest(&client, &args.bucket, prefix).await,
        Err(err) => Err(err),
    };
    match manifest {
        Ok(Some(manifest)) => (
            recover_stacks(Some(&manifest), &args.cluster_name),
            Some(format!("from {manifest_url}")),
        ),
        Ok(None) => (
            recover_stacks(None, &args.cluster_name),
            Some(format!("no {manifest_url}; default")),
        ),
        Err(err) => candidates(format!("could not read {manifest_url} ({err})")),
    }
}

/// Executes `keel bootstrap recover`.
pub async fn execute_bootstrap_recover(args: &BootstrapRecoverArgs) -> Result<String, CliError> {
    execute_bootstrap_recover_with(args, S3Credentials::from_env()).await
}

/// [`execute_bootstrap_recover`] with injected S3 credentials (tests never read the ambient
/// `AWS_*` environment).
async fn execute_bootstrap_recover_with(
    args: &BootstrapRecoverArgs,
    creds: Option<S3Credentials>,
) -> Result<String, CliError> {
    let (endpoint, backend_url) = validate_recover_args(args)?;
    let prefix = stage0_s3_prefix(&args.cluster_name);

    let mut output = String::new();
    output.push_str("Break-Glass Stage 0 Recovery\n============================\n");
    output.push_str(&format!("S3 Endpoint:   {}\n", endpoint.base_url()));
    output.push_str(&format!("State Bucket:  {}\n", args.bucket));
    output.push_str(&format!("State Prefix:  {prefix}/\n"));
    output.push_str(&format!("Backend URL:   {backend_url}\n"));

    if args.dry_run {
        let (stacks, origin) = dry_run_stacks(args, &endpoint, &prefix, creds).await;
        let cmds = plan_commands(args, &stacks);
        output.push_str(&format!("Stacks:        {}\n", stacks.join(", ")));
        if let Some(origin) = origin {
            output.push_str(&format!("               ({origin})\n"));
        }
        output.push('\n');
        output.push_str(&prerequisites_text(&args.cluster_name));
        output.push_str(
            "\nPlan (dry run; PULUMI_BACKEND_URL / credentials / passphrase passed via env):\n",
        );
        for (idx, argv) in cmds.iter().enumerate() {
            output.push_str(&format!("  {}. {}\n", idx + 1, argv.join(" ")));
        }
        return Ok(output);
    }

    let creds = creds.ok_or_else(|| {
        CliError::Validation(format!(
            "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be set for the S3 state backend.\n{}",
            prerequisites_text(&args.cluster_name)
        ))
    })?;
    let client = S3Client::new(endpoint.clone(), &args.region, creds.clone())?;

    // Discover stacks from the escrow manifest when not given explicitly.
    let manifest = fetch_escrow_manifest(&client, &args.bucket, &prefix).await?;
    let stacks: Vec<String> = if !args.stacks.is_empty() {
        args.stacks.iter().map(|s| s.trim().to_string()).collect()
    } else {
        recover_stacks(manifest.as_ref(), &args.cluster_name)
    };
    output.push_str(&format!("Stacks:        {}\n", stacks.join(", ")));

    if args.list_versions {
        output.push_str("\nState versions (newest first per key):\n");
        let versions = client
            .list_object_versions(&args.bucket, &format!("{prefix}/.pulumi/stacks/"))
            .await?;
        for v in versions.iter().filter(|v| v.key.ends_with(".json")) {
            output.push_str(&format!(
                "  {}  version={}  {}{}\n",
                v.last_modified,
                v.version_id,
                v.key,
                if v.is_latest { "  (latest)" } else { "" }
            ));
        }
        output.push_str("Snapshots:\n");
        for v in client
            .list_object_versions(&args.bucket, &format!("{prefix}/snapshots/"))
            .await?
            .iter()
            .filter(|v| v.is_latest)
        {
            output.push_str(&format!("  {}\n", v.key));
        }
        return Ok(output);
    }

    let passphrase = resolve_passphrase(args)?;
    let kubeconfig_yaml = match &args.kubeconfig {
        Some(path) => Some(zeroize::Zeroizing::new(
            std::fs::read_to_string(path).map_err(|e| {
                CliError::Validation(format!(
                    "cannot read --kubeconfig '{}': {e}",
                    path.display()
                ))
            })?,
        )),
        None => None,
    };
    if let Some(kc) = &kubeconfig_yaml {
        keel_core::validate_kubeconfig_yaml(kc, args.context.as_deref())?;
    }

    let cluster_dir = stage0_cluster_dir_in(&stage0_state_root(), &args.cluster_name);
    ensure_private_dir(&cluster_dir)?;
    let local_backend = cluster_dir.join(keel_core::STAGE0_BACKEND_DIR);
    ensure_private_dir(&local_backend)?;
    output.push('\n');

    for stack in &stacks {
        let program = program_from_stack_name(stack, &args.cluster_name)
            .ok_or_else(|| CliError::Validation(format!("unrecognized Stage 0 stack '{stack}'")))?;
        let program_dir = stage0_program_dir_in(&cluster_dir, &program);
        ensure_private_dir(&program_dir)?;

        // Program + encrypted stack config from the escrow (fallback: embedded program).
        let program_yaml = match client
            .get_object(
                &args.bucket,
                &format!("{prefix}/config/{program}/Pulumi.yaml"),
                None,
            )
            .await?
        {
            Some(raw) => raw,
            None => embedded_program_yaml(&program)
                .ok_or_else(|| {
                    CliError::Execution(format!(
                        "no program source for '{program}' in escrow or binary"
                    ))
                })?
                .into_bytes(),
        };
        tokio::fs::write(program_dir.join("Pulumi.yaml"), program_yaml).await?;
        let config_name = format!("Pulumi.{stack}.yaml");
        match client
            .get_object(
                &args.bucket,
                &format!("{prefix}/config/{program}/{config_name}"),
                None,
            )
            .await?
        {
            Some(raw) => {
                let path = program_dir.join(&config_name);
                if path.exists() {
                    let _ = tokio::fs::copy(&path, program_dir.join(format!("{config_name}.bak")))
                        .await;
                }
                write_private_file(&path, &raw)?;
            }
            None => output.push_str(&format!(
                "warning: escrowed config for {stack} not found; using local config (if any)\n"
            )),
        }

        let runner = PulumiRunner::new(&program_dir, backend_url.clone(), passphrase.expose())
            .with_s3_credentials(&creds);
        runner.select_stack(stack, false).await?;
        output.push_str(&format!("[{stack}] selected\n"));

        if args.state_version.is_some() || args.snapshot.is_some() {
            let project = noyalib::from_slice::<Value>(
                &tokio::fs::read(program_dir.join("Pulumi.yaml")).await?,
            )?
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
            let (key, version) = match (&args.state_version, &args.snapshot) {
                (Some(v), _) => (
                    format!("{prefix}/.pulumi/stacks/{project}/{stack}.json"),
                    Some(v.as_str()),
                ),
                (None, Some(snap)) => (format!("{prefix}/snapshots/{stack}/{snap}.json"), None),
                _ => unreachable!(),
            };
            let raw = client
                .get_object(&args.bucket, &key, version)
                .await?
                .ok_or_else(|| {
                    CliError::Execution(format!(
                        "state object {key} (version {version:?}) not found"
                    ))
                })?;
            let restore = cluster_dir
                .join("exports")
                .join(format!("{stack}.restore.json"));
            write_private_file(&restore, &checkpoint_to_export(&raw)?)?;
            let file_str = restore.to_string_lossy().to_string();
            runner
                .run(&[
                    "stack",
                    "import",
                    "--non-interactive",
                    "--stack",
                    stack,
                    "--file",
                    &file_str,
                ])
                .await?;
            output.push_str(&format!("[{stack}] restored state from {key}\n"));
        }

        if program_takes_kubeconfig(stack, &args.cluster_name) {
            let mut pairs: Vec<(&str, &str, bool)> = Vec::new();
            if let Some(kc) = &kubeconfig_yaml {
                pairs.push(("kubeconfig", kc.as_str(), true));
            }
            if let Some(ctx) = &args.context {
                pairs.push(("kubeContext", ctx.as_str(), false));
            }
            if !pairs.is_empty() {
                runner.set_config(stack, &pairs).await?;
                output.push_str(&format!("[{stack}] updated kubeconfig/context config\n"));
            }
        }
        if !args.skip_refresh {
            runner
                .run(&["refresh", "--yes", "--non-interactive", "--stack", stack])
                .await?;
            output.push_str(&format!("[{stack}] refresh complete\n"));
        }
        runner
            .run(&["up", "--yes", "--non-interactive", "--stack", stack])
            .await?;
        output.push_str(&format!("[{stack}] up complete\n"));

        // Keep the local durable copy in sync with the recovered state.
        let export = cluster_dir.join("exports").join(format!("{stack}.json"));
        runner.export(stack, &export).await?;
        let local = PulumiRunner::new(
            &program_dir,
            format!("file://{}", local_backend.display()),
            passphrase.expose(),
        );
        if let Err(err) = local.init_and_import(stack, &export).await {
            output.push_str(&format!(
                "warning: could not refresh local copy of {stack}: {err}\n"
            ));
        }
    }
    output.push_str(
        "\nRecovery complete. Verify with `kubectl get pods -A` and `flux get all -A`.\n",
    );

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(extra: &[&str]) -> BootstrapRecoverArgs {
        let mut argv = vec![
            "keel",
            "bootstrap",
            "recover",
            "--s3-endpoint",
            "http://127.0.0.1:8333",
        ];
        argv.extend_from_slice(extra);
        let cli = crate::cli::Cli::try_parse_from(argv).unwrap();
        match cli.command {
            crate::cli::Commands::Bootstrap(b) => match b.command {
                crate::cli::BootstrapCommands::Recover(a) => a,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn dry_run_prints_plan_without_credentials() {
        let args = parse(&[
            "--dry-run",
            "--kubeconfig",
            "/tmp/kc",
            "--state-version",
            "v1",
        ]);
        let out = execute_bootstrap_recover_with(&args, None).await.unwrap();
        // Without S3 credentials the dry run lists every control-plane stack the real run may
        // recover (it reads the escrow manifest), not just `control-plane`.
        assert!(out.contains(
            "Stacks:        stage0-control-plane-keel-mgmt, stage0-control-plane-forgejo-keel-mgmt, stage0-control-plane-rauthy-keel-mgmt\n"
        ), "{out}");
        assert!(
            out.contains("s3://pulumi-state/stage0/keel-mgmt/manifest.json"),
            "{out}"
        );
        for stack in candidate_control_plane_stacks("keel-mgmt") {
            assert!(
                out.contains(&format!(
                    "pulumi up --yes --non-interactive --stack {stack}"
                )),
                "{out}"
            );
        }
        assert!(
            out.contains("`admin_access_key_id` / `admin_secret_access_key`"),
            "{out}"
        );
        assert!(out.contains("Backend URL:   s3://pulumi-state/stage0/keel-mgmt?endpoint=127.0.0.1:8333&region=us-east-1&s3ForcePathStyle=true&disableSSL=true"));
        assert!(out.contains(
            "pulumi refresh --yes --non-interactive --stack stage0-control-plane-keel-mgmt"
        ));
        assert!(out.contains("pulumi stack import"));
        assert!(out.contains("kubeconfig <stdin>"));
        assert!(out.contains("AWS_ACCESS_KEY_ID"));
        assert!(!out.contains("pulumi login"));
    }

    fn test_creds() -> S3Credentials {
        S3Credentials {
            access_key: "test-access".to_string(),
            secret_key: zeroize::Zeroizing::new("test-secret".to_string()),
        }
    }

    /// With S3 credentials the dry run lists exactly the escrowed control-plane stacks.
    #[tokio::test]
    async fn credentialed_dry_run_reads_the_escrow_manifest() {
        let manifest = serde_json::json!({"stacks": [
            {"stack": "stage0-kind-keel-mgmt"},
            {"stack": "stage0-control-plane-forgejo-keel-mgmt"},
            {"stack": "stage0-control-plane-keel-mgmt"},
        ]});
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let manifest = manifest.clone();
            async move {
                assert!(
                    uri.path().ends_with("/stage0/keel-mgmt/manifest.json"),
                    "{uri}"
                );
                axum::Json(manifest)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut args = parse(&["--dry-run", "--kubeconfig", "/tmp/kc"]);
        args.s3_endpoint = format!("http://127.0.0.1:{port}");
        let out = execute_bootstrap_recover_with(&args, Some(test_creds()))
            .await
            .unwrap();
        assert!(
            out.contains(
                "Stacks:        stage0-control-plane-keel-mgmt, stage0-control-plane-forgejo-keel-mgmt\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("(from s3://pulumi-state/stage0/keel-mgmt/manifest.json)"),
            "{out}"
        );
        assert!(!out.contains("stage0-kind-keel-mgmt"), "{out}");
        server.abort();
    }

    /// An unreachable S3 endpoint (port-forward not up, unrelated AWS keys) never aborts the
    /// dry run: it lists the candidates and names the S3 error.
    #[tokio::test]
    async fn credentialed_dry_run_survives_an_unreachable_s3_endpoint() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut args = parse(&["--dry-run", "--kubeconfig", "/tmp/kc"]);
        args.s3_endpoint = format!("http://127.0.0.1:{port}");
        let out = execute_bootstrap_recover_with(&args, Some(test_creds()))
            .await
            .unwrap();
        assert!(
            out.contains(&format!(
                "Stacks:        {}\n",
                candidate_control_plane_stacks("keel-mgmt").join(", ")
            )),
            "{out}"
        );
        assert!(
            out.contains("could not read s3://pulumi-state/stage0/keel-mgmt/manifest.json ("),
            "{out}"
        );
        assert!(out.contains("listing candidates"), "{out}");
        assert!(!out.contains("test-secret"), "{out}");
        assert!(out.contains("Plan (dry run"), "{out}");
    }

    /// The real run and the credentialed dry run share one stack list: every escrowed
    /// control-plane stack, `control-plane` first, never the cluster-provisioning stack.
    #[test]
    fn recover_stacks_follow_the_escrow_manifest() {
        let manifest = serde_json::json!({"stacks": [
            {"stack": "stage0-kind-keel-mgmt"},
            {"stack": "stage0-control-plane-rauthy-keel-mgmt"},
            {"stack": "stage0-control-plane-forgejo-keel-mgmt"},
            {"stack": "stage0-control-plane-keel-mgmt"},
        ]});
        assert_eq!(
            recover_stacks(Some(&manifest), "keel-mgmt"),
            [
                "stage0-control-plane-keel-mgmt",
                "stage0-control-plane-rauthy-keel-mgmt",
                "stage0-control-plane-forgejo-keel-mgmt",
            ]
        );
        assert_eq!(
            recover_stacks(None, "keel-mgmt"),
            ["stage0-control-plane-keel-mgmt"]
        );
        // Every candidate is a program Keel can re-materialize.
        for stack in candidate_control_plane_stacks("keel-mgmt") {
            let program = program_from_stack_name(&stack, "keel-mgmt").unwrap();
            assert!(embedded_program_yaml(&program).is_some(), "{program}");
        }
    }

    #[tokio::test]
    async fn validation_errors_name_the_right_flag() {
        let mut args = parse(&["--dry-run"]);
        args.s3_endpoint = "ftp://s3.local".into();
        let err = execute_bootstrap_recover_with(&args, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--s3-endpoint"), "{err}");
        assert!(!err.contains("server URL"));

        let mut args = parse(&["--dry-run"]);
        args.stacks = vec!["--malicious".into()];
        assert!(execute_bootstrap_recover_with(&args, None).await.is_err());
        args.stacks = vec!["management-cluster-core".into()];
        assert!(
            execute_bootstrap_recover_with(&args, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("stage0-control-plane-keel-mgmt")
        );

        let mut args = parse(&["--dry-run"]);
        args.snapshot = Some("../x".into());
        assert!(execute_bootstrap_recover_with(&args, None).await.is_err());
    }

    #[tokio::test]
    async fn execution_requires_s3_credentials_when_not_dry_run() {
        if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
            return;
        }
        let args = parse(&[]);
        let err = execute_bootstrap_recover_with(&args, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("AWS_ACCESS_KEY_ID"), "{err}");
    }
}
