//! Break-glass S3 control-plane recovery handlers (`keel bootstrap recover`, `SDD.md` §2.2, `NFR-REL-01`).

use keel_core::{FLUX_SYSTEM_NAMESPACE, validate_dns_label};
use keel_pko::{ManagementCoreStackParams, PkoStack, synthesize_management_core_stack};

use crate::cli::BootstrapRecoverArgs;

use super::{CliError, normalize_server_url, validate_non_flag_input};

/// Outcome of `keel bootstrap recover` (`SDD.md` §2.2, `NFR-REL-01`).
#[derive(Debug, Clone)]
pub struct BootstrapRecoverOutcome {
    pub backend_url: String,
    pub recovery_stack: PkoStack,
    pub pulumi_commands: Vec<Vec<String>>,
    pub output: String,
}

/// Constructs explicit `pulumi` argument vectors (zero shell invocation per `NFR-SEC-06`)
/// for break-glass control-plane recovery against versioned S3 state.
pub fn build_pulumi_recover_argv(
    args: &BootstrapRecoverArgs,
) -> Result<(String, Vec<Vec<String>>), CliError> {
    let s3_endpoint = normalize_server_url(&args.s3_endpoint)?;
    validate_dns_label(&args.bucket, "bucket")?;
    validate_non_flag_input(&args.stack, "stack")?;

    let backend_url = format!(
        "s3://{}?endpoint={}&s3ForcePathStyle=true",
        args.bucket, s3_endpoint
    );

    let commands = vec![
        vec![
            "pulumi".to_string(),
            "login".to_string(),
            backend_url.clone(),
        ],
        vec![
            "pulumi".to_string(),
            "stack".to_string(),
            "select".to_string(),
            "--".to_string(),
            args.stack.trim().to_string(),
        ],
        vec![
            "pulumi".to_string(),
            "refresh".to_string(),
            "--yes".to_string(),
            "--stack".to_string(),
            args.stack.trim().to_string(),
        ],
        vec![
            "pulumi".to_string(),
            "up".to_string(),
            "--yes".to_string(),
            "--stack".to_string(),
            args.stack.trim().to_string(),
        ],
    ];

    Ok((backend_url, commands))
}

/// Executes `keel bootstrap recover` validating S3 recovery inputs and synthesizing the
/// anti-lockout `management-cluster-core` stack reconciliation plan.
pub async fn execute_bootstrap_recover(
    args: &BootstrapRecoverArgs,
) -> Result<BootstrapRecoverOutcome, CliError> {
    let (backend_url, pulumi_commands) = build_pulumi_recover_argv(args)?;

    let mut stack_params =
        ManagementCoreStackParams::new(&backend_url, FLUX_SYSTEM_NAMESPACE, "clusters/management");
    stack_params.pulumi_stack_name = args.stack.trim().to_string();
    let recovery_stack = synthesize_management_core_stack(&stack_params)?;

    let mut output = String::new();
    output.push_str("Break-Glass Control-Plane Recovery Plan\n");
    output.push_str("=======================================\n");
    output.push_str(&format!("S3 Endpoint:   {}\n", args.s3_endpoint.trim()));
    output.push_str(&format!("State Bucket:  {}\n", args.bucket));
    output.push_str(&format!("Pulumi Stack:  {}\n", args.stack.trim()));
    output.push_str(&format!("Backend URL:   {backend_url}\n"));
    output.push_str(&format!(
        "Anti-Lockout:  {} (protect={})\n\n",
        recovery_stack.metadata.name,
        recovery_stack.is_protected()
    ));
    output.push_str("Recovery Sequence:\n");
    for (idx, argv) in pulumi_commands.iter().enumerate() {
        output.push_str(&format!("  {}. {}\n", idx + 1, argv.join(" ")));
    }

    Ok(BootstrapRecoverOutcome {
        backend_url,
        recovery_stack,
        pulumi_commands,
        output,
    })
}
