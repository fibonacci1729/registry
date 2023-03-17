use anyhow::Result;
use clap::Parser;
use std::process::exit;
use tracing_subscriber::EnvFilter;
use warg_cli::commands::{
    DownloadCommand, InfoCommand, InitCommand, PublishCommand, RunCommand, UpdateCommand,
};
use warg_client::ClientError;

fn version() -> &'static str {
    option_env!("CARGO_VERSION_INFO").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Warg component registry client.
#[derive(Parser)]
#[clap(
    bin_name = "warg-cli",
    version,
    propagate_version = true,
    arg_required_else_help = true
)]
#[command(version = version())]
enum WargCli {
    Info(InfoCommand),
    Init(InitCommand),
    Download(DownloadCommand),
    Update(UpdateCommand),
    #[clap(subcommand)]
    Publish(PublishCommand),
    Run(RunCommand),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    if let Err(e) = match WargCli::parse() {
        WargCli::Info(cmd) => cmd.exec().await,
        WargCli::Init(cmd) => cmd.exec().await,
        WargCli::Download(cmd) => cmd.exec().await,
        WargCli::Update(cmd) => cmd.exec().await,
        WargCli::Publish(cmd) => cmd.exec().await,
        WargCli::Run(cmd) => cmd.exec().await,
    } {
        if let Some(e) = e.downcast_ref::<ClientError>() {
            describe_client_error(e);
        } else {
            eprintln!("error: {e:?}");
        }
        exit(1);
    }

    Ok(())
}

fn describe_client_error(e: &ClientError) {
    match e {
        ClientError::StorageNotInitialized => {
            eprintln!("error: {e}; use the `init` command to get started")
        }
        ClientError::MustInitializePackage { package } => {
            eprintln!(
                "error: package `{package}` is not initialized; use the `--init` option when publishing"
            )
        }
        ClientError::PackageValidationError { package, inner } => {
            eprintln!("error: the log for package `{package}` is invalid: {inner:?}")
        }
        ClientError::PackageLogEmpty { package } => {
            eprintln!(
                "error: the log for package `{package}` is empty (the registry could be lying)"
            );
            eprintln!("see issue https://github.com/bytecodealliance/registry/issues/66");
        }
        _ => eprintln!("error: {e}"),
    }
}