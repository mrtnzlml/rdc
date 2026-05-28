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
    create_engine, create_engine_field, create_hook, create_inbox, create_label,
    create_queue, create_rule, create_schema, create_workspace, locate_queue_dir,
    CreateCtx,
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

pub async fn run(src: &str, tgt: &str, mirror: bool, interactive: bool, dry_run: bool, force_overwrite_drift: bool, only: Vec<String>) -> Result<()> {
    if src == tgt {
        return Err(anyhow!(
            "src and tgt envs are the same ('{src}'). Use two different envs for `rdc deploy`."
        ));
    }

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

    let src_token = resolve_token(&cwd, src, &src_cfg.api_base).await?;
    let src_client = RossumClient::new(src_cfg.api_base.clone(), src_token)
        .context("constructing src API client")?
        .with_env_label(src);

    let tgt_token = resolve_token(&cwd, tgt, &tgt_cfg.api_base).await?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), tgt_token)
        .context("constructing tgt API client")?
        .with_env_label(tgt);

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

    // Regular-hook pre-pass: same picker, applied to non-store-extension
    // hooks. Without this, a POST /hooks for a hook whose src snapshot
    // carries a (src-env) `token_owner` user URL 400s with
    // "Invalid hyperlink — Object does not exist." Runs after the
    // store-extension pre-pass so the user is prompted at most once per
    // hook and `[defaults] store_extension_token_owner` is honored
    // across both pre-passes.
    let regular_hook_token_owners =
        crate::cli::deploy::store_extensions::plan_regular_hook_token_owners(
            &src_paths,
            &tgt_client,
            &tgt_lockfile,
            &mapping,
            &tgt_paths.overlay_file(),
            interactive,
            None,
            tgt,
            None,
        )
        .await?;

    // 0b. Auto-populate the slug-to-slug mapping for objects that already
    // exist in both envs. Without this step the apply sub-step would
    // skip every pre-existing object because the mapping would be empty.
    // User-curated entries in the mapping file (cross-env renames) are
    // preserved — `auto_match` never overwrites.
    crate::cli::deploy::map::auto_match(&mut mapping, &src_paths, &tgt_paths)?;
    // Persist immediately so the preview pass below (which calls
    // `apply::run` that reloads the mapping from disk) sees the
    // auto-matched entries. Without this, the preview reports "0
    // updates" even when same-slug pairs exist in both envs.
    mapping.save(&src_paths.mapping_file(src, tgt))?;

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
    // from tgt, and in mirror mode which are tgt-only). Field-level
    // diffs are surfaced by the preview pass below.
    let plan = compute_plan(&src_paths, &tgt_paths, &src_lockfile, &tgt_lockfile, &mapping, mirror, selection.as_ref())?;

    // 2. Pre-flight hook-secrets check. Runs before the preview so a
    // missing-keys failure surfaces immediately rather than after the
    // user reads through a (potentially long) diff. The GET
    // `/hooks/<src_id>/secrets_keys` calls hit `src_client` only and
    // are read-only. Pre-population of the target's hook-secrets file
    // is handled inside `precheck` on failure.
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
        hook_slugs_in_scope,
        tgt,
        tgt_paths.root(),
    )
    .await?;

    let started = Instant::now();

    // 3. Full-diff preview. Runs unconditionally so dry-run AND real
    // deploys both surface the same "what would change" view: create
    // bodies (would-be POST), per-object update diffs (src after
    // overlay+rewrite vs tgt remote), then — in mirror mode — delete
    // bodies (would be removed). The update preview drives apply with
    // dry_run+diff on a CLONED tgt_lockfile so the real apply pass
    // below sees pristine state and re-derives drift adoption against
    // the actual tgt remote.
    if plan.create_total() > 0 {
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
    // Preview apply runs against a CLONED tgt_lockfile so its drift
    // adoption + diff rendering is purely informational. It returns
    // the list of objects that drifted on the target since the last
    // `rdc sync <tgt>`; we surface those to the user (or auto-resolve
    // via --force-overwrite-drift) BEFORE the real apply touches the
    // remote.
    let drifted_items: Vec<crate::cli::deploy::apply::DriftedItem> = {
        let mut preview_lockfile = tgt_lockfile.clone();
        let empty_decisions: BTreeMap<
            (String, String),
            crate::cli::deploy::apply::DriftDecision,
        > = BTreeMap::new();
        let preview_outcome = crate::cli::deploy::apply::run(
            src,
            tgt,
            true,
            true,
            None,
            &mut preview_lockfile,
            selection.as_ref(),
            &hook_secrets_plan,
            &empty_decisions,
        )
        .await
        .with_context(|| "computing update preview")?;
        println!();
        println!("{}", preview_outcome.summary);
        preview_outcome.drifted
    };
    if mirror && plan.delete_total() > 0 {
        println!();
        println!("--- delete bodies (would be removed from {tgt}) ---");
        preview_delete_bodies(&plan, &tgt_paths)?;
    }

    // 4. Dry-run exit: preview was the deliverable, no writes follow.
    if dry_run {
        let elapsed = started.elapsed();
        let scope = selection.as_ref()
            .map(|s| format!(" (scoped, {} objects)", s.len()))
            .unwrap_or_default();
        if !drifted_items.is_empty() {
            println!();
            println!(
                "Note: {} target object(s) drifted from the last-sync baseline:",
                drifted_items.len()
            );
            for d in &drifted_items {
                println!("  - {}/{}", d.kind, d.slug);
            }
            println!(
                "A real deploy would prompt [k]/[o]/[s]/[a] per item \
                 (or require --force-overwrite-drift in non-TTY mode)."
            );
        }
        println!(
            "\nDry run ({src} -> {tgt}){scope}: {} would be created, {} would be deleted, \
             {:.1}s; no remote changes made.",
            plan.create_total(),
            plan.delete_total(),
            elapsed.as_secs_f64()
        );
        return Ok(());
    }

    // 5. Drift resolver. For each tgt object that was edited out-of-band
    //    since the last sync, ask the user what to do BEFORE the main
    //    "Proceed?" confirm. Without this step, deploy silently
    //    overwrites those edits — the unsafe default this resolver
    //    replaces. Non-TTY / --yes invocations refuse unless the user
    //    passed --force-overwrite-drift, which auto-overwrites all.
    let drift_decisions: BTreeMap<(String, String), crate::cli::deploy::apply::DriftDecision> =
        resolve_drift_decisions(&drifted_items, tgt, interactive, force_overwrite_drift)?;

    // 6. Confirm (TTY only). Skipped when nothing destructive is planned —
    // an "all-update" sweep still runs but PATCHes only objects whose
    // canonical content differs, which is safe to do silently.
    if interactive && (plan.create_total() > 0 || plan.delete_total() > 0) {
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

    // 6. Acquire an exclusive lock on the target env for the duration of
    // the write phase (creates + applies + deletes). Sync (one-shot or
    // --watch) targeting `tgt` will wait briefly here; deploy will wait
    // briefly if a sync is mid-cycle. Source env stays read-only —
    // atomic-rename consistency suffices for reading its snapshot.
    let _tgt_lock = crate::cli::sync::lock::EnvLock::acquire(
        &tgt_paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;

    // 7. Start the progress bar for the work phase.
    let progress: Option<Arc<Log>> =
        Some(crate::log::Log::new(crate::cli::resolve::detect_color_mode(false)));

    let mut creates_done = 0usize;
    // Seed with the per-hook GETs the secrets pre-flight just issued.
    let mut api_calls = precheck_api_calls;

    // 8. Execute creates in dependency order.
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
        regular_hook_token_owners: &regular_hook_token_owners,
    };
    // Lazily-populated cache of tgt remote hooks for store-extension
    // orphan checks. Shared across all hooks in this bootstrap run so
    // we list at most once.
    let mut remote_hooks_cache: Option<Vec<crate::model::Hook>> = None;
    // Each successful create mutates `tgt_lockfile` in memory. We
    // persist after every per-slug success so a later failure leaves
    // the on-disk lockfile in sync with what's actually on the remote
    // — otherwise a re-run can't rewrite src URLs (the tgt queue URL
    // the mapping looks up isn't in any lockfile yet), and the deploy
    // gets stuck in an unresumeable state. Persisting after every
    // create is cheap (atomic rename, single small file).
    let mut create_err: Option<anyhow::Error> = None;
    'outer: for kind in KINDS_IN_DEP_ORDER {
        let Some(slugs) = plan.creates.get(kind) else { continue };
        if slugs.is_empty() { continue; }
        for slug in slugs {
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
                    // by queue POST; here we only POST any custom
                    // (non-default) templates the user explicitly added
                    // in src that the queue auto-create didn't cover.
                    // The Rossum API rejects POSTing a second template
                    // with a unique type, so we let those flow to the
                    // update phase instead.
                    Ok(())
                }
                "hooks" => {
                    let plan = store_plans.iter().find(|p| p.src_slug == *slug);
                    create_hook(&mut ctx, slug, plan, &mut remote_hooks_cache).await
                }
                "rules" => create_rule(&mut ctx, slug).await,
                "labels" => create_label(&mut ctx, slug).await,
                "engines" => create_engine(&mut ctx, slug).await,
                "engine_fields" => create_engine_field(&mut ctx, slug).await,
                _ => Ok(()),
            };
            // create_schema/create_queue/create_inbox emit their own
            // Post event. Emit Post for kinds whose create fns don't
            // self-report.
            if result.is_ok() {
                match *kind {
                    "workspaces" | "hooks" | "rules" | "labels" | "engines"
                    | "engine_fields" => {
                        if let Some(p) = &progress {
                            p.event(Action::Post, &format!("{singular}/{slug}"));
                        }
                    }
                    _ => {}
                }
            }
            if let Err(e) = result {
                create_err = Some(e);
                break 'outer;
            }
            // Flush after each successful create so a later failure
            // can be resumed by re-running `rdc deploy` (the resume
            // path needs the tgt URLs of just-created peers to
            // rewrite hook/rule/etc. cross-references).
            if let Err(e) = ctx.tgt_lockfile.save(&tgt_paths.lockfile()) {
                create_err = Some(e.context(format!(
                    "saving tgt lockfile after creating {kind}/{slug}",
                )));
                break 'outer;
            }
            if let Err(e) = ctx.mapping.save(&src_paths.mapping_file(src, tgt)) {
                create_err = Some(e.context(format!(
                    "saving src->tgt mapping after creating {kind}/{slug}",
                )));
                break 'outer;
            }
            creates_done += 1;
            api_calls += 1;
        }
    }
    if let Some(e) = create_err {
        return Err(e);
    }

    // 9. Persist the auto-matched mapping for the apply phase to read.
    std::fs::create_dir_all(src_paths.mapping_dir())
        .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
    mapping.save(&src_paths.mapping_file(src, tgt))?;

    // 10. Real apply. Per-object diffs were rendered in the preview pass
    // above, so apply runs here with dry_run=false, diff=false and emits
    // only its progress events for each PATCH. Apply takes the tgt
    // lockfile by `&mut`; the drift_decisions map captures the resolver
    // outcomes from step 5 so each drifted object is handled per the
    // user's choice (overwrite / keep / skip).
    let apply_outcome = crate::cli::deploy::apply::run(
        src,
        tgt,
        false,
        false,
        progress.clone(),
        &mut tgt_lockfile,
        selection.as_ref(),
        &hook_secrets_plan,
        &drift_decisions,
    )
    .await
    .with_context(|| format!(
        "update phase failed after {creates_done} create(s) succeeded"
    ))?;
    let apply_summary = apply_outcome.summary;
    tgt_lockfile
        .save(&tgt_paths.lockfile())
        .with_context(|| format!("saving tgt lockfile to {}", tgt_paths.lockfile().display()))?;

    // 11. Deletes (mirror only), reverse dependency order so children
    // go before their parents.
    let mut deletes_done = 0usize;
    if mirror && plan.delete_total() > 0 {
        let mut rev = KINDS_IN_DEP_ORDER.to_vec();
        rev.reverse();
        for kind in rev {
            let Some(slugs) = plan.deletes.get(kind) else { continue };
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
        tgt_lockfile.save(&tgt_paths.lockfile())?;
    }

    // 12. Final summary.
    let elapsed = started.elapsed();
    let scope = selection.as_ref()
        .map(|s| format!(" (scoped, {} objects)", s.len()))
        .unwrap_or_default();
    if let Some(p) = &progress {
        p.event(Action::Done, &format!(
            "Deployed {src} -> {tgt}{scope}: {creates_done} created, {deletes_done} deleted, {api_calls} API calls, {:.1}s",
            elapsed.as_secs_f64()
        ));
    }
    println!("{apply_summary}");
    println!(
        "\nDeployed {src} -> {tgt}{scope}: {creates_done} created, {deletes_done} deleted, \
         {} API calls, {:.1}s",
        api_calls,
        elapsed.as_secs_f64()
    );
    Ok(())
}

// ----- plan computation ------------------------------------------------

fn compute_plan(
    src_paths: &Paths,
    tgt_paths: &Paths,
    src_lockfile: &Lockfile,
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
            // Hooks carry intra-kind ordering constraints via `run_after`
            // (a list of other hook URLs that must run before this one).
            // The alphabetical slug order produced by `list_slugs` can
            // place a dependent before its prerequisite, which makes the
            // POST 400 with "Invalid hyperlink — Object does not exist."
            // since the dependency hook hasn't been created on tgt yet.
            // Topologically sort the create list so prerequisites land
            // first; deps already on tgt are ignored (rewrite_urls will
            // translate those refs to existing tgt URLs).
            let ordered = if *kind == "hooks" {
                topo_sort_hooks_to_create(&to_create, src_paths, src_lockfile)
            } else {
                to_create
            };
            plan.creates.insert(*kind, ordered);
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

/// Topologically sort the hooks-to-create list so each hook is POSTed
/// after the hooks listed in its `run_after`. Ties (independent hooks)
/// resolve alphabetically — same order `list_slugs` produces — so the
/// output is deterministic and the only re-orderings are forced by real
/// dependencies.
///
/// Dependencies pointing at hooks that already exist on tgt (not in
/// `slugs`) impose no constraint here — `rewrite_urls` will translate
/// those URLs to existing tgt entries and the API will accept them.
/// Unresolvable URLs (point at a hook neither tgt nor src snapshot
/// knows) are ignored so a stale `run_after` entry doesn't break the
/// whole deploy plan; if it's genuinely broken it'll surface as the
/// real API error during POST instead of a confusing planner error.
///
/// On a cycle (Rossum normally rejects cyclic `run_after` chains at
/// PATCH time, but defend anyway) we preserve the original alphabetical
/// order so the cycle still surfaces as the API's own error instead of
/// a cryptic ordering failure.
fn topo_sort_hooks_to_create(
    slugs: &[String],
    src_paths: &Paths,
    src_lockfile: &Lockfile,
) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};

    let slug_set: BTreeSet<&str> = slugs.iter().map(|s| s.as_str()).collect();

    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for slug in slugs {
        deps.insert(slug.clone(), BTreeSet::new());
    }
    for slug in slugs {
        let path = src_paths.hooks_dir().join(format!("{slug}.json"));
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        let Some(run_after) = value.get("run_after").and_then(|v| v.as_array()) else { continue };
        for url_v in run_after {
            let Some(url) = url_v.as_str() else { continue };
            let Some(dep_slug) = src_lockfile.slug_for_url("hooks", url) else { continue };
            if dep_slug != slug && slug_set.contains(dep_slug) {
                deps.get_mut(slug).expect("inserted above").insert(dep_slug.to_string());
            }
        }
    }

    // Kahn's algorithm: maintain a BTreeSet of ready nodes so ties pop
    // alphabetically (deterministic, same shape as `list_slugs`).
    let mut in_degree: BTreeMap<String, usize> =
        slugs.iter().map(|s| (s.clone(), 0usize)).collect();
    let mut reverse_deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (slug, dep_set) in &deps {
        for dep in dep_set {
            *in_degree.get_mut(slug).expect("in_degree pre-populated") += 1;
            reverse_deps
                .entry(dep.clone())
                .or_default()
                .insert(slug.clone());
        }
    }
    let mut ready: BTreeSet<String> = in_degree
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(s, _)| s.clone())
        .collect();

    let mut out: Vec<String> = Vec::with_capacity(slugs.len());
    while let Some(next) = ready.iter().next().cloned() {
        ready.remove(&next);
        out.push(next.clone());
        if let Some(dependents) = reverse_deps.get(&next) {
            for dep in dependents {
                let cnt = in_degree.get_mut(dep).expect("in_degree pre-populated");
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    ready.insert(dep.clone());
                }
            }
        }
    }

    if out.len() != slugs.len() {
        return slugs.to_vec();
    }
    out
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

// ----- drift resolver -------------------------------------------------

/// Build the per-object drift-decisions map. On TTY (`interactive ==
/// true`), prompts `[k]eep tgt / [o]verwrite / [s]kip / [a]bort` for
/// each drifted item; the `[a]` choice aborts the deploy. On non-TTY,
/// refuses unless the caller passed `--force-overwrite-drift`, which
/// short-circuits every decision to `Overwrite`. Returns an empty map
/// when no drift was detected.
fn resolve_drift_decisions(
    drifted: &[crate::cli::deploy::apply::DriftedItem],
    tgt_env: &str,
    interactive: bool,
    force_overwrite_drift: bool,
) -> Result<BTreeMap<(String, String), crate::cli::deploy::apply::DriftDecision>> {
    use crate::cli::deploy::apply::DriftDecision;
    let mut decisions: BTreeMap<(String, String), DriftDecision> = BTreeMap::new();
    if drifted.is_empty() {
        return Ok(decisions);
    }
    if force_overwrite_drift {
        for d in drifted {
            decisions.insert((d.kind.clone(), d.slug.clone()), DriftDecision::Overwrite);
        }
        println!();
        println!(
            "--force-overwrite-drift: auto-overwriting {} target object(s) \
             that drifted from the last-sync baseline.",
            drifted.len()
        );
        return Ok(decisions);
    }
    if !interactive {
        let names: Vec<String> = drifted
            .iter()
            .map(|d| format!("  - {}/{}", d.kind, d.slug))
            .collect();
        return Err(anyhow!(
            "deploy refused: {} target object(s) drifted on '{tgt_env}' since the last \
             `rdc sync {tgt_env}`:\n{}\n\nRe-run with --force-overwrite-drift to overwrite \
             these out-of-band edits, or run `rdc sync {tgt_env}` first to reconcile the \
             baseline.",
            drifted.len(),
            names.join("\n"),
        ));
    }
    // TTY interactive: per-object prompt. Echo the list once so the
    // user has the full picture before answering item by item.
    println!();
    println!(
        "⚠️  {} target object(s) have been edited out-of-band since the last `rdc sync {tgt_env}`:",
        drifted.len()
    );
    for d in drifted {
        println!("  - {}/{}", d.kind, d.slug);
    }
    println!();
    println!(
        "For each item, choose: [k]eep tgt edit · [o]verwrite with src · [s]kip (defer) · [a]bort"
    );
    for d in drifted {
        let decision = prompt_drift_decision(&d.kind, &d.slug)?;
        match decision {
            Some(dec) => {
                decisions.insert((d.kind.clone(), d.slug.clone()), dec);
            }
            None => {
                println!("Aborted.");
                return Err(anyhow!("deploy aborted at drift resolver"));
            }
        }
    }
    Ok(decisions)
}

/// One per-object drift prompt. `Ok(None)` = the user picked `[a]bort`
/// — caller turns that into an error so the deploy short-circuits.
/// Unrecognised input re-prompts. Empty input is treated as `[a]bort`
/// so a stray Enter doesn't silently fall through to an unsafe
/// default.
fn prompt_drift_decision(
    kind: &str,
    slug: &str,
) -> Result<Option<crate::cli::deploy::apply::DriftDecision>> {
    use crate::cli::deploy::apply::DriftDecision;
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(anyhow!(
            "prompt_drift_decision called in non-TTY context (caller bug)"
        ));
    }
    loop {
        eprint!("  {kind}/{slug}: [k/o/s/a] ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        let stdin = std::io::stdin();
        stdin.lock().read_line(&mut line)?;
        let ans = line.trim().to_ascii_lowercase();
        return Ok(match ans.as_str() {
            "k" | "keep" => Some(DriftDecision::Keep),
            "o" | "overwrite" => Some(DriftDecision::Overwrite),
            "s" | "skip" => Some(DriftDecision::Skip),
            "a" | "abort" | "" => None,
            _ => {
                eprintln!("    please answer with k / o / s / a");
                continue;
            }
        });
    }
}

// ----- confirmation ---------------------------------------------------

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

// ----- preview body renderers ----------------------------------------

/// For every would-be create in the plan, build the body the real deploy
/// would POST (URL-rewritten via the auto-matched mapping, tgt overlay
/// applied, server-managed fields stripped) and print it as a new-file
/// unified diff. The output matches `git diff` formatting so it pipes
/// cleanly into a pager or review tool.
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
            crate::cli::resolve::print_new_file_diff(
                &format!("{kind}/{slug}.json (would create)"),
                &body,
            );
            if let Some(code) = src_code {
                let ext = match *kind {
                    "hooks" | "rules" => "py",
                    _ => "txt",
                };
                crate::cli::resolve::print_new_file_diff(
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
        "labels" => {
            let p = src_paths.labels_dir().join(format!("{slug}.json"));
            Ok((std::fs::read(&p)?, None))
        }
        "engines" => {
            let p = src_paths.engine_dir(slug).join("engine.json");
            Ok((std::fs::read(&p)?, None))
        }
        "engine_fields" => {
            // Engine fields live at engines/<engine_slug>/fields/<field_slug>.json.
            // Find which engine contains this field via the shared helper.
            let e_slug = src_paths.engine_slug_for_field(slug).ok_or_else(|| {
                anyhow!(
                    "engine_field '{slug}' not found under any engine in {}",
                    src_paths.engines_dir().display()
                )
            })?;
            let p = src_paths
                .engine_fields_dir(&e_slug)
                .join(format!("{slug}.json"));
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
            crate::cli::resolve::print_deleted_file_diff(
                &format!("{kind}/{slug}.json (would delete)"),
                &body,
            );
            if let Some(code) = code {
                let ext = match kind {
                    "hooks" | "rules" => "py",
                    _ => "txt",
                };
                crate::cli::resolve::print_deleted_file_diff(
                    &format!("{kind}/{slug}.{ext} (would delete)"),
                    &code,
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ObjectEntry;
    use tempfile::TempDir;

    fn write_hook_file(dir: &std::path::Path, slug: &str, run_after_urls: &[&str]) {
        let body = serde_json::json!({
            "id": 0,
            "name": slug,
            "type": "webhook",
            "run_after": run_after_urls,
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.json")),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    fn lf_with_hooks(entries: &[(&str, u64, &str)]) -> Lockfile {
        let mut lf = Lockfile::default();
        for (slug, id, url) in entries {
            lf.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id: *id,
                    url: Some(url.to_string()),
                    modified_at: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
        }
        lf
    }

    #[test]
    fn topo_sort_orders_dependency_before_dependent() {
        // mtr-export-to-sftp depends on pipe-and-fitting-mtr-template and
        // valve-mtr-template. Alphabetical order puts m < p < v so the
        // export hook would otherwise be created before its prereqs.
        let tmp = TempDir::new().unwrap();
        let src_paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let hooks_dir = src_paths.hooks_dir();
        write_hook_file(
            &hooks_dir,
            "mtr-export-to-sftp",
            &[
                "https://x/api/v1/hooks/1147775",
                "https://x/api/v1/hooks/1147776",
            ],
        );
        write_hook_file(&hooks_dir, "pipe-and-fitting-mtr-template", &[]);
        write_hook_file(&hooks_dir, "valve-mtr-template", &[]);
        let lf = lf_with_hooks(&[
            ("pipe-and-fitting-mtr-template", 1147775, "https://x/api/v1/hooks/1147775"),
            ("valve-mtr-template", 1147776, "https://x/api/v1/hooks/1147776"),
            ("mtr-export-to-sftp", 1147777, "https://x/api/v1/hooks/1147777"),
        ]);
        let slugs = vec![
            "mtr-export-to-sftp".to_string(),
            "pipe-and-fitting-mtr-template".to_string(),
            "valve-mtr-template".to_string(),
        ];
        let out = topo_sort_hooks_to_create(&slugs, &src_paths, &lf);
        let pos = |s: &str| out.iter().position(|x| x == s).unwrap();
        assert!(pos("pipe-and-fitting-mtr-template") < pos("mtr-export-to-sftp"));
        assert!(pos("valve-mtr-template") < pos("mtr-export-to-sftp"));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn topo_sort_ignores_deps_already_on_tgt() {
        // A run_after dep that's NOT in to_create (already exists on tgt)
        // imposes no ordering constraint — rewrite_urls will translate
        // those refs to existing tgt URLs.
        let tmp = TempDir::new().unwrap();
        let src_paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let hooks_dir = src_paths.hooks_dir();
        write_hook_file(
            &hooks_dir,
            "dependent",
            &["https://x/api/v1/hooks/100"], // dep is "already-on-tgt"
        );
        let lf = lf_with_hooks(&[
            ("already-on-tgt", 100, "https://x/api/v1/hooks/100"),
            ("dependent", 200, "https://x/api/v1/hooks/200"),
        ]);
        // Only "dependent" is in to_create; "already-on-tgt" isn't.
        let slugs = vec!["dependent".to_string()];
        let out = topo_sort_hooks_to_create(&slugs, &src_paths, &lf);
        assert_eq!(out, slugs);
    }

    #[test]
    fn topo_sort_independent_hooks_keep_alphabetical_order() {
        let tmp = TempDir::new().unwrap();
        let src_paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let hooks_dir = src_paths.hooks_dir();
        write_hook_file(&hooks_dir, "a-hook", &[]);
        write_hook_file(&hooks_dir, "b-hook", &[]);
        write_hook_file(&hooks_dir, "c-hook", &[]);
        let lf = lf_with_hooks(&[]);
        let slugs = vec!["a-hook".into(), "b-hook".into(), "c-hook".into()];
        let out = topo_sort_hooks_to_create(&slugs, &src_paths, &lf);
        assert_eq!(out, slugs);
    }

    #[test]
    fn topo_sort_preserves_order_on_cycle() {
        // Pathological: a -> b -> a. The function should fall back to the
        // input order so the API surfaces the real error.
        let tmp = TempDir::new().unwrap();
        let src_paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let hooks_dir = src_paths.hooks_dir();
        write_hook_file(&hooks_dir, "a", &["https://x/api/v1/hooks/2"]);
        write_hook_file(&hooks_dir, "b", &["https://x/api/v1/hooks/1"]);
        let lf = lf_with_hooks(&[
            ("a", 1, "https://x/api/v1/hooks/1"),
            ("b", 2, "https://x/api/v1/hooks/2"),
        ]);
        let slugs = vec!["a".into(), "b".into()];
        let out = topo_sort_hooks_to_create(&slugs, &src_paths, &lf);
        assert_eq!(out, slugs);
    }

    // ----- drift resolver -------------------------------------------------

    use crate::cli::deploy::apply::{DriftDecision, DriftedItem};

    fn drift_fixture() -> Vec<DriftedItem> {
        vec![
            DriftedItem { kind: "hooks".into(), slug: "validator".into() },
            DriftedItem { kind: "queues".into(), slug: "cost-invoices".into() },
        ]
    }

    #[test]
    fn resolver_returns_empty_map_when_no_drift() {
        let d = resolve_drift_decisions(&[], "prod", false, false).unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn resolver_short_circuits_to_overwrite_when_force_flag_is_set() {
        // --force-overwrite-drift in non-TTY: every drifted item gets
        // an Overwrite decision so the deploy can proceed unattended.
        // Same behavior in TTY (the flag overrides the prompt).
        let drifted = drift_fixture();
        let d = resolve_drift_decisions(&drifted, "prod", /*interactive=*/false, /*force=*/true)
            .unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(
            d.get(&("hooks".to_string(), "validator".to_string())),
            Some(&DriftDecision::Overwrite)
        );
        assert_eq!(
            d.get(&("queues".to_string(), "cost-invoices".to_string())),
            Some(&DriftDecision::Overwrite)
        );
    }

    #[test]
    fn resolver_refuses_in_non_tty_mode_without_force_flag() {
        // The whole point of the resolver: a CI script can't be
        // trusted to silently overwrite ad-hoc UI edits on tgt.
        // Surface the list of drifted items and the exact escape
        // hatch (`--force-overwrite-drift` or `rdc sync <tgt>`).
        let drifted = drift_fixture();
        let err = resolve_drift_decisions(&drifted, "prod", false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("deploy refused"), "msg: {msg}");
        assert!(msg.contains("hooks/validator"), "msg: {msg}");
        assert!(msg.contains("queues/cost-invoices"), "msg: {msg}");
        assert!(msg.contains("--force-overwrite-drift"), "msg: {msg}");
        assert!(msg.contains("rdc sync prod"), "msg: {msg}");
    }

    #[test]
    fn read_src_for_preview_engine_field_finds_file_via_engine_walk() {
        // Engine fields live at `engines/<engine_slug>/fields/<field_slug>.json`.
        // The preview function takes only `kind` + `slug` (no engine context),
        // so it must walk the engines tree to locate the field.
        let dir = TempDir::new().unwrap();
        let paths = Paths::for_env(dir.path(), "env");
        let fields_dir = paths.engine_fields_dir("my-engine");
        std::fs::create_dir_all(&fields_dir).unwrap();
        let body = br#"{"name":"item_qty","type":"string"}"#;
        std::fs::write(fields_dir.join("item-qty.json"), body).unwrap();

        let (bytes, code) =
            read_src_for_preview("engine_fields", "item-qty", &paths).unwrap();
        assert_eq!(bytes, body);
        assert!(code.is_none());
    }

    #[test]
    fn read_src_for_preview_engine_finds_engine_json() {
        // Engines live at `engines/<engine_slug>/engine.json`, not
        // `engines/<engine_slug>.json`.
        let dir = TempDir::new().unwrap();
        let paths = Paths::for_env(dir.path(), "env");
        let engine_dir = paths.engine_dir("my-engine");
        std::fs::create_dir_all(&engine_dir).unwrap();
        let body = br#"{"name":"my engine","type":"line_item"}"#;
        std::fs::write(engine_dir.join("engine.json"), body).unwrap();

        let (bytes, code) =
            read_src_for_preview("engines", "my-engine", &paths).unwrap();
        assert_eq!(bytes, body);
        assert!(code.is_none());
    }
}
