//! `rdc doctor <env>` — diagnose and fix a local snapshot in one pass.
//!
//! Fully offline: no API calls, no prompts. Every fix is mechanical and
//! applied automatically.
//!
//! 1. **Pre-flight** (report-only): scan for local changes not yet pushed to
//!    the remote and surface them, so the user knows what's on disk but not
//!    yet on the server. Also report any field longer than the API's
//!    declared `max_length` — `rdc sync` refuses to push those, and this is
//!    the cheapest place to find out (no network, no waiting on a listing).
//! 2. **Slug renames** (automatic): rename local files whose slug no longer
//!    matches their JSON `name`. Cascade-aware; no decision to make.
//! 3. **Base-cache GC** (automatic): drop `.rdc/state/<env>.base/` files whose
//!    env-tree counterpart no longer exists (deleted object, renamed slug
//!    after pull, etc.).
//!
//! `--dry-run` previews every step without writing.

pub mod rename_slugs;

use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{Context, Result, anyhow};

pub async fn run(env: &str, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let api_base = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?
        .api_base
        .clone();
    let paths = Paths::for_env(&cwd, env);
    let log = Log::new(crate::cli::resolve::detect_color_mode());

    // 1. Pre-flight: local changes not yet pushed to the remote (offline).
    let (unpushed, limit_violations, missing_create_fields, engine_conflicts) =
        scan_unpushed(&paths, &api_base)?;

    // Fields the API will reject on length. Reported here — the offline
    // pre-flight — because it is the cheapest place to learn: `rdc sync`
    // only refuses after the remote listing, and before that check
    // existed the value surfaced as a mid-push 400 naming just the
    // remote id. Not auto-fixable (truncating prose would destroy
    // meaning), so doctor reports and leaves the edit to a human.
    for v in &limit_violations {
        log.event(
            Action::Warn,
            &format!(
                "{}/{} -- {}: {} is {} characters, the API allows {} \
                 (shorten it by {}); `rdc sync {env}` will refuse to push until fixed",
                v.kind,
                v.slug,
                v.path.display(),
                v.field,
                v.actual,
                v.limit,
                v.actual.saturating_sub(v.limit),
            ),
        );
    }

    // Objects that would be CREATED without a field the API demands on create.
    // Same reason as above for reporting it here: the value is decidable from
    // local bytes, and the alternative is learning about it from a mid-push
    // 400 that has already half-created the env.
    for m in &missing_create_fields {
        log.event(
            Action::Warn,
            &format!(
                "{}/{} -- {}: `{}` {} and POST /{} requires it; \
                 `rdc sync {env}` will refuse to push until that is fixed — set the \
                 field (in the file, or per env in envs/{env}/overlay.toml), or \
                 migrate the object it references",
                m.kind,
                m.slug,
                m.path.display(),
                m.field,
                m.detail.as_deref().unwrap_or("is missing"),
                m.kind,
            ),
        );
    }

    // Queues naming two engines at once. Same reason again: the API accepts
    // exactly one binding, so this is decidable offline and otherwise arrives
    // as a mid-push 400 on a remote queue id. Not auto-fixable — which of the
    // two bindings the env should keep is the user's call (a re-migrate makes
    // it the source's).
    for c in &engine_conflicts {
        log.event(
            Action::Warn,
            &format!(
                "queues/{} -- {}: binds {}, but the Rossum API accepts only one; \
                 `rdc sync {env}` will refuse to push until one binding is left \
                 (set the others to null, or re-run migrate)",
                c.slug,
                c.path.display(),
                c.fields.join(", "),
            ),
        );
    }

    if unpushed > 0 {
        log.event(
            Action::Warn,
            &format!(
                "env '{env}' has {unpushed} local change(s) not yet pushed; \
                 run `rdc sync {env}` first if you want to keep them"
            ),
        );
    } else {
        log.event(
            Action::Info,
            &format!("env '{env}': no unpushed local changes"),
        );
    }

    // Slug-realign reads the lockfile, so it can't run without one. Skip it
    // when the lockfile is missing — run `rdc sync` first to create one.
    if paths.lockfile().exists() {
        // 2. Slug renames — mechanical, applied automatically.
        log.event(Action::Doctor, "checking slug alignment");
        rename_slugs::run(
            env, dry_run, /* yes = auto-apply, no per-rename prompt */ true,
        )
        .await?;

        // 3. Base-cache GC — drop `.rdc/state/<env>.base/` files whose
        //    env-tree counterpart no longer exists (deleted object,
        //    renamed slug after pull, etc.). Best-effort; dry-run
        //    reports what would be removed without writing.
        log.event(Action::Doctor, "checking base cache for orphans");
        if dry_run {
            log.event(
                Action::Info,
                "would prune orphan base cache entries (run without --dry-run to apply)",
            );
        } else {
            let pruned = crate::state::base_cache::prune_orphans(&paths)?;
            if pruned == 0 {
                log.event(Action::Info, "base cache: no orphans");
            } else {
                log.event(
                    Action::Done,
                    &format!("base cache: pruned {pruned} orphan file(s)"),
                );
            }
        }
    } else {
        log.event(
            Action::Skip,
            "slug-realign + base-cache checks skipped — no lockfile yet (run `rdc sync` first)",
        );
    }

    log.event(Action::Done, &format!("doctor finished for env '{env}'"));
    Ok(())
}

/// Offline scan of everything a push would send: local objects whose
/// content differs from the lockfile base (edits/creates) plus tombstones
/// (local deletes), together with any field that exceeds the API's
/// declared `max_length` and any queue naming more than one engine.
/// Returns everything empty when there's no lockfile yet —
/// nothing is tracked, so nothing is "unpushed".
fn scan_unpushed(
    paths: &Paths,
    api_base: &str,
) -> Result<(
    usize,
    Vec<crate::cli::push::scan::FieldLimitViolation>,
    Vec<crate::cli::push::scan::MissingCreateField>,
    Vec<crate::snapshot::limits::EngineSlotConflict>,
)> {
    let lockfile_path = paths.lockfile();
    if !lockfile_path.exists() {
        return Ok((0, Vec::new(), Vec::new(), Vec::new()));
    }
    let mut lockfile = Lockfile::load(&lockfile_path)?;
    lockfile.api_base = api_base.to_string();
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(paths, &lockfile)?;
    Ok((
        changes.total() + tombstones.total(),
        changes.field_limit_violations(),
        changes.missing_create_fields(&lockfile),
        changes.queue_engine_conflicts(),
    ))
}
