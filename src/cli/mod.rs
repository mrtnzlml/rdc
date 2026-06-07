use clap::builder::styling::{AnsiColor, Color, Effects, RgbColor, Style, Styles};
use clap::{Parser, Subcommand};
use clap_complete::{ArgValueCandidates, CompletionCandidate};

/// Dynamic shell-completion candidates for env-name args. Reads
/// `rdc.toml` from the current working directory and returns each
/// defined env as a candidate. Silent fallback to an empty `Vec` when:
///
/// - CWD can't be resolved.
/// - No `rdc.toml` exists (user isn't in a project — completion stays
///   empty rather than spamming an error mid-keystroke).
/// - `rdc.toml` is unparseable.
///
/// Called by clap_complete each time the shell asks for candidates,
/// so it must be cheap and side-effect-free. Reading the small toml
/// file is fast enough; no caching layered on top.
fn env_name_candidates() -> Vec<CompletionCandidate> {
    let Ok(cwd) = std::env::current_dir() else { return Vec::new() };
    env_name_candidates_in(&cwd)
}

/// Inner form with an injected project root. Lets tests cover the
/// rdc.toml reading branch without mutating process-wide CWD, which is
/// unsound to do concurrently with other tests.
fn env_name_candidates_in(project_root: &std::path::Path) -> Vec<CompletionCandidate> {
    let Ok(cfg) = crate::config::ProjectConfig::load(&project_root.join("rdc.toml")) else {
        return Vec::new();
    };
    cfg.envs
        .iter()
        .map(|(name, env)| {
            // Each env gets a *unique* description, which is load-bearing for
            // completion ordering under zsh — for two separate reasons:
            //
            //   1. `_describe` renders matches that HAVE a description ahead of
            //      those that don't. A bare env name (no description) sinks into
            //      the trailing undescribed bucket, below every described flag.
            //   2. zsh groups matches that SHARE a description and then
            //      re-sorts; the regrouping pushes the whole env block below the
            //      alphabetically-first flags (`-…` sorts before letters). It is
            //      common for several envs to target one cluster (same
            //      api_base), so api_base alone is not unique — the `org_id`
            //      suffix keeps the descriptions distinct.
            //
            // A unique, present description keeps every env in the same match
            // group as the flags, where clap_complete already emits envs ahead
            // of options. See zsh's `_describe` / `compdescribe -g` machinery.
            let desc = format!("{} (org {})", env.api_base, env.org_id);
            CompletionCandidate::new(name).help(Some(desc.into()))
        })
        .collect()
}

/// Help / error / usage palette inspired by Claude Code: warm amber
/// accents on a clean, theme-agnostic base. Truecolor (24-bit) is used
/// where the exact hue matters — modern terminals (iTerm2, Alacritty,
/// kitty, Windows Terminal, VS Code, recent Apple Terminal) render
/// these as-is; older terminals downsample to the closest 256-color.
///
/// Hues:
/// - `AMBER` (#ED8E47): primary accent — section headers, usage,
///   conflict markers, action-letter brackets.
/// - `GRAY`  (#888888): medium gray for placeholders — readable on
///   both light and dark backgrounds without competing with body text.
const AMBER: Color = Color::Rgb(RgbColor(237, 142, 71));
const GRAY: Color = Color::Rgb(RgbColor(136, 136, 136));

const HEADER: Style = Style::new().fg_color(Some(AMBER)).effects(Effects::BOLD);
const LITERAL: Style = Style::new().effects(Effects::BOLD);
const PLACEHOLDER: Style = Style::new().fg_color(Some(GRAY));
const ERROR: Style = AnsiColor::BrightRed.on_default().effects(Effects::BOLD);
const VALID: Style = AnsiColor::Green.on_default().effects(Effects::BOLD);
const INVALID: Style = Style::new().fg_color(Some(AMBER)).effects(Effects::BOLD);

const CLI_STYLES: Styles = Styles::styled()
    .header(HEADER)
    .usage(HEADER)
    .literal(LITERAL)
    .placeholder(PLACEHOLDER)
    .error(ERROR)
    .valid(VALID)
    .invalid(INVALID);

#[derive(Debug, Parser)]
#[command(
    name = "rdc",
    version,
    about = "Rossum Deployment as Code",
    styles = CLI_STYLES,
    disable_help_subcommand = true,
)]
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
    /// Reconcile the local snapshot and the env's remote state in one pass.
    /// Without `<env>`, picks interactively from envs defined in `rdc.toml`
    /// (or auto-selects when only one exists).
    Sync {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Print the plan and exit without making any changes.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Permit local-tombstone → remote DELETE without per-object prompts.
        #[arg(long = "allow-deletes")]
        allow_deletes: bool,
        /// Audit mode: pull changes into local but never write to the remote.
        #[arg(long = "no-push", conflicts_with = "no_pull")]
        no_push: bool,
        /// Deploy mode: write local edits to the remote but never overwrite local files.
        #[arg(long = "no-pull", conflicts_with = "no_push")]
        no_pull: bool,
        /// Watch local files + poll the env continuously; reconcile on each event.
        /// On a TTY, pressing Enter triggers a cycle immediately (ignored while
        /// a cycle is already running).
        #[arg(long = "watch", conflicts_with_all = ["dry_run"])]
        watch: bool,
        /// Poll cadence for remote drift in watch mode. Accepts human durations
        /// (`30s`, `2m`, `5m`). Default `60s`.
        #[arg(long = "poll-interval", value_name = "DURATION", default_value = "60s", requires = "watch")]
        poll_interval: String,
        /// Disable remote polling in watch mode. Outbound (file-event) sync stays.
        #[arg(long = "no-poll", requires = "watch", conflicts_with = "poll_interval")]
        no_poll: bool,
        /// Print every cycle in watch mode, including no-op cycles.
        #[arg(short = 'v', long = "verbose", requires = "watch")]
        verbose: bool,
    },
    /// Removed: replaced by `rdc migrate <src> <tgt>` + `rdc sync <tgt>`.
    ///
    /// Hidden from help. Invoking it emits a guiding error pointing at the
    /// replacement workflow rather than a generic "unrecognized subcommand".
    /// Accepts (and ignores) the former positionals/flags so the error is
    /// reached regardless of how the old command was invoked.
    #[command(hide = true)]
    Deploy {
        /// Former source environment (ignored).
        src: Option<String>,
        /// Former target environment (ignored).
        tgt: Option<String>,
        /// Former `--mirror` flag (ignored).
        #[arg(long)]
        mirror: bool,
        /// Former `--dry-run` flag (ignored).
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Former `--force-overwrite-drift` flag (ignored).
        #[arg(long = "force-overwrite-drift")]
        force_overwrite_drift: bool,
        /// Former `--only` selectors (ignored).
        #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
        only: Vec<String>,
    },
    /// Migrate a source env's snapshot into a target env's snapshot, locally.
    ///
    /// Pure-local, ZERO remote calls: copies `envs/<src>/` into `envs/<tgt>/`,
    /// renaming slugs per the auto-matched mapping, rewriting portable
    /// `rdc://<kind>/<slug>` references from src slugs to tgt slugs, and
    /// applying the target env's overlay. Afterwards review the changes with
    /// `git diff` and run `rdc sync <tgt>` to push them (sync creates objects
    /// in dependency order). This replaces the remote half of `rdc deploy`
    /// with an offline, reviewable file transform.
    Migrate {
        /// Source environment (e.g. `test`). Picks interactively when omitted.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        src: Option<String>,
        /// Target environment (e.g. `prod`). Picks interactively when omitted.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        tgt: Option<String>,
        /// Mirror semantics: delete tgt snapshot objects that don't exist in
        /// src. Default is additive (extras in tgt are left intact). The
        /// deletions are local file removals — review `git diff` before sync.
        #[arg(long)]
        mirror: bool,
        /// Print the plan (per-file source -> target remap, prunes) and exit
        /// without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Limit the migration to the given `<kind>/<slug>` selectors.
        /// Repeatable. Globs: `*` matches within the slug segment (e.g.
        /// `hooks/*`, `schemas/cost-*`). Cross-kind: `*/cost-invoices`.
        /// Without any `--only`, migrate operates on the whole snapshot.
        #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
        only: Vec<String>,
    },
    /// Set or refresh an env's API token. Validates the token before
    /// writing to `secrets/<env>.secrets.json` (mode 0600 on Unix).
    ///
    /// Provide credentials via one of:
    /// * `--token <T>` — explicit token (CI-friendly).
    /// * `--username <U>` — exchanges <U> + password (stdin or TTY
    ///   prompt) for a token via POST /v1/auth/login; the token and
    ///   computed expiry (162h from now) are written to the secrets file.
    /// * Neither — read a token from stdin (back-compat with today).
    ///
    /// Without `<env>`, picks interactively from envs defined in `rdc.toml`.
    Auth {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        #[arg(long, conflicts_with = "username")]
        token: Option<String>,
        #[arg(long, conflicts_with = "token")]
        username: Option<String>,
    },
    /// Diagnose and fix the local snapshot for `<env>` in one pass. Runs
    /// every fix automatically and only prompts where a real decision is
    /// needed. Without `<env>`, picks interactively from `rdc.toml`.
    ///
    /// Steps, in order:
    /// 1. Report local changes not yet pushed to the remote, so you know
    ///    what's at stake before anything destructive.
    /// 2. Rename any local file whose slug no longer matches its JSON
    ///    `name` — offline, cascade-aware (queue / workspace renames move
    ///    the whole subtree), applied automatically.
    /// 3. Fix hooks with `extension_source: "rossum_store"` and
    ///    `hook_template: null` (created when a client PATCHes the marker
    ///    without going through `/hooks/create`) — prompts per hook to
    ///    convert to custom (one PATCH, id preserved) or reinstall as a
    ///    store extension (new id, dependents rewired).
    /// 4. Offer to rebuild the lockfile by re-pulling from the remote —
    ///    overwrites local snapshot files; local edits not on the remote
    ///    are LOST. Interactively this is an explicit confirm (default No);
    ///    it is skipped under `--yes` / non-TTY unless `--rebuild-lock` is
    ///    passed to authorize it directly.
    Doctor {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Authorize the destructive lockfile rebuild directly, skipping the
        /// interactive confirm (for scripts / `--yes`). Re-pulls from remote
        /// and overwrites local snapshot files; local edits not on the
        /// remote are LOST.
        #[arg(long = "rebuild-lock")]
        rebuild_lock: bool,
        /// Preview every step without writing, prompting, or calling the remote.
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
        Some(Command::Sync {
            env,
            dry_run,
            allow_deletes,
            no_push,
            no_pull,
            watch,
            poll_interval,
            no_poll,
            verbose,
        }) => {
            let env = crate::cli::env_picker::pick_env("Which env to sync?", env)?;
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            if watch {
                let poll = if no_poll {
                    None
                } else {
                    Some(parse_duration(&poll_interval)?)
                };
                with_401_retry(&env, || {
                    crate::cli::sync::watch::run_watch(
                        &env,
                        interactive,
                        allow_deletes,
                        no_push,
                        no_pull,
                        poll,
                        verbose,
                    )
                })
                .await
            } else {
                with_401_retry(&env, || {
                    crate::cli::sync::run(&env, interactive, dry_run, allow_deletes, no_push, no_pull)
                })
                .await
            }
        }
        Some(Command::Deploy { .. }) => {
            anyhow::bail!(
                "`rdc deploy` has been replaced. Run `rdc migrate <src> <tgt>` to produce \
                 the target snapshot locally, review the diff, then `rdc sync <tgt>` to push it."
            )
        }
        Some(Command::Migrate { src, tgt, mirror, dry_run, only }) => {
            let src = crate::cli::env_picker::pick_env("Migrate from which env (source)?", src)?;
            let tgt = crate::cli::env_picker::pick_env_excluding(
                "Migrate to which env (target)?",
                tgt,
                &[&src],
            )?;
            // Pure-local: no remote calls, so no 401-retry wrapper needed.
            crate::cli::migrate::run(&src, &tgt, mirror, dry_run, only)
        }
        Some(Command::Auth { env, token, username }) => {
            let env = crate::cli::env_picker::pick_env("Set token for which env?", env)?;
            crate::cli::auth::run(&env, token, username).await
        }
        Some(Command::Doctor { env, rebuild_lock, check }) => {
            let env = crate::cli::env_picker::pick_env("Which env to run the doctor on?", env)?;
            with_401_retry(&env, || crate::cli::doctor::run(&env, rebuild_lock, check, cli.yes)).await
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
            Ok(())
        }
    }
}

/// Run an env-scoped API operation and, if it fails with HTTP 401,
/// prompt the user for a fresh token, save it, and retry the operation
/// once. The closure must be re-callable; we invoke it twice when the
/// first call's error chain contains an `ApiError::Status { status: 401 }`.
///
/// Non-TTY contexts (CI, piped) skip the prompt and surface the
/// original error annotated with a hint to run `rdc auth <env>`.
async fn with_401_retry<F, Fut>(env: &str, op: F) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let first = op().await;
    match first {
        Err(e) if crate::api::anyhow_has_status(&e, 401) => {
            crate::cli::auth::refresh_token_for_401(env).await?;
            op().await
        }
        other => other,
    }
}

/// Parse a human-friendly duration string (`30s`, `2m`, `5m`, `1h`) into
/// a [`std::time::Duration`]. Plain integers are treated as seconds.
/// Used to validate `--poll-interval` after clap accepts it as a string.
fn parse_duration(s: &str) -> anyhow::Result<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(s.len()),
    );
    let n: u64 = num.parse().map_err(|_| {
        anyhow::anyhow!("invalid duration '{s}'; expected forms like '30s', '2m', '5m'")
    })?;
    match unit {
        "s" | "" => Ok(std::time::Duration::from_secs(n)),
        "m" => Ok(std::time::Duration::from_secs(n * 60)),
        "h" => Ok(std::time::Duration::from_secs(n * 3600)),
        _ => anyhow::bail!("invalid duration unit '{unit}'; use s / m / h"),
    }
}

pub mod auth;
pub mod deploy;
pub mod env_picker;
pub mod index;
pub mod init;
pub mod migrate;
pub mod pull;
pub mod push;
pub mod doctor;
pub mod resolve;
pub(crate) mod stdin_coord;
pub mod sync;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn env_name_candidates_returns_envs_from_rdc_toml() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rdc.toml"),
            r#"
[envs.dev]
api_base = "https://dev.example/api/v1"
org_id = 1

[envs.prod]
api_base = "https://prod.example/api/v1"
org_id = 2
"#,
        )
        .unwrap();
        let cands = env_name_candidates_in(dir.path());
        let values: Vec<String> = cands
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        // BTreeMap iteration order is alphabetical, so dev comes before prod.
        assert_eq!(values, vec!["dev".to_string(), "prod".to_string()]);
        // Every candidate must carry a description (api_base + org_id). This is
        // what keeps env names in zsh's "described" match group so they render
        // above the flags rather than in the trailing undescribed bucket.
        let helps: Vec<String> = cands
            .iter()
            .map(|c| c.get_help().expect("env candidate has a description").to_string())
            .collect();
        assert_eq!(
            helps,
            vec![
                "https://dev.example/api/v1 (org 1)".to_string(),
                "https://prod.example/api/v1 (org 2)".to_string(),
            ]
        );
    }

    #[test]
    fn env_candidates_have_unique_descriptions_when_api_base_shared() {
        // Two envs on the same cluster share an api_base. Their completion
        // descriptions MUST stay distinct: zsh groups matches that carry an
        // identical description and re-sorts the result, which sinks the env
        // block below the flags. The org_id suffix is what keeps them apart.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rdc.toml"),
            r#"
[envs.dev-a]
api_base = "https://shared.rossum.app/api/v1"
org_id = 1

[envs.dev-b]
api_base = "https://shared.rossum.app/api/v1"
org_id = 2
"#,
        )
        .unwrap();
        let helps: Vec<String> = env_name_candidates_in(dir.path())
            .iter()
            .map(|c| c.get_help().expect("description present").to_string())
            .collect();
        assert_eq!(helps.len(), 2);
        assert_ne!(
            helps[0], helps[1],
            "envs sharing an api_base must still get distinct descriptions"
        );
    }

    #[test]
    fn env_name_candidates_silent_when_no_rdc_toml() {
        // Shell completion fires on every keystroke; if the user is
        // outside a project we must NOT bubble an error — just return
        // no candidates so the shell falls back to flags-only.
        let dir = TempDir::new().unwrap();
        assert!(env_name_candidates_in(dir.path()).is_empty());
    }

    #[test]
    fn env_name_candidates_silent_when_rdc_toml_unparseable() {
        // Same contract: a malformed project file shouldn't make the
        // user's TAB key feel broken. Surface zero candidates instead.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("rdc.toml"), "this is { not [valid toml")
            .unwrap();
        assert!(env_name_candidates_in(dir.path()).is_empty());
    }
}
