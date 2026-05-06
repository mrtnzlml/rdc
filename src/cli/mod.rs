use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rdc", version, about = "Rossum Deployment as Code")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bootstrap a new rdc project in the current directory
    Init,
    /// Pull a Rossum environment's configuration into the local snapshot
    Pull {
        /// Environment name as defined in rdc.toml (e.g., dev, test, prod)
        env: String,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init) => crate::cli::init::run().await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        None => {
            // No subcommand: print help and exit 0
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod init;
pub mod pull;
