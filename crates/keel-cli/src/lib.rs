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
    AppArgs, AppCommands, AppInstallArgs, AuthArgs, AuthCommands, AuthLoginArgs, BootstrapArgs,
    BootstrapCommands, BootstrapInitArgs, BootstrapRecoverArgs, Cli, Commands, DEFAULT_BIND_ADDR,
    DEFAULT_S3_BUCKET, DEFAULT_SERVER_URL, InfraArgs, InfraCommands, InfraListArgs,
    InfraProvisionArgs, OutputFormat, PreviewArgs, PreviewCommands, PreviewStatusArgs, ServeArgs,
};
pub use commands::{
    AuthLoginOutcome, BootstrapInitOutcome, BootstrapRecoverOutcome, CliContext, CliError,
    KyvernoPolicyGenerator, PkceChallenge, PreviewStatusOutcome, ProposalCommandOutcome,
    SeedControlPlaneArtifacts, SeedRepoInitResult, build_pulumi_recover_argv, build_serve_state,
    default_bare_repo_for_workdir, dispatch, execute_app_install, execute_auth_login,
    execute_bootstrap_init, execute_bootstrap_init_with_target, execute_bootstrap_recover,
    execute_infra_list, execute_infra_provision, execute_preview_status, execute_serve,
    execute_serve_until, format_preview_table, format_stacks_output, generate_pkce_pair,
    initialize_seed_git_repo, load_values_file, normalize_server_url,
    synthesize_seed_control_plane, validate_application_values, validate_blueprint_values,
};

/// Executes a parsed [`Cli`] command using the default [`CliContext`] and prints output to stdout.
pub async fn run(cli: &Cli) -> Result<(), CliError> {
    let ctx = CliContext::new();
    let output = dispatch(cli, &ctx).await?;
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
        ])
        .unwrap();
        run(&cli).await.unwrap();
    }
}
