use crate::cli::migrate::CarryGroup;
use crate::cli::resolve::ConflictStrategy;
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

// Long-form help for `rdc --help`. `rdc -h` keeps the one-line `about`.
//
// Written for a reader — or an agent — that has to drive rdc without having
// read the README: what a project is made of, which two commands do the work,
// and what changes when no terminal is attached. Held to the same rule as the
// rest of this file: every sentence names something the code actually does.
//
// A `&str` constant rather than a doc comment because clap_derive folds a doc
// comment's single newlines into spaces, which would collapse the layout
// blocks below into paragraphs. Hard-wrapped at 78 columns for the same
// reason: clap is built without `wrap_help`, so it prints what is written
// here.
const ROOT_LONG_ABOUT: &str = r#"Rossum Deployment as Code. Keeps a Rossum organization's configuration in git
and reconciles it with the remote organization.

Project layout:

  rdc.toml                    one [envs.<env>] block per environment, with
                              api_base and org_id.
  envs/<env>/                 the snapshot: one JSON file per object, plus
                              .py/.js files for hook code, rule trigger
                              conditions and queue formulas.
  envs/<env>/_index.md        every object in the env, one line each, with
                              its references. Every sync rewrites it.
  envs/<env>/overlay.toml     per-env field values. Only migrate applies them.
  secrets/<env>.secrets.json  the cached API token. Gitignored, mode 0600 on
                              Unix.
  .rdc/state/<env>.lock.json  slug -> remote id, and the merge base hashes.
                              Managed by rdc.
  .rdc/mapping.toml           slugs that differ between envs. Editable;
                              `rdc doctor` also writes to it.

Main commands:

  rdc sync <env>              push local changes to one env and pull its
                              remote changes.
  rdc migrate <src> <tgt>     copy one env's snapshot onto another, locally.
                              Run `rdc sync <tgt>` to push the result.

Managed objects: workspaces, queues, schemas, inboxes, email templates, hooks,
rules, labels, saved views, engines, engine fields, organization settings and
Master Data Hub datasets. Workflows and workflow steps are pulled, never
pushed.

Without a terminal (CI, agents):

  * Credentials come from $RDC_TOKEN_<ENV>, or from $RDC_USER_<ENV> and
    $RDC_PASS_<ENV>. <ENV> is the env name in upper case, with every
    non-alphanumeric character replaced by '_': env `dev-us` reads
    $RDC_TOKEN_DEV_US.
  * Every command that takes <env> requires it.
  * `rdc sync` writes conflicts to .rdc/conflicts/<env>/ and keeps the local
    file, unless `--conflict <strategy>` is given.
  * Remote deletes require `rdc sync --allow-deletes`. `--yes` does not
    allow them.
  * Errors exit 1 with the reason on stderr. Colour is off when the output
    is not a terminal or NO_COLOR is set to a non-empty value."#;

const INIT_LONG_ABOUT: &str = r#"Create an rdc project in the current directory, or add environments to an
existing one.

Writes rdc.toml, secrets/, an empty envs/<env>/ per new env, and the
scaffold: CLAUDE.md, README.md, .gitlab-ci.yml, .gitignore, .gitattributes
and a pytest harness for formulas and hooks. In an existing project it
updates only the rdc regions of those files, and adds missing rdc lines to
.gitignore and .gitattributes. See --force.

It then authenticates each new env with $RDC_TOKEN_<ENV>, or with a masked
prompt on a terminal. If the token is rejected, the project files stay; run
`rdc auth <env>` to retry. On a terminal, with at least one env
authenticated, init offers to run the first sync.

Without a terminal, --env is required and init does not prompt."#;

const SYNC_LONG_ABOUT: &str = r#"Reconcile one env's local snapshot with its remote organization.

Each run:

  1. Check envs/<env>/ offline for invalid JSON, a field over the API length
     limit, a missing field the API requires on create, a broken
     organization `settings` structure, an unshared saved view, a queue
     bound to more than one engine, and a formula field without a formula.
     Any finding stops the run before the first remote write. --dry-run
     lists them; --no-push skips the check.
  2. List the env and classify every object by comparing the local file,
     the lockfile base hash and the remote copy (three-way merge).
  3. Resolve objects that changed on both sides. See --conflict.
  4. Delete remote objects whose local file was removed, children first.
     Requires --allow-deletes or a confirmed prompt.
  5. Push local creates and edits, parents first.
  6. Pull remote creates and edits.
  7. Rewrite envs/<env>/_index.md and the lockfile.

Master Data Hub datasets are pushed and pulled separately in the same run.

A change on both sides: on a terminal, rdc prompts for it. With --conflict,
rdc applies the strategy. Otherwise rdc writes the remote copy to
.rdc/conflicts/<env>/, keeps the local file, and reports the same conflict
on the next sync.

To delete a remote object, remove its local file and run sync with
--allow-deletes. Only objects recorded in the lockfile are deleted."#;

const MIGRATE_LONG_ABOUT: &str = r#"Copy a source env's snapshot onto a target env's snapshot. Makes no remote
calls.

Copies envs/<src>/ onto envs/<tgt>/, then:

  * renames slugs listed in .rdc/mapping.toml (equal slugs need no entry);
  * rewrites `rdc://<kind>/<slug>` references to the target's slugs;
  * applies envs/<tgt>/overlay.toml;
  * keeps the target's per-organization fields. See --carry.

A file at envs/<tgt>/overlay/<relpath> replaces the migrated sidecar at
<relpath>. Sidecars are hook and rule .py/.js files, queue
formulas/<field>.py files and MDH dataset data.jsonl files. An overlay file
that matches no migrated sidecar stops the migration.

Review the result with `git diff`, then run `rdc sync <tgt>` to push it."#;

const AUTH_LONG_ABOUT: &str = r#"Set or refresh an env's API token.

rdc validates the token with GET /organizations/<org_id>, then writes it to
secrets/<env>.secrets.json (mode 0600 on Unix).

Credential sources:

  --token <T>      use this token.
  --username <U>   read a password (masked prompt on a terminal, otherwise
                   stdin), log in at POST /v1/auth/login, and cache the token
                   with a 162-hour expiry.
  --browser        enter the token in a local browser form.
  none of these    read a token from stdin, e.g. `rdc auth dev < token.txt`.

Every command looks for a token in this order: $RDC_TOKEN_<ENV>; an
unexpired token in secrets/<env>.secrets.json; a username and password from
that file or from $RDC_USER_<ENV> and $RDC_PASS_<ENV>. With those variables
set, `rdc auth` is not needed."#;

const DOCTOR_LONG_ABOUT: &str = r#"Check and repair the local snapshot of <env>. Offline: no API calls and no
prompts. Writes unless --dry-run is given.

Reports, without changing anything:

  * local objects that differ from the lockfile base (not yet pushed);
  * a field longer than the API limit;
  * a field the API requires on create that a new object lacks;
  * a queue bound to more than one engine;
  * a formula field without a formula.

`rdc sync <env>` does not push while any of the last four exist.

Repairs:

  * a file whose slug does not match its JSON `name`: renames it (a queue or
    workspace moves with its subtree) and records the rename in
    .rdc/mapping.toml;
  * a base-cache entry in .rdc/state/<env>.base/ whose object is no longer
    in the env tree: removes it.

Both repairs need the lockfile. If the env has none, doctor skips them; run
`rdc sync <env>` to create it."#;

const UPGRADE_LONG_ABOUT: &str = r#"Replace the running rdc binary with a release build.

Downloads the release asset for this platform from GitHub and replaces the
binary atomically. The previous binary is kept as <install_dir>/rdc.bak.

Refuses, and prints the manual steps instead, when the binary is under the
cargo bin directory or its directory is not writable."#;

// `infer_subcommands` lets an unambiguous prefix stand in for a verb: `rdc i`
// is `rdc init`, `rdc do` is `rdc doctor`. Exact names still win outright, so
// nothing that worked before stops working. What it costs is that a future
// verb sharing a first letter would retire that letter for everyone already
// typing it -- `every_verb_starts_with_a_distinct_letter` (tests/cli_misc.rs)
// turns that into a deliberate choice instead of a silent break.
#[derive(Debug, Parser)]
#[command(
    name = "rdc",
    version,
    about = "Rossum Deployment as Code",
    long_about = ROOT_LONG_ABOUT,
    styles = CLI_STYLES,
    disable_help_subcommand = true,
    infer_subcommands = true,
)]
pub struct Cli {
    /// Answer sync's prompts without reading stdin. Conflicts go to
    /// `.rdc/conflicts/<env>/` and drift checks answer "skip". It does not
    /// allow deletes: with deletes pending and no `--allow-deletes`, sync
    /// fails.
    ///
    /// Every command accepts it; only `rdc sync` uses it.
    ///
    /// Implied when stdin or stderr is not a terminal.
    #[arg(long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an rdc project, or add environments to an existing one.
    #[command(long_about = INIT_LONG_ABOUT)]
    Init {
        /// Environment to define, as `<env>=<api_base>:<org_id>`, e.g.
        /// `dev=https://api.elis.rossum.ai/v1:123456`. The org id is the part
        /// after the last colon. Repeatable. Without it, init prompts for
        /// envs, which needs a terminal.
        #[arg(long = "env", value_name = "ENV_SPEC")]
        envs: Vec<String>,
        /// Rewrite the scaffold files.
        ///
        /// A `CLAUDE.md`, `README.md` or `.gitlab-ci.yml` with `rdc:` region
        /// markers is not rewritten; only its regions are updated, as on
        /// every init. Such a file without markers is replaced. The test
        /// harness (`testkit/`, `conftest.py`, `pytest.ini`,
        /// `requirements-dev.txt`) is always replaced, discarding local
        /// edits. `.gitignore` and `.gitattributes` only gain missing rdc
        /// lines.
        ///
        /// Without `--env`, init does only this: no prompts, no auth, no
        /// sync, and `rdc.toml` is unchanged.
        #[arg(long)]
        force: bool,
    },
    /// Reconcile one env's local snapshot with its remote organization.
    #[command(long_about = SYNC_LONG_ABOUT)]
    Sync {
        /// Environment to sync, as named in `rdc.toml`. On a terminal,
        /// omitting it opens a picker, or selects the only env. Required
        /// without a terminal.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Print the plan and exit without writing locally or remotely.
        /// Needs a token to list the env.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Allow remote deletes of objects whose local file was removed.
        /// Without it, a terminal asks once for the whole batch, and a run
        /// without a terminal fails.
        #[arg(long = "allow-deletes")]
        allow_deletes: bool,
        /// Pull remote changes only. Never writes to the remote.
        #[arg(long = "no-push", conflicts_with = "no_pull")]
        no_push: bool,
        /// Push local changes only. Remote edits are not pulled, but a
        /// created object's server-assigned `id` and `url` are written to
        /// disk.
        #[arg(long = "no-pull", conflicts_with = "no_push")]
        no_pull: bool,
        /// Resolve every object changed on both sides with this strategy,
        /// without prompting.
        ///
        /// `use-remote` replaces the local file with the remote copy.
        /// `keep-local` pushes the local file. `skip` writes the remote copy
        /// to `.rdc/conflicts/<env>/` and keeps the local file. An edit on
        /// one side and a delete on the other follows the same choice; a
        /// remote delete still needs `--allow-deletes`.
        ///
        /// Without this flag, a terminal prompts and other runs use `skip`.
        /// Not allowed with `--watch`.
        #[arg(long = "conflict", value_enum, conflicts_with = "watch")]
        conflict: Option<ConflictStrategy>,
        /// Keep running and reconcile on every local file change and remote
        /// poll. On a terminal, Enter starts a cycle unless one is running.
        #[arg(long = "watch", conflicts_with_all = ["dry_run"])]
        watch: bool,
        /// Remote poll interval for `--watch`: `30s`, `2m`, `1h`, or a
        /// number of seconds.
        #[arg(long = "poll-interval", value_name = "DURATION", default_value = "60s", requires = "watch")]
        poll_interval: String,
        /// With `--watch`, do not poll the remote. Reconcile on local file
        /// changes only.
        #[arg(long = "no-poll", requires = "watch", conflicts_with = "poll_interval")]
        no_poll: bool,
        /// With `--watch`, print every cycle, including cycles with no
        /// changes.
        #[arg(short = 'v', long = "verbose", requires = "watch")]
        verbose: bool,
        /// With `--watch`, do not ring the terminal bell when a cycle waits
        /// for input.
        #[arg(long = "no-bell", requires = "watch")]
        no_bell: bool,
    },
    /// Copy one env's snapshot onto another's, locally.
    #[command(long_about = MIGRATE_LONG_ABOUT)]
    Migrate {
        /// Source environment, e.g. `test`. On a terminal, omitting it opens
        /// a picker. Required without a terminal.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        src: Option<String>,
        /// Target environment, e.g. `prod`. On a terminal, omitting it opens
        /// a picker. Required without a terminal.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        tgt: Option<String>,
        /// Also delete target objects that the source does not have. By
        /// default they are kept. The deletes are local file removals.
        ///
        /// Refuses when a prune deletes a live queue or engine and creates a
        /// similar one. Pushing that result deletes the target object (with
        /// a queue's documents or an engine's training) and creates a new
        /// one.
        #[arg(long)]
        mirror: bool,
        /// Allow a `--mirror` prune that deletes a live queue or engine and
        /// creates a similar one. For a renamed object, record the rename in
        /// `.rdc/mapping.toml` instead; `rdc doctor` does this.
        #[arg(long = "allow-recreate", requires = "mirror")]
        allow_recreate: bool,
        /// Print the plan (file remaps and prunes) and exit without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Migrate only objects matching `<kind>/<slug>`. Repeatable. `*`
        /// matches within a segment: `hooks/*`, `schemas/cost-*`,
        /// `*/cost-invoices`. A selector that matches nothing is an error.
        /// Related objects, such as a queue's schema, are not added.
        #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
        only: Vec<String>,
        /// Take these field groups from the source env instead of keeping
        /// the target's. Repeatable and comma-separated, e.g.
        /// `--carry score-thresholds,automation`.
        ///
        /// By default the target keeps its own values for these fields, and
        /// a new object omits them so the server defaults apply.
        ///
        /// * `score-thresholds` — a datapoint's `score_threshold` and a
        ///   queue's `default_score_threshold`.
        ///
        /// * `email-prefixes` — an inbox's `email_prefix`. Carrying it
        ///   changes the target inbox's email address; mail to the old
        ///   address is no longer delivered. A new inbox always gets the
        ///   source value, because the API requires it on create.
        ///
        /// * `automation` — a queue's `automation_enabled`,
        ///   `automation_level` and `quality_spot_check_percentage`.
        ///
        /// * `all` — every group above.
        ///
        /// A value in the target's `overlay.toml` overrides both the default
        /// and this flag.
        #[arg(
            long = "carry",
            value_name = "GROUP",
            value_enum,
            value_delimiter = ',',
            action = clap::ArgAction::Append
        )]
        carry: Vec<CarryGroup>,
    },
    /// Set or refresh an env's API token.
    #[command(long_about = AUTH_LONG_ABOUT)]
    Auth {
        /// Environment to authenticate, as named in `rdc.toml`. On a
        /// terminal, omitting it opens a picker, or selects the only env.
        /// Required without a terminal.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Use this token instead of reading one from stdin.
        #[arg(long, conflicts_with = "username")]
        token: Option<String>,
        /// Log in as this user. rdc reads the password (masked prompt on a
        /// terminal, otherwise stdin) and exchanges it at
        /// POST /v1/auth/login.
        #[arg(long, conflicts_with = "token")]
        username: Option<String>,
        /// Enter the token in a one-time form on 127.0.0.1, opened in the
        /// browser. The token does not appear in argv, stdin or output, and
        /// no terminal is needed.
        #[arg(long, conflicts_with_all = ["token", "username"])]
        browser: bool,
    },
    /// Check and repair an env's local snapshot, offline.
    #[command(long_about = DOCTOR_LONG_ABOUT)]
    Doctor {
        /// Environment to check, as named in `rdc.toml`. On a terminal,
        /// omitting it opens a picker, or selects the only env. Required
        /// without a terminal.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Report without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Edit the local project, offline.
    Edit {
        #[command(subcommand)]
        command: crate::cli::edit::EditCommand,
    },
    /// Replace the running rdc binary with a release build.
    #[command(long_about = UPGRADE_LONG_ABOUT)]
    Upgrade {
        /// Install this version instead of the latest, e.g. `0.11.0` or
        /// `v0.11.0`.
        #[arg(long)]
        version: Option<String>,
        /// Report whether a newer release exists, without installing.
        #[arg(long)]
        check: bool,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Once-daily passive nudge. Skipped for the upgrade command since
    // it computes the same answer fresh. Refresh runs first (tight 2s
    // timeout, silent on failure) so the cache is up-to-date by the
    // time we decide whether to print.
    if !matches!(cli.command, Some(Command::Upgrade { .. })) {
        crate::upgrade::refresh_cache_if_stale().await;
        crate::upgrade::emit_nudge_if_available();
    }

    match cli.command {
        Some(Command::Init { envs, force }) => crate::cli::init::run(envs, force).await,
        Some(Command::Sync {
            env,
            dry_run,
            allow_deletes,
            no_push,
            no_pull,
            conflict,
            watch,
            poll_interval,
            no_poll,
            verbose,
            no_bell,
        }) => {
            let env = crate::cli::env_picker::pick_env("Sync which environment?", "rdc sync <env>", env)?;
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
                        no_bell,
                    )
                })
                .await
            } else {
                with_401_retry(&env, || {
                    crate::cli::sync::run(
                        &env,
                        interactive,
                        dry_run,
                        allow_deletes,
                        no_push,
                        no_pull,
                        conflict,
                    )
                })
                .await
                .map(|_outcome| ())
            }
        }
        Some(Command::Migrate {
            src,
            tgt,
            mirror,
            allow_recreate,
            dry_run,
            only,
            carry,
        }) => {
            let src = crate::cli::env_picker::pick_env("Migrate from which environment?", "rdc migrate <src> <tgt>", src)?;
            let tgt = crate::cli::env_picker::pick_env_excluding(
                "Migrate to which environment?",
                "rdc migrate <src> <tgt>",
                tgt,
                &[&src],
            )?;
            // Pure-local: no remote calls, so no 401-retry wrapper needed.
            let mirror = if mirror {
                crate::cli::migrate::MirrorMode::Mirror { allow_recreate }
            } else {
                crate::cli::migrate::MirrorMode::Additive
            };
            crate::cli::migrate::run(
                &src,
                &tgt,
                mirror,
                dry_run,
                only,
                crate::cli::migrate::Carry::from_groups(&carry),
            )
        }
        Some(Command::Auth { env, token, username, browser }) => {
            let env = crate::cli::env_picker::pick_env("Set the token for which environment?", "rdc auth <env>", env)?;
            crate::cli::auth::run(&env, token, username, browser).await
        }
        Some(Command::Doctor { env, dry_run }) => {
            let env = crate::cli::env_picker::pick_env("Run the doctor on which environment?", "rdc doctor <env>", env)?;
            // doctor is fully offline — no `with_401_retry` wrapper needed.
            crate::cli::doctor::run(&env, dry_run).await
        }
        Some(Command::Edit { command }) => crate::cli::edit::run(command),
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
async fn with_401_retry<T, F, Fut>(env: &str, op: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
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
    let d = match unit {
        "s" | "" => std::time::Duration::from_secs(n),
        "m" => std::time::Duration::from_secs(n * 60),
        "h" => std::time::Duration::from_secs(n * 3600),
        _ => anyhow::bail!("invalid duration unit '{unit}'; use s / m / h"),
    };
    // The poll ticker counts whole seconds, so zero would poll every second
    // while claiming "every 0s". `--no-poll` is how to turn polling off.
    if d.is_zero() {
        anyhow::bail!("invalid duration '{s}': the poll interval must be at least 1s (use --no-poll to stop polling)");
    }
    Ok(d)
}

pub mod auth;
pub mod auth_browser;
pub mod change_view;
pub mod deploy;
pub mod gitlab_ci;
pub mod regions;
pub mod env_picker;
pub mod index;
pub mod init;
pub mod migrate;
#[cfg(test)]
pub(crate) mod overlap;
pub mod pull;
pub mod push;
pub mod doctor;
pub mod edit;
#[cfg(test)]
pub(crate) mod prompt_pin;
pub mod resolve;
pub mod scaffold_docs;
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
