use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::hook::{read_hook_value, serialize_hook, write_hook_code};
use crate::snapshot::writer::write_atomic;
use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let hooks_dir = paths.hooks_dir();
    if !hooks_dir.exists() {
        return Ok((0, 0));
    }

    // Load overlay if present (M11). With M26, overlay drives both the
    // outbound payload (apply_overrides) AND the strip applied to remote
    // bytes for hashing — so disk-bytes (already stripped) and post-strip
    // remote bytes can both be compared against `lockfile.content_hash`.
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", hooks_dir.display()))?;

    let mut remote_hooks: Option<Vec<crate::model::Hook>> = None;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }

        let local_json_path = hooks_dir.join(format!("{slug}.json"));
        let local_py_path = hooks_dir.join(format!("{slug}.py"));

        // Hash the on-disk bytes directly. Pull writes the stripped form,
        // so this hash matches `lockfile.content_hash` when the user
        // hasn't edited anything.
        let disk_json_bytes = std::fs::read(&local_json_path)
            .with_context(|| format!("reading {}", local_json_path.display()))?;
        let disk_code = if local_py_path.exists() {
            Some(std::fs::read_to_string(&local_py_path)
                .with_context(|| format!("reading {}", local_py_path.display()))?)
        } else {
            None
        };
        let local_combined = hook_combined_hash(&disk_json_bytes, &disk_code);

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
            continue;
        }

        let id = entry.id;

        // Read raw Value (with .py spliced in) so overlay can re-add fields
        // stripped by pull (M26 / spec §9.3) BEFORE typed deserialize.
        let mut payload = read_hook_value(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.hook(slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied hook '{slug}'"))?;

        // Drift check: fetch remote, serialize, strip same overlay paths,
        // hash. Compare to base (which was recorded post-strip on pull).
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
        let (remote_json_full, remote_code) = serialize_hook(remote_hook)?;
        let remote_json_stripped = maybe_strip_overlay(remote_json_full, overlay_paths)?;
        let remote_combined = hook_combined_hash(&remote_json_stripped, &remote_code);
        if &remote_combined != base {
            eprintln!(
                "warning: hooks/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
            );
            skipped += 1;
            continue;
        }

        let updated = client.update_hook(id, &payload_hook).await
            .with_context(|| format!("PATCH /hooks/{id}"))?;

        // Refresh local file with the post-strip canonical form (matches
        // what next pull would write) and update lockfile to match.
        let (updated_json_full, updated_code) = serialize_hook(&updated)?;
        let updated_json_stripped = maybe_strip_overlay(updated_json_full, overlay_paths)?;
        let updated_hash = hook_combined_hash(&updated_json_stripped, &updated_code);
        write_atomic(&local_json_path, &updated_json_stripped)
            .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
        if let Some(code) = &updated_code {
            write_hook_code(&hooks_dir, slug, code)
                .with_context(|| format!("writing hook code for '{slug}'"))?;
        }

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
