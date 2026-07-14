//! `rdc doctor <env>` — diagnose and fix a local snapshot in one pass.
//!
//! Fully offline: no API calls, no prompts. Every fix is mechanical and
//! applied automatically.
//!
//! 1. **Pre-flight** (report-only): scan for local changes not yet pushed to
//!    the remote and surface them, so the user knows what's on disk but not
//!    yet on the server.
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
    let unpushed = count_unpushed(&paths, &api_base)?;
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

/// Offline count of everything a push would send: local objects whose
/// content differs from the lockfile base (edits/creates) plus tombstones
/// (local deletes). Returns 0 when there's no lockfile yet — nothing is
/// tracked, so nothing is "unpushed".
fn count_unpushed(paths: &Paths, api_base: &str) -> Result<usize> {
    let lockfile_path = paths.lockfile();
    if !lockfile_path.exists() {
        return Ok(0);
    }
    let mut lockfile = Lockfile::load(&lockfile_path)?;
    lockfile.api_base = api_base.to_string();
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(paths, &lockfile)?;
    Ok(changes.total() + tombstones.total())
}
