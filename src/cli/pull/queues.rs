//! Queue pull driver: writes queue.json + schema.json (with formula
//! sidecars) + inbox.json under `envs/<env>/workspaces/<ws>/queues/<q>/`.
//!
//! Schema + inbox remote bytes are supplied by the caller via the
//! `schemas_by_queue_id` / `inboxes_by_queue_id` maps that
//! `pull::common::prefetch_queue_children` populates during
//! Phase 1 of sync. There is only one fetch round per cycle; the per-queue
//! write decisions stay sequential because they touch shared state
//! (lockfile, queue_locations, conflict counts).

use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object,
    skip_on_permission_denied,
};
use crate::log::{Action, Log};
use crate::model::{Inbox, Queue, Schema};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

const KIND_QUEUES: &str = "queues";
const KIND_SCHEMAS: &str = "schemas";
const KIND_INBOXES: &str = "inboxes";

/// Counts of objects pulled by the queues driver.
pub struct QueueCounts {
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
    pub conflicts: usize,
}

/// Per-queue work item produced by Sub-phase A (filter + slug + queue.json
/// write) and consumed by Sub-phase B (schema + inbox write decisions). The
/// schema / inbox bytes themselves come from the pre-fetched maps the
/// caller threads in — there is no separate id field here because the
/// lookup is by `q.id`.
struct QueueWork<'a> {
    q: &'a Queue,
    q_slug: String,
    queue_dir: std::path::PathBuf,
}

/// Phase 1: list all queues from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<Queue>> {
    skip_on_permission_denied(
        ctx.client
            .list_queues(Some(progress.clone()))
            .await
            .context("listing queues"),
        KIND_QUEUES,
        progress,
    )
}

/// Populate `ctx.queue_locations` (queue URL → (ws_slug, q_slug)) for EVERY
/// queue in the catalog, using the exact same global id-pinned slug
/// derivation as [`process`].
///
/// [`process`] records a queue's location only for queues it actually writes
/// (those in its subset), but [`crate::cli::pull::email_templates::process`]
/// needs the location of a template's owning queue even when that queue is
/// unchanged this cycle. An email-template-only pull leaves the queue subset
/// empty, so the queue driver never runs (or runs over a different subset)
/// and `queue_locations` would otherwise not cover the template's queue —
/// making the driver silently drop the write (never idempotent). Call this
/// before the email_templates dispatch to guarantee full coverage.
///
/// Uses `entry(..).or_insert(..)` so any authoritative location already
/// written by [`process`] this cycle (with a freshly-assigned slug for a
/// brand-new queue) is preserved, and the pass is safe to run before or
/// after [`process`]. Slug derivation is pure (reads only the lockfile), so
/// it never issues a request and matches the on-disk layout exactly.
pub fn locate_queues(ctx: &mut PullCtx<'_>, queues: &[Queue]) {
    let mut used_q_slugs: HashSet<String> = ctx
        .lockfile
        .objects
        .get(KIND_QUEUES)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    for q in queues {
        let Some(ws_url) = q.workspace.as_ref() else {
            continue;
        };
        let Some(ws_slug) = ctx
            .lockfile
            .slug_for_url("workspaces", ws_url)
            .map(str::to_string)
        else {
            continue;
        };
        let q_slug = match ctx.lockfile.slug_for_id(KIND_QUEUES, q.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&q.name, &used_q_slugs),
        };
        used_q_slugs.insert(q_slug.clone());
        ctx.queue_locations
            .entry(q.url.clone())
            .or_insert((ws_slug, q_slug));
    }
}

/// Phase 2: process listed queues — filter, slug, write queue.json + schema +
/// inbox. Also populates `ctx.queue_locations` for email_templates.
///
/// `subset` selects which `(kind, slug)` pairs are written. Filtering is
/// applied at the queue level: when a queue's `("queues", slug)` pair is in
/// the subset, the queue's queue.json, schema (+ formulas), and inbox are
/// all written together. Granular per-file selection (schema-only or
/// inbox-only) is intentionally not modelled here — sync (Task 13+) calls
/// classify per kind, and if a subset asks for `schemas/x` without
/// `queues/x` that's user error surfaced upstream. Queue-nested files
/// always travel as a unit.
///
/// `schemas_by_queue_id` and `inboxes_by_queue_id` are the catalog's
/// pre-fetched per-queue children populated by
/// `pull::common::prefetch_queue_children` during Phase 1.
/// A queue whose entry is missing from a given map (no schema URL, no
/// inbox URL, or a malformed URL that the prefetch silently dropped)
/// simply does not get the corresponding write here — the same outcome
/// as if the fetch had returned `None`. This matches the prefetch's
/// existing forgiving policy and removes the duplicate per-queue GET
/// round that previously lived inside this function.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    queues: Vec<Queue>,
    schemas_by_queue_id: &BTreeMap<u64, Schema>,
    inboxes_by_queue_id: &BTreeMap<u64, Inbox>,
    subset: &BTreeSet<(String, String)>,
    // `(kind, slug)` of queue-nested objects (schemas / inboxes, keyed by the
    // queue slug) that were PUSHED earlier in this same sync cycle. Their
    // Sub-phase B re-pull is skipped: the schema/inbox map here comes from the
    // PRE-PUSH Phase-1 catalog, so re-writing a just-pushed object would revert
    // the push locally and force a second sync (non-idempotent). Empty for a
    // plain `pull` (no push phase).
    pushed_nested: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<QueueCounts> {
    // Queue slug identity is GLOBAL, not per-workspace. The lockfile and the
    // sync classifier key queue-nested kinds (queues/schemas/inboxes) in a
    // single namespace, so a per-workspace used-set let two same-named queues
    // in different workspaces both claim the bare slug and collapse onto one
    // identity (silent cross-attribution). Dedup globally, pre-seeded with the
    // slugs already pinned in the lockfile (kept stable by `slug_for_id`) so a
    // newly-seen queue never steals an existing slug regardless of list order.
    let mut used_q_slugs: HashSet<String> = ctx
        .lockfile
        .objects
        .get(KIND_QUEUES)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let mut counts = QueueCounts {
        queues: 0,
        schemas: 0,
        inboxes: 0,
        conflicts: 0,
    };

    // === Sub-phase A: filter, slug, queue.json write, build work list ===
    let mut work: Vec<QueueWork> = Vec::new();
    for q in &queues {
        let ws_url = match &q.workspace {
            Some(u) => u,
            None => continue,
        };
        let ws_slug = match ctx.lockfile.slug_for_url("workspaces", ws_url) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let q_slug = match ctx.lockfile.slug_for_id(KIND_QUEUES, q.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&q.name, &used_q_slugs),
        };
        used_q_slugs.insert(q_slug.clone());

        if !subset.contains(&(KIND_QUEUES.to_string(), q_slug.clone())) {
            continue;
        }

        let queue_result: Result<()> = (|| {
            let queue_dir = ctx.paths.queue_dir(&ws_slug, &q_slug);
            std::fs::create_dir_all(&queue_dir)
                .with_context(|| format!("creating {}", queue_dir.display()))?;

            // The queue's slug is GLOBAL id-pinned (see the `used_q_slugs`
            // doc above), so if the remote moved this queue to a different
            // workspace since the last pull, its OLD workspace dir under the
            // same slug is now stale. Left alone, the two dirs would collide
            // on the one lockfile entry keyed by `q_slug` and the queue would
            // re-push on every sync forever (never idempotent) — today
            // `sync::mod`'s `detect_slug_collisions` only WARNS about this
            // after the fact. Self-heal here, before this queue's
            // new-location files are written below, so the collision never
            // has a chance to persist across a sync cycle.
            if let Some(stale_dir) = find_stale_queue_dir(ctx.paths, &ws_slug, &q_slug, q.id)
                && stale_dir != queue_dir
            {
                reconcile_moved_queue(ctx, &q_slug, &ws_slug, &stale_dir, progress)?;
            }

            ctx.queue_locations
                .insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));

            // queue.json — three-way write via KindCodec (strips modified_at +
            // redacts counts).
            let queue_path = queue_dir.join("queue.json");
            let value = serde_json::to_value(q)?;
            let art = crate::snapshot::codec::codec(KIND_QUEUES)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing queue")?;
            let queue_proposed = art.json;
            let queue_base = ctx
                .lockfile
                .objects
                .get(KIND_QUEUES)
                .and_then(|m| m.get(&q_slug))
                .and_then(|e| e.content_hash.clone());
            let queue_proposed =
                crate::cli::pull::common::portabilize_proposed(&queue_proposed, &*ctx.lockfile);
            let (q_action, q_remote_hash) =
                decide_pull_action(&queue_path, queue_base.as_deref(), &queue_proposed)?;
            if q_action == PullAction::Conflict {
                counts.conflicts += 1;
            }
            let q_recorded = apply_pull_action(
                q_action,
                crate::cli::resolve::ObjectRef { kind: KIND_QUEUES, slug: &q_slug },
                &queue_path,
                &queue_proposed,
                q_remote_hash,
                ctx.interactive,
                progress,
                ctx.paths.env(),
                queue_base.as_deref(),
                Some(ctx.paths),
            )?;
            record_object(
                ctx.lockfile,
                KIND_QUEUES,
                &q_slug,
                q.id,
                q.modified_at().map(|s| s.to_string()),
                q.modified_by().map(|s| s.to_string()),
                Some(q_recorded),
            );
            counts.queues += 1;

            // Preserve the legacy "no schema" notice. Inbox-absence is normal
            // and intentionally silent. The actual schema / inbox bytes come
            // from `schemas_by_queue_id` / `inboxes_by_queue_id` in Sub-phase
            // B below — no URL parsing is needed here because the maps are
            // keyed by `q.id`.
            if q.schema.is_none() {
                progress.event(
                    Action::Skip,
                    &format!(
                        "queue '{}' (id {}) has no schema; skipping schema + inbox",
                        q.name, q.id,
                    ),
                );
            }

            work.push(QueueWork {
                q,
                q_slug: q_slug.clone(),
                queue_dir,
            });
            Ok(())
        })();
        queue_result?;
    }

    if work.is_empty() {
        return Ok(counts);
    }

    // === Sub-phase B: schema + inbox write decisions ===
    // No fetches here — the caller pre-populated the maps during Phase 1.
    // Decisions mutate shared state (lockfile, conflict counts), so the
    // loop is intentionally sequential.
    for w in &work {
        // Skip a schema/inbox that was PUSHED this cycle: its bytes here are the
        // stale pre-push catalog, and its own push already left local == remote.
        // Re-writing it would revert the push and break single-pass convergence.
        let schema_pushed =
            pushed_nested.contains(&(KIND_SCHEMAS.to_string(), w.q_slug.clone()));
        let inbox_pushed =
            pushed_nested.contains(&(KIND_INBOXES.to_string(), w.q_slug.clone()));
        if let Some(schema) = schemas_by_queue_id.get(&w.q.id)
            && !schema_pushed
        {
            write_schema_for_queue(ctx, &mut counts, w, schema, progress)?;
        }
        if let Some(inbox) = inboxes_by_queue_id.get(&w.q.id)
            && !inbox_pushed
        {
            write_inbox_for_queue(ctx, &mut counts, w, inbox, progress)?;
        }
    }

    if counts.queues > 0 {
        progress.event(
            Action::Pull,
            &format!(
                "queues ({} pulled, schemas {}, inboxes {})",
                counts.queues, counts.schemas, counts.inboxes,
            ),
        );
    }

    Ok(counts)
}

fn write_schema_for_queue(
    ctx: &mut PullCtx<'_>,
    counts: &mut QueueCounts,
    w: &QueueWork<'_>,
    schema: &Schema,
    progress: &Arc<Log>,
) -> Result<()> {
    let queue_dir = &w.queue_dir;
    let schema_path = queue_dir.join("schema.json");
    let pre_local_json = if schema_path.exists() {
        Some(
            std::fs::read(&schema_path)
                .with_context(|| format!("reading {}", schema_path.display()))?,
        )
    } else {
        None
    };
    let pre_local_formulas = crate::snapshot::schema::read_local_formulas(queue_dir)?;

    let (remote_json_bytes, remote_formulas) = crate::snapshot::schema::serialize_schema(schema)?;
    let remote_json_bytes =
        crate::cli::pull::common::portabilize_proposed(&remote_json_bytes, &*ctx.lockfile);

    // Remote hash is computed over the canonical bytes that are actually
    // written to disk, so re-pulls don't show phantom drift.
    let remote_combined_hash =
        crate::state::schema_combined_hash(&remote_json_bytes, &remote_formulas, ctx.lockfile);

    let schema_base = ctx
        .lockfile
        .objects
        .get(KIND_SCHEMAS)
        .and_then(|m| m.get(&w.q_slug))
        .and_then(|e| e.content_hash.clone());
    let local_combined = pre_local_json
        .as_ref()
        .map(|lj| crate::state::schema_combined_hash(lj, &pre_local_formulas, ctx.lockfile));
    let s_action = super::common::classify_combined_pull(
        schema_base.as_deref(),
        local_combined.as_deref(),
        &remote_combined_hash,
    );

    let schema_recorded = match s_action {
        PullAction::Write => {
            crate::snapshot::schema::write_schema_bytes_with_cache(
                queue_dir,
                &remote_json_bytes,
                &remote_formulas,
                Some(ctx.paths),
            )
            .with_context(|| format!("writing schema for queue '{}'", w.q.name))?;
            remote_combined_hash
        }
        PullAction::KeepLocal => {
            let local_json = pre_local_json.as_ref().unwrap();
            crate::state::schema_combined_hash(local_json, &pre_local_formulas, ctx.lockfile)
        }
        PullAction::NoChange => {
            // Combined hash is already equal — no file writes needed.
            remote_combined_hash
        }
        PullAction::Conflict => {
            counts.conflicts += 1;
            let local_json = pre_local_json.as_ref().unwrap();

            // Spec §8.3: when interactive AND the formula sets align on
            // both sides (same field IDs), prompt per file. Asymmetric
            // formula sets (added/removed formulas) fall back to the
            // shadow-file flow — modeling adds/deletes isn't
            // a [k]/[r]/[e]/[s]/[a] decision shape.
            let local_ids: std::collections::BTreeSet<&str> = pre_local_formulas
                .iter()
                .map(|(id, _)| id.as_str())
                .collect();
            let remote_ids: std::collections::BTreeSet<&str> =
                remote_formulas.iter().map(|(id, _)| id.as_str()).collect();
            let symmetric = local_ids == remote_ids;

            if ctx.interactive && symmetric {
                let total = 1 + remote_formulas.len();
                let json_outcome = crate::cli::resolve::resolve_combined_file(
                    1,
                    total,
                    crate::cli::resolve::ObjectRef { kind: KIND_SCHEMAS, slug: &w.q_slug },
                    &schema_path,
                    local_json,
                    &remote_json_bytes,
                    ctx.interactive,
                    ctx.paths,
                    progress,
                )?;
                let mut preserve_base = json_outcome.is_preserve_base();
                let resolved_json = json_outcome.into_bytes();
                let mut resolved_formulas: Vec<(String, Vec<u8>)> =
                    Vec::with_capacity(remote_formulas.len());
                let local_by_id: std::collections::BTreeMap<&str, &Vec<u8>> = pre_local_formulas
                    .iter()
                    .map(|(id, b)| (id.as_str(), b))
                    .collect();
                for (i, (field_id, remote_bytes)) in remote_formulas.iter().enumerate() {
                    let local_bytes = local_by_id
                        .get(field_id.as_str())
                        .copied()
                        .cloned()
                        .unwrap_or_default();
                    let formula_path = queue_dir.join("formulas").join(format!("{field_id}.py"));
                    let outcome = crate::cli::resolve::resolve_combined_file(
                        i + 2,
                        total,
                        crate::cli::resolve::ObjectRef { kind: KIND_SCHEMAS, slug: &w.q_slug },
                        &formula_path,
                        &local_bytes,
                        remote_bytes,
                        ctx.interactive,
                        ctx.paths,
                        progress,
                    )?;
                    preserve_base |= outcome.is_preserve_base();
                    resolved_formulas.push((field_id.clone(), outcome.into_bytes()));
                }
                if preserve_base {
                    match schema_base.as_deref() {
                        Some(prior) => prior.to_string(),
                        None => crate::state::schema_combined_hash(
                            &resolved_json,
                            &resolved_formulas,
                            ctx.lockfile,
                        ),
                    }
                } else {
                    crate::state::schema_combined_hash(
                        &resolved_json,
                        &resolved_formulas,
                        ctx.lockfile,
                    )
                }
            } else {
                // Non-interactive shadow-file flow — unresolved by
                // construction. Preserve the prior lockfile base so the
                // next pull/sync re-classifies this schema as a conflict.
                // The remote side is parked in the gitignored
                // `.rdc/conflicts/<env>/` tree, mirroring the queue's
                // `schema.json` + `formulas/` layout.
                let remote_path = ctx.paths.conflict_shadow_path(&schema_path);
                let remote_formulas_dir =
                    ctx.paths.conflict_shadow_path(&queue_dir.join("formulas"));
                crate::snapshot::writer::write_atomic(&remote_path, &remote_json_bytes)?;
                if !remote_formulas.is_empty() {
                    std::fs::create_dir_all(&remote_formulas_dir)
                        .with_context(|| format!("creating {}", remote_formulas_dir.display()))?;
                    for (field_id, bytes) in &remote_formulas {
                        let p = remote_formulas_dir.join(format!("{field_id}.py"));
                        crate::snapshot::writer::write_atomic(&p, bytes)?;
                    }
                }
                progress.event(Action::Warn, &format!(
                    "{} conflict: local preserved, remote at {} (formulas at {}); lockfile base preserved",
                    schema_path.display(),
                    remote_path.display(),
                    remote_formulas_dir.display(),
                ));
                match schema_base.as_deref() {
                    Some(prior) => prior.to_string(),
                    None => crate::state::schema_combined_hash(
                        local_json,
                        &pre_local_formulas,
                        ctx.lockfile,
                    ),
                }
            }
        }
    };
    record_object(
        ctx.lockfile,
        KIND_SCHEMAS,
        &w.q_slug,
        schema.id,
        schema.modified_at().map(|s| s.to_string()),
        schema.modified_by().map(|s| s.to_string()),
        Some(schema_recorded),
    );
    counts.schemas += 1;
    Ok(())
}

fn write_inbox_for_queue(
    ctx: &mut PullCtx<'_>,
    counts: &mut QueueCounts,
    w: &QueueWork<'_>,
    inbox: &Inbox,
    progress: &Arc<Log>,
) -> Result<()> {
    let inbox_path = w.queue_dir.join("inbox.json");

    // Canonical on-disk bytes via KindCodec: strips `modified_at`.
    let value = serde_json::to_value(inbox)?;
    let art = crate::snapshot::codec::codec(KIND_INBOXES)
        .unwrap()
        .disk_bytes(&value)
        .context("serializing inbox")?;
    let inbox_proposed = art.json;

    let inbox_base = ctx
        .lockfile
        .objects
        .get(KIND_INBOXES)
        .and_then(|m| m.get(&w.q_slug))
        .and_then(|e| e.content_hash.clone());
    let inbox_proposed =
        crate::cli::pull::common::portabilize_proposed(&inbox_proposed, &*ctx.lockfile);
    let (i_action, i_remote_hash) =
        decide_pull_action(&inbox_path, inbox_base.as_deref(), &inbox_proposed)?;
    if i_action == PullAction::Conflict {
        counts.conflicts += 1;
    }
    let i_recorded = apply_pull_action(
        i_action,
        crate::cli::resolve::ObjectRef { kind: KIND_INBOXES, slug: &w.q_slug },
        &inbox_path,
        &inbox_proposed,
        i_remote_hash,
        ctx.interactive,
        progress,
        ctx.paths.env(),
        inbox_base.as_deref(),
        Some(ctx.paths),
    )?;
    record_object(
        ctx.lockfile,
        KIND_INBOXES,
        &w.q_slug,
        inbox.id,
        inbox.modified_at().map(|s| s.to_string()),
        inbox.modified_by().map(|s| s.to_string()),
        Some(i_recorded),
    );
    counts.inboxes += 1;
    Ok(())
}

/// Locate a STALE pre-move copy of queue `q_id`/`q_slug`: a
/// `<other_ws>/queues/<q_slug>/queue.json` under any workspace OTHER than
/// `current_ws_slug` whose parsed `"id"` matches `q_id`.
///
/// Matching on the parsed id (not just the directory's slug name) is what
/// makes this safe — the slug alone can't be trusted to prove identity, but
/// slugs are id-pinned and globally unique (see the `used_q_slugs` doc in
/// [`process`]), so a *different* queue could never legitimately share this
/// exact slug. Finding `<ws_slug>/queues/<q_slug>/queue.json` with a
/// matching id under a different workspace therefore means: this is the same
/// queue, and the remote moved it since the last pull.
fn find_stale_queue_dir(
    paths: &crate::paths::Paths,
    current_ws_slug: &str,
    q_slug: &str,
    q_id: u64,
) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(paths.workspaces_dir()).ok()?;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let ws_slug = entry.file_name().to_string_lossy().into_owned();
        if ws_slug == current_ws_slug {
            continue;
        }
        let candidate = paths.queue_dir(&ws_slug, q_slug);
        let Ok(bytes) = std::fs::read(candidate.join("queue.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value.get("id").and_then(|v| v.as_u64()) == Some(q_id) {
            return Some(candidate);
        }
    }
    None
}

/// Self-heal a queue that moved workspace on the remote: delete the stale
/// pre-move dir when it's provably unedited (see
/// [`is_stale_queue_dir_clean`]), otherwise warn and leave it in place.
///
/// SAFETY: losing a user's un-synced local edit is far worse than leaving a
/// stray directory and a warning, so any doubt about "clean" must resolve to
/// "edited" (handled inside `is_stale_queue_dir_clean`, which this function
/// trusts without a second opinion).
fn reconcile_moved_queue(
    ctx: &PullCtx<'_>,
    q_slug: &str,
    new_ws_slug: &str,
    stale_dir: &std::path::Path,
    progress: &Arc<Log>,
) -> Result<()> {
    let old_ws_slug = stale_dir
        .parent() // .../queues
        .and_then(|queues_dir| queues_dir.parent()) // .../<ws_slug>
        .and_then(|ws_dir| ws_dir.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_string());

    if is_stale_queue_dir_clean(ctx, q_slug, stale_dir)? {
        std::fs::remove_dir_all(stale_dir)
            .with_context(|| format!("removing stale queue dir {}", stale_dir.display()))?;
        progress.event(
            Action::Info,
            &format!(
                "relocated queue '{q_slug}': workspace '{old_ws_slug}' -> '{new_ws_slug}' \
                 (removed stale copy after remote move)"
            ),
        );
    } else {
        progress.event(
            Action::Warn,
            &format!(
                "queue '{q_slug}' moved to workspace '{new_ws_slug}' on the remote, but the \
                 local copy in '{old_ws_slug}' has un-synced edits — not auto-removed (resolve \
                 manually to avoid losing them)"
            ),
        );
    }
    Ok(())
}

/// Whether the stale pre-move queue dir has NO un-synced local edits, i.e.
/// it's safe to delete outright.
///
/// Checks `queue.json`, `schema.json` (+ `formulas/*.py`), and `inbox.json` —
/// whichever of these are present in `dir` — against the lockfile's recorded
/// base `content_hash` for `q_slug`. The hash computations mirror EXACTLY
/// what this driver's own write path already does for a "current" queue, so
/// "clean" here means precisely what the three-way merge means elsewhere in
/// this file:
///   - `queue.json` / `inbox.json`: `content_hash` over `Lockfile::default()`,
///     the same call [`decide_pull_action`] makes internally for these
///     single-file kinds.
///   - `schema.json` + `formulas/`: `schema_combined_hash` over `ctx.lockfile`,
///     the same call [`write_schema_for_queue`] makes for its `local_combined`.
///
/// Conservative by construction: a file present with no recorded base, or
/// whose hash doesn't match the recorded base, makes the WHOLE dir not
/// clean — never guess.
fn is_stale_queue_dir_clean(ctx: &PullCtx<'_>, q_slug: &str, dir: &std::path::Path) -> Result<bool> {
    // queue.json always exists — it's how `find_stale_queue_dir` identified
    // this dir in the first place.
    let queue_path = dir.join("queue.json");
    let Some(queue_base) = ctx
        .lockfile
        .objects
        .get(KIND_QUEUES)
        .and_then(|m| m.get(q_slug))
        .and_then(|e| e.content_hash.as_deref())
    else {
        return Ok(false);
    };
    let queue_bytes = std::fs::read(&queue_path)
        .with_context(|| format!("reading {}", queue_path.display()))?;
    if crate::state::content_hash(&queue_bytes, &crate::state::Lockfile::default()) != queue_base {
        return Ok(false);
    }

    let schema_path = dir.join("schema.json");
    if schema_path.exists() {
        let Some(schema_base) = ctx
            .lockfile
            .objects
            .get(KIND_SCHEMAS)
            .and_then(|m| m.get(q_slug))
            .and_then(|e| e.content_hash.as_deref())
        else {
            return Ok(false);
        };
        let schema_bytes = std::fs::read(&schema_path)
            .with_context(|| format!("reading {}", schema_path.display()))?;
        let formulas = crate::snapshot::schema::read_local_formulas(dir)?;
        let combined = crate::state::schema_combined_hash(&schema_bytes, &formulas, ctx.lockfile);
        if combined != schema_base {
            return Ok(false);
        }
    }

    let inbox_path = dir.join("inbox.json");
    if inbox_path.exists() {
        let Some(inbox_base) = ctx
            .lockfile
            .objects
            .get(KIND_INBOXES)
            .and_then(|m| m.get(q_slug))
            .and_then(|e| e.content_hash.as_deref())
        else {
            return Ok(false);
        };
        let inbox_bytes = std::fs::read(&inbox_path)
            .with_context(|| format!("reading {}", inbox_path.display()))?;
        if crate::state::content_hash(&inbox_bytes, &crate::state::Lockfile::default())
            != inbox_base
        {
            return Ok(false);
        }
    }

    Ok(true)
}
