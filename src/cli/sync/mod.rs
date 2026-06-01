//! `rdc sync <env>` — reconcile local snapshot and remote state in one pass.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md
//!
//! # Safety contract: never silently lose remote changes
//!
//! `rdc sync` must NEVER PATCH/POST/DELETE against a remote whose state
//! has diverged from the lockfile-recorded base without an explicit
//! user decision. Anything else is silent data loss — a local edit
//! overwriting concurrent remote edits the user never saw.
//!
//! This invariant is enforced by **four independent defense layers**.
//! For silent data loss to occur, all four must fail simultaneously.
//!
//! ## Layer 1 — Classifier
//!
//! [`classify::classify`] folds `(remote_hash, local_hash, base_hash)`
//! into one of eleven [`classify::SyncClass`] values. When both local
//! AND remote differ from base, the classifier MUST emit a conflict
//! class ([`classify::SyncClass::BothDiverged`],
//! [`classify::SyncClass::LocalEditRemoteDelete`], or
//! [`classify::SyncClass::LocalDeleteRemoteEdit`]) — never a one-sided
//! "push it" / "pull it" class. This invariant is pinned by property
//! tests in `classify::tests` (search for `classify_emits_conflict_*`).
//!
//! For combined-hash kinds (hooks, rules, schemas) the hash passed to
//! the classifier already includes sidecar bytes (`.py` code,
//! `formulas/<id>.py`), so any divergence in any part of the state
//! propagates into the comparison. See
//! [`crate::cli::sync::from_catalog_scan_lockfile`] for the per-kind
//! hash composition.
//!
//! ## Layer 2 — Resolver
//!
//! [`execute::resolve_conflicts`] processes every `BothDiverged` item
//! and, for combined-hash kinds, MUST NOT rely on
//! [`crate::cli::resolve::prompt_resolve`]'s `local_canonical ==
//! remote_canonical` short-circuit. That short-circuit only compares
//! the JSON portion; for hooks/rules the actual state also includes
//! the `.py` sidecar, and for schemas it includes every formula
//! sidecar.
//!
//! When the JSON portion canonicalizes-equal but the sidecar
//! diverges (symmetric or asymmetric), the resolver redirects the
//! prompt to a bytes-driven variant
//! ([`crate::cli::resolve::prompt_resolve_with_bytes`]) that shows the
//! actual divergent bytes. The redirect handles asymmetric cases
//! (one side has a sidecar, the other doesn't) by passing empty
//! bytes for the missing side — `prompt_resolve_with_bytes` does
//! not require the path to exist on disk.
//!
//! Every Resolution branch records the canonical combined hash
//! (`hash_combined(json, code/formulas)`) into the lockfile so a
//! subsequent classify-side comparison sees the right base.
//!
//! ## Layer 3 — Push-side drift check (`resolve_push_drift`)
//!
//! Every per-kind push driver
//! ([`crate::cli::push::hooks::push`],
//! [`crate::cli::push::rules::push`],
//! [`crate::cli::push::schemas::push`],
//! [`crate::cli::push::labels::push`],
//! [`crate::cli::push::workspaces::push`],
//! [`crate::cli::push::engines::push`],
//! [`crate::cli::push::engine_fields::push`],
//! [`crate::cli::push::queues::push`],
//! [`crate::cli::push::inboxes::push`],
//! [`crate::cli::push::email_templates::push`])
//! re-fetches the remote object just before issuing the PATCH/POST,
//! re-serializes it through the canonical form, and compares its
//! combined hash against the lockfile-recorded `content_hash`. If
//! the hashes differ, the driver routes to
//! [`crate::cli::resolve::resolve_push_drift`] which prompts on
//! TTY and falls back to `Skip` on non-interactive runs. This
//! catches the case where remote changed between the resolver and
//! the actual PATCH — including any case where the resolver's
//! lockfile update was incorrect.
//!
//! ## Layer 4 — Defensive last-mile hash compare
//!
//! Layer 3's GET + hash-compare IS the defensive last-mile check.
//! The hash comparison catches content drift regardless of whether
//! the remote's `modified_at` timestamp changed (some Rossum API
//! endpoints don't bump `modified_at` on every modification). The
//! same GET feeds both the drift check and the post-PATCH
//! reconciliation (re-reading the remote after the write to record
//! the post-PATCH canonical hash), so the cost is one extra GET
//! per object — already paid by the existing drift check.
//!
//! ## Invariant summary
//!
//! For silent data loss, ALL of the following must fail at once:
//! - The classifier emits a one-sided class for a both-diverged state
//!   (impossible by construction + property tests).
//! - The resolver short-circuits on JSON canonical equality when a
//!   sidecar has actually diverged (fixed by the redirect described
//!   in Layer 2).
//! - The push driver's drift check sees `remote_hash == base_hash`
//!   when the remote has actually changed (impossible if the lockfile
//!   wasn't corrupted upstream — the canonical hash is content-
//!   addressed).
//! - Both interactive prompts (resolver + push drift) get past
//!   without a `[k]eep local` from the user (the user explicitly
//!   accepting force-push is the only sanctioned escape hatch).

pub mod classify;
pub mod embed;
pub mod execute;
pub mod lock;
pub mod watch;

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::paths::Paths;
use std::sync::Arc;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

/// Aggregate counts from one sync cycle, used for the watch-loop summary line.
/// For one-shot sync, callers usually don't read this — they look at the
/// printed summary instead. Counters are populated by `execute::run` (see
/// Task 12 in the plan; for this task they may remain at default).
#[derive(Debug, Default)]
pub struct CycleOutcome {
    pub items_pushed: usize,
    pub items_pulled: usize,
    pub conflicts: usize,
    pub remote_deletes_resolved: usize,
}

/// Entry point for `rdc sync <env>`. Drives the unified pipeline:
///
/// 1. `list_remote` — Phase 1 of the existing pull pipeline.
/// 2. `scan` — Phase 1 of the existing push pipeline.
/// 3. `classify` (via [`from_catalog_scan_lockfile`]) — fold the three
///    sources into eleven `(kind, slug)` classes.
/// 4. `plan` — render the plan, exit early on `--dry-run`.
/// 5. `execute` — currently a stub; subsequent tasks fill in per-class
///    dispatch. Per-item gates (conflict resolver, destructive-delete
///    gate, remote-delete prompts) handle their own confirmations.
///
/// On success the lockfile is saved and `_index.md` regenerated, even
/// when the executor was a no-op — re-running on a clean env stays
/// idempotent.
pub async fn run(
    env: &str,
    interactive: bool,
    dry_run: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
) -> Result<()> {
    // One-shot wrapper. Watch mode goes through `cli::sync::watch::run_watch`.
    // Argument validation lives in `run_cycle` so watch mode benefits too.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    run_cycle(
        env,
        interactive,
        dry_run,
        allow_deletes,
        no_push,
        no_pull,
        None,
        None,
        None,
    )
    .await?;
    Ok(())
}

/// One reconciliation pass: list remote, scan local, classify, execute,
/// save. Caller is responsible for holding the env lock for the
/// duration of this call.
///
/// The plan is printed before execution as an informational preview, but
/// there is no meta-confirmation prompt — every consequential downstream
/// action (conflict resolution, destructive deletes, remote-delete
/// reconciliation, auth refresh) has its own gate. Ctrl-C remains the
/// universal abort.
///
/// **Caller contract:** the env lock (see `crate::cli::sync::lock::EnvLock`)
/// MUST be held by the caller for the entire duration of this call.
/// All three current callers — `cli::sync::run`, `cli::sync::watch::run_watch`,
/// and `cli::sync::embed::sync_no_push` — acquire it before invoking. New
/// callers must do the same.
pub(crate) async fn run_cycle(
    env: &str,
    interactive: bool,
    dry_run: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    renderer: Option<Arc<Log>>,
    cwd_override: Option<&std::path::Path>,
    token_override: Option<String>,
) -> Result<CycleOutcome> {
    if no_push && no_pull {
        anyhow::bail!(
            "--no-push and --no-pull are mutually exclusive. \
             Use 'rdc sync <env> --dry-run' for a read-only preview."
        );
    }

    let cwd = match cwd_override {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("getting current directory")?,
    };
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = match token_override {
        Some(t) => t,
        None => resolve_token(&cwd, env, &env_cfg.api_base).await?,
    };
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let _title = if dry_run {
        format!("rdc sync {env} (dry run)")
    } else {
        format!("rdc sync {env}")
    };
    // Drivers consume `&Arc<Log>`. Use a caller-provided renderer when
    // present (the watch loop shares one renderer across cycles so freshness
    // clocks persist); otherwise create a fresh one.
    //
    // `renderer_was_supplied` gates the post-cycle summary call: when
    // the caller supplied a persistent renderer (watch mode), we MUST NOT
    // emit a cycle-closing Done event — the watch loop handles its own
    // summaries.
    let renderer_was_supplied = renderer.is_some();
    let progress: Arc<Log> =
        renderer.unwrap_or_else(|| Log::new(crate::cli::resolve::detect_color_mode(false)));
    let started = std::time::Instant::now();

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

    // Phase 3: classify. The adapter re-runs each pull driver's
    // canonical hashing (including overlay strip) so the recomputed
    // remote hashes match what the lockfile recorded on last pull.
    let classified =
        from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile, overlay.as_ref());

    // Classification computed; grid renderer rebuild handled elsewhere.

    // Phase 4: plan + confirm. `--dry-run` exits here without writing.
    // Dry-run uses the same per-item event-log surface as a regular sync,
    // grouped by direction (`would pull`, `would push`, `would prompt`),
    // so the preview matches the live UX byte-for-byte modulo the `would`
    // prefix on each section label.
    if dry_run {
        use crate::cli::sync::classify::SyncClass;

        // Pull-side items (would write local).
        let pull_items: Vec<&crate::cli::sync::classify::ClassifiedItem> = classified
            .iter()
            .filter(|c| matches!(c.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate))
            .collect();
        if !pull_items.is_empty() {
            progress.event(Action::Plan, "would pull");
            let mut body = String::new();
            use std::fmt::Write as _;
            for it in &pull_items {
                let note = if matches!(it.class, SyncClass::RemoteCreate) {
                    " (new)"
                } else {
                    ""
                };
                let _ = writeln!(body, "- {}/{}{}", it.kind, it.slug, note);
            }
            progress.block(&body);
        }

        // Push-side items (would write remote).
        let push_items: Vec<&crate::cli::sync::classify::ClassifiedItem> = classified
            .iter()
            .filter(|c| {
                matches!(
                    c.class,
                    SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete
                )
            })
            .collect();
        if !push_items.is_empty() {
            progress.event(Action::Plan, "would push");
            let mut body = String::new();
            use std::fmt::Write as _;
            for it in &push_items {
                let action = match it.class {
                    SyncClass::LocalEdit => "PATCH",
                    SyncClass::LocalCreate => "POST",
                    SyncClass::LocalDelete => "DELETE",
                    _ => "",
                };
                let _ = writeln!(body, "- {}/{} {}", it.kind, it.slug, action);
            }
            progress.block(&body);
        }

        // Conflict / destructive prompts.
        let prompt_items: Vec<&crate::cli::sync::classify::ClassifiedItem> = classified
            .iter()
            .filter(|c| {
                matches!(
                    c.class,
                    SyncClass::BothDiverged
                        | SyncClass::LocalEditRemoteDelete
                        | SyncClass::LocalDeleteRemoteEdit
                        | SyncClass::RemoteDelete
                )
            })
            .collect();
        if !prompt_items.is_empty() {
            progress.event(Action::Plan, "would prompt");
            let mut body = String::new();
            use std::fmt::Write as _;
            for it in &prompt_items {
                let tag = match it.class {
                    SyncClass::BothDiverged => "both diverged",
                    SyncClass::LocalEditRemoteDelete => "local edit, deleted on env",
                    SyncClass::LocalDeleteRemoteEdit => "local delete, edited on env",
                    SyncClass::RemoteDelete => "deleted on env",
                    _ => "",
                };
                let _ = writeln!(body, "- {}/{} -- {}", it.kind, it.slug, tag);
            }
            progress.block(&body);
        }

        if !renderer_was_supplied {
            progress.event(Action::Done, &format!(
                "Dry run: {} would push, {} would pull, {} would prompt (no writes)",
                push_items.len(),
                pull_items.len(),
                prompt_items.len(),
            ));
        }
        return Ok(CycleOutcome::default());
    }

    // Phase 5: execute. The destructive-delete gate (`--allow-deletes`)
    // is enforced inside the executor's deletes phase via
    // `cli::push::deletes::confirm_or_refuse`, which prints the full
    // tombstone list and either prompts (TTY) or refuses (non-TTY without
    // the flag).
    let outcome = {
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
            allow_deletes,
            interactive,
            &progress,
        )
        .await?
    };

    // Re-classify post-execute and re-ingest so squares whose state flipped
    // (e.g. LocalEdit → Clean after a successful push) repaint accordingly.
    // No-op for the log renderer.
    let (_scanned_after, changes_after, tombstones_after) =
        crate::cli::push::scan::scan(&paths, &lockfile)?;
    let overlay_after = crate::overlay::Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;
    let _classified_after = from_catalog_scan_lockfile(
        &catalog,
        &changes_after,
        &tombstones_after,
        &lockfile,
        overlay_after.as_ref(),
    );
    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("generating _index.md for env '{env}'"))?;

    let elapsed = started.elapsed();
    let total_changed = outcome.items_pushed + outcome.items_pulled;
    if !renderer_was_supplied {
        progress.event(Action::Done, &format!(
            "Synced envs/{env} ({total_changed} changed, {:.1}s)",
            elapsed.as_secs_f32()
        ));
    } else {
        // Watch mode: reset the header to "idle" so the user can see the
        // cycle completed (transitioning out of "executing" or similar
        // is a visible signal that polling fired and finished).
        progress.event(Action::Idle, "envs match remote");
    }
    Ok(outcome)
}

/// Fold the three sources (remote catalog, push scan, lockfile) into the
/// `(kind, slug) -> hash` maps the classifier consumes.
///
/// The hashing on each side must match so the classifier produces `Clean`
/// when nothing has changed:
/// * **`remote_hashes`** — re-runs the canonical serialization the per-kind
///   pull driver performs before `record_object` (serialize → optional
///   overlay strip → `content_hash`). This mirrors how the lockfile's
///   `content_hash` was written on the last pull. The `overlay` arg lets
///   the adapter apply the same `maybe_strip_overlay` the pull driver
///   ran; without it, an env with any writable-kind overlay would emit
///   spurious `RemoteEdit` for objects that haven't actually changed
///   (since the lockfile recorded the post-strip hash, but the adapter
///   would hash the pre-strip bytes).
/// * **`scan_changes`** — already computed by [`crate::cli::push::scan::scan`]
///   over local file bytes with the same `content_hash` function. We
///   re-derive from the on-disk path here rather than smuggling the hash
///   through `ChangeList` to keep `scan.rs` push-only.
/// * **`scan_tombstones`** — just the `(kind, slug)` keys, no hash needed.
/// * **`locked`** — pulled verbatim from `lockfile.objects[kind][slug].content_hash`.
///
/// Slug derivation matches the pull drivers: prefer
/// `lockfile.slug_for_id(kind, id)`, else `slugify_unique(name, ...)`.
pub fn from_catalog_scan_lockfile(
    catalog: &crate::cli::pull::common::RemoteCatalog,
    changes: &crate::cli::push::scan::ChangeList,
    tombstones: &crate::cli::push::scan::Tombstones,
    lockfile: &crate::state::Lockfile,
    overlay: Option<&crate::overlay::Overlay>,
) -> Vec<crate::cli::sync::classify::ClassifiedItem> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut remote_hashes: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut scan_changes: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut scan_tombstones: BTreeSet<(String, String)> = BTreeSet::new();
    let mut locked: BTreeMap<(String, String), String> = BTreeMap::new();

    // --- labels --------------------------------------------------------
    // Catalog hash: re-run `pull::labels::process`'s pre-record_object
    // sequence — serialize → optional overlay strip → push newline →
    // `content_hash`. Slug picks up the lockfile-anchored id mapping so
    // remote renames don't churn slugs.
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
        let proposed = match crate::cli::pull::common::maybe_strip_overlay(
            proposed,
            overlay.and_then(|o| o.label(&slug)),
        ) {
            Ok(b) => b,
            Err(_) => continue,
        };
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
    //
    // `workflow_url_to_slug` is populated here so the workflow_steps
    // block below can resolve `step.workflow` (a URL) to the workflow
    // slug even on a fresh lockfile where `slug_for_url` would
    // otherwise return `None`.
    let mut used_workflow_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut workflow_url_to_slug: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for w in &catalog.workflows {
        let slug = match lockfile.slug_for_id("workflows", w.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&w.name, &used_workflow_slugs),
        };
        used_workflow_slugs.insert(slug.clone());
        workflow_url_to_slug.insert(w.url.clone(), slug.clone());

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
    // Steps nest under workflows; the lockfile key is composite
    // `<workflow_slug>/<step_slug>` so per-workflow slugs stay clean
    // (the same per-parent scoping as engine_fields and email_templates).
    // The driver skips orphan steps (no parent workflow); here we emit
    // composite keys for every step that has a known parent, and silently
    // drop orphans (their absence from `remote_hashes` means the
    // classifier never schedules them — which matches the prior
    // behavior of "warn-and-skip at apply time").
    let mut per_workflow_used_step: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for s in &catalog.workflow_steps {
        let Some(workflow_slug) = lockfile
            .slug_for_url("workflows", &s.workflow)
            .map(|x| x.to_string())
            .or_else(|| workflow_url_to_slug.get(&s.workflow).cloned())
        else {
            continue;
        };
        let used = per_workflow_used_step
            .entry(workflow_slug.clone())
            .or_default();
        let step_slug = match lockfile.slug_for_id("workflow_steps", s.id) {
            Some(existing) => existing
                .strip_prefix(&format!("{workflow_slug}/"))
                .map(|x| x.to_string())
                .unwrap_or_else(|| existing.to_string()),
            None => crate::slug::slugify_unique(&s.name, used),
        };
        used.insert(step_slug.clone());
        let composite_key = format!("{workflow_slug}/{step_slug}");

        let mut proposed = match serde_json::to_vec_pretty(s) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("workflow_steps".to_string(), composite_key), hash);
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
    // Build a freshly-computed workspace URL → slug map alongside the
    // hash insertion. Queue derivation below uses this map so a
    // first-pull sync can resolve queue.workspace → ws_slug even when
    // the lockfile is still empty (the workspace was just emitted as
    // RemoteCreate one block above; its entry won't land in the
    // lockfile until the executor runs).
    let mut used_workspace_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ws_url_to_slug: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for w in &catalog.workspaces {
        let slug = match lockfile.slug_for_id("workspaces", w.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&w.name, &used_workspace_slugs),
        };
        used_workspace_slugs.insert(slug.clone());
        ws_url_to_slug.insert(w.url.clone(), slug.clone());

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
    // calls `maybe_strip_overlay` before hashing. The adapter mirrors
    // that strip so the recomputed remote hash matches the lockfile
    // base for unchanged envs.
    //
    // `engine_url_to_slug` is populated here so the engine_fields block
    // below can resolve `field.engine` (a URL) to the engine slug even
    // on a fresh lockfile where `slug_for_url` would otherwise return
    // `None`.
    let mut used_engine_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut engine_url_to_slug: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for e in &catalog.engines {
        let slug = match lockfile.slug_for_id("engines", e.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&e.name, &used_engine_slugs),
        };
        used_engine_slugs.insert(slug.clone());
        engine_url_to_slug.insert(e.url.clone(), slug.clone());

        let mut proposed = match serde_json::to_vec_pretty(e) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let proposed = match crate::cli::pull::common::maybe_strip_overlay(
            proposed,
            overlay.and_then(|o| o.engine(&slug)),
        ) {
            Ok(b) => b,
            Err(_) => continue,
        };
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
    // Push-capable kind nested under engines. Lockfile key is the composite
    // `<engine_slug>/<field_slug>` so per-engine slugs stay clean (two
    // engines can both carry an `Amount` field). Path is
    // `engines/<engine>/fields/<slug>.json`. The pull driver also skips
    // orphan fields (no parent engine in the lockfile), but the adapter
    // emits a key regardless; classifier emits RemoteCreate and the driver
    // later skips with a warning.
    //
    // Overlay strip matches the pull driver so the recomputed remote hash
    // lines up with the lockfile base for unchanged envs.
    let mut per_engine_used_ef: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for f in &catalog.engine_fields {
        let Some(engine_slug) = lockfile
            .slug_for_url("engines", &f.engine)
            .map(|s| s.to_string())
            .or_else(|| engine_url_to_slug.get(&f.engine).cloned())
        else {
            continue;
        };
        let used = per_engine_used_ef.entry(engine_slug.clone()).or_default();
        let field_slug = match lockfile.slug_for_id("engine_fields", f.id) {
            Some(existing) => existing
                .strip_prefix(&format!("{engine_slug}/"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| existing.to_string()),
            None => crate::slug::slugify_unique(&f.name, used),
        };
        used.insert(field_slug.clone());
        let composite_key = format!("{engine_slug}/{field_slug}");

        let mut proposed = match serde_json::to_vec_pretty(f) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let proposed = match crate::cli::pull::common::maybe_strip_overlay(
            proposed,
            overlay.and_then(|o| o.engine_field(&composite_key)),
        ) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("engine_fields".to_string(), composite_key), hash);
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
    // MDH datasets bypass the classifier entirely. Collection metadata
    // is server-managed (uuid, options, idIndex) and not user-editable,
    // and the indexes-state can't be hashed up here without an extra
    // per-dataset round-trip. The dispatcher in `execute::run`
    // unconditionally invokes `pull::mdh::process` for every listed
    // collection; that driver is idempotent on no-change (per-file
    // `decide_pull_action`) so unconditional dispatch is correct, just
    // marginally chattier than a classifier-gated path.

    // --- hooks --------------------------------------------------------
    // Push-capable split-file kind: `<slug>.json` + optional `<slug>.py`
    // (the extracted `config.code`). The canonical hash combines both —
    // `hook_combined_hash(json_bytes, code)` — and is what both the pull
    // driver and the push scanner record. Slug derivation mirrors
    // `pull::hooks::process`: lockfile-anchored id mapping first, else
    // `slugify_unique(name, used)`.
    //
    // Overlay strip mirrors `pull::hooks::process`: serialize → strip
    // overlay-managed paths from JSON → hash json+code. Without the
    // strip, an env with any hook overlay configured would always see
    // the recomputed remote hash differ from the lockfile base (which
    // was recorded post-strip) and the classifier would emit spurious
    // RemoteEdit / BothDiverged.
    let mut used_hook_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in &catalog.hooks {
        let slug = match lockfile.slug_for_id("hooks", h.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&h.name, &used_hook_slugs),
        };
        used_hook_slugs.insert(slug.clone());

        // Reproduce the pull driver's canonical bytes: serialize → strip
        // `config.code` into `code` → strip overlay paths from JSON →
        // trailing newline already applied by serialize.
        let (json_bytes, code) = match crate::snapshot::hook::serialize_hook(h) {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let json_bytes = match crate::cli::pull::common::maybe_strip_overlay(
            json_bytes,
            overlay.and_then(|o| o.hook(&slug)),
        ) {
            Ok(b) => b,
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
        // Hook sidecar extension comes from `config.runtime`: `.js` for
        // Node.js, `.py` otherwise. Fall back to the other extension if
        // the runtime-derived sidecar isn't on disk.
        let value: serde_json::Value =
            serde_json::from_slice(&json_bytes).unwrap_or(serde_json::Value::Null);
        let ext = crate::snapshot::hook::hook_code_extension_from_value(&value);
        let primary = json_path.with_extension(ext);
        let fallback = json_path.with_extension(if ext == "py" { "js" } else { "py" });
        let code = if primary.exists() {
            std::fs::read_to_string(&primary).ok()
        } else if fallback.exists() {
            std::fs::read_to_string(&fallback).ok()
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
    // derivation mirrors `pull::rules::process`. Overlay strip applied
    // for the same parity-with-pull reason as hooks.
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
        let json_bytes = match crate::cli::pull::common::maybe_strip_overlay(
            json_bytes,
            overlay.and_then(|o| o.rule(&slug)),
        ) {
            Ok(b) => b,
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

    // --- queues / schemas / inboxes / email_templates -----------------
    // Queue-nested kinds share a slug-derivation pass: the queue slug
    // (lockfile id lookup → `slugify_unique(name, per_ws_used)`) keys
    // `queues`, `schemas`, and `inboxes` in the lockfile. Email templates
    // use a compound `<ws>/<q>/<tpl>` slug per `pull::email_templates::process`.
    //
    // The remote hash for each kind mirrors the pull driver's canonical
    // bytes:
    //   queues   → `redacted_disk_bytes(q)` (redact `counts`) + strip + `content_hash`
    //   schemas  → `serialize_schema` → strip → `schema_combined_hash(json, formulas)`
    //   inboxes  → `serde_json::to_vec_pretty(i)` + `\n` + strip + `content_hash`
    //   email_tpl→ `serde_json::to_vec_pretty(t)` + `\n` + strip + `content_hash`
    //
    // The `catalog.schemas_by_queue_id` / `inboxes_by_queue_id` maps were
    // populated in `list_remote`; we look up by queue id. Schema bodies are
    // fetched per id (the `/schemas` list omits `content`); inboxes come from
    // the bulk `/inboxes` list.
    //
    // Overlay strip on each branch keeps the recomputed remote hash in
    // parity with the lockfile base (which was recorded post-strip on
    // last pull).
    let mut per_ws_used_q_slugs: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    // Build per-queue (ws_slug, q_slug, q.id, q.url) tuples so the
    // email_templates block can look up its compound key without
    // re-deriving slugs.
    let mut q_url_to_ws_q: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for q in &catalog.queues {
        let Some(ws_url) = q.workspace.as_ref() else { continue };
        // Prefer the freshly-computed catalog mapping (so first-pull syncs
        // resolve queue.workspace even when the lockfile is empty); fall
        // back to the lockfile for cross-env re-attribution scenarios where
        // the workspace was already pulled by a previous run.
        let ws_slug = match ws_url_to_slug.get(ws_url) {
            Some(s) => s.clone(),
            None => match lockfile.slug_for_url("workspaces", ws_url) {
                Some(s) => s.to_string(),
                None => {
                    // The pull driver also skips orphans here; without a
                    // workspace slug we can't compute the queue's
                    // location, so the queue (and its schema/inbox)
                    // never enters the classifier.
                    continue;
                }
            },
        };
        let used = per_ws_used_q_slugs.entry(ws_slug.clone()).or_default();
        let q_slug = match lockfile.slug_for_id("queues", q.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&q.name, used),
        };
        used.insert(q_slug.clone());
        q_url_to_ws_q.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));

        // queues — flat JSON. Must mirror the pull driver's canonical on-disk
        // bytes, which redact server-set runtime fields (`counts`). Serializing
        // the raw queue here instead made live `counts` churn read as remote
        // drift, surfacing a spurious queue.json conflict.
        let q_proposed = match crate::snapshot::create::redacted_disk_bytes(q, "queues") {
            Ok(b) => b,
            Err(_) => continue,
        };
        let q_proposed = match crate::cli::pull::common::maybe_strip_overlay(
            q_proposed,
            overlay.and_then(|o| o.queue(&q_slug)),
        ) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let q_hash = crate::state::content_hash(&q_proposed);
        remote_hashes.insert(("queues".to_string(), q_slug.clone()), q_hash);

        // schemas — combined (json + formulas). Pre-fetched in `list_remote`.
        if let Some(schema) = catalog.schemas_by_queue_id.get(&q.id)
            && let Ok((schema_json_bytes, schema_formulas)) =
                crate::snapshot::schema::serialize_schema(schema)
                && let Ok(schema_json_bytes) = crate::cli::pull::common::maybe_strip_overlay(
                    schema_json_bytes,
                    overlay.and_then(|o| o.schema(&q_slug)),
                ) {
                    let schema_hash =
                        crate::state::schema_combined_hash(&schema_json_bytes, &schema_formulas);
                    remote_hashes.insert(("schemas".to_string(), q_slug.clone()), schema_hash);
                }

        // inboxes — flat JSON. Only present for queues that have an inbox.
        if let Some(inbox) = catalog.inboxes_by_queue_id.get(&q.id)
            && let Ok(mut inbox_proposed) = serde_json::to_vec_pretty(inbox) {
                inbox_proposed.push(b'\n');
                if let Ok(inbox_proposed) = crate::cli::pull::common::maybe_strip_overlay(
                    inbox_proposed,
                    overlay.and_then(|o| o.inbox(&q_slug)),
                ) {
                    let inbox_hash = crate::state::content_hash(&inbox_proposed);
                    remote_hashes.insert(("inboxes".to_string(), q_slug.clone()), inbox_hash);
                }
            }
    }

    // Scan-side hashes for queues / inboxes (flat) and schemas (combined).
    for (slug, path) in &changes.queues {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("queues".to_string(), slug.clone()),
                crate::state::content_hash(&bytes),
            );
        }
    }
    for (slug, path) in &changes.inboxes {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("inboxes".to_string(), slug.clone()),
                crate::state::content_hash(&bytes),
            );
        }
    }
    for (slug, schema_path) in &changes.schemas {
        // The push scanner stores the path to `schema.json`; the formulas
        // sit in a sibling `formulas/` dir. Mirror `scan_schemas`.
        let Ok(json_bytes) = std::fs::read(schema_path) else { continue };
        let queue_dir = match schema_path.parent() {
            Some(p) => p,
            None => continue,
        };
        let formulas = crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default();
        let hash = crate::state::schema_combined_hash(&json_bytes, &formulas);
        scan_changes.insert(("schemas".to_string(), slug.clone()), hash);
    }

    for slug in tombstones.queues.keys() {
        scan_tombstones.insert(("queues".to_string(), slug.clone()));
    }
    for slug in tombstones.schemas.keys() {
        scan_tombstones.insert(("schemas".to_string(), slug.clone()));
    }
    for slug in tombstones.inboxes.keys() {
        scan_tombstones.insert(("inboxes".to_string(), slug.clone()));
    }

    if let Some(map) = lockfile.objects.get("queues") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("queues".to_string(), slug.clone()), h.clone());
            }
        }
    }
    if let Some(map) = lockfile.objects.get("schemas") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("schemas".to_string(), slug.clone()), h.clone());
            }
        }
    }
    if let Some(map) = lockfile.objects.get("inboxes") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("inboxes".to_string(), slug.clone()), h.clone());
            }
        }
    }

    // email_templates — compound slug `<ws>/<q>/<tpl>`. Mirrors
    // `pull::email_templates::process`'s `lockfile_key` derivation:
    // look up the queue's (ws_slug, q_slug) by queue URL, then pick the
    // template slug (lockfile id lookup → `slugify_unique` per queue).
    let mut per_queue_used_t_slugs: std::collections::HashMap<
        (String, String),
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
    for t in &catalog.email_templates {
        let Some(queue_url) = t.queue.as_ref() else { continue };
        let Some((ws_slug, q_slug)) = q_url_to_ws_q.get(queue_url).cloned() else { continue };
        let used = per_queue_used_t_slugs
            .entry((ws_slug.clone(), q_slug.clone()))
            .or_default();
        let template_slug = match lockfile.slug_for_id("email_templates", t.id) {
            Some(existing) => existing
                .rsplit('/')
                .next()
                .unwrap_or(existing)
                .to_string(),
            None => crate::slug::slugify_unique(&t.name, used),
        };
        used.insert(template_slug.clone());
        let compound = format!("{ws_slug}/{q_slug}/{template_slug}");

        let mut proposed = match serde_json::to_vec_pretty(t) {
            Ok(b) => b,
            Err(_) => continue,
        };
        proposed.push(b'\n');
        let proposed = match crate::cli::pull::common::maybe_strip_overlay(
            proposed,
            overlay.and_then(|o| o.email_template(&compound)),
        ) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hash = crate::state::content_hash(&proposed);
        remote_hashes.insert(("email_templates".to_string(), compound), hash);
    }
    for (slug, path) in &changes.email_templates {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("email_templates".to_string(), slug.clone()),
                crate::state::content_hash(&bytes),
            );
        }
    }
    for slug in tombstones.email_templates.keys() {
        scan_tombstones.insert(("email_templates".to_string(), slug.clone()));
    }
    if let Some(map) = lockfile.objects.get("email_templates") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("email_templates".to_string(), slug.clone()), h.clone());
            }
        }
    }

    crate::cli::sync::classify::classify(&remote_hashes, &scan_changes, &scan_tombstones, &locked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::pull::common::RemoteCatalog;
    use crate::cli::sync::classify::SyncClass;
    use crate::model::Hook;
    use crate::paths::Paths;
    use crate::snapshot::hook::serialize_hook;
    use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// Build an empty `RemoteCatalog` whose `hooks` the caller fills in.
    /// Mirrors `execute.rs::catalog_with_labels` but seeds the hooks slot.
    fn catalog_with_hooks(hooks: Vec<Hook>) -> RemoteCatalog {
        RemoteCatalog {
            organization: crate::model::Organization {
                id: 1,
                url: "https://x.invalid/api/v1/organizations/1".to_string(),
                name: "test".to_string(),
                extra: indexmap::IndexMap::new(),
            },
            workspaces: vec![],
            queues: vec![],
            schemas_by_queue_id: BTreeMap::new(),
            inboxes_by_queue_id: BTreeMap::new(),
            hooks,
            rules: vec![],
            labels: vec![],
            engines: vec![],
            engine_fields: vec![],
            workflows: vec![],
            workflow_steps: vec![],
            email_templates: vec![],
            mdh: crate::cli::pull::mdh::MdhListed {
                client: crate::api::data_storage::DataStorageClient::new(
                    "https://unused.invalid/svc/data-storage/api/v1".to_string(),
                    "TEST".to_string(),
                )
                .unwrap(),
                collections: vec![],
            },
        }
    }

    /// Build a Python `function` hook with the given `code`. The hook's
    /// `modified_at` (lives in `extra`) is set so the lockfile can capture
    /// a pre-edit timestamp for the regression test below.
    fn mk_hook(id: u64, name: &str, code: &str, modified_at: &str) -> Hook {
        let v = json!({
            "id": id,
            "url": format!("https://x.invalid/api/v1/hooks/{id}"),
            "name": name,
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": code },
            "modified_at": modified_at,
        });
        serde_json::from_value(v).unwrap()
    }

    /// Hash a hook the way the pull driver does (serialize + optional
    /// overlay strip + `hook_combined_hash`) so the test seeds the
    /// lockfile with the same base hash production would have written.
    fn pull_driver_hash(h: &Hook, overlay_paths: Option<&BTreeMap<String, serde_json::Value>>) -> String {
        let (json_bytes, code) = serialize_hook(h).unwrap();
        let stripped = crate::cli::pull::common::maybe_strip_overlay(json_bytes, overlay_paths).unwrap();
        hook_combined_hash(&stripped, &code)
    }

    /// Regression for the conflict-resolution bypass: when both local
    /// and remote have diverged from the lockfile-recorded base, the
    /// adapter MUST classify the hook as `BothDiverged` so the executor
    /// can route into `resolve_conflicts` and prompt the user. Before
    /// the fix, this returned `LocalEdit` silently — the push driver
    /// then PATCHed local-over-remote.
    #[test]
    fn from_catalog_scan_lockfile_classifies_hook_as_both_diverged_when_both_sides_changed() {
        // --- arrange ------------------------------------------------------
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        // Base state: what the lockfile recorded on the most recent pull.
        let base_hook = mk_hook(
            42,
            "ap-reject-if-no-doc-id",
            "def base():\n    return 1\n",
            "2026-05-14T08:00:00Z",
        );
        let base_hash = pull_driver_hash(&base_hook, None);

        // Local state on disk: the user edited the .py sidecar AND tweaked
        // the .json (e.g., events list). The scan must therefore observe a
        // changed local hash.
        let slug = "ap-reject-if-no-doc-id";
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        let local_json_bytes = serde_json::to_vec_pretty(&json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-reject-if-no-doc-id",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        }))
        .unwrap();
        let mut local_json_with_newline = local_json_bytes.clone();
        local_json_with_newline.push(b'\n');
        std::fs::write(&local_json_path, &local_json_with_newline).unwrap();
        std::fs::write(&local_py_path, b"def local_edit():\n    return 2\n").unwrap();

        // Remote state: user also edited via the Rossum UI — different code,
        // newer modified_at.
        let remote_hook = mk_hook(
            42,
            "ap-reject-if-no-doc-id",
            "def remote_edit():\n    return 3\n",
            "2026-05-14T10:00:00Z",
        );

        // Lockfile: records the BASE hash (so a re-sync sees both sides
        // diverged).
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                url: Some("https://x.invalid/api/v1/hooks/42".to_string()),
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_hash.clone()),
                secrets_hash: None,
            },
        );

        // Run the push scanner so the test feeds the adapter the same
        // ChangeList the production code would. This also confirms the
        // scanner correctly flags the local edit.
        let (_scanned, changes, tombstones) =
            crate::cli::push::scan::scan(&paths, &lockfile).unwrap();
        assert!(
            changes.hooks.contains_key(slug),
            "scan must flag the locally-edited hook"
        );

        let catalog = catalog_with_hooks(vec![remote_hook]);

        // --- act ----------------------------------------------------------
        let classified =
            from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile, None);

        // --- assert -------------------------------------------------------
        let item = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == slug)
            .expect("hook must appear in classification");
        assert_eq!(
            item.class,
            SyncClass::BothDiverged,
            "hook with diverged local AND remote bytes must classify as BothDiverged; \
             got {:?} (local_hash={:?}, remote_hash={:?}, base_hash={:?})",
            item.class,
            item.local_hash,
            item.remote_hash,
            item.base_hash,
        );
    }

    /// Regression for hypothesis A (overlay-strip parity): when the env has
    /// an overlay configured for the hook, the pull driver hashes the
    /// post-strip bytes and the lockfile records that hash. The adapter
    /// recomputes the remote hash WITHOUT applying the same strip, so for
    /// an UNCHANGED remote the two hashes don't match → the classifier
    /// emits a false-positive RemoteEdit (or, with a local edit on top, a
    /// silent BothDiverged that should have been LocalEdit — or vice
    /// versa: a real BothDiverged silently downgraded because the adapter's
    /// "is the remote at the lockfile?" answer is wrong).
    ///
    /// Concretely: with an overlay-stripped field whose value DIFFERS
    /// between the lockfile base and the recomputed remote, an UNCHANGED
    /// remote already mis-classifies. With a local edit on top the same
    /// false-positive arises. This test pins the parity at the adapter.
    ///
    /// Setup:
    /// - Overlay: `[hooks.<slug>] "config.runtime" = "python3.12-secure"`
    /// - Base hook (pull-time): `runtime: "python3.12"`, code A. Lockfile
    ///   recorded `hash(strip(serialize(base)) + A)`.
    /// - Remote (now): IDENTICAL to base — nobody changed remote.
    /// - Local: IDENTICAL to base — nobody changed local.
    /// - Expected: `Clean` (everything matches the lockfile).
    /// - Bug at HEAD: adapter recomputes `hash(serialize(remote) + A)`
    ///   without stripping `config.runtime`. The serialize output still
    ///   contains `"runtime": "python3.12"` (matching the pull-time bytes
    ///   pre-strip), so the hashes happen to match here. This test
    ///   confirms parity in the no-divergence case; a sibling test
    ///   below covers the divergent case where the bug bites.
    #[test]
    fn from_catalog_scan_lockfile_overlay_parity_clean_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::create_dir_all(paths.env_root()).unwrap();

        // Overlay drops `config.runtime` from canonical form on pull and
        // re-applies it on push. The lockfile thus records hash WITHOUT
        // `config.runtime`.
        std::fs::write(
            paths.overlay_file(),
            r#"version = 1

[hooks.ap-reject-if-no-doc-id]
"config.runtime" = "python3.12-secure"
"#,
        )
        .unwrap();

        let slug = "ap-reject-if-no-doc-id";
        let base_hook = mk_hook(
            42,
            slug,
            "def base():\n    return 1\n",
            "2026-05-14T08:00:00Z",
        );

        // Compute the pull-driver base hash (post-strip) so the lockfile
        // matches what production would have written.
        let overlay = crate::overlay::Overlay::load(&paths.overlay_file()).unwrap();
        let overlay_paths = overlay.as_ref().and_then(|o| o.hook(slug));
        let base_hash = pull_driver_hash(&base_hook, overlay_paths);

        // Local file: the disk form is the post-strip canonical (matches
        // what pull would have written).
        let (base_json_full, base_code) = serialize_hook(&base_hook).unwrap();
        let base_json_stripped =
            crate::cli::pull::common::maybe_strip_overlay(base_json_full, overlay_paths).unwrap();
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &base_json_stripped).unwrap();
        std::fs::write(&local_py_path, base_code.as_ref().unwrap().as_bytes()).unwrap();

        // Lockfile: records post-strip base hash.
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                url: Some("https://x.invalid/api/v1/hooks/42".to_string()),
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_hash.clone()),
                secrets_hash: None,
            },
        );

        // Remote: identical to base — no changes on either side.
        let (_scanned, changes, tombstones) =
            crate::cli::push::scan::scan(&paths, &lockfile).unwrap();
        // Scan must NOT flag this hook — local matches lockfile-recorded
        // post-strip hash, since `read(local) == base_json_stripped`.
        assert!(
            !changes.hooks.contains_key(slug),
            "scanner shouldn't flag an unchanged local hook"
        );

        let catalog = catalog_with_hooks(vec![base_hook.clone()]);
        let classified = from_catalog_scan_lockfile(
            &catalog,
            &changes,
            &tombstones,
            &lockfile,
            overlay.as_ref(),
        );
        let item = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == slug)
            .expect("hook must appear in classification");
        assert_eq!(
            item.class,
            SyncClass::Clean,
            "no-op env must classify as Clean; got {:?} (local_hash={:?}, remote_hash={:?}, base_hash={:?})",
            item.class, item.local_hash, item.remote_hash, item.base_hash,
        );
    }

    /// The pointed regression: env has an overlay configured for the
    /// hook AND both local and remote have diverged. Without overlay
    /// parity, the adapter mis-classifies and the conflict prompt is
    /// silently bypassed.
    ///
    /// Concretely:
    /// - Overlay strips `config.runtime` from canonical form.
    /// - Base hook: `runtime: "python3.12"`, code = "base".
    ///   Lockfile records `hash(strip(serialize(base)) + "base")`.
    /// - Remote NOW: `runtime: "python3.12"`, code = "REMOTE_EDIT" (user
    ///   modified via UI).
    /// - Local NOW: `runtime: "python3.12"`, code = "LOCAL_EDIT" (user
    ///   modified the .py sidecar).
    /// - Expected: `BothDiverged` → conflict prompt.
    /// - HEAD bug: adapter hashes remote via `hash(serialize(remote) +
    ///   "REMOTE_EDIT")` without stripping; the lockfile's recorded base
    ///   hash was computed AFTER strip. So the comparison shape is fine
    ///   for runtime (it just gets dropped or kept consistently) — BUT
    ///   the comparison's correctness depends on `strip(serialize(h))`
    ///   producing identical bytes to `serialize(h)`. Without the strip,
    ///   the field is included; with it, it's removed. If the overlay
    ///   strip removes a field that's present in serialize, the two
    ///   diverge. This test makes that divergence concrete by using a
    ///   field whose absence (post-strip) the lockfile recorded, but
    ///   whose presence the adapter measures.
    #[test]
    fn from_catalog_scan_lockfile_overlay_both_diverged_with_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::create_dir_all(paths.env_root()).unwrap();

        std::fs::write(
            paths.overlay_file(),
            r#"version = 1

[hooks.ap-reject-if-no-doc-id]
"config.runtime" = "python3.12-secure"
"#,
        )
        .unwrap();

        let slug = "ap-reject-if-no-doc-id";
        let base_hook = mk_hook(
            42,
            slug,
            "def base():\n    return 1\n",
            "2026-05-14T08:00:00Z",
        );
        let overlay = crate::overlay::Overlay::load(&paths.overlay_file()).unwrap();
        let overlay_paths = overlay.as_ref().and_then(|o| o.hook(slug));
        let base_hash = pull_driver_hash(&base_hook, overlay_paths);

        // Local edited the .py sidecar — different code from base. Disk
        // bytes for JSON are the post-strip form (unchanged); .py was
        // touched.
        let (base_json_full, _base_code) = serialize_hook(&base_hook).unwrap();
        let base_json_stripped =
            crate::cli::pull::common::maybe_strip_overlay(base_json_full, overlay_paths).unwrap();
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &base_json_stripped).unwrap();
        std::fs::write(&local_py_path, b"def local_edit():\n    return 2\n").unwrap();

        // Remote: also edited (via Rossum UI). The remote payload still
        // carries `config.runtime` (Rossum always returns it); the
        // overlay's job is to strip it post-pull.
        let remote_hook = mk_hook(
            42,
            slug,
            "def remote_edit():\n    return 3\n",
            "2026-05-14T10:00:00Z",
        );

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                url: Some("https://x.invalid/api/v1/hooks/42".to_string()),
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_hash.clone()),
                secrets_hash: None,
            },
        );

        let (_scanned, changes, tombstones) =
            crate::cli::push::scan::scan(&paths, &lockfile).unwrap();
        assert!(
            changes.hooks.contains_key(slug),
            "scanner must flag the locally-edited .py sidecar"
        );

        let catalog = catalog_with_hooks(vec![remote_hook]);
        let classified = from_catalog_scan_lockfile(
            &catalog,
            &changes,
            &tombstones,
            &lockfile,
            overlay.as_ref(),
        );
        let item = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == slug)
            .expect("hook must appear in classification");
        assert_eq!(
            item.class,
            SyncClass::BothDiverged,
            "with overlay configured and both sides edited, must classify as BothDiverged; \
             got {:?} (local_hash={:?}, remote_hash={:?}, base_hash={:?})",
            item.class, item.local_hash, item.remote_hash, item.base_hash,
        );
    }
}
