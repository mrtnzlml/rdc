use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::snapshot::schema::{read_schema_value, serialize_schema, write_schema_bytes};
use crate::state::{schema_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Push locally-edited schemas. Iterates the pre-computed change list (from
/// phase 1 scan). Each entry's path is the schema.json file; the queue dir
/// is derived as path.parent(). Drift-checks remote post-strip, and PATCHes.
/// Post-PATCH disk write is also stripped so the snapshot matches
/// lockfile.content_hash.
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
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Schema> =
        std::collections::HashMap::new();

    for (q_slug, schema_path) in changes {
        // queue_dir is the parent of schema.json
        let queue_dir = schema_path.parent()
            .with_context(|| format!("schema path has no parent: {}", schema_path.display()))?;

        let entry = lockfile.objects.get("schemas").and_then(|m| m.get(q_slug.as_str()));
        let Some(entry) = entry else {
            progress.println(format!("warning: schema for queue '{q_slug}' — no lockfile entry, skipping"));
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            progress.println(format!("warning: schema for queue '{q_slug}' — lockfile entry has no content_hash, skipping"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        // Read raw Value (formulas spliced inline), apply overlay,
        // deserialize for the PATCH body.
        let mut payload = read_schema_value(queue_dir)
            .with_context(|| format!("reading local schema for queue '{q_slug}'"))?;
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.schema(q_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_schema: crate::model::Schema = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied schema '{q_slug}'"))?;

        let id = entry.id;
        let remote_schema = if let Some(s) = remote_cache.get(&id) {
            s.clone()
        } else {
            let s = client.get_schema(id, Some(progress.clone())).await
                .with_context(|| format!("fetching schema {id} to verify drift before push"))?;
            remote_cache.insert(id, s.clone());
            s
        };
        let (remote_json, remote_formulas) = serialize_schema(&remote_schema)?;
        let remote_json = maybe_strip_overlay(remote_json, overlay_paths)?;
        let remote_combined = schema_combined_hash(&remote_json, &remote_formulas);
        let mut payload_to_send = payload_schema;
        if &remote_combined != &base {
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, schema_path, &remote_json)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited schema for queue '{q_slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    // Schema is a combined-hash kind — adopt both
                    // the JSON and every formula from remote.
                    write_schema_bytes(queue_dir, &remote_json, &remote_formulas)
                        .with_context(|| format!("adopting remote schema for queue '{q_slug}'"))?;
                    lockfile.upsert(
                        "schemas",
                        q_slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_schema.url.clone()),
                            modified_at: remote_schema.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                        },
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.println(format!(
                        "warning: schema for queue '{q_slug}' — remote has changed since last pull, skipping push (run `rdc pull` first)"
                    ));
                    skipped += 1;
                    continue;
                }
            }
        }

        let updated = client.update_schema(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /schemas/{id}"))?;

        let (updated_json, updated_formulas) = serialize_schema(&updated)?;
        let updated_json = maybe_strip_overlay(updated_json, overlay_paths)?;
        let updated_hash = schema_combined_hash(&updated_json, &updated_formulas);
        write_schema_bytes(queue_dir, &updated_json, &updated_formulas)
            .with_context(|| format!("writing post-push canonical form for schema '{q_slug}'"))?;

        lockfile.upsert(
            "schemas",
            q_slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
            },
        );
        progress.tick(q_slug.as_str());
        pushed += 1;
    }

    Ok((pushed, skipped))
}
