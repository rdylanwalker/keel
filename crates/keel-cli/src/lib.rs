//! `keel-cli`
//!
//! Command-line interface and binary entrypoint for Keel (`keel`).
//! Supports provider-agnostic Stage 0 cluster bootstrapping, break-glass S3 recovery,
//! two-phase authentication (Stage 0 token + OIDC PKCE), infrastructure management,
//! application catalog installation, ephemeral PKO v2 preview inspection, and running
//! the stateless control-plane server (`keel serve`).

pub mod cli;
pub mod commands;

pub use cli::{
    AppArgs, AppCommands, AppInstallArgs, AppListArgs, AuthArgs, AuthCommands, AuthLoginArgs,
    AuthServerArgs, BootstrapArgs, BootstrapCommands, BootstrapInitArgs, BootstrapRecoverArgs,
    CatalogArgs, CatalogCommands, CatalogListArgs, CatalogShowArgs, Cli, Commands,
    DEFAULT_BIND_ADDR, DEFAULT_S3_BUCKET, DEFAULT_SERVER_URL, InfraArgs, InfraCommands,
    InfraListArgs, InfraProvisionArgs, InfraSshArgs, OutputFormat, PreviewArgs, PreviewCommands,
    PreviewStatusArgs, ProposalArgs, ProposalCommands, ProposalListArgs, ProposalRefArgs,
    ProposalStatusArg, ServeArgs, TenantArgs, TenantCommands, TenantMigrateBaselineArgs,
    TenantMigrateStackSourcesArgs, TenantMigrateStateArgs,
};
pub use commands::{
    AuthLoginOutcome, BootstrapInitOutcome, BootstrapRecoverOutcome, CliContext, CliError,
    KyvernoPolicyGenerator, PkceChallenge, PreviewStatusOutcome, ProposalCommandOutcome,
    SeedControlPlaneArtifacts, SeedRepoInitResult, build_pulumi_recover_argv, build_serve_state,
    build_ssh_argv, default_bare_repo_for_workdir, dispatch, execute_app_install,
    execute_auth_login, execute_auth_logout, execute_auth_status, execute_bootstrap_init,
    execute_bootstrap_init_with_target, execute_bootstrap_recover, execute_infra_list,
    execute_infra_provision, execute_infra_ssh, execute_preview_status, execute_serve,
    execute_serve_until, format_preview_table, format_ssh_info_output, format_stacks_output,
    generate_pkce_pair, initialize_seed_git_repo, load_values_file, normalize_server_url,
    synthesize_seed_control_plane, validate_application_values, validate_blueprint_values,
};

/// Executes a parsed [`Cli`] command using the default [`CliContext`] and prints output to stdout.
///
/// Errors are returned to the caller; use [`CliError::render`] to print them (one line by
/// default, full cause chain with `-v`).
pub async fn run(cli: &Cli) -> Result<(), CliError> {
    let ctx = CliContext::new();
    let output = match dispatch(cli, &ctx).await {
        Ok(output) => output,
        Err(CliError::Incomplete { output, message }) => {
            if !output.is_empty() {
                println!("{output}");
            }
            return Err(CliError::Execution(message));
        }
        Err(err) => return Err(err),
    };
    if !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[tokio::test]
    async fn run_executes_recover_subcommand() {
        let cli = Cli::try_parse_from([
            "keel",
            "bootstrap",
            "recover",
            "--s3-endpoint",
            "https://s3.keel.internal:9000",
            "--dry-run",
        ])
        .unwrap();
        run(&cli).await.unwrap();
    }
}
