use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, parse_id_from_url,
    record_object, skip_on_permission_denied, PullAction, PullCtx,
};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

/// Counts of objects pulled by the queues driver.
pub struct QueueCounts {
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
    pub conflicts: usize,
}

pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<QueueCounts> {
    let queues = skip_on_permission_denied(
        ctx.client.list_queues().await.context("listing queues"),
        "queues",
    )?;

    let mut per_ws_used_slugs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut counts = QueueCounts { queues: 0, schemas: 0, inboxes: 0, conflicts: 0 };

    for q in &queues {
        let ws_url = match &q.workspace {
            Some(u) => u,
            None => {
                eprintln!(
                    "warning: skipping queue '{}' (id {}) — no workspace (orphan/hidden)",
                    q.name, q.id
                );
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

        // Record location so subsequent queue-nested drivers (email_templates)
        // can resolve queue URL → (ws_slug, q_slug).
        ctx.queue_locations.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));

        // 1. queue.json — three-way
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
        let q_recorded = apply_pull_action(q_action, &queue_path, &queue_proposed, q_remote_hash)?;
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

        // 2. schema.json — three-way (formula .py files always overwritten in M8).
        // If the queue has no schema URL, skip schema + inbox steps for this queue.
        let schema_url = match &q.schema {
            Some(u) => u,
            None => {
                eprintln!(
                    "warning: queue '{}' (id {}) has no schema — skipping schema + inbox",
                    q.name, q.id
                );
                continue;
            }
        };
        let schema_id = parse_id_from_url(schema_url)
            .with_context(|| format!("parsing schema URL '{}' for queue '{}'", schema_url, q.name))?;
        let schema = ctx
            .client
            .get_schema(schema_id)
            .await
            .with_context(|| format!("fetching schema {schema_id} for queue '{}'", q.name))?;

        // Combined-hash 3-way over schema.json + formulas/*.py (M9).
        let schema_path = queue_dir.join("schema.json");
        let pre_local_json = if schema_path.exists() {
            Some(std::fs::read(&schema_path)
                .with_context(|| format!("reading {}", schema_path.display()))?)
        } else {
            None
        };
        let pre_local_formulas = crate::snapshot::schema::read_local_formulas(&queue_dir)?;

        let (remote_json_bytes, remote_formulas) =
            crate::snapshot::schema::serialize_schema(&schema)?;
        // Strip overlay-managed paths from the schema JSON (M26).
        // Formulas (extracted to formulas/<id>.py) are unaffected.
        let remote_json_bytes = maybe_strip_overlay(
            remote_json_bytes,
            ctx.overlay.as_ref().and_then(|o| o.schema(&q_slug)),
        )?;
        let remote_combined_hash =
            crate::state::schema_combined_hash(&remote_json_bytes, &remote_formulas);

        let schema_base = ctx
            .lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(&q_slug))
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
                // Use the (possibly stripped) bytes computed above instead of
                // re-serializing the typed schema — overlay strip would be lost.
                crate::snapshot::schema::write_schema_bytes(
                    &queue_dir, &remote_json_bytes, &remote_formulas,
                )
                .with_context(|| format!("writing schema for queue '{}'", q.name))?;
                remote_combined_hash
            }
            PullAction::KeepLocal => {
                let local_json = pre_local_json.as_ref().unwrap();
                crate::state::schema_combined_hash(local_json, &pre_local_formulas)
            }
            PullAction::Conflict => {
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
                eprintln!(
                    "warning: {} conflict — local preserved, remote at {} (formulas at {})",
                    schema_path.display(),
                    queue_dir.join("schema.json.remote").display(),
                    queue_dir.join("formulas.remote").display()
                );
                counts.conflicts += 1;
                let local_json = pre_local_json.as_ref().unwrap();
                crate::state::schema_combined_hash(local_json, &pre_local_formulas)
            }
        };
        record_object(
            ctx.lockfile,
            "schemas",
            &q_slug,
            schema.id,
            Some(schema.url.clone()),
            schema.modified_at().map(|s| s.to_string()),
            Some(schema_recorded),
        );
        counts.schemas += 1;

        // 3. inbox.json — three-way (only if queue has inbox)
        if let Some(inbox_url) = &q.inbox {
            let inbox_id = parse_id_from_url(inbox_url)
                .with_context(|| format!("parsing inbox URL '{}' for queue '{}'", inbox_url, q.name))?;
            let inbox = ctx
                .client
                .get_inbox(inbox_id)
                .await
                .with_context(|| format!("fetching inbox {inbox_id} for queue '{}'", q.name))?;

            let inbox_path = queue_dir.join("inbox.json");
            let mut inbox_proposed = serde_json::to_vec_pretty(&inbox).context("serializing inbox")?;
            inbox_proposed.push(b'\n');
            let inbox_proposed = maybe_strip_overlay(
                inbox_proposed,
                ctx.overlay.as_ref().and_then(|o| o.inbox(&q_slug)),
            )?;
            let inbox_base = ctx
                .lockfile
                .objects
                .get("inboxes")
                .and_then(|m| m.get(&q_slug))
                .and_then(|e| e.content_hash.clone());
            let (i_action, i_remote_hash) =
                decide_pull_action(&inbox_path, inbox_base.as_deref(), &inbox_proposed)?;
            if i_action == PullAction::Conflict {
                counts.conflicts += 1;
            }
            let i_recorded = apply_pull_action(i_action, &inbox_path, &inbox_proposed, i_remote_hash)?;
            record_object(
                ctx.lockfile,
                "inboxes",
                &q_slug,
                inbox.id,
                Some(inbox.url.clone()),
                inbox.modified_at().map(|s| s.to_string()),
                Some(i_recorded),
            );
            counts.inboxes += 1;
        }
    }

    Ok(counts)
}
