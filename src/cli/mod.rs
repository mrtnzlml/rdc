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
        #[arg(long)]
        name: String,
        #[arg(long = "env", value_name = "ENV_SPEC", required = true)]
        envs: Vec<String>,
    },
    /// Pull a Rossum environment's configuration into the local snapshot.
    Pull { env: String },
    /// Push locally-edited hooks back to the Rossum environment.
    Push { env: String },
    /// Auto-match hooks by slug between two envs and write the mapping file.
    Map { src: String, tgt: String },
    /// Show what `rdc apply --from <src> --to <tgt>` would do.
    Plan {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Push src env's hooks (with tgt overlay applied) to tgt env per the mapping.
    Apply {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Read-only health check: token, auth, lockfile, local edits.
    /// With no `env`, runs for every env defined in `rdc.toml`.
    Status {
        env: Option<String>,
    },
    /// Show diffs.
    /// `rdc diff <env>` — local snapshot vs remote (one GET per edited object).
    /// `rdc diff <a> <b>` — two local snapshots, no API calls.
    Diff {
        left: String,
        right: Option<String>,
    },
    /// Set or refresh an env's API token. Validates the token before
    /// writing to `secrets/<env>.secrets.json` (mode 0600 on Unix).
    /// Provide the token via `--token` or pipe it on stdin.
    Auth {
        env: String,
        #[arg(long)]
        token: Option<String>,
    },
    /// Recover from a corrupted or stale lockfile by re-pulling and
    /// reconstructing it. Backs up the existing lockfile to
    /// `<name>.bak.<unix-ts>`. Local snapshot files are overwritten with
    /// remote contents — back up first if you have unsaved edits.
    Repair {
        env: String,
        #[arg(long = "rebuild-lock")]
        rebuild_lock: bool,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init { name, envs }) => crate::cli::init::run(&name, &envs).await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        Some(Command::Push { env }) => crate::cli::push::run(&env).await,
        Some(Command::Map { src, tgt }) => crate::cli::deploy::map::run(&src, &tgt).await,
        Some(Command::Plan { from, to }) => crate::cli::deploy::plan::run(&from, &to).await,
        Some(Command::Apply { from, to }) => crate::cli::deploy::apply::run(&from, &to).await,
        Some(Command::Status { env }) => crate::cli::status::run(env).await,
        Some(Command::Diff { left, right }) => crate::cli::diff::run(left, right).await,
        Some(Command::Auth { env, token }) => crate::cli::auth::run(&env, token).await,
        Some(Command::Repair { env, rebuild_lock }) => {
            crate::cli::repair::run(&env, rebuild_lock).await
        }
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod auth;
pub mod deploy;
pub mod diff;
pub mod index;
pub mod init;
pub mod pull;
pub mod push;
pub mod repair;
pub mod status;
