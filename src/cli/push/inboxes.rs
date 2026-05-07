use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::inbox::{read_inbox, write_inbox};
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

/// Push locally-edited inboxes. Inboxes are 1:1 with queues; the lockfile
/// keys them by queue slug. Walks every queue dir and looks for an
/// `inbox.json` file. Returns `(pushed, skipped)`.
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
            let inbox_path = queue_dir.join("inbox.json");
            if !inbox_path.exists() {
                continue;
            }

            let local_inbox = read_inbox(&queue_dir)
                .with_context(|| format!("reading local inbox for queue '{q_slug}'"))?;

            let mut payload = serde_json::to_value(&local_inbox)
                .context("serializing local inbox to value")?;
            if let Some(ov) = &overlay {
                if let Some(inbox_overrides) = ov.inbox(&q_slug) {
                    apply_overrides(&mut payload, inbox_overrides);
                }
            }
            let payload_inbox: crate::model::Inbox = serde_json::from_value(payload)
                .with_context(|| format!("re-deserializing overlay-applied inbox '{q_slug}'"))?;

            let mut post_overlay_bytes = serde_json::to_vec_pretty(&payload_inbox)
                .context("serializing inbox")?;
            post_overlay_bytes.push(b'\n');
            let local_combined = content_hash(&post_overlay_bytes);

            let entry = lockfile.objects.get("inboxes").and_then(|m| m.get(&q_slug));
            let Some(entry) = entry else {
                eprintln!("warning: inbox for queue '{q_slug}' — no lockfile entry, skipping");
                skipped += 1;
                continue;
            };
            let Some(base) = &entry.content_hash else {
                eprintln!("warning: inbox for queue '{q_slug}' — lockfile entry has no content_hash, skipping");
                skipped += 1;
                continue;
            };
            if &local_combined == base {
                continue;
            }

            let id = entry.id;
            let remote_inbox = client.get_inbox(id).await
                .with_context(|| format!("fetching inbox {id} to verify drift before push"))?;
            let mut remote_bytes = serde_json::to_vec_pretty(&remote_inbox)
                .context("serializing remote inbox")?;
            remote_bytes.push(b'\n');
            let remote_combined = content_hash(&remote_bytes);
            if &remote_combined != base {
                eprintln!(
                    "warning: inbox for queue '{q_slug}' — remote has changed since last pull, skipping push (run `rdc pull` first)"
                );
                skipped += 1;
                continue;
            }

            let updated = client.update_inbox(id, &payload_inbox).await
                .with_context(|| format!("PATCH /inboxes/{id}"))?;

            write_inbox(&queue_dir, &updated)
                .with_context(|| format!("writing post-push canonical form for inbox '{q_slug}'"))?;

            let mut updated_bytes = serde_json::to_vec_pretty(&updated)
                .context("serializing updated inbox")?;
            updated_bytes.push(b'\n');
            let updated_hash = content_hash(&updated_bytes);

            lockfile.upsert(
                "inboxes",
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
