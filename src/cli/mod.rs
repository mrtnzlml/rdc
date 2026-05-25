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
        .keys()
        .map(|name| CompletionCandidate::new(name))
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
    /// Deploy a source env to a target env in one shot.
    ///
    /// First-class cross-env operation: bootstraps a fresh target (POSTing
    /// missing resources in dependency order, rewriting cross-references
    /// from src URLs to tgt URLs as it goes) AND patches existing ones for
    /// field-level deltas. Diff-before-apply: the full per-object diff
    /// (create bodies, update diffs, delete bodies) prints before the
    /// confirmation prompt so the user commits with the actual delta in
    /// hand. Idempotent: re-running on an in-sync target performs zero
    /// write API calls.
    Deploy {
        /// Source environment (e.g. `test`). Picks interactively when omitted.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        src: Option<String>,
        /// Target environment (e.g. `prod`). Picks interactively when omitted.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        tgt: Option<String>,
        /// Mirror semantics: delete tgt objects that don't exist in src.
        /// Default is additive (extras in tgt are left intact). Mirror is
        /// always gated behind an explicit confirmation, regardless of
        /// `--yes`, because the deletions are irreversible.
        #[arg(long)]
        mirror: bool,
        /// Print the full diff and exit without making any remote changes.
        /// Useful for previewing a promotion in CI or before promoting
        /// to a sensitive environment. The same code paths run that
        /// would run in a real deploy (URL rewriting, drift checks,
        /// overlay application) — only the actual POST/PATCH/DELETE
        /// calls are suppressed.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Auto-overwrite target objects that have been edited out-of-band
        /// since the last `rdc sync <tgt>`. Without this flag, the deploy
        /// prompts per-object [k]/[o]/[s]/[a] on TTY, or refuses on
        /// non-TTY / `--yes` to prevent a CI script from silently blowing
        /// away ad-hoc edits made via the Rossum UI.
        #[arg(long = "force-overwrite-drift")]
        force_overwrite_drift: bool,
        /// Limit the deploy to the given `<kind>/<slug>` selectors. Repeatable.
        /// Globs: `*` matches within the slug segment (e.g. `hooks/*`,
        /// `schemas/cost-*`). Cross-kind: `*/cost-invoices` matches any kind.
        /// Email templates use the compound `<ws>/<q>/<tpl>` slug, e.g.
        /// `email_templates/main/cost-invoices/rejection`. Without any
        /// `--only`, deploy operates on the whole snapshot (default).
        #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
        only: Vec<String>,
    },
    /// Show diffs.
    /// `rdc diff <env>` — local snapshot vs remote (one GET per edited object).
    /// `rdc diff <a> <b>` — two local snapshots, no API calls.
    Diff {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        left: String,
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        right: Option<String>,
    },
    /// Set or refresh an env's API token. Validates the token before
    /// writing to `secrets/<env>.secrets.json` (mode 0600 on Unix).
    /// Provide the token via `--token` or pipe it on stdin. Without
    /// `<env>`, picks interactively from envs defined in `rdc.toml`.
    Auth {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Bring the local snapshot of `<env>` back into a clean state.
    /// Pick one of the modes — there's no implicit default because they
    /// touch on-disk files (and `--fix-store-anomaly` also touches the
    /// remote) in irreversible ways:
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
    /// * `--fix-store-anomaly` — repair hooks with
    ///   `extension_source: "rossum_store"` and `hook_template: null`
    ///   (created when a client PATCHes the marker without going
    ///   through `/hooks/create`). Interactive per hook: convert to
    ///   custom (one PATCH, id preserved) or reinstall as store
    ///   extension (new id, dependents rewired).
    Repair {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Re-pull from remote and reconstruct the lockfile. Backs up
        /// the existing one to `<name>.bak.<unix-ts>`. Destroys local
        /// edits not present on remote.
        #[arg(long = "rebuild-lock", conflicts_with_all = ["rename_slugs", "fix_store_anomaly"])]
        rebuild_lock: bool,
        /// Rename local files whose slug no longer matches their JSON
        /// `name` field. Offline (no API calls).
        #[arg(long = "rename-slugs", conflicts_with_all = ["fix_store_anomaly"])]
        rename_slugs: bool,
        /// Repair hooks with `extension_source: "rossum_store"` and
        /// `hook_template: null`. Interactive per hook: convert to
        /// custom (one PATCH) or reinstall as store extension (new
        /// hook id, dependents rewired). Non-TTY default: convert;
        /// override with env var `RDC_REPAIR_CURE=reinstall` (or
        /// `=skip`).
        #[arg(long = "fix-store-anomaly")]
        fix_store_anomaly: bool,
        /// With `--rename-slugs` or `--fix-store-anomaly`: print the
        /// plan and exit without writing anything.
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
        Some(Command::Deploy { src, tgt, mirror, dry_run, force_overwrite_drift, only }) => {
            let src = crate::cli::env_picker::pick_env("Deploy from which env (source)?", src)?;
            let tgt = crate::cli::env_picker::pick_env_excluding(
                "Deploy to which env (target)?",
                tgt,
                &[&src],
            )?;
            let interactive = crate::cli::resolve::is_interactive(cli.yes);
            with_401_retry_envs(&[&src, &tgt], || {
                let only = only.clone();
                crate::cli::deploy::run::run(&src, &tgt, mirror, interactive, dry_run, force_overwrite_drift, only)
            })
            .await
        }
        Some(Command::Diff { left, right }) => crate::cli::diff::run(left, right).await,
        Some(Command::Auth { env, token }) => {
            let env = crate::cli::env_picker::pick_env("Set token for which env?", env)?;
            crate::cli::auth::run(&env, token).await
        }
        Some(Command::Repair { env, rebuild_lock, rename_slugs, fix_store_anomaly, check }) => {
            let env = crate::cli::env_picker::pick_env("Which env to repair?", env)?;
            with_401_retry(&env, || {
                crate::cli::repair::run(&env, rebuild_lock, rename_slugs, fix_store_anomaly, check, cli.yes)
            })
            .await
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
            crate::cli::auth::refresh_token_interactively(env).await?;
            op().await
        }
        other => other,
    }
}

/// Like [`with_401_retry`] but for commands that juggle more than one env
/// (notably `rdc deploy`, which holds clients for both src and tgt).
///
/// On a 401 we use the [`EnvTag`](crate::api::EnvTag) that the failing
/// client attached to the error chain to refresh the right env's token,
/// then retry the operation once. If no tag is present (a 401 from an
/// untagged code path), we fall back to prompting the user which env to
/// refresh — that keeps the "never fail with a raw 401" promise even for
/// callers that haven't opted into env-tagging yet.
async fn with_401_retry_envs<F, Fut>(envs: &[&str], op: F) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let first = op().await;
    match first {
        Err(e) if crate::api::anyhow_has_status(&e, 401) => {
            let target_env = match crate::api::anyhow_status_env(&e, 401) {
                Some(env_from_tag) => env_from_tag,
                None => prompt_pick_env_for_401(envs)?,
            };
            crate::cli::auth::refresh_token_interactively(&target_env).await?;
            op().await
        }
        other => other,
    }
}

/// Interactive fallback when a 401 isn't tagged with an env. Lets the
/// user pick which env's token to refresh. Bails (with the original 401
/// hint) on non-TTY contexts.
fn prompt_pick_env_for_401(envs: &[&str]) -> anyhow::Result<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Rossum API returned 401 for one of: {}. \
             Re-run on a TTY to refresh interactively, or run \
             `rdc auth <env> --token <new-token>` for the affected env.",
            envs.join(", ")
        );
    }
    if envs.len() == 1 {
        return Ok(envs[0].to_string());
    }
    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    log.event(
        crate::log::Action::Auth,
        "token rejected (401); which env's token needs refreshing?",
    );
    let choices: Vec<String> = envs.iter().map(|s| s.to_string()).collect();
    let picked = inquire::Select::new("Refresh token for which env?", choices)
        .prompt()
        .map_err(|e| anyhow::anyhow!("env prompt failed: {e}"))?;
    Ok(picked)
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
pub mod diff;
pub mod env_picker;
pub mod index;
pub mod init;
pub mod pull;
pub mod push;
pub mod repair;
pub mod resolve;
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
        let cands: Vec<String> = env_name_candidates_in(dir.path())
            .into_iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        // BTreeMap iteration order is alphabetical, so dev comes before prod.
        assert_eq!(cands, vec!["dev".to_string(), "prod".to_string()]);
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
