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

/// Outcome of `keel bootstrap recover`.
#[derive(Debug, Clone)]
pub struct BootstrapRecoverOutcome {
    /// Pulumi DIY backend URL of the escrowed Stage 0 state.
    pub backend_url: String,
    /// Stage 0 stacks covered by the plan.
    pub stacks: Vec<String>,
    /// Planned `pulumi` argv per step (env-only secrets are not shown).
    pub pulumi_commands: Vec<Vec<String>>,
    /// `true` when the plan was executed (not `--dry-run` / `--list-versions`).
    pub executed: bool,
    pub output: String,
}

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
pub fn build_pulumi_recover_argv(
    args: &BootstrapRecoverArgs,
) -> Result<(String, Vec<Vec<String>>), CliError> {
    let (_, backend_url) = validate_recover_args(args)?;
    let stacks = if args.stacks.is_empty() {
        vec![stage0_stack_name("control-plane", &args.cluster_name)]
    } else {
        args.stacks.iter().map(|s| s.trim().to_string()).collect()
    };
    Ok((backend_url, plan_commands(args, &stacks)))
}

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
         \x20 - AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY: S3 keys for the state bucket (SeaweedFS admin keys from\n\
         \x20   keel-system/s3-state-backend-seaweedfs-s3-secret, or your escrowed copy)\n\
         \x20 - Passphrase: PULUMI_CONFIG_PASSPHRASE, --passphrase-file, or ~/.config/keel/stage0/{cluster}/pulumi-passphrase\n\
         \x20 - --kubeconfig/--context for the target cluster if it changed since bootstrap\n"
    )
}

/// Executes `keel bootstrap recover`.
pub async fn execute_bootstrap_recover(
    args: &BootstrapRecoverArgs,
) -> Result<BootstrapRecoverOutcome, CliError> {
    let (endpoint, backend_url) = validate_recover_args(args)?;
    let prefix = stage0_s3_prefix(&args.cluster_name);

    let mut output = String::new();
    output.push_str("Break-Glass Stage 0 Recovery\n============================\n");
    output.push_str(&format!("S3 Endpoint:   {}\n", endpoint.base_url()));
    output.push_str(&format!("State Bucket:  {}\n", args.bucket));
    output.push_str(&format!("State Prefix:  {prefix}/\n"));
    output.push_str(&format!("Backend URL:   {backend_url}\n"));

    if args.dry_run {
        let (_, cmds) = build_pulumi_recover_argv(args)?;
        let stacks = if args.stacks.is_empty() {
            vec![stage0_stack_name("control-plane", &args.cluster_name)]
        } else {
            args.stacks.clone()
        };
        output.push_str(&format!("Stacks:        {}\n\n", stacks.join(", ")));
        output.push_str(&prerequisites_text(&args.cluster_name));
        output.push_str(
            "\nPlan (dry run; PULUMI_BACKEND_URL / credentials / passphrase passed via env):\n",
        );
        for (idx, argv) in cmds.iter().enumerate() {
            output.push_str(&format!("  {}. {}\n", idx + 1, argv.join(" ")));
        }
        return Ok(BootstrapRecoverOutcome {
            backend_url,
            stacks,
            pulumi_commands: cmds,
            executed: false,
            output,
        });
    }

    let creds = S3Credentials::from_env().ok_or_else(|| {
        CliError::Validation(format!(
            "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be set for the S3 state backend.\n{}",
            prerequisites_text(&args.cluster_name)
        ))
    })?;
    let client = S3Client::new(endpoint.clone(), &args.region, creds.clone())?;

    // Discover stacks from the escrow manifest when not given explicitly.
    let manifest: Option<Value> = client
        .get_object(&args.bucket, &format!("{prefix}/manifest.json"), None)
        .await?
        .and_then(|raw| serde_json::from_slice(&raw).ok());
    let stacks: Vec<String> = if !args.stacks.is_empty() {
        args.stacks.iter().map(|s| s.trim().to_string()).collect()
    } else if let Some(list) = manifest
        .as_ref()
        .and_then(|m| m.get("stacks"))
        .and_then(Value::as_array)
    {
        // Control-plane first; the cluster-provisioning stack (kind/cloud) is only re-run on request.
        let mut s: Vec<String> = list
            .iter()
            .filter_map(|e| e.get("stack").and_then(Value::as_str).map(str::to_string))
            .filter(|s| program_takes_kubeconfig(s, &args.cluster_name))
            .collect();
        s.sort_by_key(|n| n != &stage0_stack_name("control-plane", &args.cluster_name));
        s
    } else {
        vec![stage0_stack_name("control-plane", &args.cluster_name)]
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
        return Ok(BootstrapRecoverOutcome {
            backend_url,
            pulumi_commands: plan_commands(args, &stacks),
            stacks,
            executed: false,
            output,
        });
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
            let project = serde_yml::from_slice::<Value>(
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

    Ok(BootstrapRecoverOutcome {
        backend_url,
        pulumi_commands: plan_commands(args, &stacks),
        stacks,
        executed: true,
        output,
    })
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
        let out = execute_bootstrap_recover(&args).await.unwrap();
        assert!(!out.executed);
        assert_eq!(out.stacks, vec!["stage0-control-plane-keel-mgmt"]);
        assert!(out.backend_url.starts_with("s3://pulumi-state/stage0/keel-mgmt?endpoint=127.0.0.1:8333&region=us-east-1&s3ForcePathStyle=true&disableSSL=true"));
        assert!(out.output.contains(
            "pulumi refresh --yes --non-interactive --stack stage0-control-plane-keel-mgmt"
        ));
        assert!(out.output.contains("pulumi stack import"));
        assert!(out.output.contains("kubeconfig <stdin>"));
        assert!(out.output.contains("AWS_ACCESS_KEY_ID"));
        assert!(!out.output.contains("pulumi login"));
    }

    #[tokio::test]
    async fn validation_errors_name_the_right_flag() {
        let mut args = parse(&["--dry-run"]);
        args.s3_endpoint = "ftp://s3.local".into();
        let err = execute_bootstrap_recover(&args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--s3-endpoint"), "{err}");
        assert!(!err.contains("server URL"));

        let mut args = parse(&["--dry-run"]);
        args.stacks = vec!["--malicious".into()];
        assert!(execute_bootstrap_recover(&args).await.is_err());
        args.stacks = vec!["management-cluster-core".into()];
        assert!(
            execute_bootstrap_recover(&args)
                .await
                .unwrap_err()
                .to_string()
                .contains("stage0-control-plane-keel-mgmt")
        );

        let mut args = parse(&["--dry-run"]);
        args.snapshot = Some("../x".into());
        assert!(execute_bootstrap_recover(&args).await.is_err());
    }

    #[tokio::test]
    async fn execution_requires_s3_credentials_when_not_dry_run() {
        if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
            return;
        }
        let args = parse(&[]);
        let err = execute_bootstrap_recover(&args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("AWS_ACCESS_KEY_ID"), "{err}");
    }
}
