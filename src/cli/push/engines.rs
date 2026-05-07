use crate::api::{anyhow_has_status, RossumClient};
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let kind_dir = paths.engines_dir();
    if !kind_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: Option<Vec<crate::model::Engine>> = None;

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
        let disk_bytes = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let local_combined = content_hash(&disk_bytes);

        let entry = lockfile.objects.get("engines").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: engines/{slug}.json — no lockfile entry, skipping");
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            eprintln!("warning: engines/{slug}.json — lockfile has no content_hash, skipping");
            skipped += 1;
            continue;
        };
        if &local_combined == base {
            continue;
        }

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.engine(slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_engine: crate::model::Engine = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied engine '{slug}'"))?;

        let id = entry.id;
        if remote_cache.is_none() {
            remote_cache = Some(client.list_engines().await
                .context("listing engines to verify no drift before push")?);
        }
        let Some(remote_engine) = remote_cache.as_ref().unwrap().iter().find(|e| e.id == id) else {
            eprintln!("warning: engines/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_engine)
            .context("serializing remote engine")?;
        remote_bytes.push(b'\n');
        let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
        let remote_combined = content_hash(&remote_bytes);
        if &remote_combined != base {
            eprintln!(
                "warning: engines/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
            );
            skipped += 1;
            continue;
        }

        let updated = match client.update_engine(id, &payload_engine).await
            .with_context(|| format!("PATCH /engines/{id}"))
        {
            Ok(u) => u,
            Err(e) if anyhow_has_status(&e, 405) => {
                eprintln!(
                    "warning: engines are not writable via PATCH on this Rossum org/plan (405 Method Not Allowed). Skipping all engine pushes."
                );
                skipped += 1;
                break;
            }
            Err(e) => return Err(e),
        };

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated engine")?;
        updated_bytes.push(b'\n');
        let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
        let updated_hash = content_hash(&updated_bytes);
        write_atomic(&path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for engine '{slug}'"))?;

        lockfile.upsert(
            "engines",
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
