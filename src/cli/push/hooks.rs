use crate::api::RossumClient;
use crate::paths::Paths;
use crate::snapshot::hook::{read_hook, serialize_hook};
use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

/// Push locally-edited hooks to the remote.
/// Returns `(pushed, skipped_due_to_conflict)`.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let hooks_dir = paths.hooks_dir();
    if !hooks_dir.exists() {
        return Ok((0, 0));
    }

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", hooks_dir.display()))?;

    // Lazy-fetch remote list (only if we find a hook with local edits).
    let mut remote_hooks: Option<Vec<crate::model::Hook>> = None;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }

        let local_hook = read_hook(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;

        let (local_json, local_code) = serialize_hook(&local_hook)?;
        let local_combined = hook_combined_hash(&local_json, &local_code);

        let entry = lockfile.objects.get("hooks").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: hooks/{slug}.json — no lockfile entry, skipping (creates not supported in M10)");
            skipped += 1;
            continue;
        };

        let Some(base) = &entry.content_hash else {
            eprintln!("warning: hooks/{slug}.json — lockfile entry has no content_hash, skipping");
            skipped += 1;
            continue;
        };

        if &local_combined == base {
            // No local edits.
            continue;
        }

        let id = entry.id;

        // Lazy-fetch remote.
        if remote_hooks.is_none() {
            remote_hooks = Some(
                client.list_hooks().await
                    .context("listing hooks to verify no drift before push")?,
            );
        }
        let remote_list = remote_hooks.as_ref().unwrap();
        let Some(remote_hook) = remote_list.iter().find(|h| h.id == id) else {
            eprintln!("warning: hooks/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };

        let (remote_json, remote_code) = serialize_hook(remote_hook)?;
        let remote_combined = hook_combined_hash(&remote_json, &remote_code);

        if &remote_combined != base {
            eprintln!(
                "warning: hooks/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
            );
            skipped += 1;
            continue;
        }

        let updated = client.update_hook(id, &local_hook).await
            .with_context(|| format!("PATCH /hooks/{id}"))?;

        let (updated_json, updated_code) = serialize_hook(&updated)?;
        let updated_hash = hook_combined_hash(&updated_json, &updated_code);
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
            },
        );
        pushed += 1;
    }

    Ok((pushed, skipped))
}
