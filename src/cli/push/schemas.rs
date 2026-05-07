use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::schema::{read_schema, serialize_schema, write_schema};
use crate::state::{schema_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

/// Push locally-edited schemas to the Rossum API.
///
/// Schemas live nested under `envs/<env>/workspaces/<ws>/queues/<q>/schema.json`
/// alongside the per-formula `formulas/<field_id>.py` files. The driver walks
/// every queue directory under the env, splices formulas back into the
/// schema's `content[]` tree (via `read_schema`), and PATCHes the result.
///
/// The combined hash (schema.json + sorted formulas) is the merge base,
/// matching the M9 pull-side algorithm. Returns `(pushed, skipped)`.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Schema> =
        std::collections::HashMap::new();

    for ws_entry in std::fs::read_dir(&workspaces_dir)
        .with_context(|| format!("reading {}", workspaces_dir.display()))?
    {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }

        for q_entry in std::fs::read_dir(&queues_dir)
            .with_context(|| format!("reading {}", queues_dir.display()))?
        {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            let queue_dir = paths.queue_dir(&ws_slug, &q_slug);
            let schema_path = queue_dir.join("schema.json");
            if !schema_path.exists() {
                continue;
            }

            // Read full schema (formulas spliced back inline).
            let local_schema = read_schema(&queue_dir)
                .with_context(|| format!("reading local schema for queue '{q_slug}'"))?;

            // Apply overlay (if any) to a JSON Value, then re-deserialize.
            let mut payload = serde_json::to_value(&local_schema)
                .context("serializing local schema to value")?;
            if let Some(ov) = &overlay {
                if let Some(schema_overrides) = ov.schema(&q_slug) {
                    apply_overrides(&mut payload, schema_overrides);
                }
            }
            let payload_schema: crate::model::Schema = serde_json::from_value(payload)
                .with_context(|| format!("re-deserializing overlay-applied schema '{q_slug}'"))?;

            let (post_overlay_json, post_overlay_formulas) = serialize_schema(&payload_schema)?;
            let local_combined = schema_combined_hash(&post_overlay_json, &post_overlay_formulas);

            let entry = lockfile.objects.get("schemas").and_then(|m| m.get(&q_slug));
            let Some(entry) = entry else {
                eprintln!("warning: schema for queue '{q_slug}' — no lockfile entry, skipping (creates not supported)");
                skipped += 1;
                continue;
            };
            let Some(base) = &entry.content_hash else {
                eprintln!("warning: schema for queue '{q_slug}' — lockfile entry has no content_hash, skipping");
                skipped += 1;
                continue;
            };

            if &local_combined == base {
                continue;
            }

            let id = entry.id;

            let remote_schema = if let Some(s) = remote_cache.get(&id) {
                s.clone()
            } else {
                let s = client.get_schema(id).await
                    .with_context(|| format!("fetching schema {id} to verify drift before push"))?;
                remote_cache.insert(id, s.clone());
                s
            };
            let (remote_json, remote_formulas) = serialize_schema(&remote_schema)?;
            let remote_combined = schema_combined_hash(&remote_json, &remote_formulas);

            if &remote_combined != base {
                eprintln!(
                    "warning: schema for queue '{q_slug}' — remote has changed since last pull, skipping push (run `rdc pull` first)"
                );
                skipped += 1;
                continue;
            }

            let updated = client.update_schema(id, &payload_schema).await
                .with_context(|| format!("PATCH /schemas/{id}"))?;

            // Write canonical form back so the local snapshot matches the
            // server response immediately (mirrors hooks/rules/labels post-push refresh).
            write_schema(&queue_dir, &updated)
                .with_context(|| format!("writing post-push canonical form for schema '{q_slug}'"))?;

            let (updated_json, updated_formulas) = serialize_schema(&updated)?;
            let updated_hash = schema_combined_hash(&updated_json, &updated_formulas);

            lockfile.upsert(
                "schemas",
                &q_slug,
                ObjectEntry {
                    id: updated.id,
                    url: Some(updated.url.clone()),
                    modified_at: updated.modified_at().map(|s| s.to_string()),
                    content_hash: Some(updated_hash),
                },
            );
            pushed += 1;
        }
    }

    Ok((pushed, skipped))
}
