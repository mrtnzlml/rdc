use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rdc", version, about = "Rossum Deployment as Code")]
pub struct Cli {
    /// Disable ANSI color in output. Also honored via `NO_COLOR`.
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,
    /// Skip interactive prompts (conflict resolver, init wizard).
    /// Conflicts fall back to the shadow-file flow; the wizard exits
    /// with usage hints. Auto-enabled when stdin isn't a TTY.
    #[arg(long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bootstrap an rdc project in the current directory, or add a new
    /// environment to an existing one. `--env` may be repeated; when
    /// omitted, prompts interactively (if stdin is a TTY).
    Init {
        #[arg(long = "env", value_name = "ENV_SPEC")]
        envs: Vec<String>,
    },
    /// Pull a Rossum environment's configuration into the local snapshot.
    Pull { env: String },
    /// Push locally-edited hooks back to the Rossum environment.
    Push { env: String },
    /// Align slugs.
    ///
    /// * `rdc map <env>` — within-env: rename any local slug that no
    ///   longer matches its current JSON `name` field. Pull never moves
    ///   files; this is the explicit user-driven action that brings
    ///   stale slugs into alignment. Cascade-aware (queue and workspace
    ///   renames move the whole subtree).
    ///
    /// * `rdc map <src> <tgt>` — cross-env: auto-match by slug and
    ///   write the mapping file used by `rdc plan` / `rdc apply`.
    Map {
        src: String,
        tgt: Option<String>,
        /// Print pending renames (within-env) or proposed matches
        /// (cross-env) without writing anything.
        #[arg(long)]
        check: bool,
    },
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
    /// Download and install the latest rdc release in place. Replaces
    /// the running binary atomically; keeps the previous binary as
    /// `<install_dir>/rdc.bak` for one-shot rollback.
    Upgrade {
        /// Pin to a specific version instead of the latest (emergency
        /// downgrade; you may need to re-pull afterward).
        #[arg(long)]
        version: Option<String>,
        /// Only check for a newer version; don't install.
        #[arg(long)]
        check: bool,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    crate::cli::resolve::set_no_color_flag(cli.no_color);

    // Once-daily passive nudge. Skipped for the upgrade command since
    // it computes the same answer fresh. Refresh runs first (tight 2s
    // timeout, silent on failure) so the cache is up-to-date by the
    // time we decide whether to print.
    if !matches!(cli.command, Some(Command::Upgrade { .. })) {
        crate::upgrade::refresh_cache_if_stale().await;
        crate::upgrade::emit_nudge_if_available();
    }

    match cli.command {
        Some(Command::Init { envs }) => crate::cli::init::run(envs).await,
        Some(Command::Pull { env }) => {
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            crate::cli::pull::run(&env, interactive).await
        }
        Some(Command::Push { env }) => {
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            crate::cli::push::run(&env, interactive).await
        }
        Some(Command::Map { src, tgt, check }) => match tgt {
            Some(tgt) => crate::cli::deploy::map::run(&src, &tgt, check).await,
            None => crate::cli::deploy::realign::run_within_env(&src, check, cli.yes).await,
        },
        Some(Command::Plan { from, to }) => crate::cli::deploy::plan::run(&from, &to).await,
        Some(Command::Apply { from, to }) => crate::cli::deploy::apply::run(&from, &to).await,
        Some(Command::Status { env }) => crate::cli::status::run(env).await,
        Some(Command::Diff { left, right }) => crate::cli::diff::run(left, right).await,
        Some(Command::Auth { env, token }) => crate::cli::auth::run(&env, token).await,
        Some(Command::Repair { env, rebuild_lock }) => {
            crate::cli::repair::run(&env, rebuild_lock).await
        }
        Some(Command::Upgrade { version, check }) => {
            let target = match version {
                Some(v) => Some(crate::upgrade::Version::parse(&v)?),
                None => None,
            };
            crate::upgrade::run_upgrade(target, check).await
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
pub mod resolve;
pub mod status;
