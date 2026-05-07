use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::queue::{read_queue, write_queue};
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

/// Push locally-edited queues to the Rossum API. Walks every queue directory
/// under `envs/<env>/workspaces/<ws>/queues/<q>/queue.json`. Drift detection
/// uses the lockfile's plain `content_hash` (no formula complexity — queues
/// are a simple JSON shape). Returns `(pushed, skipped)`.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Queue> =
        std::collections::HashMap::new();

    for ws_entry in std::fs::read_dir(&workspaces_dir)
        .with_context(|| format!("reading {}", workspaces_dir.display()))?
    {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }

        for q_entry in std::fs::read_dir(&queues_dir)
            .with_context(|| format!("reading {}", queues_dir.display()))?
        {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            let queue_dir = paths.queue_dir(&ws_slug, &q_slug);
            let queue_path = queue_dir.join("queue.json");
            if !queue_path.exists() {
                continue;
            }

            let local_queue = read_queue(&queue_dir)
                .with_context(|| format!("reading local queue '{q_slug}'"))?;

            let mut payload = serde_json::to_value(&local_queue)
                .context("serializing local queue to value")?;
            if let Some(ov) = &overlay {
                if let Some(queue_overrides) = ov.queue(&q_slug) {
                    apply_overrides(&mut payload, queue_overrides);
                }
            }
            let payload_queue: crate::model::Queue = serde_json::from_value(payload)
                .with_context(|| format!("re-deserializing overlay-applied queue '{q_slug}'"))?;

            let mut post_overlay_bytes = serde_json::to_vec_pretty(&payload_queue)
                .context("serializing queue")?;
            post_overlay_bytes.push(b'\n');
            let local_combined = content_hash(&post_overlay_bytes);

            let entry = lockfile.objects.get("queues").and_then(|m| m.get(&q_slug));
            let Some(entry) = entry else {
                eprintln!("warning: queue '{q_slug}' — no lockfile entry, skipping");
                skipped += 1;
                continue;
            };
            let Some(base) = &entry.content_hash else {
                eprintln!("warning: queue '{q_slug}' — lockfile entry has no content_hash, skipping");
                skipped += 1;
                continue;
            };
            if &local_combined == base {
                continue;
            }

            let id = entry.id;
            // Pre-push drift verification: list once, find by id.
            if remote_cache.is_empty() {
                let remotes = client.list_queues().await
                    .context("listing queues to verify no drift before push")?;
                for r in remotes {
                    remote_cache.insert(r.id, r);
                }
            }
            let Some(remote_queue) = remote_cache.get(&id).cloned() else {
                eprintln!("warning: queue '{q_slug}' — id {id} not found on remote, skipping");
                skipped += 1;
                continue;
            };
            let mut remote_bytes = serde_json::to_vec_pretty(&remote_queue)
                .context("serializing remote queue")?;
            remote_bytes.push(b'\n');
            let remote_combined = content_hash(&remote_bytes);
            if &remote_combined != base {
                eprintln!(
                    "warning: queue '{q_slug}' — remote has changed since last pull, skipping push (run `rdc pull` first)"
                );
                skipped += 1;
                continue;
            }

            let updated = client.update_queue(id, &payload_queue).await
                .with_context(|| format!("PATCH /queues/{id}"))?;

            // Refresh on-disk file with the canonical server response.
            write_queue(&queue_dir, &updated)
                .with_context(|| format!("writing post-push canonical form for queue '{q_slug}'"))?;

            let mut updated_bytes = serde_json::to_vec_pretty(&updated)
                .context("serializing updated queue")?;
            updated_bytes.push(b'\n');
            let updated_hash = content_hash(&updated_bytes);

            lockfile.upsert(
                "queues",
                &q_slug,
                ObjectEntry {
                    id: updated.id,
                    url: Some(updated.url.clone()),
                    modified_at: updated.modified_at().map(|s| s.to_string()),
                    content_hash: Some(updated_hash),
                },
            );
            pushed += 1;
        }
    }

    Ok((pushed, skipped))
}
