use super::common::{
    apply_pull_action, decide_pull_action, hash_for_lockfile, parse_id_from_url,
    record_object, PullAction, PullCtx,
};
use crate::slug::slugify_unique;
use crate::snapshot::schema::write_schema;
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
    let queues = ctx
        .client
        .list_queues()
        .await
        .context("listing queues")?;

    let mut per_ws_used_slugs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut counts = QueueCounts { queues: 0, schemas: 0, inboxes: 0, conflicts: 0 };

    for q in &queues {
        let ws_slug = match ctx.lockfile.slug_for_url("workspaces", &q.workspace) {
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

        // 1. queue.json — three-way
        let queue_path = queue_dir.join("queue.json");
        let mut queue_proposed = serde_json::to_vec_pretty(q).context("serializing queue")?;
        queue_proposed.push(b'\n');
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

        // 2. schema.json — three-way (formula .py files always overwritten in M8)
        let schema_id = parse_id_from_url(&q.schema)
            .with_context(|| format!("parsing schema URL '{}' for queue '{}'", q.schema, q.name))?;
        let schema = ctx
            .client
            .get_schema(schema_id)
            .await
            .with_context(|| format!("fetching schema {schema_id} for queue '{}'", q.name))?;

        let schema_path = queue_dir.join("schema.json");
        let pre_schema_local = if schema_path.exists() {
            Some(std::fs::read(&schema_path)
                .with_context(|| format!("reading {}", schema_path.display()))?)
        } else {
            None
        };
        let schema_proposed_bytes = write_schema(&queue_dir, &schema)
            .with_context(|| format!("writing schema for queue '{}'", q.name))?;
        let schema_remote_hash = hash_for_lockfile(&schema_proposed_bytes);
        let schema_base = ctx
            .lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(&q_slug))
            .and_then(|e| e.content_hash.clone());
        let s_action = match (schema_base.as_deref(), &pre_schema_local) {
            (None, _) => PullAction::Write,
            (_, None) => PullAction::Write,
            (Some(base), Some(local)) => {
                let local_hash = hash_for_lockfile(local);
                let local_matches = local_hash == base;
                let remote_matches = schema_remote_hash == base;
                match (local_matches, remote_matches) {
                    (true, _) => PullAction::Write,
                    (false, true) => PullAction::KeepLocal,
                    (false, false) => PullAction::Conflict,
                }
            }
        };
        let schema_recorded = match s_action {
            PullAction::Write => schema_remote_hash,
            PullAction::KeepLocal => {
                let local = pre_schema_local.as_ref().unwrap();
                crate::snapshot::writer::write_atomic(&schema_path, local)?;
                hash_for_lockfile(local)
            }
            PullAction::Conflict => {
                let local = pre_schema_local.as_ref().unwrap();
                crate::snapshot::writer::write_atomic(&schema_path, local)?;
                let remote_path = queue_dir.join("schema.json.remote");
                crate::snapshot::writer::write_atomic(&remote_path, &schema_proposed_bytes)?;
                eprintln!(
                    "warning: {} conflict — local preserved, remote at {}",
                    schema_path.display(),
                    remote_path.display()
                );
                counts.conflicts += 1;
                hash_for_lockfile(local)
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
