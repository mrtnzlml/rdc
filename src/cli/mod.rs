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
// comment's single newlines into spaces, which would collapse the two layout
// blocks below into paragraphs.
const ROOT_LONG_ABOUT: &str = r#"Rossum Deployment as Code — keep a Rossum organization's configuration in git
and reconcile it with the live tenant.

A project is a directory holding:

  rdc.toml                    one [envs.<env>] block per environment, each
                              carrying an api_base and an org_id.
  envs/<env>/                 the snapshot: one JSON file per object, plus
                              Python/JavaScript sidecars for hook code, rule
                              trigger conditions and queue formulas.
  envs/<env>/_index.md        generated inventory of the env — every object,
                              its path, and what it references. Read this
                              first; every sync rewrites it.
  envs/<env>/overlay.toml     per-env field values, applied by migrate only.
  secrets/<env>.secrets.json  cached API token (gitignored, mode 0600 on Unix).
  .rdc/state/<env>.lock.json  slug -> remote id, plus the base hashes the
                              three-way merge compares against. rdc owns it.
  .rdc/mapping.toml           slug names that differ between envs. Hand-
                              editable; `rdc doctor` also writes to it.

Two commands do the work:

  rdc sync <env>              reconcile one env against its tenant in a single
                              pass — resolve conflicts, push local edits, then
                              pull remote ones.
  rdc migrate <src> <tgt>     copy one env's snapshot onto another's, entirely
                              offline. A promotion is therefore a file
                              transform you read with `git diff`, followed by
                              a `rdc sync <tgt>` you can dry-run first.

rdc manages workspaces, queues, schemas, inboxes, email templates, hooks,
rules, labels, saved views, engines, engine fields, organization settings and
Master Data Hub datasets. Workflows and workflow steps are pulled but never
written — the Rossum API rejects PATCH on them.

With no terminal attached (CI, or an agent shelling out):

  * Credentials come from $RDC_TOKEN_<ENV>, or from $RDC_USER_<ENV> plus
    $RDC_PASS_<ENV> when rdc should log in itself. <ENV> is the env name
    uppercased with every non-alphanumeric character replaced by '_', so env
    `dev-us` reads $RDC_TOKEN_DEV_US.
  * Pass <env> explicitly to every command that takes one. There is no picker
    and no single-env default without a terminal.
  * `rdc sync --conflict <strategy>` decides objects that changed on both
    sides. Without it they are parked under .rdc/conflicts/<env>/ and the
    local file is kept.
  * `rdc sync --allow-deletes` is required before any remote DELETE. `--yes`
    does not grant it.
  * Any failure exits 1 with the reason on stderr. ANSI styling is dropped
    when the stream it would colour is not a terminal, and by setting
    NO_COLOR to a non-empty value."#;

const INIT_LONG_ABOUT: &str = r#"Bootstrap an rdc project in the current directory, or add environments to an
existing one.

Writes rdc.toml, a scaffold (CLAUDE.md, README.md, .gitlab-ci.yml, .gitignore,
.gitattributes and the pytest formula/hook test harness), a secrets/
directory, and an empty envs/<env>/ tree per new environment. Re-running on an
existing project refreshes only the generated regions inside those files, and
adds any rdc line .gitignore or .gitattributes is missing — see --force.

For each new env it then tries to authenticate: $RDC_TOKEN_<ENV> if that is
set, otherwise a masked prompt when a terminal is attached. A token that fails
validation is reported and the project files stay; re-run `rdc auth <env>`
once the credential is sorted out. With a terminal attached and at least one
env authenticated, init offers to run the first sync for you.

Without a terminal, `--env` is required and nothing is prompted for."#;

const SYNC_LONG_ABOUT: &str = r#"Reconcile the local snapshot and the env's remote state in one pass.

One cycle, in order:

  1. Scan envs/<env>/ and refuse, before any remote write, on the defects that
     are decidable offline: invalid JSON, a field past the Rossum API's length
     limit, a field the API demands on create, a structural problem in the
     organization's settings, a saved view that is not shared, and a queue
     binding more than one engine. (--dry-run lists these instead of failing;
     --no-push skips the check, having nothing to half-apply.)
  2. List the env and classify every object by comparing local bytes, the
     lockfile's base hash and the remote — a three-way merge.
  3. Resolve the objects that changed on both sides (see --conflict).
  4. Delete, for each object whose local file is gone: children before
     parents, and only once --allow-deletes or the batch prompt says so.
  5. Push local creates and edits, parents before children.
  6. Pull remote creates and edits into the snapshot.
  7. Rewrite envs/<env>/_index.md and the lockfile.

Push runs before pull, so a local edit reaches the tenant even on a cycle that
also has remote changes to bring down. Master Data Hub datasets bypass this
classifier and run their own staged push/pull inside the same cycle.

An object that changed on both sides is never resolved silently. On a terminal
the inline resolver opens; otherwise the env's copy is parked under
.rdc/conflicts/<env>/, the local file is left as it is, and the lockfile's
base hash is held back so the next sync raises the same conflict again.

Deleting takes two deliberate acts: remove the local file, then authorise the
DELETE with --allow-deletes (or answer the batch prompt on a terminal). The
lockfile entry left behind by the removed file is what makes it a delete
rather than an untracked file."#;

const MIGRATE_LONG_ABOUT: &str = r#"Migrate a source env's snapshot into a target env's snapshot, locally.

Zero remote calls. Copies envs/<src>/ onto envs/<tgt>/, renaming slugs
wherever .rdc/mapping.toml records that the two envs name an object
differently (identical slugs need no entry), rewriting portable
`rdc://<kind>/<slug>` references from src slugs to tgt slugs, and applying
envs/<tgt>/overlay.toml. Fields each organization tunes for itself are left to
the target — see --carry.

Review the result with `git diff`, then run `rdc sync <tgt>` to push it (sync
creates objects in dependency order). A promotion is therefore two reviewable
steps: this offline file transform, and a push you can dry-run.

Per-env code overrides: a file at envs/<tgt>/overlay/<relpath> replaces the
migrated sidecar at <relpath> — a hook's or rule's .py/.js, a queue's
formulas/<field>.py, or an MDH dataset's data.jsonl. A file under overlay/
that overrides no migrated sidecar aborts the migration."#;

const AUTH_LONG_ABOUT: &str = r#"Set or refresh an env's API token.

The token is validated with GET /organizations/<org_id> before anything is
written, so a typo is caught immediately. It is then written to
secrets/<env>.secrets.json, which `rdc init` gitignores, with mode 0600 on
Unix.

Three ways to supply a credential:

  --token <T>      use this token as given.
  --username <U>   read a password (masked prompt on a terminal, otherwise
                   stdin), exchange it at POST /v1/auth/login, and cache the
                   issued token with an expiry 162 hours out.
  neither          read a token from stdin, e.g. `rdc auth dev < token.txt`.

This is only one of the places rdc looks, and not the one a pipeline should
use. Every command resolves a token in this order: $RDC_TOKEN_<ENV>; then an
unexpired token in secrets/<env>.secrets.json; then a username and password,
taken from that same file or from $RDC_USER_<ENV> plus $RDC_PASS_<ENV>, which
rdc exchanges for a token itself. A pipeline that sets those variables never
needs `rdc auth`."#;

const DOCTOR_LONG_ABOUT: &str = r#"Diagnose and repair the local snapshot for <env> in one pass. Fully offline:
no API calls and no prompts. It WRITES unless --dry-run is passed.

It reports, changing nothing:

  * local objects whose content differs from the lockfile base — what is on
    disk but not yet on the tenant;
  * a field longer than the Rossum API's limit for that field;
  * a field the API requires on create that a to-be-created object lacks;
  * a queue binding more than one engine, which the API rejects.

`rdc sync <env>` refuses to push while any of the last three stands, and
finding them here costs no network round-trip. None is auto-fixable —
shortening prose or choosing an engine is your call, not a mechanical rewrite.

It then repairs, automatically:

  * a file whose slug no longer matches its JSON `name` — renamed, cascading
    through a queue's or workspace's whole subtree, with each rename recorded
    in .rdc/mapping.toml so a later `rdc migrate` renames the target env's
    object instead of deleting it and creating a replacement;
  * a base-cache entry under .rdc/state/<env>.base/ with no counterpart left
    in the env tree.

Both repairs read the lockfile, so both are skipped when the env has none yet;
the run says so. Create one with `rdc sync <env>`."#;

const UPGRADE_LONG_ABOUT: &str = r#"Replace the running rdc binary with a release build.

Asks the GitHub API for the release, downloads the asset built for this
platform, and swaps the binary in place atomically — keeping the one it
replaced as <install_dir>/rdc.bak for a one-shot rollback.

Two installs are refused rather than overwritten, each with the manual
commands printed in its place: a binary under the cargo bin directory, which
`cargo install` owns, and a binary in a directory this process cannot write
to."#;

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
    /// Answer sync's blocking prompts without reading stdin: an object that
    /// changed on both sides takes the shadow-file fallback instead of opening
    /// the resolver, and every drift check resolves to "skip" rather than
    /// asking. It does NOT authorise deletion — with tombstones pending and no
    /// `--allow-deletes`, sync fails instead of prompting.
    ///
    /// Accepted by every command so a pipeline can pass it uniformly, but only
    /// `rdc sync` reads it. `init`, `auth`, `doctor` and `migrate` decide for
    /// themselves whether a terminal is attached, so `rdc init --yes` still
    /// opens the wizard.
    ///
    /// Already implied whenever stdin or stderr is not a terminal.
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
        /// Environment to define, as `<env>=<api_base>:<org_id>` — for example
        /// `dev=https://api.elis.rossum.ai/v1:123456`. The org id is the digits
        /// after the last colon; everything between `=` and that colon is the
        /// API base URL. Repeat the flag to define several envs at once. Omit
        /// it entirely and init prompts for them, which needs a terminal.
        #[arg(long = "env", value_name = "ENV_SPEC")]
        envs: Vec<String>,
        /// Rewrite the scaffold files this binary carries a template for.
        ///
        /// It does not rewrite a `CLAUDE.md`, `README.md` or `.gitlab-ci.yml`
        /// that already carries the `rdc:` region markers: every init refreshes
        /// those regions and leaves the rest of the file alone, with or without
        /// this flag. `--force` rewrites such a file only when the markers are
        /// absent, and always rewrites the test harness — `testkit/`,
        /// `conftest.py`, `pytest.ini` and `requirements-dev.txt` — so hand
        /// edits there are lost. `.gitignore` and `.gitattributes` only ever
        /// gain their missing rdc lines.
        ///
        /// With no `--env`, this is all init does: no wizard, no auth, no sync,
        /// and `rdc.toml` is left untouched.
        #[arg(long)]
        force: bool,
    },
    /// Reconcile the local snapshot and the env's remote state in one pass.
    #[command(long_about = SYNC_LONG_ABOUT)]
    Sync {
        /// Environment to sync, as named in `rdc.toml`. With a terminal
        /// attached, omitting it opens a picker — or takes the only env when
        /// exactly one is defined. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Print the plan and exit, writing nothing locally or remotely. Still
        /// needs a token: the plan is computed against a live listing of the
        /// env.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Authorise remote DELETEs for the objects whose local file is gone.
        /// Without it a terminal gets one `[y/N]` prompt covering the whole
        /// batch, and a run with no terminal fails rather than deleting.
        #[arg(long = "allow-deletes")]
        allow_deletes: bool,
        /// Audit mode: pull remote changes into the snapshot and never write to
        /// the remote.
        #[arg(long = "no-push", conflicts_with = "no_pull")]
        no_push: bool,
        /// Deploy mode: push local edits and do not apply remote edits to the
        /// snapshot. Local files are still written where the push itself
        /// produces content — a created object's server-assigned `id` and `url`
        /// are recorded on disk.
        #[arg(long = "no-pull", conflicts_with = "no_push")]
        no_pull: bool,
        /// Resolve every object that changed on both sides without reading
        /// stdin, overriding the inline resolver even on a terminal:
        /// `use-remote` overwrites the local file with the env's copy,
        /// `keep-local` keeps the local file and pushes it to the env, and
        /// `skip` parks the env's copy under `.rdc/conflicts/<env>/` and leaves
        /// the local file alone. Omit the flag and a terminal prompts while
        /// everything else skips. Not supported together with `--watch`.
        #[arg(long = "conflict", value_enum, conflicts_with = "watch")]
        conflict: Option<ConflictStrategy>,
        /// Watch local files and poll the env continuously, reconciling on each
        /// event. On a terminal, pressing Enter runs a cycle immediately
        /// (ignored while one is already running).
        #[arg(long = "watch", conflicts_with_all = ["dry_run"])]
        watch: bool,
        /// How often watch mode polls the env for remote drift. Accepts `30s`,
        /// `2m`, `1h`; a bare number is read as seconds. Requires `--watch`.
        #[arg(long = "poll-interval", value_name = "DURATION", default_value = "60s", requires = "watch")]
        poll_interval: String,
        /// Stop watch mode polling the env, while it keeps reconciling on local
        /// file events. Requires `--watch`.
        #[arg(long = "no-poll", requires = "watch", conflicts_with = "poll_interval")]
        no_poll: bool,
        /// Print every watch cycle, including the ones that change nothing.
        /// Requires `--watch`.
        #[arg(short = 'v', long = "verbose", requires = "watch")]
        verbose: bool,
        /// Stop watch mode ringing the terminal bell when a cycle blocks for
        /// input (a conflict, delete, drift or token prompt). The bell rings by
        /// default on a terminal. Requires `--watch`.
        #[arg(long = "no-bell", requires = "watch")]
        no_bell: bool,
    },
    /// Migrate a source env's snapshot into a target env's snapshot, locally.
    #[command(long_about = MIGRATE_LONG_ABOUT)]
    Migrate {
        /// Source environment, e.g. `test`. With a terminal attached, omitting
        /// it opens a picker; without one it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        src: Option<String>,
        /// Target environment, e.g. `prod`. With a terminal attached, omitting
        /// it opens a picker; without one it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        tgt: Option<String>,
        /// Mirror semantics: delete tgt snapshot objects that don't exist in
        /// src. Default is additive (extras in tgt are left intact). The
        /// deletions are local file removals — review `git diff` before sync.
        ///
        /// Refuses when a prune would destroy a LIVE target object while
        /// creating another of the same kind: that is what an unrecorded slug
        /// rename looks like, and pushing it deletes the target object (for a
        /// queue, its documents) and creates a replacement.
        #[arg(long)]
        mirror: bool,
        /// Proceed with a `--mirror` prune that deletes live target objects
        /// while creating others of the same kind. Only for objects that
        /// really are unrelated — a renamed one belongs in `.rdc/mapping.toml`
        /// instead, which `rdc doctor` records for you.
        #[arg(long = "allow-recreate", requires = "mirror")]
        allow_recreate: bool,
        /// Print the plan (per-file source -> target remap, prunes) and exit
        /// without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Limit the migration to the given `<kind>/<slug>` selectors.
        /// Repeatable. Globs: `*` matches within the slug segment (e.g.
        /// `hooks/*`, `schemas/cost-*`). Cross-kind: `*/cost-invoices`. A
        /// selector that matches no object in either snapshot is an error, so a
        /// typo can't quietly migrate nothing. Selection is per object: it does
        /// not pull a queue's schema along. Without any `--only`, migrate
        /// operates on the whole snapshot.
        #[arg(long = "only", value_name = "SELECTOR", action = clap::ArgAction::Append)]
        only: Vec<String>,
        /// Carry a group of the target env's own fields from the source env
        /// instead. Repeatable and comma-separated:
        /// `--carry score-thresholds,automation`.
        ///
        /// By default migrate leaves each group to the TARGET env, because
        /// these are tuned per organization rather than promoted with the
        /// solution: a matched target keeps its own values, and a brand-new
        /// object drops the fields so the server's defaults apply.
        ///
        /// * `score-thresholds` — a datapoint's `score_threshold` and a
        ///   queue's `default_score_threshold`.
        ///
        /// * `email-prefixes` — an inbox's `email_prefix`, the left-hand side
        ///   of its public address (`<email_prefix>-<hash>@<host>`): carrying
        ///   it re-addresses the target's mailbox, so mail to the old address
        ///   stops arriving. A brand-new inbox keeps the source's regardless,
        ///   because the field is mandatory on create.
        ///
        /// * `automation` — a queue's `automation_enabled`,
        ///   `automation_level` and `quality_spot_check_percentage`.
        ///
        /// * `all` — every group above.
        ///
        /// To give a target env its own value deliberately, declare it in that
        /// env's `overlay.toml`: an overlay wins over both the reconcile and
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
        /// Environment to authenticate, as named in `rdc.toml`. With a terminal
        /// attached, omitting it opens a picker — or takes the only env when
        /// exactly one is defined. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Use this token instead of reading one from stdin. Validated before
        /// it is written.
        #[arg(long, conflicts_with = "username")]
        token: Option<String>,
        /// Log in as this user instead of supplying a token: rdc reads the
        /// password (masked prompt on a terminal, otherwise stdin) and
        /// exchanges the pair at POST /v1/auth/login.
        #[arg(long, conflicts_with = "token")]
        username: Option<String>,
    },
    /// Diagnose and repair the local snapshot for `<env>` in one offline pass.
    #[command(long_about = DOCTOR_LONG_ABOUT)]
    Doctor {
        /// Environment to check, as named in `rdc.toml`. With a terminal
        /// attached, omitting it opens a picker — or takes the only env when
        /// exactly one is defined. Without a terminal it is required.
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        env: Option<String>,
        /// Report every step without writing. Doctor writes by default.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Replace the running rdc binary with a release build.
    #[command(long_about = UPGRADE_LONG_ABOUT)]
    Upgrade {
        /// Install this version instead of the newest release (an emergency
        /// downgrade; you may need to re-pull afterward). Accepts `0.11.0` or
        /// `v0.11.0`.
        #[arg(long)]
        version: Option<String>,
        /// Report whether a newer release exists and exit without installing
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
            let env = crate::cli::env_picker::pick_env("Which env to sync?", "rdc sync <env>", env)?;
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
            let src = crate::cli::env_picker::pick_env("Migrate from which env (source)?", "rdc migrate <src> <tgt>", src)?;
            let tgt = crate::cli::env_picker::pick_env_excluding(
                "Migrate to which env (target)?",
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
            let env = crate::cli::env_picker::pick_env("Set token for which env?", "rdc auth <env>", env)?;
            crate::cli::auth::run(&env, token, username).await
        }
        Some(Command::Doctor { env, dry_run }) => {
            let env = crate::cli::env_picker::pick_env("Which env to run the doctor on?", "rdc doctor <env>", env)?;
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
