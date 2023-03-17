use super::CommonOptions;
use anyhow::Result;
use clap::Args;

/// Update all local package logs for a registry.
#[derive(Args)]
pub struct UpdateCommand {
    /// The common command options.
    #[clap(flatten)]
    pub common: CommonOptions,
}

impl UpdateCommand {
    /// Executes the command.
    pub async fn exec(self) -> Result<()> {
        println!("updating package logs to the latest available versions...");
        let mut client = self.common.create_client().await?;
        client.update().await?;
        Ok(())
    }
}