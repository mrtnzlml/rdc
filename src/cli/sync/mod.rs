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
/// The hashing on each side must match so the classifier produces `Clean`
/// when nothing has changed:
/// * **`remote_hashes`** — re-runs the canonical serialization the per-kind
///   pull driver performs before `record_object` (serialize → optional
///   overlay strip → `content_hash`). This mirrors how the lockfile's
///   `content_hash` was written on the last pull.
/// * **`scan_changes`** — already computed by [`crate::cli::push::scan::scan`]
///   over local file bytes with the same `content_hash` function. We
///   re-derive from the on-disk path here rather than smuggling the hash
///   through `ChangeList` to keep `scan.rs` push-only.
/// * **`scan_tombstones`** — just the `(kind, slug)` keys, no hash needed.
/// * **`locked`** — pulled verbatim from `lockfile.objects[kind][slug].content_hash`.
///
/// Slug derivation matches the pull drivers: prefer
/// `lockfile.slug_for_id(kind, id)`, else `slugify_unique(name, ...)`.
///
/// TODO(sync-impl): only `labels` is wired today. Add hashing for the
/// remaining kinds — `workspaces`, `queues`, `hooks` (combined hash),
/// `rules` (combined hash), `schemas` (combined hash), `inboxes`,
/// `engines`, `engine_fields`, `email_templates`, `mdh` — as their
/// integration tests land. Each kind reuses its own pull driver's
/// canonicalization rules; see `cli::pull::<kind>::process` for the
/// authoritative serialization order.
pub fn from_catalog_scan_lockfile(
    catalog: &crate::cli::pull::common::RemoteCatalog,
    changes: &crate::cli::push::scan::ChangeList,
    tombstones: &crate::cli::push::scan::Tombstones,
    lockfile: &crate::state::Lockfile,
) -> Vec<crate::cli::sync::classify::ClassifiedItem> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut remote_hashes: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut scan_changes: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut scan_tombstones: BTreeSet<(String, String)> = BTreeSet::new();
    let mut locked: BTreeMap<(String, String), String> = BTreeMap::new();

    // --- labels --------------------------------------------------------
    // Catalog hash: re-run `pull::labels::process`'s pre-record_object
    // sequence — serialize → push newline → `content_hash`. Slug picks
    // up the lockfile-anchored id mapping so remote renames don't churn
    // slugs.
    //
    // Overlay note: the pull driver also runs `maybe_strip_overlay` on
    // the proposed bytes when the env has a label overlay. We skip that
    // step here because (a) threading the overlay through this signature
    // is invasive for one TODO-listed concern, and (b) `content_hash`
    // canonicalizes via `canonicalize_for_hash`, which only strips the
    // server-noise fields — it does *not* strip overlay paths. As long
    // as the pull driver also doesn't have a label overlay configured,
    // both sides hash the same bytes. Once an overlay-aware test arrives,
    // thread `&Overlay` into the adapter and call `maybe_strip_overlay`
    // here.
    let mut used_label_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for l in &catalog.labels {
        let slug = match lockfile.slug_for_id("labels", l.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&l.name, &used_label_slugs),
        };
        used_label_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(l) {
            Ok(b) => b,
            // If a single body fails to serialize, skip it — better than
            // tanking the whole classifier. The pull driver itself would
            // also have errored, so the lockfile won't contain a hash and
            // we'll get a spurious RemoteCreate. Acceptable.
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("labels".to_string(), slug), hash);
    }

    // Scan changes: re-hash each on-disk path the scanner already
    // flagged. The scanner skipped Clean items, so anything in
    // `changes.labels` is by definition a local edit/create candidate.
    for (slug, path) in &changes.labels {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hash = crate::state::content_hash(&bytes);
        scan_changes.insert(("labels".to_string(), slug.clone()), hash);
    }

    // Tombstones: just the keys.
    for slug in tombstones.labels.keys() {
        scan_tombstones.insert(("labels".to_string(), slug.clone()));
    }

    // Locked: lockfile-recorded base hashes.
    if let Some(map) = lockfile.objects.get("labels") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("labels".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- organization (pull-only singleton) ---------------------------
    // The org is a singleton — slug is always "self", matching
    // `pull::organization::process`'s `record_object` key. Hash
    // computation mirrors that driver: `serde_json::to_vec_pretty` →
    // push `\n` → `content_hash`. Push side never touches the org, so
    // there's no `scan_changes` / tombstones lookup here.
    {
        let org = &catalog.organization;
        if let Ok(mut proposed) = serde_json::to_vec_pretty(org) {
            proposed.push(b'\n');
            let hash = crate::state::content_hash(&proposed);
            remote_hashes.insert(("organization".to_string(), "self".to_string()), hash);
        }
        if let Some(map) = lockfile.objects.get("organization") {
            for (slug, entry) in map {
                if let Some(h) = &entry.content_hash {
                    locked.insert(("organization".to_string(), slug.clone()), h.clone());
                }
            }
        }
    }

    // --- workflows (pull-only) ----------------------------------------
    // Slug derivation mirrors `pull::workflows::process`: prefer the
    // lockfile-anchored id mapping, else `slugify_unique(name, used)`.
    // Hash computation matches the driver byte-for-byte.
    let mut used_workflow_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for w in &catalog.workflows {
        let slug = match lockfile.slug_for_id("workflows", w.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&w.name, &used_workflow_slugs),
        };
        used_workflow_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(w) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("workflows".to_string(), slug), hash);
    }
    if let Some(map) = lockfile.objects.get("workflows") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("workflows".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- workflow_steps (pull-only) -----------------------------------
    // Flat slug derivation, like engine_fields. The driver also skips
    // orphan steps (no parent workflow in the lockfile) but here we
    // emit the catalog-side slug regardless — the classifier doesn't
    // care, and an orphan step still classifies as RemoteCreate (the
    // driver's `process` will then skip it and emit a warning).
    let mut used_step_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in &catalog.workflow_steps {
        let slug = match lockfile.slug_for_id("workflow_steps", s.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&s.name, &used_step_slugs),
        };
        used_step_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(s) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("workflow_steps".to_string(), slug), hash);
    }
    if let Some(map) = lockfile.objects.get("workflow_steps") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("workflow_steps".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // TODO(sync-impl): repeat the four-source extraction for
    // workspaces / queues / hooks / rules / schemas / inboxes /
    // engines / engine_fields / email_templates / mdh. Each kind reuses
    // its own pull driver's canonicalization; tests will guide which
    // kind gets wired next.

    crate::cli::sync::classify::classify(&remote_hashes, &scan_changes, &scan_tombstones, &locked)
}
