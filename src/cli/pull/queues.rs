//! Queue pull driver: writes queue.json + schema.json (with formula
//! sidecars) + inbox.json under `envs/<env>/workspaces/<ws>/queues/<q>/`.
//!
//! Schema + inbox fetches are pipelined via `buffer_unordered(N)` (per
//! spec §16, default N=5) so a queue tree of 25+ queues doesn't take 50
//! sequential round-trips. The per-queue write decisions stay sequential
//! because they touch shared state (lockfile, queue_locations, conflict
//! counts).

use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, parse_id_from_url,
    record_object, skip_on_permission_denied, PullAction, PullCtx,
};
use crate::model::{Inbox, Queue, Schema};
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use futures::stream::{StreamExt, TryStreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Counts of objects pulled by the queues driver.
pub struct QueueCounts {
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
    pub conflicts: usize,
}

/// Per-queue work item produced by Phase 1 (filter + slug + queue.json
/// write) and consumed by Phase 3 (schema + inbox write decisions).
struct QueueWork<'a> {
    q: &'a Queue,
    q_slug: String,
    queue_dir: std::path::PathBuf,
    schema_id: Option<u64>,
    inbox_id: Option<u64>,
}

pub async fn pull(ctx: &mut PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<QueueCounts> {
    progress.start_phase("queues");
    let queues = skip_on_permission_denied(
        ctx.client.list_queues(Some(progress.clone())).await.context("listing queues"),
        "queues",
        progress,
    )?;

    progress.inc_total(queues.len() as u64);
    let mut per_ws_used_slugs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut counts = QueueCounts { queues: 0, schemas: 0, inboxes: 0, conflicts: 0 };

    // === Phase 1: filter, slug, queue.json write, build work list ===
    let mut work: Vec<QueueWork> = Vec::new();
    for q in &queues {
        let ws_url = match &q.workspace {
            Some(u) => u,
            None => {
                progress.skipped_orphan();
                continue;
            }
        };
        let ws_slug = match ctx.lockfile.slug_for_url("workspaces", ws_url) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let used = per_ws_used_slugs.entry(ws_slug.clone()).or_default();
        let q_slug = match ctx.lockfile.slug_for_id("queues", q.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&q.name, used),
        };
        used.insert(q_slug.clone());

        let queue_dir = ctx.paths.queue_dir(&ws_slug, &q_slug);
        std::fs::create_dir_all(&queue_dir)
            .with_context(|| format!("creating {}", queue_dir.display()))?;

        ctx.queue_locations.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));

        // queue.json — three-way write (local-only, no fetch).
        let queue_path = queue_dir.join("queue.json");
        let mut queue_proposed = serde_json::to_vec_pretty(q).context("serializing queue")?;
        queue_proposed.push(b'\n');
        let queue_proposed = maybe_strip_overlay(
            queue_proposed,
            ctx.overlay.as_ref().and_then(|o| o.queue(&q_slug)),
        )?;
        let queue_base = ctx
            .lockfile
            .objects
            .get("queues")
            .and_then(|m| m.get(&q_slug))
            .and_then(|e| e.content_hash.clone());
        let (q_action, q_remote_hash) =
            decide_pull_action(&queue_path, queue_base.as_deref(), &queue_proposed)?;
        if q_action == PullAction::Conflict {
            counts.conflicts += 1;
        }
        let q_recorded = apply_pull_action(q_action, &queue_path, &queue_proposed, q_remote_hash, ctx.interactive, progress)?;
        record_object(
            ctx.lockfile,
            "queues",
            &q_slug,
            q.id,
            Some(q.url.clone()),
            q.modified_at().map(|s| s.to_string()),
            Some(q_recorded),
        );
        counts.queues += 1;

        // Resolve schema + inbox IDs upfront. Either may be missing for
        // orphan/hidden queues; we don't fetch what isn't there.
        let schema_id = match &q.schema {
            Some(url) => Some(parse_id_from_url(url)
                .with_context(|| format!("parsing schema URL '{}' for queue '{}'", url, q.name))?),
            None => {
                progress.println(format!(
                    "warning: queue '{}' (id {}) has no schema — skipping schema + inbox",
                    q.name, q.id,
                ));
                None
            }
        };
        let inbox_id = match &q.inbox {
            Some(url) => Some(parse_id_from_url(url)
                .with_context(|| format!("parsing inbox URL '{}' for queue '{}'", url, q.name))?),
            None => None,
        };

        work.push(QueueWork { q, q_slug, queue_dir, schema_id, inbox_id });
    }

    // === Phase 2: concurrent schema + inbox fetches ===
    // Per spec §16 #4 + §7.2: bounded by ctx.concurrency.
    let client = ctx.client;
    let progress_inner = progress.clone();
    let fetched_vec: Vec<(u64, Option<Schema>, Option<Inbox>)> = futures::stream::iter(
        work.iter().map(|w| (w.q.id, w.q.name.clone(), w.schema_id, w.inbox_id))
    )
    .map(|(qid, qname, sid_opt, iid_opt)| {
        let p = progress_inner.clone();
        async move {
            let schema = match sid_opt {
                Some(sid) => Some(client.get_schema(sid, Some(p.clone())).await
                    .with_context(|| format!("fetching schema {sid} for queue '{qname}'"))?),
                None => None,
            };
            let inbox = match iid_opt {
                Some(iid) => Some(client.get_inbox(iid, Some(p.clone())).await
                    .with_context(|| format!("fetching inbox {iid} for queue '{qname}'"))?),
                None => None,
            };
            Ok::<_, anyhow::Error>((qid, schema, inbox))
        }
    })
    .buffer_unordered(ctx.concurrency)
    .try_collect()
    .await?;
    let fetched: HashMap<u64, (Option<Schema>, Option<Inbox>)> =
        fetched_vec.into_iter().map(|(qid, s, i)| (qid, (s, i))).collect();

    // === Phase 3: schema + inbox write decisions (sequential, mutates lockfile) ===
    for w in &work {
        let Some((schema_opt, inbox_opt)) = fetched.get(&w.q.id) else { continue };

        if let Some(schema) = schema_opt {
            write_schema_for_queue(ctx, &mut counts, w, schema, progress)?;
        }
        if let Some(inbox) = inbox_opt {
            write_inbox_for_queue(ctx, &mut counts, w, inbox, progress)?;
        }
        progress.tick(&w.q.name);
    }

    Ok(counts)
}

fn write_schema_for_queue(
    ctx: &mut PullCtx<'_>,
    counts: &mut QueueCounts,
    w: &QueueWork<'_>,
    schema: &Schema,
    progress: &Arc<OverallProgress>,
) -> Result<()> {
    let queue_dir = &w.queue_dir;
    let schema_path = queue_dir.join("schema.json");
    let pre_local_json = if schema_path.exists() {
        Some(std::fs::read(&schema_path)
            .with_context(|| format!("reading {}", schema_path.display()))?)
    } else {
        None
    };
    let pre_local_formulas = crate::snapshot::schema::read_local_formulas(queue_dir)?;

    let (remote_json_bytes, remote_formulas) =
        crate::snapshot::schema::serialize_schema(schema)?;
    let remote_json_bytes = maybe_strip_overlay(
        remote_json_bytes,
        ctx.overlay.as_ref().and_then(|o| o.schema(&w.q_slug)),
    )?;
    let remote_combined_hash =
        crate::state::schema_combined_hash(&remote_json_bytes, &remote_formulas);

    let schema_base = ctx
        .lockfile
        .objects
        .get("schemas")
        .and_then(|m| m.get(&w.q_slug))
        .and_then(|e| e.content_hash.clone());
    let s_action = match (schema_base.as_deref(), &pre_local_json) {
        (None, _) => PullAction::Write,
        (_, None) => PullAction::Write,
        (Some(base), Some(local_json)) => {
            let local_combined =
                crate::state::schema_combined_hash(local_json, &pre_local_formulas);
            let local_matches = local_combined == base;
            let remote_matches = remote_combined_hash == base;
            match (local_matches, remote_matches) {
                (true, _) => PullAction::Write,
                (false, true) => PullAction::KeepLocal,
                (false, false) => PullAction::Conflict,
            }
        }
    };

    let schema_recorded = match s_action {
        PullAction::Write => {
            crate::snapshot::schema::write_schema_bytes(
                queue_dir, &remote_json_bytes, &remote_formulas,
            ).with_context(|| format!("writing schema for queue '{}'", w.q.name))?;
            remote_combined_hash
        }
        PullAction::KeepLocal => {
            let local_json = pre_local_json.as_ref().unwrap();
            crate::state::schema_combined_hash(local_json, &pre_local_formulas)
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
            let local_ids: std::collections::BTreeSet<&str> =
                pre_local_formulas.iter().map(|(id, _)| id.as_str()).collect();
            let remote_ids: std::collections::BTreeSet<&str> =
                remote_formulas.iter().map(|(id, _)| id.as_str()).collect();
            let symmetric = local_ids == remote_ids;

            if ctx.interactive && symmetric {
                let total = 1 + remote_formulas.len();
                let resolved_json = crate::cli::resolve::resolve_combined_file(
                    1, total,
                    &schema_path,
                    local_json,
                    &remote_json_bytes,
                    ctx.interactive,
                )?;
                let mut resolved_formulas: Vec<(String, Vec<u8>)> =
                    Vec::with_capacity(remote_formulas.len());
                let local_by_id: std::collections::BTreeMap<&str, &Vec<u8>> =
                    pre_local_formulas.iter().map(|(id, b)| (id.as_str(), b)).collect();
                for (i, (field_id, remote_bytes)) in remote_formulas.iter().enumerate() {
                    let local_bytes = local_by_id.get(field_id.as_str()).copied()
                        .cloned().unwrap_or_default();
                    let formula_path = queue_dir.join("formulas").join(format!("{field_id}.py"));
                    let bytes = crate::cli::resolve::resolve_combined_file(
                        i + 2, total,
                        &formula_path,
                        &local_bytes,
                        remote_bytes,
                        ctx.interactive,
                    )?;
                    resolved_formulas.push((field_id.clone(), bytes));
                }
                crate::state::schema_combined_hash(&resolved_json, &resolved_formulas)
            } else {
                // Legacy shadow-file flow.
                let remote_path = queue_dir.join("schema.json.remote");
                crate::snapshot::writer::write_atomic(&remote_path, &remote_json_bytes)?;
                if !remote_formulas.is_empty() {
                    let remote_formulas_dir = queue_dir.join("formulas.remote");
                    std::fs::create_dir_all(&remote_formulas_dir)
                        .with_context(|| format!("creating {}", remote_formulas_dir.display()))?;
                    for (field_id, bytes) in &remote_formulas {
                        let p = remote_formulas_dir.join(format!("{field_id}.py"));
                        crate::snapshot::writer::write_atomic(&p, bytes)?;
                    }
                }
                progress.println(format!(
                    "warning: {} conflict — local preserved, remote at {} (formulas at {})",
                    schema_path.display(),
                    queue_dir.join("schema.json.remote").display(),
                    queue_dir.join("formulas.remote").display(),
                ));
                crate::state::schema_combined_hash(local_json, &pre_local_formulas)
            }
        }
    };
    record_object(
        ctx.lockfile,
        "schemas",
        &w.q_slug,
        schema.id,
        Some(schema.url.clone()),
        schema.modified_at().map(|s| s.to_string()),
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
    progress: &Arc<OverallProgress>,
) -> Result<()> {
    let inbox_path = w.queue_dir.join("inbox.json");
    let mut inbox_proposed = serde_json::to_vec_pretty(inbox).context("serializing inbox")?;
    inbox_proposed.push(b'\n');
    let inbox_proposed = maybe_strip_overlay(
        inbox_proposed,
        ctx.overlay.as_ref().and_then(|o| o.inbox(&w.q_slug)),
    )?;
    let inbox_base = ctx
        .lockfile
        .objects
        .get("inboxes")
        .and_then(|m| m.get(&w.q_slug))
        .and_then(|e| e.content_hash.clone());
    let (i_action, i_remote_hash) =
        decide_pull_action(&inbox_path, inbox_base.as_deref(), &inbox_proposed)?;
    if i_action == PullAction::Conflict {
        counts.conflicts += 1;
    }
    let i_recorded = apply_pull_action(i_action, &inbox_path, &inbox_proposed, i_remote_hash, ctx.interactive, progress)?;
    record_object(
        ctx.lockfile,
        "inboxes",
        &w.q_slug,
        inbox.id,
        Some(inbox.url.clone()),
        inbox.modified_at().map(|s| s.to_string()),
        Some(i_recorded),
    );
    counts.inboxes += 1;
    Ok(())
}
