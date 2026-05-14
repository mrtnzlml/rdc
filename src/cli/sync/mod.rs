//! `rdc sync <env>` — reconcile local snapshot and remote state in one pass.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md

pub mod classify;
pub mod execute;
pub mod plan;

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

/// Entry point for `rdc sync <env>`. Drives the unified pipeline:
///
/// 1. `list_remote` — Phase 1 of the existing pull pipeline.
/// 2. `scan` — Phase 1 of the existing push pipeline.
/// 3. `classify` (via [`from_catalog_scan_lockfile`]) — fold the three
///    sources into eleven `(kind, slug)` classes.
/// 4. `plan` — render the plan, exit early on `--dry-run`.
/// 5. (interactive) confirm.
/// 6. `execute` — currently a stub; subsequent tasks fill in per-class
///    dispatch.
///
/// On success the lockfile is saved and `_index.md` regenerated, even
/// when the executor was a no-op — re-running on a clean env stays
/// idempotent.
pub async fn run(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
) -> Result<()> {
    if no_push && no_pull {
        anyhow::bail!(
            "--no-push and --no-pull are mutually exclusive. \
             Use 'rdc status' for read-only inspection."
        );
    }

    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let progress = OverallProgress::start(format!("sync envs/{env}"));

    // Phase 1: list remote. Mirrors `pull::run`'s `PullCtx` construction
    // verbatim so the listing semantics are identical.
    let catalog = {
        let mut ctx = crate::cli::pull::common::PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            overlay: overlay.clone(),
            interactive,
        };
        crate::cli::pull::common::list_remote(&mut ctx, env_cfg, env, &token, &progress).await?
    };

    // Phase 2: scan local. Reuses the push scanner unchanged so behavior
    // matches `push --dry-run` byte for byte.
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(&paths, &lockfile)?;

    // Phase 3: classify. The clean-env adapter is intentionally minimal;
    // subsequent tasks fill in per-kind hashing.
    let classified = from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile);

    // Phase 4: plan + confirm. `--dry-run` exits here without writing.
    let plan_text = crate::cli::sync::plan::render_plan(env, &classified);
    print!("{plan_text}");
    if dry_run {
        // `--diff` rendering will hook in here once the executor is
        // wired; defer until per-object bodies are available.
        let _ = diff;
        progress.finish();
        println!("Dry run sync envs/{env}: 0 writes.");
        return Ok(());
    }

    // Destructive-delete gate: subsequent tasks will refuse to proceed
    // with `LocalDelete` items unless `--allow-deletes` is set. Today
    // the executor is a no-op, so the flag is recorded for parity.
    let _ = allow_deletes;

    if interactive && !classified.is_empty() && !confirm("Proceed?")? {
        eprintln!("sync aborted by user.");
        return Ok(());
    }

    // Phase 5: execute. Stub today — fills in across subsequent tasks.
    {
        let mut ctx = crate::cli::pull::common::PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            overlay,
            interactive,
        };
        execute::run(
            &mut ctx,
            &catalog,
            &classified,
            no_push,
            no_pull,
            interactive,
            &progress,
        )
        .await?;
    }

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("generating _index.md for env '{env}'"))?;

    progress.finish();
    println!("Synced envs/{env}.");
    Ok(())
}

/// y/N confirmation prompt on stdin/stdout. Returns false for any input
/// other than "y" / "Y".
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(line.trim().chars().next(), Some('y') | Some('Y')))
}

/// Fold the three sources (remote catalog, push scan, lockfile) into the
/// `(kind, slug) -> hash` maps the classifier consumes.
///
/// TODO(sync-impl): compute per-kind hashes from catalog bodies and
/// local file bytes. Each kind has its own canonicalization rules — hooks
/// strip `code`, schemas combine formulas, rules strip
/// `trigger_condition`, etc. — so this isn't a one-liner; it grows across
/// subsequent tasks.
///
/// Today's stub: produce empty maps. The clean-env smoke test exercises
/// the pipeline shape (list → scan → classify → plan → confirm → execute)
/// without depending on the hashes. Subsequent tasks build out hashing
/// per kind.
pub fn from_catalog_scan_lockfile(
    catalog: &crate::cli::pull::common::RemoteCatalog,
    changes: &crate::cli::push::scan::ChangeList,
    tombstones: &crate::cli::push::scan::Tombstones,
    lockfile: &crate::state::Lockfile,
) -> Vec<crate::cli::sync::classify::ClassifiedItem> {
    use std::collections::{BTreeMap, BTreeSet};
    let _ = (catalog, changes, tombstones, lockfile);
    let remote_hashes: BTreeMap<(String, String), String> = BTreeMap::new();
    let scan_changes: BTreeMap<(String, String), String> = BTreeMap::new();
    let scan_tombstones: BTreeSet<(String, String)> = BTreeSet::new();
    let locked: BTreeMap<(String, String), String> = BTreeMap::new();
    crate::cli::sync::classify::classify(&remote_hashes, &scan_changes, &scan_tombstones, &locked)
}
