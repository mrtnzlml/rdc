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
    /// Push locally-edited resources back to the Rossum environment.
    Push {
        env: String,
        /// Scan + report what would be POSTed / PATCHed without sending
        /// anything to the API.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Align slugs within a single environment: rename any local slug
    /// that no longer matches its current JSON `name` field. Pull never
    /// moves files; this is the explicit user-driven action that brings
    /// stale slugs into alignment. Cascade-aware (queue and workspace
    /// renames move the whole subtree).
    ///
    /// For cross-env promotion, use `rdc deploy` — it builds the
    /// slug-to-slug mapping automatically.
    Map {
        env: String,
        /// Print pending renames without writing anything.
        #[arg(long)]
        check: bool,
    },
    /// Deploy a source env to a target env in one shot.
    ///
    /// First-class cross-env operation: bootstraps a fresh target (POSTing
    /// missing resources in dependency order, rewriting cross-references
    /// from src URLs to tgt URLs as it goes) AND patches existing ones for
    /// field-level deltas. Plan-before-apply: a confirmation prompt summarises
    /// what will be created / updated / deleted before any write hits the
    /// target. Idempotent: re-running on an in-sync target performs zero
    /// API calls.
    ///
    /// `rdc apply` stays the lower-level primitive for the "I already have
    /// the slugs lined up, just push field-level edits" workflow.
    Deploy {
        /// Source environment (e.g. `test`).
        src: String,
        /// Target environment (e.g. `prod`).
        tgt: String,
        /// Mirror semantics: delete tgt objects that don't exist in src.
        /// Default is additive (extras in tgt are left intact). Mirror is
        /// always gated behind an explicit confirmation, regardless of
        /// `--yes`, because the deletions are irreversible.
        #[arg(long)]
        mirror: bool,
        /// Print the plan and exit without making any remote changes.
        /// Useful for previewing a promotion in CI or before promoting
        /// to a sensitive environment. The same code paths run that
        /// would run in a real deploy (URL rewriting, drift checks,
        /// overlay application) — only the actual POST/PATCH/DELETE
        /// calls are suppressed.
        #[arg(long = "dry-run")]
        dry_run: bool,
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
        Some(Command::Push { env, dry_run }) => {
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            crate::cli::push::run(&env, interactive, dry_run).await
        }
        Some(Command::Map { env, check }) => {
            crate::cli::deploy::realign::run_within_env(&env, check, cli.yes).await
        }
        Some(Command::Deploy { src, tgt, mirror, dry_run }) => {
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            crate::cli::deploy::run::run(&src, &tgt, mirror, interactive, dry_run).await
        }
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
