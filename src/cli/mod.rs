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
and reconciles it with the live tenant.

A project is a directory with these files:

  rdc.toml                    one [envs.<env>] block per environment. Each
                              one gives an api_base and an org_id.
  envs/<env>/                 the snapshot. One JSON file per object, plus
                              .py/.js files for hook code, rule trigger
                              conditions and queue formulas.
  envs/<env>/_index.md        a list of every object in the env, its path,
                              and what it references. Read this first. Every
                              sync rewrites it.
  envs/<env>/overlay.toml     per-env field values. Only migrate applies
                              them.
  secrets/<env>.secrets.json  the cached API token. Gitignored, mode 0600 on
                              Unix.
  .rdc/state/<env>.lock.json  slug -> remote id, and the base hashes the
                              three-way merge needs. rdc owns this file.
  .rdc/mapping.toml           slug names that differ between envs. You can
                              edit it; `rdc doctor` writes to it too.

Two commands do the work:

  rdc sync <env>              reconcile one env against its tenant in one
                              pass: conflicts, then push, then pull.
  rdc migrate <src> <tgt>     copy one env's snapshot onto another's. No
                              remote calls: read it with `git diff`, then
                              run `rdc sync <tgt>` to push it.

rdc manages workspaces, queues, schemas, inboxes, email templates, hooks,
rules, labels, saved views, engines, engine fields, organization settings and
Master Data Hub datasets. It pulls workflows and workflow steps but never
writes them, because the Rossum API rejects PATCH on both.

With no terminal attached (CI, or an agent):

  * Credentials come from $RDC_TOKEN_<ENV>, or from $RDC_USER_<ENV> and
    $RDC_PASS_<ENV>, which rdc exchanges for a token. <ENV> is the env name
    in upper case, with every non-alphanumeric character replaced by '_'.
    Env `dev-us` reads $RDC_TOKEN_DEV_US.
  * Name <env> on every command that takes one. There is no picker, and no
    default for a single env.
  * `rdc sync --conflict <strategy>` decides objects that changed on both
    sides. Without it, rdc parks them under .rdc/conflicts/<env>/ and keeps
    the local file.
  * `rdc sync --allow-deletes` is required before any remote DELETE. `--yes`
    does not grant it.
  * Failures exit 1, with the reason on stderr. Colour is off when the
    output is not a terminal, or NO_COLOR is set to a non-empty value."#;

const INIT_LONG_ABOUT: &str = r#"Bootstrap an rdc project in the current directory, or add environments to an
existing one.

Writes rdc.toml, a scaffold (CLAUDE.md, README.md, .gitlab-ci.yml,
.gitignore, .gitattributes, and the pytest harness for testing formulas and
hooks), a secrets/ directory, and an empty envs/<env>/ tree per new
environment. On an existing project it refreshes only the generated regions
in those files, and adds any rdc line missing from .gitignore or
.gitattributes. See --force.

For each new env it then tries to authenticate. It uses $RDC_TOKEN_<ENV> if
that is set, otherwise a masked prompt when a terminal is attached. If the
token fails validation, rdc reports it and keeps the project files; run
`rdc auth <env>` once the credential is sorted out. With a terminal and at
least one env authenticated, init offers to run the first sync.

Without a terminal, `--env` is required and rdc prompts for nothing."#;

const SYNC_LONG_ABOUT: &str = r#"Reconcile the local snapshot and the env's remote state in one pass.

One cycle, in order:

  1. Scan envs/<env>/ for problems rdc can find offline: invalid JSON, a
     field past the API's length limit, a field the API needs on create, a
     broken organization `settings` structure, a saved view that is not
     shared, a queue bound to more than one engine. Any one stops the run
     before the first remote write. (--dry-run lists them instead.
     --no-push skips the check.)
  2. List the env. Classify every object by comparing the local file, the
     lockfile's base hash and the remote copy. That is a three-way merge.
  3. Resolve the objects that changed on both sides. See --conflict.
  4. Delete the objects whose local file is gone: children before parents,
     and only after --allow-deletes or the batch prompt.
  5. Push local creates and edits: parents before children.
  6. Pull remote creates and edits into the snapshot.
  7. Rewrite envs/<env>/_index.md and the lockfile.

Push runs before pull, so a local edit reaches the tenant even on a cycle
that also brings remote changes down. Master Data Hub datasets skip this
classifier and run their own push and pull inside the same cycle.

rdc never resolves a both-sides change on its own. On a terminal it opens the
inline resolver. Otherwise it writes the env's copy to .rdc/conflicts/<env>/,
leaves the local file alone, and holds the lockfile's base hash back, so the
next sync raises the same conflict again.

Deleting takes two steps: remove the local file, then allow the DELETE with
--allow-deletes (or answer the batch prompt on a terminal). What makes it a
delete, and not an untracked file, is the lockfile entry the removed file
leaves behind."#;

const MIGRATE_LONG_ABOUT: &str = r#"Migrate a source env's snapshot into a target env's snapshot, locally.

No remote calls. Copies envs/<src>/ onto envs/<tgt>/, then renames slugs
where .rdc/mapping.toml says the two envs name an object differently
(identical slugs need no entry), rewrites portable `rdc://<kind>/<slug>`
references from src slugs to tgt slugs, and applies envs/<tgt>/overlay.toml.
Fields that each organization tunes for itself stay with the target. See
--carry.

Read the result with `git diff`, then run `rdc sync <tgt>` to push it. Sync
creates objects in dependency order. A promotion is therefore two steps you
can review: this offline file transform, and a push you can dry-run.

Per-env code overrides: a file at envs/<tgt>/overlay/<relpath> replaces the
migrated sidecar at <relpath>. A sidecar is a hook's or rule's .py/.js, a
queue's formulas/<field>.py, or an MDH dataset's data.jsonl. A file under
overlay/ that overrides no migrated sidecar stops the migration."#;

const AUTH_LONG_ABOUT: &str = r#"Set or refresh an env's API token.

rdc validates the token with GET /organizations/<org_id> before writing
anything, so a typo is caught at once. It then writes the token to
secrets/<env>.secrets.json, with mode 0600 on Unix. `rdc init` gitignores
that directory.

Three ways to give a credential:

  --token <T>      use this token as it is.
  --username <U>   read a password (masked prompt on a terminal, otherwise
                   stdin), exchange it at POST /v1/auth/login, and cache the
                   token with an expiry 162 hours out.
  neither          read a token from stdin, e.g. `rdc auth dev < token.txt`.

This is only one of the places rdc looks. Every command looks for a token in
this order: $RDC_TOKEN_<ENV>; an unexpired token in
secrets/<env>.secrets.json; a username and password, from that same file or
from $RDC_USER_<ENV> and $RDC_PASS_<ENV>. A pipeline that sets those
variables never needs `rdc auth`."#;

const DOCTOR_LONG_ABOUT: &str = r#"Diagnose and repair the local snapshot for <env> in one pass. Fully offline:
no API calls, no prompts. It writes unless you pass --dry-run.

It reports these, and changes nothing:

  * local objects that differ from the lockfile base: on disk, not yet on
    the tenant;
  * a field longer than the Rossum API's limit for that field;
  * a field the API needs on create that a new object does not have;
  * a queue bound to more than one engine, which the API rejects.

`rdc sync <env>` refuses to push while any of the last three is there.
Finding them here costs no network call. rdc cannot fix them: shortening
text, or picking one engine, is your decision.

It repairs these on its own:

  * a file whose slug no longer matches its JSON `name`. rdc renames it, and
    a queue or workspace rename moves the whole subtree. Each rename goes
    into .rdc/mapping.toml, so a later `rdc migrate` renames the target env's
    object instead of deleting it and creating a new one;
  * a base-cache entry under .rdc/state/<env>.base/ whose object is gone from
    the env tree.

Both repairs read the lockfile. When the env has none yet, rdc skips them and
says so. Run `rdc sync <env>` to create one."#;

const UPGRADE_LONG_ABOUT: &str = r#"Replace the running rdc binary with a release build.

Asks the GitHub API for the release, downloads the asset built for this
platform, and swaps the binary in place atomically. It keeps the binary it
replaced as <install_dir>/rdc.bak, so you can roll back once.

rdc refuses two kinds of install instead of overwriting them, and prints the
manual commands in their place: a binary under the cargo bin directory, which
`cargo install` owns, and a binary in a directory it cannot write to."#;

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
    /// Answer sync's blocking prompts without reading stdin. A both-sides
    /// change takes the shadow-file fallback instead of the resolver, and
    /// every drift check answers "skip". It does NOT allow deletion: with
    /// tombstones pending and no `--allow-deletes`, sync fails instead of
    /// prompting.
    ///
    /// Every command accepts it, so a pipeline can pass it everywhere. Only
    /// `rdc sync` reads it: `init`, `auth`, `doctor` and `migrate` check for
    /// a terminal themselves, so `rdc init --yes` still opens the wizard.
    ///
    /// Already in effect when stdin or stderr is not a terminal.
    #[arg(long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bootstrap an rdc project in the current directory, or add environments
    /// to an existing one.
    #[command(long_about = INIT_LONG_ABOUT)]
    Init {
        /// Environment to define, as `<env>=<api_base>:<org_id>`. For example
        /// `dev=https://api.elis.rossum.ai/v1:123456`. The org id is the
        /// digits after the last colon; everything between `=` and that colon
        /// is the API base URL. Repeat the flag to define several envs. Leave
        /// it out and init prompts for them, which needs a terminal.
        #[arg(long = "env", value_name = "ENV_SPEC")]
        envs: Vec<String>,
        /// Rewrite the scaffold files this binary carries a template for.
        ///
        /// It does not rewrite a `CLAUDE.md`, `README.md` or `.gitlab-ci.yml`
        /// that already has the `rdc:` region markers. Every init refreshes
        /// those regions and leaves the rest of the file alone, with or
        /// without this flag. `--force` rewrites such a file only when the
        /// markers are missing. It always rewrites the test harness —
        /// `testkit/`, `conftest.py`, `pytest.ini` and
        /// `requirements-dev.txt` — so edits there are lost. `.gitignore` and
        /// `.gitattributes` only ever gain the rdc lines they are missing.
        ///
        /// With no `--env`, this is all init does: no wizard, no auth, no
        /// sync, and `rdc.toml` is untouched.
        #[arg(long)]
        force: bool,
    },
    /// Reconcile the local snapshot and the env's remote state in one pass.
    #[command(long_about = SYNC_LONG_ABOUT)]
    Sync {
        /// Environment to sync, as named in `rdc.toml`. With a terminal,
        /// leaving it out opens a picker, or takes the only env when just one
        /// is defined. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Print the plan and exit. Writes nothing, locally or remotely. It
        /// still needs a token, because it lists the env to build the plan.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Allow remote DELETEs for the objects whose local file is gone.
        /// Without it, a terminal gets one `[y/N]` prompt for the whole
        /// batch, and a run with no terminal fails instead of deleting.
        #[arg(long = "allow-deletes")]
        allow_deletes: bool,
        /// Audit mode. Pull remote changes into the snapshot and never write
        /// to the remote.
        #[arg(long = "no-push", conflicts_with = "no_pull")]
        no_push: bool,
        /// Deploy mode. Push local edits and do not apply remote edits to the
        /// snapshot. rdc still writes local files where the push itself
        /// produces content: a created object's server-assigned `id` and
        /// `url` go to disk.
        #[arg(long = "no-pull", conflicts_with = "no_push")]
        no_pull: bool,
        /// Resolve every object that changed on both sides without reading
        /// stdin. This overrides the inline resolver, even on a terminal.
        /// `use-remote` overwrites the local file with the env's copy.
        /// `keep-local` keeps the local file and pushes it to the env. `skip`
        /// writes the env's copy to `.rdc/conflicts/<env>/` and leaves the
        /// local file alone. Leave the flag out and a terminal prompts, while
        /// everything else skips. Cannot be used with `--watch`.
        #[arg(long = "conflict", value_enum, conflicts_with = "watch")]
        conflict: Option<ConflictStrategy>,
        /// Watch local files and poll the env, reconciling on each event. On
        /// a terminal, press Enter to run a cycle at once; that is ignored
        /// while a cycle is already running.
        #[arg(long = "watch", conflicts_with_all = ["dry_run"])]
        watch: bool,
        /// How often watch mode polls the env for remote drift. Accepts
        /// `30s`, `2m`, `1h`. A bare number means seconds. Requires
        /// `--watch`.
        #[arg(long = "poll-interval", value_name = "DURATION", default_value = "60s", requires = "watch")]
        poll_interval: String,
        /// Stop watch mode polling the env. It keeps reconciling on local
        /// file events. Requires `--watch`.
        #[arg(long = "no-poll", requires = "watch", conflicts_with = "poll_interval")]
        no_poll: bool,
        /// Print every watch cycle, including the ones that change nothing.
        /// Requires `--watch`.
        #[arg(short = 'v', long = "verbose", requires = "watch")]
        verbose: bool,
        /// Stop watch mode ringing the terminal bell when a cycle waits for
        /// input (a conflict, delete, drift or token prompt). The bell rings
        /// by default on a terminal. Requires `--watch`.
        #[arg(long = "no-bell", requires = "watch")]
        no_bell: bool,
    },
    /// Migrate a source env's snapshot into a target env's snapshot, locally.
    #[command(long_about = MIGRATE_LONG_ABOUT)]
    Migrate {
        /// Source environment, e.g. `test`. With a terminal, leaving it out
        /// opens a picker. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        src: Option<String>,
        /// Target environment, e.g. `prod`. With a terminal, leaving it out
        /// opens a picker. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        tgt: Option<String>,
        /// Mirror semantics: delete tgt snapshot objects that src does not
        /// have. The default is additive, which leaves extras in tgt alone.
        /// The deletions are local file removals — read `git diff` before you
        /// sync.
        ///
        /// rdc refuses when a prune would destroy a LIVE target object while
        /// creating another of the same kind. That is what an unrecorded slug
        /// rename looks like, and pushing it deletes the target object (for a
        /// queue, its documents) and creates a replacement.
        #[arg(long)]
        mirror: bool,
        /// Go ahead with a `--mirror` prune that deletes live target objects
        /// while creating others of the same kind. Use it only for objects
        /// that really are unrelated. A renamed one belongs in
        /// `.rdc/mapping.toml`, which `rdc doctor` writes for you.
        #[arg(long = "allow-recreate", requires = "mirror")]
        allow_recreate: bool,
        /// Print the plan (per-file source -> target remap, prunes) and exit
        /// without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Limit the migration to the given `<kind>/<slug>` selectors.
        /// Repeatable. `*` matches inside the slug segment, e.g. `hooks/*` or
        /// `schemas/cost-*`; `*/cost-invoices` matches across kinds. A
        /// selector that matches nothing in either snapshot is an error, so a
        /// typo cannot quietly migrate nothing. Selection is per object: it
        /// does not bring a queue's schema along. Without `--only`, migrate
        /// works on the whole snapshot.
        #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
        only: Vec<String>,
        /// Carry a group of the target env's own fields from the source env
        /// instead. Repeatable and comma-separated:
        /// `--carry score-thresholds,automation`.
        ///
        /// By default migrate leaves each group to the TARGET env, because
        /// each organization tunes these rather than promoting them with the
        /// solution. A matched target keeps its own values, and a brand-new
        /// object drops the fields so the server's defaults apply.
        ///
        /// * `score-thresholds` — a datapoint's `score_threshold` and a
        ///   queue's `default_score_threshold`.
        ///
        /// * `email-prefixes` — an inbox's `email_prefix`, the left-hand side
        ///   of its public address (`<email_prefix>-<hash>@<host>`). Carrying
        ///   it re-addresses the target's mailbox, so mail to the old address
        ///   stops arriving. A brand-new inbox keeps the source's value
        ///   either way, because the field is mandatory on create.
        ///
        /// * `automation` — a queue's `automation_enabled`,
        ///   `automation_level` and `quality_spot_check_percentage`.
        ///
        /// * `all` — every group above.
        ///
        /// To give a target env its own value on purpose, declare it in that
        /// env's `overlay.toml`. An overlay wins over both the reconcile and
        /// this flag.
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
        /// Environment to authenticate, as named in `rdc.toml`. With a
        /// terminal, leaving it out opens a picker, or takes the only env
        /// when just one is defined. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Use this token instead of reading one from stdin. Validated before
        /// rdc writes it.
        #[arg(long, conflicts_with = "username")]
        token: Option<String>,
        /// Log in as this user instead of giving a token. rdc reads the
        /// password (masked prompt on a terminal, otherwise stdin) and
        /// exchanges the pair at POST /v1/auth/login.
        #[arg(long, conflicts_with = "token")]
        username: Option<String>,
    },
    /// Diagnose and repair the local snapshot for `<env>` in one offline pass.
    #[command(long_about = DOCTOR_LONG_ABOUT)]
    Doctor {
        /// Environment to check, as named in `rdc.toml`. With a terminal,
        /// leaving it out opens a picker, or takes the only env when just one
        /// is defined. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Report every step without writing. Doctor writes by default.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Replace the running rdc binary with a release build.
    #[command(long_about = UPGRADE_LONG_ABOUT)]
    Upgrade {
        /// Install this version instead of the newest release. An emergency
        /// downgrade; you may need to re-pull afterwards. Accepts `0.11.0` or
        /// `v0.11.0`.
        #[arg(long)]
        version: Option<String>,
        /// Report whether a newer release exists, and exit without installing
        /// anything.
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
        Some(Command::Auth { env, token, username }) => {
            let env = crate::cli::env_picker::pick_env("Set the token for which environment?", "rdc auth <env>", env)?;
            crate::cli::auth::run(&env, token, username).await
        }
        Some(Command::Doctor { env, dry_run }) => {
            let env = crate::cli::env_picker::pick_env("Run the doctor on which environment?", "rdc doctor <env>", env)?;
            // doctor is fully offline — no `with_401_retry` wrapper needed.
            crate::cli::doctor::run(&env, dry_run).await
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
    match unit {
        "s" | "" => Ok(std::time::Duration::from_secs(n)),
        "m" => Ok(std::time::Duration::from_secs(n * 60)),
        "h" => Ok(std::time::Duration::from_secs(n * 3600)),
        _ => anyhow::bail!("invalid duration unit '{unit}'; use s / m / h"),
    }
}

pub mod auth;
pub mod change_view;
pub mod deploy;
pub mod gitlab_ci;
pub mod regions;
pub mod env_picker;
pub mod index;
pub mod init;
pub mod migrate;
pub mod pull;
pub mod push;
pub mod doctor;
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
