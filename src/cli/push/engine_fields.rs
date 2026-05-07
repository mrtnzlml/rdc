use crate::api::{anyhow_has_status, RossumClient};
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

/// Push locally-edited engine fields. Flat layout under
/// `envs/<env>/engine-fields/`. Returns `(pushed, skipped)`.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let kind_dir = paths.engine_fields_dir();
    if !kind_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: Option<Vec<crate::model::EngineField>> = None;

    let entries: Vec<_> = std::fs::read_dir(&kind_dir)
        .with_context(|| format!("reading {}", kind_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", kind_dir.display()))?;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }
        let path = kind_dir.join(format!("{slug}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let local: crate::model::EngineField = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        let mut payload = serde_json::to_value(&local).context("serializing local engine field")?;
        if let Some(ov) = &overlay {
            if let Some(overrides) = ov.engine_field(slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_field: crate::model::EngineField = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied engine field '{slug}'"))?;

        let mut post_overlay_bytes = serde_json::to_vec_pretty(&payload_field)
            .context("serializing engine field")?;
        post_overlay_bytes.push(b'\n');
        let local_combined = content_hash(&post_overlay_bytes);

        let entry = lockfile.objects.get("engine_fields").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: engine-fields/{slug}.json — no lockfile entry, skipping");
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            eprintln!("warning: engine-fields/{slug}.json — lockfile has no content_hash, skipping");
            skipped += 1;
            continue;
        };
        if &local_combined == base {
            continue;
        }

        let id = entry.id;
        if remote_cache.is_none() {
            remote_cache = Some(client.list_engine_fields().await
                .context("listing engine fields to verify no drift before push")?);
        }
        let Some(remote_field) = remote_cache.as_ref().unwrap().iter().find(|f| f.id == id) else {
            eprintln!("warning: engine-fields/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_field)
            .context("serializing remote engine field")?;
        remote_bytes.push(b'\n');
        let remote_combined = content_hash(&remote_bytes);
        if &remote_combined != base {
            eprintln!(
                "warning: engine-fields/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
            );
            skipped += 1;
            continue;
        }

        let updated = match client.update_engine_field(id, &payload_field).await
            .with_context(|| format!("PATCH /engine_fields/{id}"))
        {
            Ok(u) => u,
            Err(e) if anyhow_has_status(&e, 405) => {
                eprintln!(
                    "warning: engine fields are not writable via PATCH on this Rossum org/plan (405 Method Not Allowed). Skipping all engine field pushes."
                );
                skipped += 1;
                break;
            }
            Err(e) => return Err(e),
        };

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated engine field")?;
        updated_bytes.push(b'\n');
        let updated_hash = content_hash(&updated_bytes);
        write_atomic(&path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for engine field '{slug}'"))?;

        lockfile.upsert(
            "engine_fields",
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
