use clap::Parser;
use keel_cli::{Cli, run};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let default_level = if cli.verbose > 1 { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match run(&cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {}", err.render(cli.verbose > 0));
            std::process::ExitCode::FAILURE
        }
    }
}
