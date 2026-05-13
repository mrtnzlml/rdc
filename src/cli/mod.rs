use clap::{Parser, Subcommand};

/// Cargo-style colour palette for `--help`, error messages, and usage
/// strings. Picked up from the `clap_cargo` crate so the look matches
/// other modern Rust CLIs (cargo, rustup, rustfmt).
const CLI_STYLES: clap::builder::Styles = clap_cargo::style::CLAP_STYLING;

#[derive(Debug, Parser)]
#[command(name = "rdc", version, about = "Rossum Deployment as Code", styles = CLI_STYLES)]
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
        /// Scan + report what would be POSTed / PATCHed / DELETEd
        /// without sending anything to the API.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Print a unified diff per changed file (local vs current
        /// remote, or the would-be POST body for new resources, or the
        /// remote body for would-be deletes). Requires `--dry-run`;
        /// one GET per changed object.
        #[arg(long = "diff", requires = "dry_run")]
        diff: bool,
        /// Authorize destructive deletes for objects whose local file
        /// is missing but whose lockfile entry remains. Required on
        /// non-TTY (CI); on a TTY this flag skips the per-batch
        /// confirmation prompt that would otherwise be shown.
        /// `--yes` does NOT bypass delete confirmation.
        #[arg(long = "allow-deletes")]
        allow_deletes: bool,
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
        /// In addition to the plan summary, print full unified diffs
        /// per object: the would-be POST body for creates, the
        /// src-vs-tgt diff for updates, and the would-be-removed body
        /// for deletes (`--mirror` only). Both `.json` and any
        /// extracted `.py` / formula files are shown. Requires
        /// `--dry-run`.
        #[arg(long = "diff", requires = "dry_run")]
        diff: bool,
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
    /// Bring the local snapshot of `<env>` back into a clean state.
    /// Pick one of the modes — there's no implicit default because both
    /// touch on-disk files in irreversible ways:
    ///
    /// * `--rebuild-lock` — back up the existing lockfile and re-pull
    ///   from remote. Local snapshot files are overwritten with remote
    ///   contents. Used after a lockfile corruption or a hash-input
    ///   change in a new rdc release.
    /// * `--rename-slugs` — rename any local file whose slug no longer
    ///   matches its JSON `name`. Pull never moves files; this is the
    ///   explicit user-driven action that brings stale slugs into
    ///   alignment. Cascade-aware (queue / workspace renames move the
    ///   whole subtree). Offline — no API calls.
    Repair {
        env: String,
        /// Re-pull from remote and reconstruct the lockfile. Backs up
        /// the existing one to `<name>.bak.<unix-ts>`. Destroys local
        /// edits not present on remote.
        #[arg(long = "rebuild-lock", conflicts_with = "rename_slugs")]
        rebuild_lock: bool,
        /// Rename local files whose slug no longer matches their JSON
        /// `name` field. Offline (no API calls).
        #[arg(long = "rename-slugs")]
        rename_slugs: bool,
        /// With `--rename-slugs`: print pending renames and exit
        /// without writing anything.
        #[arg(long)]
        check: bool,
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
        Some(Command::Push { env, dry_run, diff, allow_deletes }) => {
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            crate::cli::push::run(&env, interactive, dry_run, diff, allow_deletes).await
        }
        Some(Command::Deploy { src, tgt, mirror, dry_run, diff }) => {
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            crate::cli::deploy::run::run(&src, &tgt, mirror, interactive, dry_run, diff).await
        }
        Some(Command::Status { env }) => crate::cli::status::run(env).await,
        Some(Command::Diff { left, right }) => crate::cli::diff::run(left, right).await,
        Some(Command::Auth { env, token }) => crate::cli::auth::run(&env, token).await,
        Some(Command::Repair { env, rebuild_lock, rename_slugs, check }) => {
            crate::cli::repair::run(&env, rebuild_lock, rename_slugs, check, cli.yes).await
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
