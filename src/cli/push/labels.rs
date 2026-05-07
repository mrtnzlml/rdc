use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let kind_dir = paths.labels_dir();
    if !kind_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&kind_dir)
        .with_context(|| format!("reading {}", kind_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", kind_dir.display()))?;

    let mut remote_labels: Option<Vec<crate::model::Label>> = None;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }

        let path = kind_dir.join(format!("{slug}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let local_label: crate::model::Label = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        let mut payload = serde_json::to_value(&local_label)
            .context("serializing local label to value")?;
        if let Some(ov) = &overlay {
            if let Some(label_overrides) = ov.label(slug) {
                apply_overrides(&mut payload, label_overrides);
            }
        }
        let payload_label: crate::model::Label = serde_json::from_value(payload.clone())
            .with_context(|| format!("re-deserializing overlay-applied label '{slug}'"))?;

        let mut post_overlay_bytes = serde_json::to_vec_pretty(&payload_label)
            .context("serializing label")?;
        post_overlay_bytes.push(b'\n');
        let local_combined = content_hash(&post_overlay_bytes);

        let entry = lockfile.objects.get("labels").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: labels/{slug}.json — no lockfile entry, skipping");
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            eprintln!("warning: labels/{slug}.json — lockfile entry has no content_hash, skipping");
            skipped += 1;
            continue;
        };

        if &local_combined == base {
            continue;
        }

        let id = entry.id;

        if remote_labels.is_none() {
            remote_labels = Some(client.list_labels().await
                .context("listing labels to verify no drift before push")?);
        }
        let remote_list = remote_labels.as_ref().unwrap();
        let Some(remote_label) = remote_list.iter().find(|l| l.id == id) else {
            eprintln!("warning: labels/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };

        let mut remote_bytes = serde_json::to_vec_pretty(remote_label)
            .context("serializing remote label")?;
        remote_bytes.push(b'\n');
        let remote_combined = content_hash(&remote_bytes);

        if &remote_combined != base {
            eprintln!("warning: labels/{slug}.json — remote has changed since last pull, skipping push");
            skipped += 1;
            continue;
        }

        let updated = client.update_label(id, &payload_label).await
            .with_context(|| format!("PATCH /labels/{id}"))?;

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated label")?;
        updated_bytes.push(b'\n');
        let updated_hash = content_hash(&updated_bytes);
        lockfile.upsert(
            "labels",
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
