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
/// TODO(sync-impl): remaining kinds — `queues`, `schemas` (combined
/// hash), `inboxes`, `email_templates` — plug in as their integration
/// tests land. Each kind reuses its own pull driver's canonicalization
/// rules; see `cli::pull::<kind>::process` for the authoritative
/// serialization order.
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

    // --- workspaces ---------------------------------------------------
    // Push-capable flat kind. Slug derivation mirrors
    // `pull::workspaces::process`: lockfile-anchored id mapping first,
    // else `slugify_unique(name, used)`. Hash matches the driver's
    // canonical form (`serde_json::to_vec_pretty` → `\n` → content_hash);
    // workspaces have no overlay so no strip step.
    let mut used_workspace_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for w in &catalog.workspaces {
        let slug = match lockfile.slug_for_id("workspaces", w.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&w.name, &used_workspace_slugs),
        };
        used_workspace_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(w) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("workspaces".to_string(), slug), hash);
    }
    for (slug, path) in &changes.workspaces {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("workspaces".to_string(), slug.clone()),
                crate::state::content_hash(&bytes),
            );
        }
    }
    for slug in tombstones.workspaces.keys() {
        scan_tombstones.insert(("workspaces".to_string(), slug.clone()));
    }
    if let Some(map) = lockfile.objects.get("workspaces") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("workspaces".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- engines ------------------------------------------------------
    // Push-capable flat kind. Engines have an overlay; the pull driver
    // calls `maybe_strip_overlay` before hashing. When no overlay is
    // configured (`overlay.engines.<slug>` empty or unset),
    // `maybe_strip_overlay` is a no-op and the adapter's hash matches
    // the driver byte-for-byte. Once an overlay-aware test arrives,
    // thread `&Overlay` into the adapter and replicate the strip — same
    // caveat as labels.
    let mut used_engine_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in &catalog.engines {
        let slug = match lockfile.slug_for_id("engines", e.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&e.name, &used_engine_slugs),
        };
        used_engine_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(e) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("engines".to_string(), slug), hash);
    }
    for (slug, path) in &changes.engines {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("engines".to_string(), slug.clone()),
                crate::state::content_hash(&bytes),
            );
        }
    }
    for slug in tombstones.engines.keys() {
        scan_tombstones.insert(("engines".to_string(), slug.clone()));
    }
    if let Some(map) = lockfile.objects.get("engines") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("engines".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- engine_fields ------------------------------------------------
    // Push-capable flat kind keyed by field slug alone (matching the
    // lockfile + push scanner). Path is `engines/<engine>/fields/<slug>.json`
    // and `change_list_from_classified` already sweeps for it. The pull
    // driver also skips orphan fields (no parent engine in the lockfile),
    // but the adapter emits a slug regardless; classifier emits
    // RemoteCreate and the driver later skips with a warning.
    //
    // Same overlay caveat as engines / labels: no strip done here today.
    let mut used_ef_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for f in &catalog.engine_fields {
        let slug = match lockfile.slug_for_id("engine_fields", f.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&f.name, &used_ef_slugs),
        };
        used_ef_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(f) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("engine_fields".to_string(), slug), hash);
    }
    for (slug, path) in &changes.engine_fields {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("engine_fields".to_string(), slug.clone()),
                crate::state::content_hash(&bytes),
            );
        }
    }
    for slug in tombstones.engine_fields.keys() {
        scan_tombstones.insert(("engine_fields".to_string(), slug.clone()));
    }
    if let Some(map) = lockfile.objects.get("engine_fields") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("engine_fields".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- mdh ----------------------------------------------------------
    // MDH is two kinds in the lockfile (`mdh_collections` and
    // `mdh_indexes`) but a single dataset slug. The pull driver derives
    // the slug from `Collection::name` only — there's no id-anchored
    // lookup, so re-running the slugger is sufficient.
    //
    // Surface both kinds in `remote_hashes` / `locked` because they're
    // tracked separately in the lockfile; the dispatcher in `execute::run`
    // re-pulls both via `mdh::process` when either differs. The catalog
    // doesn't carry the index set (it's fetched per-dataset inside
    // `mdh::process`), so we only have the collection bytes available
    // here for hashing. The index hash will land in `locked` if the
    // lockfile recorded one; the classifier will compare it against an
    // empty remote_hashes entry → RemoteEdit, which is fine: the executor
    // dispatches to `mdh::process`, which fetches and writes both.
    let mut used_mdh_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for c in &catalog.mdh.collections {
        let slug = crate::slug::slugify_unique(&c.name, &used_mdh_slugs);
        used_mdh_slugs.insert(slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(c) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("mdh".to_string(), slug), hash);
    }
    if let Some(map) = lockfile.objects.get("mdh_collections") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("mdh".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- hooks --------------------------------------------------------
    // Push-capable split-file kind: `<slug>.json` + optional `<slug>.py`
    // (the extracted `config.code`). The canonical hash combines both —
    // `hook_combined_hash(json_bytes, code)` — and is what both the pull
    // driver and the push scanner record. Slug derivation mirrors
    // `pull::hooks::process`: lockfile-anchored id mapping first, else
    // `slugify_unique(name, used)`.
    //
    // Overlay caveat (same as labels/engines): the pull driver runs
    // `maybe_strip_overlay` on `proposed_json` before hashing. We skip
    // that step here; once an overlay-aware adapter test arrives, thread
    // `&Overlay` in and call `maybe_strip_overlay` on the same paths.
    let mut used_hook_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in &catalog.hooks {
        let slug = match lockfile.slug_for_id("hooks", h.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&h.name, &used_hook_slugs),
        };
        used_hook_slugs.insert(slug.clone());

        // Reproduce the pull driver's canonical bytes: serialize → strip
        // `config.code` into `code` → trailing newline on JSON.
        let (json_bytes, code) = match crate::snapshot::hook::serialize_hook(h) {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let hash = crate::state::hook_combined_hash(&json_bytes, &code);
        remote_hashes.insert(("hooks".to_string(), slug), hash);
    }
    for (slug, json_path) in &changes.hooks {
        let json_bytes = match std::fs::read(json_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let py_path = json_path.with_extension("py");
        let code = if py_path.exists() {
            std::fs::read_to_string(&py_path).ok()
        } else {
            None
        };
        let hash = crate::state::hook_combined_hash(&json_bytes, &code);
        scan_changes.insert(("hooks".to_string(), slug.clone()), hash);
    }
    for slug in tombstones.hooks.keys() {
        scan_tombstones.insert(("hooks".to_string(), slug.clone()));
    }
    if let Some(map) = lockfile.objects.get("hooks") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("hooks".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // --- rules --------------------------------------------------------
    // Push-capable split-file kind, identical shape to hooks except the
    // code lives in `trigger_condition` (top-level) rather than
    // `config.code`. The canonical hash is `rule_combined_hash`. Slug
    // derivation mirrors `pull::rules::process`.
    let mut used_rule_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &catalog.rules {
        let slug = match lockfile.slug_for_id("rules", r.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&r.name, &used_rule_slugs),
        };
        used_rule_slugs.insert(slug.clone());

        let (json_bytes, code) = match crate::snapshot::rule::serialize_rule(r) {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let hash = crate::state::rule_combined_hash(&json_bytes, &code);
        remote_hashes.insert(("rules".to_string(), slug), hash);
    }
    for (slug, json_path) in &changes.rules {
        let json_bytes = match std::fs::read(json_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let py_path = json_path.with_extension("py");
        let code = if py_path.exists() {
            std::fs::read_to_string(&py_path).ok()
        } else {
            None
        };
        let hash = crate::state::rule_combined_hash(&json_bytes, &code);
        scan_changes.insert(("rules".to_string(), slug.clone()), hash);
    }
    for slug in tombstones.rules.keys() {
        scan_tombstones.insert(("rules".to_string(), slug.clone()));
    }
    if let Some(map) = lockfile.objects.get("rules") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("rules".to_string(), slug.clone()), h.clone());
            }
        }
    }

    crate::cli::sync::classify::classify(&remote_hashes, &scan_changes, &scan_tombstones, &locked)
}
