//! `rdc doctor <env>` — diagnose and fix a local snapshot in one pass.
//!
//! Runs every fix automatically, prompting only where a real decision is
//! required:
//!
//! 1. **Pre-flight** (offline): scan for local changes not yet pushed to the
//!    remote and surface them, so the user knows what's at stake before any
//!    destructive step.
//! 2. **Slug renames** (offline, automatic): rename local files whose slug no
//!    longer matches their JSON `name`. Cascade-aware; no decision to make.
//! 3. **Store-anomaly hooks** (online): hooks with `extension_source:
//!    "rossum_store"` and `hook_template: null`. Per hook the user picks the
//!    cure (Convert / Reinstall / Skip).
//! 4. **Rebuild lockfile** (online, DESTRUCTIVE): re-pull from remote,
//!    overwriting local snapshot files. Offered behind a confirm (default
//!    No) that names how many unpushed changes would be lost. Skipped under
//!    `--yes` / non-TTY — destruction is never auto-authorized.
//!
//! `--check` previews every step without writing or prompting.

pub mod rebuild_lock;
pub mod rename_slugs;
pub mod store_anomaly;

use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub async fn run(env: &str, rebuild_lock: bool, check: bool, yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    if !cfg.envs.contains_key(env) {
        return Err(anyhow!("env '{env}' is not defined in rdc.toml"));
    }
    let paths = Paths::for_env(&cwd, env);
    let log = Log::new(crate::cli::resolve::detect_color_mode(false));

    // 1. Pre-flight: local changes not yet pushed to the remote (offline).
    let unpushed = count_unpushed(&paths)?;
    if unpushed > 0 {
        log.event(
            Action::Warn,
            &format!(
                "env '{env}' has {unpushed} local change(s) not yet pushed; \
                 run `rdc sync {env}` first if you want to keep them"
            ),
        );
    } else {
        log.event(Action::Info, &format!("env '{env}': no unpushed local changes"));
    }

    // Slug-realign and store-anomaly both read (and the cure mutates) the
    // lockfile, so they can't run without one. Skip them when it's missing —
    // that's the recovery case `--rebuild-lock` below exists for.
    if paths.lockfile().exists() {
        // 2. Slug renames — mechanical, applied automatically.
        log.event(Action::Doctor, "checking slug alignment");
        rename_slugs::run(env, check, /* yes = auto-apply, no per-rename prompt */ true).await?;

        // 3. Store-anomaly hooks — per-hook decision, prompted (unless --yes/non-TTY).
        log.event(Action::Doctor, "checking store-extension hooks");
        store_anomaly::run(env, check, yes).await?;
    } else {
        log.event(
            Action::Skip,
            "slug-realign + store-anomaly checks skipped — no lockfile yet (sync first, or rebuild below)",
        );
    }

    // 4. Rebuild lockfile — destructive; explicit confirm, or `--rebuild-lock`
    //    to authorize it directly.
    maybe_rebuild_lock(env, &paths, rebuild_lock, check, yes, &log).await?;

    log.event(Action::Done, &format!("doctor finished for env '{env}'"));
    Ok(())
}

/// Offline count of everything a push would send: local objects whose
/// content differs from the lockfile base (edits/creates) plus tombstones
/// (local deletes). Returns 0 when there's no lockfile yet — nothing is
/// tracked, so nothing is "unpushed".
fn count_unpushed(paths: &Paths) -> Result<usize> {
    let lockfile_path = paths.lockfile();
    if !lockfile_path.exists() {
        return Ok(0);
    }
    let lockfile = Lockfile::load(&lockfile_path)?;
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(paths, &lockfile)?;
    Ok(changes.total() + tombstones.total())
}

/// Offer the destructive lockfile rebuild. `--check` only reports that it's
/// available. Otherwise it prompts (default No) and refuses to proceed
/// without an explicit `y`; `--yes` / non-TTY skip it entirely so a scripted
/// run never silently discards local edits. The prompt re-counts unpushed
/// changes (the earlier fixes may have changed the total) and names how many
/// would be lost.
async fn maybe_rebuild_lock(
    env: &str,
    paths: &Paths,
    force: bool,
    check: bool,
    yes: bool,
    log: &std::sync::Arc<Log>,
) -> Result<()> {
    if check {
        log.event(
            Action::Info,
            "would offer to rebuild the lockfile from remote (re-pull; discards local edits)",
        );
        return Ok(());
    }

    let unpushed = count_unpushed(paths)?;
    let loss = if unpushed > 0 {
        format!(" — {unpushed} unpushed local change(s) will be LOST")
    } else {
        String::new()
    };

    // `--rebuild-lock` is explicit authorization: run it directly, no confirm
    // (it works under --yes / non-TTY precisely because the flag is consent).
    if force {
        log.event(Action::Doctor, &format!("rebuilding lockfile (--rebuild-lock){loss}"));
        return rebuild_lock::run(env).await;
    }
    // No flag: only offer it interactively. Under --yes / non-TTY there's no
    // way to authorize destruction, so skip rather than wipe edits silently.
    if !crate::cli::resolve::is_interactive(yes) {
        log.event(
            Action::Skip,
            "rebuild-lock skipped — pass --rebuild-lock to authorize it under --yes / non-TTY",
        );
        return Ok(());
    }

    let prompt = format!(
        "Rebuild the lockfile by re-pulling '{env}' from the remote? \
         Overwrites local snapshot files{loss}"
    );
    let proceed = match inquire::Confirm::new(&prompt)
        .with_default(false)
        .with_help_message("discards local edits not present on the remote; backs up the old lockfile first")
        .prompt()
    {
        Ok(b) => b,
        // Esc / Ctrl+C = don't rebuild.
        Err(inquire::error::InquireError::OperationCanceled)
        | Err(inquire::error::InquireError::OperationInterrupted) => false,
        Err(e) => return Err(anyhow!("rebuild-lock prompt failed: {e}")),
    };
    if proceed {
        rebuild_lock::run(env).await?;
    } else {
        log.event(Action::Skip, "rebuild-lock declined");
    }
    Ok(())
}
