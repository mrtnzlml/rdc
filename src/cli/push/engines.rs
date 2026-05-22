use crate::api::{anyhow_has_status, RossumClient};
use crate::cli::pull::common::maybe_strip_overlay;
use crate::log::{Action, Log};
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;

use crate::snapshot::create::strip_for_create;
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
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: Option<Vec<crate::model::Engine>> = None;

    for (slug, path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.engine(slug));

        // Missing lockfile entry → new engine, POST.
        if lockfile.objects.get("engines").and_then(|m| m.get(slug.as_str())).is_none() {
            let disk_bytes = std::fs::read(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "engines");
            let create_result = client.create_engine(&payload, Some(progress.clone())).await
                .with_context(|| format!("POST /engines (creating '{slug}')"));
            let created = create_result?;
            let mut created_bytes = serde_json::to_vec_pretty(&created)
                .context("serializing created engine")?;
            created_bytes.push(b'\n');
            let created_bytes = maybe_strip_overlay(created_bytes, overlay_paths)?;
            let created_hash = content_hash(&created_bytes);
            write_atomic(path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            lockfile.upsert(
                "engines",
                slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("engine/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        let disk_bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let entry = lockfile.objects.get("engines").and_then(|m| m.get(slug.as_str())).unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("engine/{slug} (no content_hash)"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_engine: crate::model::Engine = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied engine '{slug}'"))?;

        let id = entry.id;
        if remote_cache.is_none() {
            remote_cache = Some(client.list_engines(Some(progress.clone())).await
                .context("listing engines to verify no drift before push")?);
        }
        let Some(remote_engine) = remote_cache.as_ref().unwrap().iter().find(|e| e.id == id) else {
            progress.event(Action::Skip, &format!("engine/{slug} (remote id {id} missing)"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_engine)
            .context("serializing remote engine")?;
        remote_bytes.push(b'\n');
        let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
        let remote_combined = content_hash(&remote_bytes);
        let mut payload_to_send = payload_engine;
        if &remote_combined != &base {
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited engine '{slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(path, &remote_bytes)
                        .with_context(|| format!("adopting remote into {}", path.display()))?;
                    lockfile.upsert(
                        "engines",
                        slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_engine.url.clone()),
                            modified_at: remote_engine.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.event(Action::Warn, &format!("engine/{slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(Action::Skip, &format!("engine/{slug} (remote changed; rdc sync first)"));
                    skipped += 1;
                    continue;
                }
            }
        }

        let patch_result = client.update_engine(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /engines/{id}"));
        let updated = match patch_result {
            Ok(u) => u,
            Err(e) if anyhow_has_status(&e, 405) => {
                progress.event(Action::Skip, &format!("engine/{slug} (PATCH 405 — engines read-only on this plan)"));
                skipped += 1;
                break;
            }
            Err(e) => {
                return Err(e);
            }
        };

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated engine")?;
        updated_bytes.push(b'\n');
        let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
        let updated_hash = content_hash(&updated_bytes);
        write_atomic(path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for engine '{slug}'"))?;

        lockfile.upsert(
            "engines",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("engine/{slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
