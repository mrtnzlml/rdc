use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<OverallProgress>,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let mut remote_labels: Option<Vec<crate::model::Label>> = None;

    for (slug, path) in changes {
        let disk_bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let entry = lockfile.objects.get("labels").and_then(|m| m.get(slug.as_str()));
        let Some(entry) = entry else {
            progress.println(format!("warning: labels/{slug}.json — no lockfile entry, skipping"));
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            progress.println(format!("warning: labels/{slug}.json — lockfile entry has no content_hash, skipping"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let id = entry.id;

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.label(slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_label: crate::model::Label = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied label '{slug}'"))?;

        if remote_labels.is_none() {
            remote_labels = Some(client.list_labels(Some(progress.clone())).await
                .context("listing labels to verify no drift before push")?);
        }
        let remote_list = remote_labels.as_ref().unwrap();
        let Some(remote_label) = remote_list.iter().find(|l| l.id == id) else {
            progress.println(format!("warning: labels/{slug}.json — id {id} not found on remote, skipping"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_label)
            .context("serializing remote label")?;
        remote_bytes.push(b'\n');
        let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
        let remote_combined = content_hash(&remote_bytes);
        let mut payload_to_send = payload_label;
        if &remote_combined != &base {
            // Drift detected. Spec §7.3 step 5: prompt on TTY; fall back
            // to legacy skip+warn otherwise.
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, path, &remote_bytes)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited label '{slug}'"))?;
                    }
                    // Fall through to PATCH below.
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(path, &remote_bytes)
                        .with_context(|| format!("adopting remote into {}", path.display()))?;
                    lockfile.upsert(
                        "labels",
                        slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_label.url.clone()),
                            modified_at: remote_label.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                        },
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.println(format!("warning: labels/{slug}.json — remote has changed since last pull, skipping push"));
                    skipped += 1;
                    continue;
                }
            }
        }

        let updated = client.update_label(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /labels/{id}"))?;

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated label")?;
        updated_bytes.push(b'\n');
        let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
        let updated_hash = content_hash(&updated_bytes);
        write_atomic(path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;

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
        progress.tick(slug.as_str());
        pushed += 1;
    }

    Ok((pushed, skipped))
}
