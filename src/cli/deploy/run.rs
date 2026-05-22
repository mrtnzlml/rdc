//! `rdc deploy <src> <tgt>` — the first-class one-shot cross-env deploy.
//!
//! Semantics:
//! - **Additive by default**: tgt-only objects are kept intact. Pass
//!   `--mirror` to delete them so PROD becomes exactly TEST.
//! - **Plan-before-apply**: counts (and, on TTY, an interactive confirm)
//!   are printed before any write hits the target.
//! - **Idempotent**: re-running on an in-sync target is `0` API calls.
//!
//! Pipeline:
//!   1. Compute the plan by walking both local snapshots.
//!   2. Print + (TTY) confirm.
//!   3. **Create** missing tgt objects in dependency order — workspaces →
//!      schemas → queues → inboxes → email_templates → hooks → rules →
//!      labels → engines → engine_fields. Each create updates the
//!      in-memory tgt lockfile + mapping so subsequent kinds can rewrite
//!      cross-references to the just-created peers.
//!   4. **Update** field-level deltas (the in-process update phase that
//!      sees the full mapping built in step 1).
//!   5. **Delete** (mirror only) tgt-only objects in REVERSE dependency
//!      order so a queue is gone before its workspace.
//!   6. Save the updated lockfile + mapping; print a summary.

use crate::api::RossumClient;
use crate::cli::deploy::create::{
    create_hook, create_inbox, create_label, create_queue, create_rule, create_schema,
    create_workspace, locate_queue_dir, CreateCtx,
};
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::mapping::Mapping;
use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Instant;

/// Route a warning through the progress bar (if active) or directly to stderr.
fn warn(progress: Option<&Arc<Log>>, msg: String) {
    match progress {
        Some(p) => p.event(Action::Warn, &msg),
        None => eprintln!("{msg}"),
    }
}

/// Plan summary: how many objects per kind fall into each bucket.
#[derive(Debug, Default)]
struct PlanCounts {
    creates: BTreeMap<&'static str, Vec<String>>,
    deletes: BTreeMap<&'static str, Vec<String>>,
}

impl PlanCounts {
    fn create_total(&self) -> usize {
        self.creates.values().map(|v| v.len()).sum()
    }
    fn delete_total(&self) -> usize {
        self.deletes.values().map(|v| v.len()).sum()
    }
}

/// Kinds we deploy, in dependency order (POST order). Workflows are
/// pull-only at the Rossum API (PATCH returns 405) and so are not
/// deployable; MDH is not yet writable.
pub const KINDS_IN_DEP_ORDER: &[&str] = &[
    "workspaces",
    "schemas",
    "queues",
    "inboxes",
    "email_templates",
    "hooks",
    "rules",
    "labels",
    "engines",
    "engine_fields",
];

pub async fn run(src: &str, tgt: &str, mirror: bool, interactive: bool, dry_run: bool, diff: bool, only: Vec<String>) -> Result<()> {
    if src == tgt {
        return Err(anyhow!(
            "src and tgt envs are the same ('{src}'). Use two different envs for `rdc deploy`."
        ));
    }
    // A dry-run is for previewing changes — there's no useful brief
    // form, so always show the full per-object diff. The `--diff` flag
    // stays for backward compatibility but is now a no-op.
    let diff = diff || dry_run;

    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())?;
    let src_cfg = cfg
        .envs
        .get(src)
        .ok_or_else(|| anyhow!("env '{src}' is not defined in rdc.toml"))?;
    let tgt_cfg = cfg
        .envs
        .get(tgt)
        .ok_or_else(|| anyhow!("env '{tgt}' is not defined in rdc.toml"))?;

    let src_token = resolve_token(&cwd, src)?;
    let src_client = RossumClient::new(src_cfg.api_base.clone(), src_token)
        .context("constructing src API client")?;

    let tgt_token = resolve_token(&cwd, tgt)?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), tgt_token)
        .context("constructing tgt API client")?;

    let src_lockfile = Lockfile::load(&src_paths.lockfile())
        .with_context(|| format!("loading src lockfile from {}", src_paths.lockfile().display()))?;
    let mut tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())
        .with_context(|| format!("loading tgt lockfile from {}", tgt_paths.lockfile().display()))?;
    let mut mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file())
        .with_context(|| format!("loading tgt overlay from {}", tgt_paths.overlay_file().display()))?;

    // Store-extension pre-pass: resolve cross-cluster template URLs and
    // prompt for any missing token_owner values. Runs before any writes so
    // failures abort cleanly. The resulting plans feed the bootstrap loop
    // below, which routes store extensions through `/hooks/create` + PATCH.
    let store_plans = crate::cli::deploy::store_extensions::plan_store_extension_bootstrap(
        &src_paths,
        &tgt_paths,
        &src_client,
        &tgt_client,
        &src_lockfile,
        &tgt_lockfile,
        &mut mapping,
        &tgt_paths.overlay_file(),
        interactive,
        None, // self_user_id — picker just won't tag "you" in the list
        tgt,
        None,
    ).await?;
    // Persist the template URL pairs added by the pre-pass.
    std::fs::create_dir_all(src_paths.mapping_dir())
        .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
    mapping.save(&src_paths.mapping_file(src, tgt))?;

    // 0b. Auto-populate the slug-to-slug mapping for objects that already
    // exist in both envs. Without this step the apply sub-step would
    // skip every pre-existing object because the mapping would be empty.
    // User-curated entries in the mapping file (cross-env renames) are
    // preserved — `auto_match` never overwrites.
    crate::cli::deploy::map::auto_match(&mut mapping, &src_paths, &tgt_paths)?;

    // Resolve --only selectors against the local snapshots, then run a
    // cross-ref dep check. On TTY with missing deps, prompt to include
    // them. Non-TTY (--yes / CI) with missing deps → refuse.
    //
    // Placed AFTER auto_match so dep_check sees the same mapping the apply
    // phase will use, including renames and same-slug auto-matched pairs.
    let mut selection = crate::cli::deploy::selection::resolve(&only, &src_paths, &tgt_paths)?;
    if let Some(sel) = selection.as_mut() {
        let mut prompt_fn = |unresolved: &[crate::cli::deploy::selection::Unresolved]| -> bool {
            eprintln!();
            eprintln!("The following objects in your selection reference peers that aren't in");
            eprintln!("the selection and don't exist in '{tgt}' yet:");
            eprintln!();
            for u in unresolved {
                eprintln!(
                    "  {}/{}  -> {}/{} (missing)",
                    u.from.0, u.from.1, u.to.0, u.to.1,
                );
            }
            eprintln!();
            let mut deduped: std::collections::BTreeSet<(String, String)> = Default::default();
            for u in unresolved { deduped.insert(u.to.clone()); }
            eprint!("Include these {} dependencies in the selection? [Y/n] ", deduped.len());
            let _ = std::io::stderr().flush();
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_err() { return false; }
            let ans = input.trim().to_ascii_lowercase();
            ans.is_empty() || ans == "y" || ans == "yes"
        };
        crate::cli::deploy::selection::dep_check(
            sel,
            &src_paths,
            &src_lockfile,
            &tgt_lockfile,
            &mapping,
            interactive,
            &mut prompt_fn,
        )?;
    }
    // 1. Compute plan — file-system level only (which slugs are missing
    // from tgt, and in mirror mode which are tgt-only). Field-level diffs
    // are deferred to the apply sub-step because computing them eagerly
    // costs one GET per object across the whole snapshot.
    let plan = compute_plan(&src_paths, &tgt_paths, &src_lockfile, &tgt_lockfile, &mapping, mirror, selection.as_ref())?;

    // 2. Display plan
    print_plan(src, tgt, &plan, mirror, dry_run, &store_plans, selection.as_ref());

    // 3. Confirm (TTY only) — skip the prompt when nothing destructive is
    // planned OR when dry-run is set (no writes will happen). An
    // "all-update" sweep still runs (PATCHes only the objects whose
    // canonical content differs), which is safe to do silently.
    if !dry_run
        && interactive
        && (plan.create_total() > 0 || plan.delete_total() > 0)
    {
        if !confirm_or_abort("Proceed with deploy?")? {
            return Ok(());
        }
        if mirror && plan.delete_total() > 0 && !confirm_or_abort(&format!(
            "Mirror mode will DELETE {} object(s) from {tgt}. Continue?",
            plan.delete_total()
        ))? {
            return Ok(());
        }
    }

    // Pre-flight hook-secrets check. Runs BEFORE the target lock is
    // acquired so a missing-keys failure costs no contention. The
    // GET `/hooks/<src_id>/secrets_keys` calls only hit `src_client`
    // and are read-only.
    //
    // For each hook being POSTed (creates) or PATCHed (updates) on the
    // target, we ensure the local `secrets/<tgt>.hook-secrets.json`
    // declares a value for every key the source hook actually uses.
    // If any are missing the deploy aborts with a per-hook report and
    // no writes happen. Surplus keys in the target file are filtered
    // and warned about.
    let tgt_hook_secrets = crate::secrets::load_hook_secrets(tgt_paths.root(), tgt)
        .with_context(|| format!("loading hook secrets for target env '{tgt}'"))?;
    let mut hook_slugs_in_scope: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    if let Some(create_slugs) = plan.creates.get("hooks") {
        hook_slugs_in_scope.extend(create_slugs.iter().cloned());
    }
    // Update list = src→tgt mapping entries that already exist on tgt
    // (the apply phase iterates `mapping.hooks` to PATCH them).
    hook_slugs_in_scope.extend(mapping.hooks.keys().cloned());
    let precheck_api_calls = hook_slugs_in_scope.len();
    let hook_secrets_plan = crate::cli::deploy::hook_secrets::precheck(
        &src_client,
        &src_lockfile,
        &tgt_hook_secrets,
        hook_slugs_in_scope.into_iter(),
        tgt,
    )
    .await?;

    // Acquire an exclusive lock on the target env for the duration of the
    // write phase (creates + applies). Sync (one-shot or --watch) targeting
    // `tgt` will wait briefly here; deploy will wait briefly if a sync is
    // mid-cycle. Source env stays read-only — atomic-rename consistency
    // suffices for reading its snapshot.
    let _tgt_lock = crate::cli::sync::lock::EnvLock::acquire(
        &tgt_paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;

    // Start the progress bar for the work phase (not in dry-run).
    let progress: Option<Arc<Log>> = if !dry_run {
        Some(crate::log::Log::new(crate::cli::resolve::detect_color_mode(false)))
    } else {
        None
    };

    let started = Instant::now();
    let mut creates_done = 0usize;
    // Seed with the per-hook GETs the secrets pre-flight just issued.
    let mut api_calls = precheck_api_calls;

    // 4. Execute creates in dependency order. In dry-run mode we don't
    // POST — we just print what would happen. The create phase has no
    // pre-flight comparison to do (a slug missing in tgt is a single
    // file-system check, already done during plan computation), so
    // "previewing" a create amounts to re-stating the plan's per-kind
    // count line.
    if dry_run {
        for kind in KINDS_IN_DEP_ORDER {
            if let Some(slugs) = plan.creates.get(kind) {
                if !slugs.is_empty() {
                    println!("  -> {kind:18} {} would be created", slugs.len());
                }
            }
        }
        if diff && plan.create_total() > 0 {
            // Surface the full POST body per would-be-created object so
            // the user can audit exactly what `rdc deploy` would send.
            // The body goes through the same shaping the real create path
            // uses (URL rewrite, tgt overlay, server-field strip) so what
            // you see here is what would land on the wire.
            println!();
            println!("--- create bodies (would-be POST) ---");
            preview_create_bodies(
                &plan,
                &src_paths,
                &src_lockfile,
                &tgt_lockfile,
                &mapping,
                &tgt_overlay,
            )?;
        }
    } else {
        let mut ctx = CreateCtx {
            src_paths: &src_paths,
            tgt_paths: &tgt_paths,
            src_lockfile: &src_lockfile,
            tgt_lockfile: &mut tgt_lockfile,
            mapping: &mut mapping,
            tgt_overlay: &tgt_overlay,
            tgt_client: &tgt_client,
            progress: progress.clone(),
            hook_secrets_plan: &hook_secrets_plan,
        };
        // Lazily-populated cache of tgt remote hooks for store-extension
        // orphan checks. Shared across all hooks in this bootstrap run so
        // we list at most once.
        let mut remote_hooks_cache: Option<Vec<crate::model::Hook>> = None;
        for kind in KINDS_IN_DEP_ORDER {
            let Some(slugs) = plan.creates.get(kind) else { continue };
            if slugs.is_empty() { continue; }
            for slug in slugs {
                // Singular noun mapping for the event body.
                let singular = kind.trim_end_matches('s');
                let result = match *kind {
                    "workspaces" => create_workspace(&mut ctx, slug).await,
                    "schemas" => create_schema(&mut ctx, slug).await,
                    "queues" => {
                        // queue create also fetches its auto-peer templates,
                        // costing an extra LIST call per queue.
                        api_calls += 1;
                        create_queue(&mut ctx, slug).await
                    }
                    "inboxes" => create_inbox(&mut ctx, slug).await,
                    "email_templates" => {
                        // Email templates with unique types are auto-created
                        // by queue POST; here we only POST any custom (non-default)
                        // templates the user explicitly added in src that the
                        // queue auto-create didn't cover. The Rossum API rejects
                        // POSTing a second template with a unique type, so we let
                        // those flow to the update phase instead.
                        Ok(())
                    }
                    "hooks" => {
                        let plan = store_plans.iter().find(|p| p.src_slug == *slug);
                        create_hook(&mut ctx, slug, plan, &mut remote_hooks_cache).await
                    }
                    "rules" => create_rule(&mut ctx, slug).await,
                    "labels" => create_label(&mut ctx, slug).await,
                    "engines" | "engine_fields" => {
                        // Engines/engine_fields creation isn't exercised in
                        // current orgs and may 403; fall through silently to
                        // the update phase (which already has 405/403 handling).
                        Ok(())
                    }
                    _ => Ok(()),
                };
                // create_schema/create_queue/create_inbox emit their own Post event.
                // Emit Post for kinds whose create fns don't self-report.
                if result.is_ok() {
                    match *kind {
                        "workspaces" | "hooks" | "rules" | "labels" | "engines" | "engine_fields" => {
                            if let Some(p) = &progress {
                                p.event(Action::Post, &format!("{singular}/{slug}"));
                            }
                        }
                        _ => {}
                    }
                }
                result?;
                creates_done += 1;
                api_calls += 1;
            }
        }
        // Persist tgt lockfile after creates; apply re-reads from disk.
        tgt_lockfile
            .save(&tgt_paths.lockfile())
            .with_context(|| format!("saving tgt lockfile to {}", tgt_paths.lockfile().display()))?;
    }

    // 5. Persist the auto-matched mapping (in both dry-run and real-run).
    // In real-run, apply re-reads this from disk and uses it. In dry-run,
    // it leaves a clean state for a subsequent real `rdc deploy`.
    std::fs::create_dir_all(src_paths.mapping_dir())
        .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
    mapping.save(&src_paths.mapping_file(src, tgt))?;

    // 6. Run apply for the field-level update sweep. Apply now sees the
    // fully-populated mapping + tgt lockfile and PATCHes only the objects
    // whose canonical content (after URL rewrite + overlay) actually
    // differs from the tgt remote. Apply takes the tgt lockfile by `&mut`
    // so it can auto-adopt any out-of-band tgt changes detected by its
    // drift check (refreshing the recorded `content_hash`) without
    // bailing out — the upcoming PATCH overwrites the drift anyway.
    let apply_summary = crate::cli::deploy::apply::run(
        src,
        tgt,
        dry_run,
        diff,
        progress.clone(),
        &mut tgt_lockfile,
        selection.as_ref(),
        &hook_secrets_plan,
    )
    .await
    .with_context(|| format!(
        "update phase failed after {creates_done} create(s) succeeded"
    ))?;
    if !dry_run {
        tgt_lockfile
            .save(&tgt_paths.lockfile())
            .with_context(|| format!("saving tgt lockfile to {}", tgt_paths.lockfile().display()))?;
    }

    // 7. Deletes (mirror only), reverse dependency order so children go
    // before their parents. Dry-run reports what would be deleted but
    // doesn't issue DELETE calls; with `--diff`, the tgt-side body of
    // each soon-to-be-removed object is dumped first so the user can
    // see exactly what would disappear.
    let mut deletes_done = 0usize;
    if dry_run && diff && mirror && plan.delete_total() > 0 {
        println!();
        println!("--- delete bodies (would be removed from {tgt}) ---");
        preview_delete_bodies(&plan, &tgt_paths)?;
    }
    if mirror && plan.delete_total() > 0 {
        let mut rev = KINDS_IN_DEP_ORDER.to_vec();
        rev.reverse();
        for kind in rev {
            let Some(slugs) = plan.deletes.get(kind) else { continue };
            if dry_run {
                if !slugs.is_empty() {
                    println!("  - {kind:18} {} would be deleted", slugs.len());
                }
                continue;
            }
            for slug in slugs {
                if let Err(e) = delete_one(&tgt_client, &mut tgt_lockfile, &tgt_paths, kind, slug).await {
                    warn(progress.as_ref(), format!("warning: failed to delete {kind}/{slug}: {e:#}"));
                } else if let Some(p) = &progress {
                    let singular = kind.trim_end_matches('s');
                    p.event(Action::Delete, &format!("{singular}/{slug}"));
                }
                deletes_done += 1;
                api_calls += 1;
            }
        }
        if !dry_run {
            tgt_lockfile.save(&tgt_paths.lockfile())?;
        }
    }

    let elapsed = started.elapsed();
    if dry_run {
        let scope = selection.as_ref()
            .map(|s| format!(" (scoped, {} objects)", s.len()))
            .unwrap_or_default();
        // Emit a done event (no-op if None).
        if let Some(p) = &progress {
            p.event(Action::Done, &format!(
                "Dry run ({src} -> {tgt}){scope}: {} would be created, {} would be deleted, {:.1}s",
                plan.create_total(),
                plan.delete_total(),
                elapsed.as_secs_f64()
            ));
        }
        println!(
            "\nDry run ({src} -> {tgt}){scope}: {} would be created, {} would be deleted, \
             {:.1}s; no remote changes made.",
            plan.create_total(),
            plan.delete_total(),
            elapsed.as_secs_f64()
        );
    } else {
        let scope = selection.as_ref()
            .map(|s| format!(" (scoped, {} objects)", s.len()))
            .unwrap_or_default();
        // Emit a done event so the success summary lands after all phase output.
        if let Some(p) = &progress {
            p.event(Action::Done, &format!(
                "Deployed {src} -> {tgt}{scope}: {creates_done} created, {deletes_done} deleted, {api_calls} API calls, {:.1}s",
                elapsed.as_secs_f64()
            ));
        }
        // Print apply summary.
        println!("{apply_summary}");
        println!(
            "\nDeployed {src} -> {tgt}{scope}: {creates_done} created, {deletes_done} deleted, \
             {} API calls, {:.1}s",
            api_calls,
            elapsed.as_secs_f64()
        );
    }
    Ok(())
}

// ----- plan computation ------------------------------------------------

fn compute_plan(
    src_paths: &Paths,
    tgt_paths: &Paths,
    _src_lockfile: &Lockfile,
    _tgt_lockfile: &Lockfile,
    _mapping: &Mapping,
    mirror: bool,
    selection: Option<&crate::cli::deploy::selection::Selection>,
) -> Result<PlanCounts> {
    let mut plan = PlanCounts::default();

    for kind in KINDS_IN_DEP_ORDER {
        let mut src_slugs = list_slugs(src_paths, kind)?;
        let mut tgt_slugs: std::collections::HashSet<String> =
            list_slugs(tgt_paths, kind)?.into_iter().collect();

        if let Some(sel) = selection {
            src_slugs.retain(|s| sel.contains(kind, s));
            tgt_slugs.retain(|s| sel.contains(kind, s));
        }
        let mut to_create = Vec::new();
        for slug in &src_slugs {
            if !tgt_slugs.contains(slug) {
                to_create.push(slug.clone());
            }
        }
        if !to_create.is_empty() {
            plan.creates.insert(*kind, to_create);
        }
        if mirror {
            let src_set: std::collections::HashSet<&String> = src_slugs.iter().collect();
            let to_delete: Vec<String> = tgt_slugs
                .iter()
                .filter(|s| !src_set.contains(*s))
                .cloned()
                .collect();
            if !to_delete.is_empty() {
                plan.deletes.insert(*kind, to_delete);
            }
        }
    }
    Ok(plan)
}

/// List slugs from the local snapshot. The layout is kind-specific; we
/// reuse the same scanners `rdc map` uses for cross-env auto-matching.
pub(crate) fn list_slugs(paths: &Paths, kind: &str) -> Result<Vec<String>> {
    match kind {
        "workspaces" => list_workspace_slugs(paths),
        "schemas" | "queues" | "inboxes" => list_queue_nested(paths, kind),
        "email_templates" => list_email_template_keys(paths),
        "hooks" | "rules" | "labels" => list_flat_kind(paths, kind),
        "engines" => list_engine_slugs(paths),
        "engine_fields" => list_engine_field_slugs(paths),
        _ => Ok(Vec::new()),
    }
}

fn list_workspace_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.workspaces_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join("workspace.json").exists() {
            out.push(slug);
        }
    }
    out.sort();
    Ok(out)
}

fn list_queue_nested(paths: &Paths, kind: &str) -> Result<Vec<String>> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return Ok(Vec::new());
    }
    let file_name = match kind {
        "queues" => "queue.json",
        "schemas" => "schema.json",
        "inboxes" => "inbox.json",
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for ws_entry in std::fs::read_dir(&ws_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let queues_dir = ws_entry.path().join("queues");
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            if q_entry.path().join(file_name).exists() {
                out.push(q_entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn list_email_template_keys(paths: &Paths) -> Result<Vec<String>> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for ws_entry in std::fs::read_dir(&ws_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().into_owned();
        let queues_dir = ws_entry.path().join("queues");
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().into_owned();
            let et_dir = q_entry.path().join("email-templates");
            if !et_dir.exists() {
                continue;
            }
            for f in std::fs::read_dir(&et_dir)? {
                let f = f?;
                let name = f.file_name().to_string_lossy().into_owned();
                if crate::paths::is_shadow_artifact(&name, paths.env()) {
                    continue;
                }
                if let Some(stem) = name.strip_suffix(".json") {
                    out.push(format!("{ws_slug}/{q_slug}/{stem}"));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn list_flat_kind(paths: &Paths, kind: &str) -> Result<Vec<String>> {
    let dir = match kind {
        "hooks" => paths.hooks_dir(),
        "rules" => paths.rules_dir(),
        "labels" => paths.labels_dir(),
        _ => return Ok(Vec::new()),
    };
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if crate::paths::is_shadow_artifact(&name, paths.env()) {
            continue;
        }
        if let Some(stem) = name.strip_suffix(".json") {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
}

fn list_engine_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.engines_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.path().join("engine.json").exists() {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}

fn list_engine_field_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.engines_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e_entry in std::fs::read_dir(&dir)? {
        let e_entry = e_entry?;
        if !e_entry.file_type()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().into_owned();
        let fields_dir = paths.engine_fields_dir(&e_slug);
        if !fields_dir.exists() {
            continue;
        }
        for f_entry in std::fs::read_dir(&fields_dir)? {
            let f_entry = f_entry?;
            let name = f_entry.file_name().to_string_lossy().into_owned();
            if crate::paths::is_shadow_artifact(&name, paths.env()) {
                continue;
            }
            if let Some(stem) = name.strip_suffix(".json") {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

// ----- plan display + confirmation ------------------------------------

fn print_plan(
    src: &str,
    tgt: &str,
    plan: &PlanCounts,
    mirror: bool,
    dry_run: bool,
    store_plans: &[crate::cli::deploy::store_extensions::StorePlan],
    selection: Option<&crate::cli::deploy::selection::Selection>,
) {
    let suffix = if dry_run { "  (dry run; no remote changes)" } else { "" };
    if let Some(sel) = selection {
        println!("Plan: {src} -> {tgt}  (selection: {} objects via --only){suffix}", sel.len());
        println!("  Selected:");
        for (kind, slug) in &sel.items {
            println!("    {kind}/{slug}");
        }
    } else {
        println!("Plan: {src} -> {tgt}{suffix}");
    }
    if plan.create_total() == 0 {
        println!("  + create:  (none)");
    } else {
        let parts: Vec<String> = KINDS_IN_DEP_ORDER
            .iter()
            .filter_map(|k| plan.creates.get(k).map(|v| format!("{} {}", v.len(), k)))
            .collect();
        println!("  + create:  {}", parts.join(", "));

        // Surface store-extension subset when any of the hooks-to-create are
        // store extensions. Print a sub-line showing count, operation shape,
        // and a per-hook detail line with slug → template name + id.
        if !store_plans.is_empty() {
            let n = store_plans.len();
            let hooks_to_create = plan.creates.get("hooks").map(|v| v.len()).unwrap_or(0);
            if hooks_to_create > 0 && hooks_to_create >= n {
                println!(
                    "             > {} of the {} hooks are store extensions (POST /hooks/create + PATCH each):",
                    n, hooks_to_create
                );
            } else {
                let ext_word = if n == 1 { "store extension" } else { "store extensions" };
                println!(
                    "             > {} {} will be installed (POST /hooks/create + PATCH each):",
                    n, ext_word
                );
            }
            for sp in store_plans {
                let tgt_id = sp.tgt_template_url.rsplit('/').next().unwrap_or("?");
                println!(
                    "                 {} -> template '{}' on {} (id {})",
                    sp.src_slug, sp.tgt_template_name, tgt, tgt_id
                );
            }
        }
    }

    // Updates are computed lazily by the update phase below. The plan
    // summary only counts creates and deletes; the per-field diffs print
    // inline as part of the update sweep.
    println!("  ~ update:  field-level diffs printed below");

    if mirror {
        if plan.delete_total() == 0 {
            println!("  - delete:  (none; tgt has no extras)");
        } else {
            let parts: Vec<String> = KINDS_IN_DEP_ORDER
                .iter()
                .rev()
                .filter_map(|k| plan.deletes.get(k).map(|v| format!("{} {}", v.len(), k)))
                .collect();
            println!("  - delete:  {} (--mirror)", parts.join(", "));
        }
    }
    println!();
}

fn confirm_or_abort(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        // Non-TTY: respect the caller's earlier `interactive` check; if we
        // got here, the user wants a prompt, but with no terminal we can't
        // show one. Abort safely.
        eprintln!("note: non-TTY environment, refusing to proceed without --yes");
        return Ok(false);
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_ascii_lowercase();
    if ans == "y" || ans == "yes" {
        Ok(true)
    } else {
        println!("Aborted.");
        Ok(false)
    }
}

// ----- delete --------------------------------------------------------

async fn delete_one(
    client: &RossumClient,
    tgt_lockfile: &mut Lockfile,
    tgt_paths: &Paths,
    kind: &str,
    slug: &str,
) -> Result<()> {
    let entry = tgt_lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(slug))
        .ok_or_else(|| anyhow!("no lockfile entry for {kind}/{slug}"))?
        .clone();
    let id = entry.id;
    match kind {
        "hooks" => client.delete_hook(id, None).await?,
        "rules" => client.delete_rule(id, None).await?,
        "labels" => client.delete_label(id, None).await?,
        "queues" => client.delete_queue(id, None).await?,
        "schemas" => client.delete_schema(id, None).await?,
        "inboxes" => client.delete_inbox(id, None).await?,
        "email_templates" => client.delete_email_template(id, None).await?,
        "engines" => client.delete_engine(id, None).await?,
        "engine_fields" => client.delete_engine_field(id, None).await?,
        "workspaces" => client.delete_workspace(id, None).await?,
        _ => return Err(anyhow!("kind '{kind}' not deletable")),
    }
    // Remove local file(s).
    remove_local_file(tgt_paths, kind, slug);
    // Remove lockfile entry.
    if let Some(map) = tgt_lockfile.objects.get_mut(kind) {
        map.remove(slug);
    }
    Ok(())
}

fn remove_local_file(paths: &Paths, kind: &str, slug: &str) {
    match kind {
        "hooks" => {
            let _ = std::fs::remove_file(paths.hooks_dir().join(format!("{slug}.json")));
            // The sidecar may be `.py` (Python hook) or `.js` (Node.js
            // hook); best-effort remove both since this is a wholesale
            // local cleanup and the wrong one is just absent.
            let _ = std::fs::remove_file(paths.hooks_dir().join(format!("{slug}.py")));
            let _ = std::fs::remove_file(paths.hooks_dir().join(format!("{slug}.js")));
        }
        "rules" => {
            let _ = std::fs::remove_file(paths.rules_dir().join(format!("{slug}.json")));
            let _ = std::fs::remove_file(paths.rules_dir().join(format!("{slug}.py")));
        }
        "labels" => {
            let _ = std::fs::remove_file(paths.labels_dir().join(format!("{slug}.json")));
        }
        "workspaces" => {
            let _ = std::fs::remove_dir_all(paths.workspace_dir(slug));
        }
        "queues" | "schemas" | "inboxes" => {
            if let Some(q_dir) = locate_queue_dir(paths, slug) {
                match kind {
                    "queues" => {
                        let _ = std::fs::remove_dir_all(q_dir);
                    }
                    "schemas" => {
                        let _ = std::fs::remove_file(q_dir.join("schema.json"));
                        let _ = std::fs::remove_dir_all(q_dir.join("formulas"));
                    }
                    "inboxes" => {
                        let _ = std::fs::remove_file(q_dir.join("inbox.json"));
                    }
                    _ => {}
                }
            }
        }
        "email_templates" => {
            // slug is `<ws>/<q>/<template>`
            let parts: Vec<&str> = slug.splitn(3, '/').collect();
            if parts.len() == 3 {
                let p = paths
                    .queue_email_templates_dir(parts[0], parts[1])
                    .join(format!("{}.json", parts[2]));
                let _ = std::fs::remove_file(p);
            }
        }
        _ => {}
    }
}

// ----- dry-run --diff body previews ----------------------------------

/// For every would-be create in the plan, build the body the real deploy
/// would POST (URL-rewritten via the auto-matched mapping, tgt overlay
/// applied, server-managed fields stripped) and print it as a new-file
/// unified diff. The output matches `git diff` / `rdc diff` formatting so
/// it pipes cleanly into a pager or review tool.
fn preview_create_bodies(
    plan: &PlanCounts,
    src_paths: &Paths,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
    tgt_overlay: &Option<Overlay>,
) -> Result<()> {
    use crate::cli::deploy::common::rewrite_urls;
    use crate::overlay::apply_overrides;
    use crate::snapshot::create::strip_for_create;

    for kind in KINDS_IN_DEP_ORDER {
        let Some(slugs) = plan.creates.get(kind) else { continue };
        for slug in slugs {
            // For each kind, work out the on-disk path of the src file +
            // (when the kind is hooks/rules/schemas) the extracted code
            // sidecar that ships alongside the JSON.
            let (src_json, src_code) = match read_src_for_preview(kind, slug, src_paths) {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("warning: cannot preview {kind}/{slug}: {e:#}");
                    continue;
                }
            };
            // Shape the body the same way create_*() does so the preview
            // matches the real POST byte-for-byte.
            let mut value: serde_json::Value = match serde_json::from_slice(&src_json) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("warning: parsing {kind}/{slug}: {e:#}");
                    continue;
                }
            };
            rewrite_urls(&mut value, src_lockfile, tgt_lockfile, mapping, &std::collections::BTreeMap::new());
            if let Some(overlay_paths) = overlay_for(kind, slug, tgt_overlay) {
                apply_overrides(&mut value, overlay_paths);
            }
            strip_for_create(&mut value, kind);
            let body = serde_json::to_string_pretty(&value)?;
            crate::cli::diff::print_new_file_diff(
                &format!("{kind}/{slug}.json (would create)"),
                &body,
            );
            if let Some(code) = src_code {
                let ext = match *kind {
                    "hooks" | "rules" => "py",
                    _ => "txt",
                };
                crate::cli::diff::print_new_file_diff(
                    &format!("{kind}/{slug}.{ext} (would create)"),
                    &code,
                );
            }
        }
    }
    Ok(())
}

fn read_src_for_preview(
    kind: &str,
    slug: &str,
    src_paths: &Paths,
) -> Result<(Vec<u8>, Option<String>)> {
    match kind {
        "workspaces" => {
            let p = src_paths.workspace_dir(slug).join("workspace.json");
            Ok((std::fs::read(&p)?, None))
        }
        "hooks" => {
            let json_p = src_paths.hooks_dir().join(format!("{slug}.json"));
            let json_bytes = std::fs::read(&json_p)?;
            // Hook sidecar may be `.py` (Python) or `.js` (Node.js)
            // depending on the JSON's `config.runtime`. Pick the
            // runtime-matching path; fall back to the other extension
            // if the runtime-matched file is missing (defensive).
            let value: serde_json::Value =
                serde_json::from_slice(&json_bytes).unwrap_or(serde_json::Value::Null);
            let ext = crate::snapshot::hook::hook_code_extension_from_value(&value);
            let primary = src_paths.hooks_dir().join(format!("{slug}.{ext}"));
            let fallback = src_paths
                .hooks_dir()
                .join(format!("{slug}.{}", if ext == "py" { "js" } else { "py" }));
            let code = if primary.exists() {
                Some(std::fs::read_to_string(&primary)?)
            } else if fallback.exists() {
                Some(std::fs::read_to_string(&fallback)?)
            } else {
                None
            };
            Ok((json_bytes, code))
        }
        "rules" => {
            let json_p = src_paths.rules_dir().join(format!("{slug}.json"));
            let py_p = src_paths.rules_dir().join(format!("{slug}.py"));
            let py = if py_p.exists() {
                Some(std::fs::read_to_string(&py_p)?)
            } else {
                None
            };
            Ok((std::fs::read(&json_p)?, py))
        }
        "labels" | "engines" | "engine_fields" => {
            let dir = match kind {
                "labels" => src_paths.labels_dir(),
                "engines" => src_paths.engines_dir(),
                _ => src_paths.engines_dir(),
            };
            let p = dir.join(format!("{slug}.json"));
            Ok((std::fs::read(&p)?, None))
        }
        "queues" | "schemas" | "inboxes" => {
            let q_dir = crate::cli::deploy::create::locate_queue_dir(src_paths, slug)
                .ok_or_else(|| anyhow!("queue dir for '{slug}' not found"))?;
            let fname = match kind {
                "queues" => "queue.json",
                "schemas" => "schema.json",
                "inboxes" => "inbox.json",
                _ => unreachable!(),
            };
            Ok((std::fs::read(q_dir.join(fname))?, None))
        }
        "email_templates" => {
            // key is "<ws>/<q>/<template>"
            let parts: Vec<&str> = slug.splitn(3, '/').collect();
            if parts.len() != 3 {
                return Err(anyhow!("bad email_template key '{slug}'"));
            }
            let p = src_paths
                .queue_email_templates_dir(parts[0], parts[1])
                .join(format!("{}.json", parts[2]));
            Ok((std::fs::read(&p)?, None))
        }
        _ => Err(anyhow!("unsupported kind '{kind}' for preview")),
    }
}

fn overlay_for<'a>(
    kind: &str,
    slug: &str,
    overlay: &'a Option<Overlay>,
) -> Option<&'a std::collections::BTreeMap<String, serde_json::Value>> {
    let ov = overlay.as_ref()?;
    match kind {
        "hooks" => ov.hook(slug),
        "rules" => ov.rule(slug),
        "labels" => ov.label(slug),
        "schemas" => ov.schema(slug),
        "queues" => ov.queue(slug),
        "inboxes" => ov.inbox(slug),
        "email_templates" => ov.email_template(slug),
        _ => None,
    }
}

/// For every would-be delete (`--mirror` mode), dump the tgt on-disk body
/// labelled as deletion. Reads files directly off the snapshot — no API
/// calls.
fn preview_delete_bodies(plan: &PlanCounts, tgt_paths: &Paths) -> Result<()> {
    let mut rev = KINDS_IN_DEP_ORDER.to_vec();
    rev.reverse();
    for kind in rev {
        let Some(slugs) = plan.deletes.get(kind) else { continue };
        for slug in slugs {
            let (json, code) = match read_src_for_preview(kind, slug, tgt_paths) {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("warning: cannot read tgt {kind}/{slug} for delete preview: {e:#}");
                    continue;
                }
            };
            let body = String::from_utf8_lossy(&json);
            crate::cli::diff::print_deleted_file_diff(
                &format!("{kind}/{slug}.json (would delete)"),
                &body,
            );
            if let Some(code) = code {
                let ext = match kind {
                    "hooks" | "rules" => "py",
                    _ => "txt",
                };
                crate::cli::diff::print_deleted_file_diff(
                    &format!("{kind}/{slug}.{ext} (would delete)"),
                    &code,
                );
            }
        }
    }
    Ok(())
}
