use crate::api::{RossumClient, anyhow_has_status};
use crate::cli::pull::common::maybe_strip_overlay;
use crate::log::{Action, Log};
use crate::overlay::{Overlay, apply_overrides};
use crate::paths::Paths;

use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::{strip_for_create, strip_patch_extra};
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
    let mut remote_cache: Option<Vec<crate::model::Engine>> = None;

    for (slug, path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.engine(slug));

        // Missing lockfile entry → new engine, POST.
        if lockfile
            .objects
            .get("engines")
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
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "engines");
            let create_result = client
                .create_engine(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /engines (creating '{slug}')"));
            let created = create_result?;
            // Canonical on-disk bytes via KindCodec: redacts `agenda_id` and
            // strips hidden fields — matching exactly what pull produces.
            let codec = crate::snapshot::codec::codec("engines").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created engine")?)
                .context("codec disk_bytes for created engine")?;
            let created_bytes = maybe_strip_overlay(created_art.json, overlay_paths)?;
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars, lockfile);
            write_atomic(path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            lockfile.upsert(
                "engines",
                slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("engine/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        let disk_bytes =
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let entry = lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get(slug.as_str()))
            .unwrap();
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
        crate::snapshot::refs::resolve_value(&mut payload, lockfile);
        let payload_engine: crate::model::Engine = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied engine '{slug}'"))?;

        let id = entry.id;
        if remote_cache.is_none() {
            remote_cache = Some(
                client
                    .list_engines(Some(progress.clone()))
                    .await
                    .context("listing engines to verify no drift before push")?,
            );
        }
        let Some(remote_engine) = remote_cache.as_ref().unwrap().iter().find(|e| e.id == id) else {
            progress.event(
                Action::Skip,
                &format!("engine/{slug} (remote id {id} missing)"),
            );
            skipped += 1;
            continue;
        };
        // Drift compare: use the same KindCodec path the pull driver uses so
        // the hash matches the lockfile baseline (redacts `agenda_id`, strips
        // hidden fields).
        let codec = crate::snapshot::codec::codec("engines").unwrap();
        let remote_art = codec
            .disk_bytes(
                &serde_json::to_value(remote_engine)
                    .context("serializing remote engine for drift check")?,
            )
            .context("codec disk_bytes for remote engine")?;
        let remote_bytes = maybe_strip_overlay(remote_art.json, overlay_paths)?;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
        let mut payload_to_send = payload_engine;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited engine '{slug}'"))?;
                        crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                        payload_to_send = serde_json::from_value(ov)
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
                            modified_at: remote_engine.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.event(
                        Action::Warn,
                        &format!("engine/{slug} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("engine/{slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        // A PATCH must not echo server-managed fields. `agenda_id` is a
        // read-only, per-env identifier that Rossum refreshes on training;
        // echoing the redacted sentinel back is ignored at best and
        // overwrites/400s the engine's identifier at worst. Strip it (and the
        // other server fields) off `extra`, matching the CREATE contract.
        strip_patch_extra(&mut payload_to_send.extra, "engines", false);
        let patch_result = client
            .update_engine(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /engines/{id}"));
        let updated = match patch_result {
            Ok(u) => u,
            Err(e) if anyhow_has_status(&e, 405) => {
                progress.event(
                    Action::Skip,
                    &format!("engine/{slug} (PATCH 405 — engines read-only on this plan)"),
                );
                skipped += 1;
                break;
            }
            Err(e) => {
                return Err(e);
            }
        };

        // Post-PATCH disk write via KindCodec so `agenda_id` is redacted to
        // the sentinel (bug c fix: previously used raw to_vec_pretty, which
        // re-emitted the live agenda_id into engine.json after each PATCH).
        let codec = crate::snapshot::codec::codec("engines").unwrap();
        let updated_art = codec
            .disk_bytes(
                &serde_json::to_value(&updated)
                    .context("serializing updated engine for disk write")?,
            )
            .context("codec disk_bytes for updated engine")?;
        let updated_bytes = maybe_strip_overlay(updated_art.json, overlay_paths)?;
        let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
        crate::state::base_cache::write_disk_and_cache(paths, path, &updated_bytes)
            .with_context(|| format!("writing post-push canonical form for engine '{slug}'"))?;

        lockfile.upsert(
            "engines",
            slug,
            ObjectEntry {
                id: updated.id,
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
