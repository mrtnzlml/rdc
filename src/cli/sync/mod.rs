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
pub mod collision;
pub mod embed;
pub mod execute;
pub mod lock;
pub mod watch;

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{Context, Result, anyhow};
use std::sync::Arc;

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
    // Set the env's api_base so the lockfile can DERIVE object URLs from
    // ids (push ref resolution, deploy cross-ref rewriting). Without this an
    // empty api_base makes `url_for_slug` return None and push refs fail loud.
    lockfile.api_base = env_cfg.api_base.clone();

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
            interactive,
        };
        crate::cli::pull::common::list_remote(&mut ctx, env_cfg, env, &token, &progress).await?
    };

    // Phase 2: scan local. Reuses the push scanner unchanged so behavior
    // matches `push --dry-run` byte for byte.
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(&paths, &lockfile)?;

    // Phase 3: classify. The adapter re-runs each pull driver's
    // canonical hashing so the recomputed remote hashes match what the
    // lockfile recorded on last pull.
    let classified =
        from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile)?;

    // Changed local files that don't parse as JSON. Surfaced as a
    // dedicated dry-run section, and a hard refusal before any push —
    // otherwise the file rides classification on its raw-byte hash and
    // only explodes mid-push, after earlier kinds already landed.
    let parse_errors = changes.json_parse_errors();

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

        if !parse_errors.is_empty() {
            progress.event(Action::Plan, "parse errors");
            let mut body = String::new();
            use std::fmt::Write as _;
            for e in &parse_errors {
                let _ = writeln!(body, "- {}/{} -- {}: {}", e.kind, e.slug, e.path.display(), e.error);
            }
            progress.block(&body);
        }

        if !renderer_was_supplied {
            let parse_suffix = if parse_errors.is_empty() {
                String::new()
            } else {
                format!(
                    ", {} parse error{}",
                    parse_errors.len(),
                    if parse_errors.len() == 1 { "" } else { "s" }
                )
            };
            progress.event(
                Action::Done,
                &format!(
                    "Dry run: {} would push, {} would pull, {} would prompt{} (no writes)",
                    push_items.len(),
                    pull_items.len(),
                    prompt_items.len(),
                    parse_suffix,
                ),
            );
        }
        return Ok(CycleOutcome::default());
    }

    // Refuse to push with unparseable local files — fail BEFORE the first
    // remote write so a malformed file can never cause a partial push.
    // `--no-push` (audit) proceeds: there is nothing to half-apply.
    if !no_push && !parse_errors.is_empty() {
        let mut msg = format!(
            "{} changed local file(s) are not valid JSON; refusing to push before any remote write:",
            parse_errors.len()
        );
        for e in &parse_errors {
            use std::fmt::Write as _;
            let _ = write!(msg, "\n  - {}/{} -- {}: {}", e.kind, e.slug, e.path.display(), e.error);
        }
        anyhow::bail!("{msg}");
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
    // Post-execute repaint re-classify. Collisions were already caught
    // pre-execute (above), so any error here would be spurious — discard it
    // and fall back to an empty repaint set rather than abort after a
    // successful sync.
    let _classified_after =
        from_catalog_scan_lockfile(&catalog, &changes_after, &tombstones_after, &lockfile)
            .unwrap_or_default();
    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("generating _index.md for env '{env}'"))?;

    let elapsed = started.elapsed();
    let total_changed = outcome.items_pushed + outcome.items_pulled;
    if !renderer_was_supplied {
        progress.event(
            Action::Done,
            &format!(
                "Synced envs/{env} ({total_changed} changed, {:.1}s)",
                elapsed.as_secs_f32()
            ),
        );
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
///   pull driver performs before `record_object` (serialize →
///   `content_hash`). This mirrors how the lockfile's `content_hash` was
///   written on the last pull.
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
) -> anyhow::Result<Vec<crate::cli::sync::classify::ClassifiedItem>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut remote_hashes: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut scan_changes: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut scan_tombstones: BTreeSet<(String, String)> = BTreeSet::new();
    let mut locked: BTreeMap<(String, String), String> = BTreeMap::new();

    // Augment the lockfile with any catalog objects it's missing (e.g. after
    // `doctor --rebuild-lock` wipes it). Reference normalization needs the
    // object's URL in the lockfile to rewrite a remote URL to its `rdc://`
    // form; without this the remote (URL) and the on-disk snapshot (rdc://)
    // canonicalize differently and an unchanged object false-conflicts on the
    // post-rebuild sync. This is a NO-OP whenever the lockfile already tracks
    // the object (the normal path), so it can't affect a populated env.
    let augmented = {
        let mut lf = lockfile.clone();
        let mut add = |kind: &str, id: u64, name: &str| {
            if lf.slug_for_id(kind, id).is_none() {
                // Allocate a UNIQUE slug (against slugs already in this kind)
                // so a same-name sibling never clobbers an existing entry.
                // Plain `slugify` + `upsert` would overwrite the tracked
                // object's id + base hash and collapse both objects onto one
                // slug — dropping one and making the survivor false-classify
                // as RemoteCreate every sync (perpetual non-idempotency).
                // Mirrors the per-kind `slugify_unique` the classify loops use.
                let used: std::collections::HashSet<String> = lf
                    .objects
                    .get(kind)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                let slug = crate::slug::slugify_unique(name, &used);
                lf.upsert(
                    kind,
                    &slug,
                    crate::state::ObjectEntry {
                        id,
                        modified_at: None,
                        content_hash: None,
                        secrets_hash: None,
                    },
                );
            }
        };
        for x in &catalog.labels {
            add("labels", x.id, &x.name);
        }
        for x in &catalog.hooks {
            add("hooks", x.id, &x.name);
        }
        for x in &catalog.rules {
            add("rules", x.id, &x.name);
        }
        for x in &catalog.engines {
            add("engines", x.id, &x.name);
        }
        for x in &catalog.workspaces {
            add("workspaces", x.id, &x.name);
        }
        for x in &catalog.queues {
            // Only seed queues rdc actually TRACKS — those that belong to a
            // workspace. A workspace-less queue (orphan / `deletion_requested`,
            // still returned by GET /queues) is excluded by the pull driver and
            // the classify queue-loop alike, so it never enters the lockfile.
            // Seeding it here would let `portabilize_proposed` rewrite a hook's
            // (or another object's) raw URL ref to that queue into `rdc://`
            // form during classify, while the pull-recorded base — computed
            // against the real lockfile that never tracked the queue — left the
            // ref a raw URL. The two hashes then diverge on EVERY sync, so the
            // referencing object re-pulls forever (RemoteEdit). Mirroring the
            // pull driver's `workspace.is_some()` gate keeps both paths leaving
            // an unresolvable ref raw, so they hash identically.
            if x.workspace.is_some() {
                add("queues", x.id, &x.name);
            }
        }
        for x in catalog.schemas_by_queue_id.values() {
            add("schemas", x.id, &x.name);
        }
        for x in catalog.inboxes_by_queue_id.values() {
            add("inboxes", x.id, &x.name);
        }
        for x in &catalog.workflows {
            add("workflows", x.id, &x.name);
        }
        lf
    };
    let lockfile = &augmented;

    // --- labels --------------------------------------------------------
    // Catalog hash: route through the KindCodec so the adapter hash equals
    // the hash of the bytes actually written to disk by the pull driver.
    // This prevents phantom drift when fields like `modified_at` differ
    // between what the API returns and what the codec writes to disk.
    let labels_codec = crate::snapshot::codec::codec("labels").expect("labels codec must exist");
    let mut used_label_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for l in &catalog.labels {
        let slug = match lockfile.slug_for_id("labels", l.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&l.name, &used_label_slugs),
        };
        used_label_slugs.insert(slug.clone());

        let value = match serde_json::to_value(l) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match labels_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
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
        let hash = crate::state::content_hash(&bytes, &crate::state::Lockfile::default());
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
    // The org is a singleton — slug is always "self". Route through the
    // KindCodec so the adapter hash matches the pull baseline (the codec
    // strips `modified_at` before hashing).
    {
        let org_codec =
            crate::snapshot::codec::codec("organization").expect("organization codec must exist");
        let org = &catalog.organization;
        if let Ok(value) = serde_json::to_value(org)
            && let Ok(art) = org_codec.disk_bytes(&value)
        {
            let json = art.json;
            if !json.is_empty() {
                let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
                remote_hashes.insert(("organization".to_string(), "self".to_string()), hash);
            }
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
    // Slug derivation mirrors `pull::workflows::process`. Route through
    // the KindCodec for hash parity with the pull baseline.
    //
    // `workflow_url_to_slug` is populated here so the workflow_steps
    // block below can resolve `step.workflow` (a URL) to the workflow
    // slug even on a fresh lockfile where `slug_for_url` would
    // otherwise return `None`.
    let workflows_codec =
        crate::snapshot::codec::codec("workflows").expect("workflows codec must exist");
    let mut used_workflow_slugs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut workflow_url_to_slug: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for w in &catalog.workflows {
        let slug = match lockfile.slug_for_id("workflows", w.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&w.name, &used_workflow_slugs),
        };
        used_workflow_slugs.insert(slug.clone());
        workflow_url_to_slug.insert(w.url.clone(), slug.clone());

        let value = match serde_json::to_value(w) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match workflows_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
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
    // `<workflow_slug>/<step_slug>`. Route through the KindCodec for
    // hash parity with the pull baseline.
    let workflow_steps_codec =
        crate::snapshot::codec::codec("workflow_steps").expect("workflow_steps codec must exist");
    let mut per_workflow_used_step: std::collections::HashMap<
        String,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
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

        let value = match serde_json::to_value(s) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match workflow_steps_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
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
    // else `slugify_unique(name, used)`.
    //
    // BUG FIX: the previous adapter hashed workspaces with raw
    // `serde_json::to_vec_pretty` (which keeps `modified_at`), while the
    // pull driver writes via the KindCodec which strips `modified_at`
    // recursively. This caused a workspace whose remote `modified_at`
    // changed to phantom-drift (RemoteEdit / BothDiverged) even when no
    // meaningful content changed. Routing through the codec fixes this.
    //
    // Build a freshly-computed workspace URL → slug map alongside the
    // hash insertion. Queue derivation below uses this map so a
    // first-pull sync can resolve queue.workspace → ws_slug even when
    // the lockfile is still empty (the workspace was just emitted as
    // RemoteCreate one block above; its entry won't land in the
    // lockfile until the executor runs).
    let workspaces_codec =
        crate::snapshot::codec::codec("workspaces").expect("workspaces codec must exist");
    let mut used_workspace_slugs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut ws_url_to_slug: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for w in &catalog.workspaces {
        let slug = match lockfile.slug_for_id("workspaces", w.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&w.name, &used_workspace_slugs),
        };
        used_workspace_slugs.insert(slug.clone());
        ws_url_to_slug.insert(w.url.clone(), slug.clone());

        let value = match serde_json::to_value(w) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match workspaces_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
        remote_hashes.insert(("workspaces".to_string(), slug), hash);
    }
    for (slug, path) in &changes.workspaces {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("workspaces".to_string(), slug.clone()),
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default()),
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
    // Push-capable flat kind. Engines have an overlay and server-set
    // fields (`agenda_id`) that are redacted before hashing. Route
    // through the KindCodec for parity with the pull baseline (the codec
    // applies both redaction and `modified_at` strip internally).
    //
    // `engine_url_to_slug` is populated here so the engine_fields block
    // below can resolve `field.engine` (a URL) to the engine slug even
    // on a fresh lockfile where `slug_for_url` would otherwise return
    // `None`.
    let engines_codec = crate::snapshot::codec::codec("engines").expect("engines codec must exist");
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

        let value = match serde_json::to_value(e) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match engines_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
        remote_hashes.insert(("engines".to_string(), slug), hash);
    }
    for (slug, path) in &changes.engines {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("engines".to_string(), slug.clone()),
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default()),
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
    // `<engine_slug>/<field_slug>`. Route through the KindCodec for hash
    // parity with the pull baseline.
    let engine_fields_codec =
        crate::snapshot::codec::codec("engine_fields").expect("engine_fields codec must exist");
    let mut per_engine_used_ef: std::collections::HashMap<
        String,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
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

        let value = match serde_json::to_value(f) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match engine_fields_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
        remote_hashes.insert(("engine_fields".to_string(), composite_key), hash);
    }
    for (slug, path) in &changes.engine_fields {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("engine_fields".to_string(), slug.clone()),
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default()),
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
    // (the extracted `config.code`). Route through the KindCodec for hash
    // parity with the pull baseline. The codec extracts the code sidecar
    // and `combined_hash` folds it in, matching the combined hash the pull
    // driver records.
    let hooks_codec = crate::snapshot::codec::codec("hooks").expect("hooks codec must exist");
    let mut used_hook_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in &catalog.hooks {
        let slug = match lockfile.slug_for_id("hooks", h.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&h.name, &used_hook_slugs),
        };
        used_hook_slugs.insert(slug.clone());

        let value = match serde_json::to_value(h) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match hooks_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
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
        let hash = crate::state::hook_combined_hash(
            &json_bytes,
            &code,
            &crate::state::Lockfile::default(),
        );
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
    // code lives in `trigger_condition`. Route through the KindCodec for
    // hash parity with the pull baseline.
    let rules_codec = crate::snapshot::codec::codec("rules").expect("rules codec must exist");
    let mut used_rule_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &catalog.rules {
        let slug = match lockfile.slug_for_id("rules", r.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&r.name, &used_rule_slugs),
        };
        used_rule_slugs.insert(slug.clone());

        let value = match serde_json::to_value(r) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match rules_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
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
        let hash = crate::state::rule_combined_hash(
            &json_bytes,
            &code,
            &crate::state::Lockfile::default(),
        );
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
    // Queue-nested kinds share a slug-derivation pass. All remote hashes
    // route through the KindCodec for parity with the pull baseline.
    let queues_codec = crate::snapshot::codec::codec("queues").expect("queues codec must exist");
    let schemas_codec = crate::snapshot::codec::codec("schemas").expect("schemas codec must exist");
    let inboxes_codec = crate::snapshot::codec::codec("inboxes").expect("inboxes codec must exist");
    // Queue slug identity is GLOBAL (single lockfile/classifier namespace).
    // Dedup globally, pre-seeded with already-pinned slugs (kept stable by
    // slug_for_id), so two same-named queues in different workspaces never
    // collapse onto one bare slug.
    let mut used_q_slugs: std::collections::HashSet<String> = lockfile
        .objects
        .get("queues")
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    // Build per-queue (ws_slug, q_slug, q.id, q.url) tuples so the
    // email_templates block can look up its compound key without
    // re-deriving slugs.
    let mut q_url_to_ws_q: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    // Defense-in-depth: detect when two distinct remote queues would be
    // assigned the same bare `(kind, slug)` identity key (e.g. same-named
    // queues in different workspaces). Such a collision silently collapses
    // them in the lockfile/classifier and cross-attributes one queue's
    // schema/formulas/inbox onto the other on the next sync. We abort loudly
    // below instead. Covers schemas + inboxes too — they share this slug.
    let mut qdetect = crate::cli::sync::collision::CollisionDetector::new();
    for q in &catalog.queues {
        let Some(ws_url) = q.workspace.as_ref() else {
            continue;
        };
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
        let q_slug = match lockfile.slug_for_id("queues", q.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&q.name, &used_q_slugs),
        };
        used_q_slugs.insert(q_slug.clone());
        q_url_to_ws_q.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));
        qdetect.observe("queues", &q_slug, q.id, &q.name, &ws_slug);

        // queues — route through the KindCodec (redacts `counts`,
        // strips `modified_at`) for hash parity with the pull baseline.
        if let Ok(q_value) = serde_json::to_value(q)
            && let Ok(q_art) = queues_codec.disk_bytes(&q_value)
        {
            let q_json = crate::cli::pull::common::portabilize_proposed(&q_art.json, lockfile);
            let q_hash = crate::snapshot::codec::combined_hash(&q_json, &q_art.sidecars, lockfile);
            remote_hashes.insert(("queues".to_string(), q_slug.clone()), q_hash);
        }

        // schemas — combined (json + formulas).
        if let Some(schema) = catalog.schemas_by_queue_id.get(&q.id)
            && let Ok(s_value) = serde_json::to_value(schema)
            && let Ok(s_art) = schemas_codec.disk_bytes(&s_value)
        {
            let s_json = crate::cli::pull::common::portabilize_proposed(&s_art.json, lockfile);
            let s_hash = crate::snapshot::codec::combined_hash(&s_json, &s_art.sidecars, lockfile);
            remote_hashes.insert(("schemas".to_string(), q_slug.clone()), s_hash);
        }

        // inboxes — route through the KindCodec for hash parity.
        if let Some(inbox) = catalog.inboxes_by_queue_id.get(&q.id)
            && let Ok(i_value) = serde_json::to_value(inbox)
            && let Ok(i_art) = inboxes_codec.disk_bytes(&i_value)
        {
            let i_json = crate::cli::pull::common::portabilize_proposed(&i_art.json, lockfile);
            let i_hash = crate::snapshot::codec::combined_hash(&i_json, &i_art.sidecars, lockfile);
            remote_hashes.insert(("inboxes".to_string(), q_slug.clone()), i_hash);
        }
    }

    // Abort BEFORE classify/execute (no writes have happened yet) if two
    // distinct queues collapsed onto one identity key.
    let collisions = qdetect.collisions();
    if !collisions.is_empty() {
        anyhow::bail!(
            "{}",
            crate::cli::sync::collision::format_collisions(&collisions)
        );
    }

    // Scan-side hashes for queues / inboxes (flat) and schemas (combined).
    for (slug, path) in &changes.queues {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("queues".to_string(), slug.clone()),
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default()),
            );
        }
    }
    for (slug, path) in &changes.inboxes {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("inboxes".to_string(), slug.clone()),
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default()),
            );
        }
    }
    for (slug, schema_path) in &changes.schemas {
        // The push scanner stores the path to `schema.json`; the formulas
        // sit in a sibling `formulas/` dir. Mirror `scan_schemas`.
        let Ok(json_bytes) = std::fs::read(schema_path) else {
            continue;
        };
        let queue_dir = match schema_path.parent() {
            Some(p) => p,
            None => continue,
        };
        let formulas = crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default();
        let hash = crate::state::schema_combined_hash(
            &json_bytes,
            &formulas,
            &crate::state::Lockfile::default(),
        );
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

    // email_templates — compound slug `<ws>/<q>/<tpl>`. Route through the
    // KindCodec for hash parity with the pull baseline.
    let email_templates_codec =
        crate::snapshot::codec::codec("email_templates").expect("email_templates codec must exist");
    let mut per_queue_used_t_slugs: std::collections::HashMap<
        (String, String),
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
    for t in &catalog.email_templates {
        let Some(queue_url) = t.queue.as_ref() else {
            continue;
        };
        let Some((ws_slug, q_slug)) = q_url_to_ws_q.get(queue_url).cloned() else {
            continue;
        };
        let used = per_queue_used_t_slugs
            .entry((ws_slug.clone(), q_slug.clone()))
            .or_default();
        let template_slug = match lockfile.slug_for_id("email_templates", t.id) {
            Some(existing) => existing.rsplit('/').next().unwrap_or(existing).to_string(),
            None => crate::slug::slugify_unique(&t.name, used),
        };
        used.insert(template_slug.clone());
        let compound = format!("{ws_slug}/{q_slug}/{template_slug}");

        let value = match serde_json::to_value(t) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let art = match email_templates_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = art.json;
        let json = crate::cli::pull::common::portabilize_proposed(&json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
        remote_hashes.insert(("email_templates".to_string(), compound), hash);
    }
    for (slug, path) in &changes.email_templates {
        if let Ok(bytes) = std::fs::read(path) {
            scan_changes.insert(
                ("email_templates".to_string(), slug.clone()),
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default()),
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

    Ok(crate::cli::sync::classify::classify(
        &remote_hashes,
        &scan_changes,
        &scan_tombstones,
        &locked,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::pull::common::RemoteCatalog;
    use crate::cli::sync::classify::SyncClass;
    use crate::model::Hook;
    use crate::paths::Paths;
    use crate::snapshot::hook::serialize_hook;
    use crate::state::{Lockfile, ObjectEntry, hook_combined_hash};
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

    /// Hash a hook the way the pull driver does (serialize +
    /// `hook_combined_hash`) so the test seeds the lockfile with the same
    /// base hash production would have written.
    fn pull_driver_hash(h: &Hook) -> String {
        let (json_bytes, code) = serialize_hook(h).unwrap();
        // Simulate the pull driver's FINAL recorded base (after the portabilize
        // post-pass): the hook is in the lockfile, so its self-url normalizes to
        // `rdc://`. Hash with a minimal lockfile holding this hook so the base
        // matches the classifier's reference-form-agnostic remote hash.
        let mut lf = Lockfile::default();
        lf.upsert(
            "hooks",
            &crate::slug::slugify(&h.name),
            ObjectEntry {
                id: h.id,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        hook_combined_hash(&json_bytes, &code, &lf)
    }

    /// Regression for the deletion_requested-queue hook churn (Bug #1).
    ///
    /// A hook references a queue that rdc does NOT track (e.g. a
    /// `deletion_requested` queue with `workspace: null`). The pull driver
    /// excludes such queues, so it records the hook base with the queue ref
    /// left as a RAW URL (un-portabilized). The classifier's catalog-augment,
    /// however, used to seed EVERY catalog queue — including the untracked one
    /// — into the working lockfile, so `portabilize_proposed` rewrote the raw
    /// ref to `rdc://queues/<slug>`. The recomputed remote hash then diverged
    /// from the stable pull-recorded base on EVERY sync → perpetual RemoteEdit
    /// re-pull. The fix excludes untracked (workspace-less) queues from the
    /// augment so both paths leave the unresolvable ref raw and hash equally.
    #[test]
    fn from_catalog_scan_lockfile_hook_with_untracked_queue_ref_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        let api_base = "https://x.invalid/api/v1";

        // Untracked queue 999 (workspace=null → deletion_requested; rdc never
        // tracks it, yet GET /queues still returns it).
        let deletion_queue: crate::model::Queue = serde_json::from_value(json!({
            "id": 999,
            "url": format!("{api_base}/queues/999"),
            "name": "Deletion Requested Queue",
            "workspace": serde_json::Value::Null,
            "schema": serde_json::Value::Null,
            "status": "deletion_requested",
        }))
        .unwrap();

        // Hook references ONLY the untracked queue 999. As pulled, this ref
        // stays a raw URL (it can't portabilize — 999 isn't tracked).
        let hook: Hook = serde_json::from_value(json!({
            "id": 501,
            "url": format!("{api_base}/hooks/501"),
            "name": "Churning Logger",
            "type": "function",
            "queues": [format!("{api_base}/queues/999")],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "def x(p):\n    return {}\n" },
            "modified_at": "2026-04-20T08:00:00Z",
        }))
        .unwrap();
        let slug = "churning-logger";

        // The real (production) lockfile: tracks the hook (so its self-url
        // normalizes to rdc:// during pull/post-pass), but NOT queue 999.
        let mut lockfile = Lockfile {
            api_base: api_base.to_string(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 501,
                modified_at: Some("2026-04-20T08:00:00Z".to_string()),
                content_hash: None,
                secrets_hash: None,
            },
        );

        // Pull-recorded hook base: portabilize against the REAL lockfile (the
        // hook is tracked → self-url becomes rdc://; queue 999 is absent →
        // stays raw), then hash. Mirrors the pull driver + post-pass exactly.
        let (hook_json, hook_code) = serialize_hook(&hook).unwrap();
        let portabilized = crate::cli::pull::common::portabilize_proposed(&hook_json, &lockfile);
        let base_hash = hook_combined_hash(&portabilized, &hook_code, &lockfile);

        // Write the on-disk hook (portabilized form) + .py sidecar so the
        // scanner sees an unchanged local.
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        std::fs::write(&local_json_path, &portabilized).unwrap();
        std::fs::write(
            paths.hooks_dir().join(format!("{slug}.py")),
            hook_code.as_ref().unwrap().as_bytes(),
        )
        .unwrap();

        // Record the base hash now that we've computed it.
        lockfile
            .objects
            .get_mut("hooks")
            .unwrap()
            .get_mut(slug)
            .unwrap()
            .content_hash = Some(base_hash.clone());

        let (_scanned, changes, tombstones) =
            crate::cli::push::scan::scan(&paths, &lockfile).unwrap();
        assert!(
            !changes.hooks.contains_key(slug),
            "scanner must not flag an unchanged hook"
        );

        // Catalog includes the untracked queue 999 + the hook (this is what
        // drives the augment to seed queue 999 before the fix).
        let mut catalog = catalog_with_hooks(vec![hook]);
        catalog.queues = vec![deletion_queue];

        let classified =
            from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile).unwrap();
        let item = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == slug)
            .expect("hook must appear in classification");
        assert_eq!(
            item.class,
            SyncClass::Clean,
            "hook referencing an untracked queue must classify Clean; got {:?} \
             (remote_hash={:?}, base_hash={:?})",
            item.class,
            item.remote_hash,
            item.base_hash,
        );
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
        let base_hash = pull_driver_hash(&base_hook);

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
            from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile).unwrap();

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

    /// Regression for the catalog-augment slug-collision clobber: two
    /// objects of the same kind share a name (→ same base slug), and only
    /// the first is already tracked in the lockfile. The pre-rebuild augment
    /// (which seeds catalog objects missing from the lockfile so URL→rdc://
    /// normalization works) MUST allocate a *unique* slug for the untracked
    /// sibling instead of `slugify(name)` + clobbering `upsert`. Before the
    /// fix the augment overwrote the tracked object's entry (id + base hash)
    /// with the sibling at the same slug key, which (a) dropped the tracked
    /// object's base hash so an unchanged object false-classified as
    /// RemoteCreate every sync (perpetual non-idempotency) and (b) collapsed
    /// both siblings onto one slug so the second object was lost. Verified
    /// live against org 214757: `Trial vendor` ×2 (labels) plus seeded
    /// `Collision Hook/Rule/Queue/Workspace` pairs churned forever.
    #[test]
    fn from_catalog_scan_lockfile_same_name_collision_yields_distinct_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        // Two distinct remote hooks share a name → both slugify to the same
        // base slug "collision-hook". Fresh env: the lockfile is empty, so the
        // augment seeds BOTH (neither is tracked yet).
        let hook_a = mk_hook(
            100,
            "Collision Hook",
            "def a():\n    return 1\n",
            "2026-05-14T08:00:00Z",
        );
        let hook_b = mk_hook(
            200,
            "Collision Hook",
            "def b():\n    return 2\n",
            "2026-05-14T08:00:00Z",
        );

        let lockfile = Lockfile::default();
        let (_scanned, changes, tombstones) =
            crate::cli::push::scan::scan(&paths, &lockfile).unwrap();

        let catalog = catalog_with_hooks(vec![hook_a, hook_b]);
        let classified =
            from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile).unwrap();

        // Both siblings must surface as DISTINCT RemoteCreate items. Before the
        // fix the augment used `slugify(name)` + clobbering `upsert`, collapsing
        // both onto "collision-hook" so only one survived (the other object was
        // silently lost and the survivor re-pulled forever).
        let hooks: Vec<_> = classified.iter().filter(|c| c.kind == "hooks").collect();
        assert_eq!(
            hooks.len(),
            2,
            "two same-name remote hooks must classify as two items, got {}: {:?}",
            hooks.len(),
            hooks
                .iter()
                .map(|c| (&c.slug, &c.class))
                .collect::<Vec<_>>()
        );
        let first = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == "collision-hook")
            .expect("first sibling keeps the base slug");
        let second = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == "collision-hook-2")
            .expect("second sibling must get a deduped slug (collision-hook-2), not collapse");
        assert_eq!(first.class, SyncClass::RemoteCreate);
        assert_eq!(second.class, SyncClass::RemoteCreate);
    }

    /// Regression for classifier hash-parity: the pull driver records the
    /// base hash over the canonical on-disk bytes, and the adapter
    /// recomputes the remote hash over the same canonical bytes. For an
    /// UNCHANGED remote the two hashes must match → `Clean`. If they ever
    /// diverged the classifier would emit a false-positive RemoteEdit (or,
    /// with a local edit on top, a silent BothDiverged that should have
    /// been LocalEdit — or vice versa: a real BothDiverged silently
    /// downgraded because the adapter's "is the remote at the lockfile?"
    /// answer is wrong).
    ///
    /// This test pins the no-divergence case; a sibling test below covers
    /// the divergent (`BothDiverged`) case.
    ///
    /// Setup:
    /// - Base hook (pull-time): `runtime: "python3.12"`, code A. Lockfile
    ///   recorded `hash(serialize(base) + A)`.
    /// - Remote (now): IDENTICAL to base — nobody changed remote.
    /// - Local: IDENTICAL to base — nobody changed local.
    /// - Expected: `Clean` (everything matches the lockfile).
    #[test]
    fn from_catalog_scan_lockfile_overlay_parity_clean_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::create_dir_all(paths.env_root()).unwrap();

        let slug = "ap-reject-if-no-doc-id";
        let base_hook = mk_hook(
            42,
            slug,
            "def base():\n    return 1\n",
            "2026-05-14T08:00:00Z",
        );

        // Compute the pull-driver base hash so the lockfile matches what
        // production would have written.
        let base_hash = pull_driver_hash(&base_hook);

        // Local file: the disk form is the codec's canonical form AND
        // portabilized to `rdc://` form — matching what the pull post-pass
        // writes to disk in production. A minimal lockfile holding this hook
        // lets the self-url normalize identically to base/remote.
        let (base_json, base_code) = serialize_hook(&base_hook).unwrap();
        let mut local_lf = Lockfile::default();
        local_lf.upsert(
            "hooks",
            &crate::slug::slugify(slug),
            ObjectEntry {
                id: 42,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let base_json =
            crate::cli::pull::common::portabilize_proposed(&base_json, &local_lf);
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &base_json).unwrap();
        std::fs::write(&local_py_path, base_code.as_ref().unwrap().as_bytes()).unwrap();

        // Lockfile: records the canonical base hash.
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_hash.clone()),
                secrets_hash: None,
            },
        );

        // Remote: identical to base — no changes on either side.
        let (_scanned, changes, tombstones) =
            crate::cli::push::scan::scan(&paths, &lockfile).unwrap();
        // Scan must NOT flag this hook — local matches lockfile-recorded
        // canonical hash, since `read(local) == base_json`.
        assert!(
            !changes.hooks.contains_key(slug),
            "scanner shouldn't flag an unchanged local hook"
        );

        let catalog = catalog_with_hooks(vec![base_hook.clone()]);
        let classified =
            from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile).unwrap();
        let item = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == slug)
            .expect("hook must appear in classification");
        assert_eq!(
            item.class,
            SyncClass::Clean,
            "no-op env must classify as Clean; got {:?} (local_hash={:?}, remote_hash={:?}, base_hash={:?})",
            item.class,
            item.local_hash,
            item.remote_hash,
            item.base_hash,
        );
    }

    /// The pointed regression: both local and remote have diverged from
    /// the recorded base. The adapter must classify this as
    /// `BothDiverged` so the conflict prompt fires.
    ///
    /// Concretely:
    /// - Base hook: `runtime: "python3.12"`, code = "base".
    ///   Lockfile records `hash(serialize(base) + "base")`.
    /// - Remote NOW: `runtime: "python3.12"`, code = "REMOTE_EDIT" (user
    ///   modified via UI).
    /// - Local NOW: `runtime: "python3.12"`, code = "LOCAL_EDIT" (user
    ///   modified the .py sidecar).
    /// - Expected: `BothDiverged` → conflict prompt.
    #[test]
    fn from_catalog_scan_lockfile_overlay_both_diverged_with_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::create_dir_all(paths.env_root()).unwrap();

        let slug = "ap-reject-if-no-doc-id";
        let base_hook = mk_hook(
            42,
            slug,
            "def base():\n    return 1\n",
            "2026-05-14T08:00:00Z",
        );
        let base_hash = pull_driver_hash(&base_hook);

        // Local edited the .py sidecar — different code from base. Disk
        // bytes for JSON are unchanged; .py was touched.
        let (base_json, _base_code) = serialize_hook(&base_hook).unwrap();
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &base_json).unwrap();
        std::fs::write(&local_py_path, b"def local_edit():\n    return 2\n").unwrap();

        // Remote: also edited (via Rossum UI).
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
        let classified =
            from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile).unwrap();
        let item = classified
            .iter()
            .find(|c| c.kind == "hooks" && c.slug == slug)
            .expect("hook must appear in classification");
        assert_eq!(
            item.class,
            SyncClass::BothDiverged,
            "with overlay configured and both sides edited, must classify as BothDiverged; \
             got {:?} (local_hash={:?}, remote_hash={:?}, base_hash={:?})",
            item.class,
            item.local_hash,
            item.remote_hash,
            item.base_hash,
        );
    }
}
