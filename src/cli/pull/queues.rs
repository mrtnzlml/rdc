use super::common::{hash_for_lockfile, parse_id_from_url, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::inbox::write_inbox;
use crate::snapshot::queue::write_queue;
use crate::snapshot::schema::write_schema;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

/// Counts of objects pulled by the queues driver. Each queue contributes one
/// queue + one schema + 0 or 1 inboxes.
pub struct QueueCounts {
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
}

/// Pull all queues, plus for each queue: its schema (with formula extraction)
/// and its optional inbox. Queues whose workspace was filtered out (i.e., not
/// present in the lockfile under "workspaces") are skipped silently.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<QueueCounts> {
    let queues = ctx
        .client
        .list_queues()
        .await
        .context("listing queues")?;

    // Per-workspace slug pools so queue slugs are unique within a workspace.
    let mut per_ws_used_slugs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut counts = QueueCounts { queues: 0, schemas: 0, inboxes: 0 };

    for q in &queues {
        let ws_slug = match ctx
            .lockfile
            .slug_for_url("workspaces", &q.workspace)
        {
            Some(s) => s.to_string(),
            None => continue, // workspace was filtered out or not yet pulled; skip queue
        };

        let used = per_ws_used_slugs.entry(ws_slug.clone()).or_default();
        let q_slug = slugify_unique(&q.name, used);
        used.insert(q_slug.clone());

        let queue_dir = ctx.paths.queue_dir(&ws_slug, &q_slug);
        std::fs::create_dir_all(&queue_dir)
            .with_context(|| format!("creating {}", queue_dir.display()))?;

        let bytes = write_queue(&queue_dir, q)
            .with_context(|| format!("writing queue '{}' to disk", q.name))?;
        let hash = hash_for_lockfile(&bytes);
        record_object(
            ctx.lockfile,
            "queues",
            &q_slug,
            q.id,
            Some(q.url.clone()),
            q.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
        counts.queues += 1;

        // Pull the queue's schema.
        let schema_id = parse_id_from_url(&q.schema)
            .with_context(|| format!("parsing schema URL '{}' for queue '{}'", q.schema, q.name))?;
        let schema = ctx
            .client
            .get_schema(schema_id)
            .await
            .with_context(|| format!("fetching schema {schema_id} for queue '{}'", q.name))?;
        let schema_bytes = write_schema(&queue_dir, &schema)
            .with_context(|| format!("writing schema for queue '{}'", q.name))?;
        let schema_hash = hash_for_lockfile(&schema_bytes);
        record_object(
            ctx.lockfile,
            "schemas",
            // Schemas don't have their own slug — they are 1:1 with queues.
            // Use the queue slug for symmetry; the file path makes it unambiguous.
            &q_slug,
            schema.id,
            Some(schema.url.clone()),
            schema.modified_at().map(|s| s.to_string()),
            Some(schema_hash),
        );
        counts.schemas += 1;

        // Pull the queue's inbox, if any.
        if let Some(inbox_url) = &q.inbox {
            let inbox_id = parse_id_from_url(inbox_url)
                .with_context(|| format!("parsing inbox URL '{}' for queue '{}'", inbox_url, q.name))?;
            let inbox = ctx
                .client
                .get_inbox(inbox_id)
                .await
                .with_context(|| format!("fetching inbox {inbox_id} for queue '{}'", q.name))?;
            let inbox_bytes = write_inbox(&queue_dir, &inbox)
                .with_context(|| format!("writing inbox for queue '{}'", q.name))?;
            let inbox_hash = hash_for_lockfile(&inbox_bytes);
            record_object(
                ctx.lockfile,
                "inboxes",
                &q_slug,
                inbox.id,
                Some(inbox.url.clone()),
                inbox.modified_at().map(|s| s.to_string()),
                Some(inbox_hash),
            );
            counts.inboxes += 1;
        }
    }

    Ok(counts)
}

