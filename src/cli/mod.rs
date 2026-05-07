use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rdc", version, about = "Rossum Deployment as Code")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bootstrap a new rdc project in the current directory.
    Init {
        /// Project name (written into rdc.toml).
        #[arg(long)]
        name: String,

        /// One or more env definitions of the form `<env>=<api_base>:<org_id>`.
        /// Example: `--env dev=https://demo.rossum.app/api/v1:285704`
        #[arg(long = "env", value_name = "ENV_SPEC", required = true)]
        envs: Vec<String>,
    },
    /// Pull a Rossum environment's configuration into the local snapshot.
    Pull {
        /// Environment name as defined in rdc.toml.
        env: String,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init { name, envs }) => crate::cli::init::run(&name, &envs).await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod index;
pub mod init;
pub mod pull;
