use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::log::{Action, Log};
use crate::overlay::{Overlay, apply_overrides};
use crate::paths::Paths;

use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::strip_for_create;
use crate::snapshot::writer::write_atomic;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let mut remote_labels: Option<Vec<crate::model::Label>> = None;

    for (slug, path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.label(slug));

        // Missing lockfile entry → new label, POST.
        if lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get(slug.as_str()))
            .is_none()
        {
            let disk_bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "labels");
            let result = client
                .create_label(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /labels (creating '{slug}')"));
            let created = result?;
            let codec = crate::snapshot::codec::codec("labels").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created label")?)
                .context("codec disk_bytes for created label")?;
            let created_bytes = maybe_strip_overlay(created_art.json, overlay_paths)?;
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars);
            write_atomic(path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            lockfile.upsert(
                "labels",
                slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("label/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        let disk_bytes =
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let entry = lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get(slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("label/{slug} (no content_hash)"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let id = entry.id;

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_label: crate::model::Label = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied label '{slug}'"))?;

        if remote_labels.is_none() {
            remote_labels = Some(
                client
                    .list_labels(Some(progress.clone()))
                    .await
                    .context("listing labels to verify no drift before push")?,
            );
        }
        let remote_list = remote_labels.as_ref().unwrap();
        let Some(remote_label) = remote_list.iter().find(|l| l.id == id) else {
            progress.event(
                Action::Skip,
                &format!("label/{slug} (remote id {id} missing)"),
            );
            skipped += 1;
            continue;
        };
        let codec = crate::snapshot::codec::codec("labels").unwrap();
        let remote_art = codec
            .disk_bytes(
                &serde_json::to_value(remote_label)
                    .context("serializing remote label for drift check")?,
            )
            .context("codec disk_bytes for remote label")?;
        let remote_bytes = maybe_strip_overlay(remote_art.json, overlay_paths)?;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars);
        let mut payload_to_send = payload_label;
        if remote_combined != base {
            // Drift detected. Spec §7.3 step 5: prompt on TTY; fall back
            // to legacy skip+warn otherwise.
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, path, &remote_bytes, env)? {
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
                            secrets_hash: None,
                        },
                    );
                    progress.event(
                        Action::Warn,
                        &format!("label/{slug} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("label/{slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        let result = client
            .update_label(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /labels/{id}"));
        let updated = result?;

        let codec = crate::snapshot::codec::codec("labels").unwrap();
        let updated_art = codec
            .disk_bytes(
                &serde_json::to_value(&updated)
                    .context("serializing updated label for disk write")?,
            )
            .context("codec disk_bytes for updated label")?;
        let updated_bytes = maybe_strip_overlay(updated_art.json, overlay_paths)?;
        let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars);
        crate::state::base_cache::write_disk_and_cache(paths, path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;

        lockfile.upsert(
            "labels",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("label/{slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
