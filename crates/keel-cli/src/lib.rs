//! `keel-cli`
//!
//! Command-line interface and binary entrypoint for Keel (`keel`).
//! Supports provider-agnostic Stage 0 cluster bootstrapping, break-glass S3 recovery,
//! two-phase authentication (Stage 0 token + OIDC PKCE), infrastructure management,
//! application catalog installation, ephemeral PKO v2 preview inspection, and running
//! the stateless control-plane server (`keel serve`).

mod cli;
mod commands;

pub use cli::Cli;
pub use commands::CliError;
use commands::{CliContext, dispatch};

/// Executes a parsed [`Cli`] command using the default `CliContext` and prints output to stdout.
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
    use crate::cli::{Commands, TenantCommands};
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

    #[test]
    fn tenant_repair_credentials_parses_with_and_without_an_id() {
        for group in ["tenant", "tenants"] {
            let cli = Cli::try_parse_from(["keel", group, "repair-credentials", "acme"]).unwrap();
            let Commands::Tenant(t) = cli.command else {
                panic!("expected tenant command");
            };
            let TenantCommands::RepairCredentials(args) = t.command;
            assert_eq!(args.tenant.as_deref(), Some("acme"));
        }
        let cli = Cli::try_parse_from(["keel", "tenant", "repair-credentials"]).unwrap();
        let Commands::Tenant(t) = cli.command else {
            panic!("expected tenant command");
        };
        let TenantCommands::RepairCredentials(args) = t.command;
        assert!(args.tenant.is_none());
        for removed in ["migrate-state", "migrate-stack-sources", "migrate-baseline"] {
            assert!(Cli::try_parse_from(["keel", "tenant", removed, "--tenant", "x"]).is_err());
        }
    }
}
