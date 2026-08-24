use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::snapshot::create::{strip_for_create, strip_patch_extra};
use crate::snapshot::schema::{
    read_schema_value, serialize_schema, write_schema_bytes, write_schema_bytes_with_cache,
};
use crate::state::{Lockfile, ObjectEntry, schema_combined_hash};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Push locally-edited schemas. Iterates the pre-computed change list (from
/// phase 1 scan). Each entry's path is the schema.json file; the queue dir
/// is derived as path.parent(). Drift-checks the remote's canonical
/// on-disk form, and PATCHes. The post-PATCH disk write is the same
/// canonical form so the snapshot matches lockfile.content_hash.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Schema> =
        std::collections::HashMap::new();

    for (q_slug, schema_path) in changes {
        // queue_dir is the parent of schema.json
        let queue_dir = schema_path
            .parent()
            .with_context(|| format!("schema path has no parent: {}", schema_path.display()))?;

        // Missing lockfile entry → new schema, POST.
        if lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(q_slug.as_str()))
            .is_none()
        {
            let mut payload = read_schema_value(queue_dir)
                .with_context(|| format!("reading local schema for queue '{q_slug}' to create"))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "schemas");
            let create_result = client
                .create_schema(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /schemas (creating for queue '{q_slug}')"));
            let created = create_result?;
            let (created_json, created_formulas) = serialize_schema(&created)?;
            // Register the new schema's id NOW so its own `url` (and its `queues`
            // back-ref) portabilizes to `rdc://`. Concrete env URLs must never
            // touch disk, even transiently (an interrupted sync whose portabilize
            // post-pass never runs would freeze them into the snapshot).
            lockfile.upsert(
                "schemas",
                q_slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let created_json =
                crate::cli::pull::common::portabilize_proposed(&created_json, lockfile);
            let created_hash = schema_combined_hash(&created_json, &created_formulas, lockfile);
            write_schema_bytes(queue_dir, &created_json, &created_formulas).with_context(|| {
                format!("writing post-create canonical form for schema '{q_slug}'")
            })?;
            lockfile.upsert(
                "schemas",
                q_slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("schema/{q_slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        let entry = lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(q_slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("schema/{q_slug} (no content_hash)"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        // Read raw Value (formulas spliced inline), deserialize for the
        // PATCH body.
        let mut payload = read_schema_value(queue_dir)
            .with_context(|| format!("reading local schema for queue '{q_slug}'"))?;
        crate::snapshot::refs::resolve_value(&mut payload, lockfile);
        let payload_schema: crate::model::Schema = serde_json::from_value(payload)
            .with_context(|| format!("deserializing schema '{q_slug}'"))?;

        let id = entry.id;
        let remote_schema = if let Some(s) = remote_cache.get(&id) {
            s.clone()
        } else {
            let s = client
                .get_schema(id, Some(progress.clone()))
                .await
                .with_context(|| format!("fetching schema {id} to verify drift before push"))?;
            remote_cache.insert(id, s.clone());
            s
        };
        let (remote_json, remote_formulas) = serialize_schema(&remote_schema)?;
        let remote_combined = schema_combined_hash(&remote_json, &remote_formulas, lockfile);
        let mut payload_to_send = payload_schema;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, schema_path, &remote_json, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                            .with_context(|| {
                                format!("re-deserializing edited schema for queue '{q_slug}'")
                            })?;
                        crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                        payload_to_send = serde_json::from_value(ov).with_context(|| {
                            format!("re-deserializing edited schema for queue '{q_slug}'")
                        })?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    // Schema is a combined-hash kind — adopt both
                    // the JSON and every formula from remote. Portabilize first
                    // so concrete env URLs never land on disk (schema is pinned).
                    let remote_json =
                        crate::cli::pull::common::portabilize_proposed(&remote_json, lockfile);
                    write_schema_bytes(queue_dir, &remote_json, &remote_formulas)
                        .with_context(|| format!("adopting remote schema for queue '{q_slug}'"))?;
                    lockfile.upsert(
                        "schemas",
                        q_slug,
                        ObjectEntry {
                            id,
                            modified_at: remote_schema.modified_at().map(|s| s.to_string()),
                            modified_by: remote_schema.modified_by().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.event(
                        Action::Warn,
                        &format!("schema/{q_slug} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("schema/{q_slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        // Strip server-managed fields from `extra` so the PATCH matches the
        // CREATE contract (e.g. the server-computed `queues` back-ref).
        strip_patch_extra(&mut payload_to_send.extra, "schemas", false);
        let patch_result = client
            .update_schema(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /schemas/{id}"));
        let updated = patch_result?;

        let (updated_json, updated_formulas) = serialize_schema(&updated)?;
        // Re-portabilize the server response so concrete env URLs never land on
        // disk (the schema is lockfile-pinned, so self + `queues` resolve to rdc://).
        let updated_json =
            crate::cli::pull::common::portabilize_proposed(&updated_json, lockfile);
        let updated_hash = schema_combined_hash(&updated_json, &updated_formulas, lockfile);
        // Mirror the schema JSON + formula sidecars into the base cache (matching
        // the pull path) so a later `BothDiverged` conflict merges against a
        // current base instead of a stale/absent one.
        write_schema_bytes_with_cache(queue_dir, &updated_json, &updated_formulas, Some(paths))
            .with_context(|| format!("writing post-push canonical form for schema '{q_slug}'"))?;

        lockfile.upsert(
            "schemas",
            q_slug,
            ObjectEntry {
                id: updated.id,
                modified_at: updated.modified_at().map(|s| s.to_string()),
                modified_by: updated.modified_by().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("schema/{q_slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Regression: after a schema PATCH push, both the `schema.json` and every
    /// formula sidecar (`formulas/<field_id>.py`) must be mirrored into the base
    /// cache — exactly as the pull path does via `write_schema_bytes_with_cache`
    /// — so a later `BothDiverged` conflict merges against a current base.
    /// Before the fix the schema push used the no-cache `write_schema_bytes`
    /// (and `_paths` was unused), leaving the base cache stale/absent.
    #[tokio::test]
    async fn push_patch_schema_caches_json_and_formulas_to_base() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let queue_dir = paths.env_root().join("workspaces/w/queues/q");
        std::fs::create_dir_all(&queue_dir).unwrap();

        // Local schema.json (formula extracted to formulas/f.py, portable url).
        let local = serde_json::json!({
            "name": "My Schema",
            "url": "rdc://schemas/q",
            "queues": [],
            "content": [ { "category": "datapoint", "id": "f" } ]
        });
        std::fs::write(
            queue_dir.join("schema.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();
        std::fs::write(queue_dir.join("formulas/f.py"), b"1 + 1\n").unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "schemas",
            "q",
            ObjectEntry {
                id: 800,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // Remote schema carries the inline formula (extracted on serialize).
        let remote = serde_json::json!({
            "id": 800,
            "url": format!("{api}/schemas/800"),
            "name": "My Schema",
            "queues": [],
            "content": [ { "category": "datapoint", "id": "f", "formula": "1 + 1\n" } ]
        });
        let remote_schema: crate::model::Schema = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_schema(&remote_schema).unwrap();
        let base = schema_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "schemas",
            "q",
            ObjectEntry {
                id: 800,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/800"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote.clone()))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/schemas/800"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut changes = BTreeMap::new();
        changes.insert("q".to_string(), queue_dir.join("schema.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!(pushed, 1, "the schema should be patched");

        // Both the schema JSON and the formula sidecar must be in the base cache.
        let base_json = crate::state::base_cache::cache_mirror(&paths, &queue_dir.join("schema.json"))
            .expect("schema path under env root");
        assert!(
            base_json.exists(),
            "base cache must contain schema.json:\n{}",
            base_json.display()
        );
        let base_formula =
            crate::state::base_cache::cache_mirror(&paths, &queue_dir.join("formulas/f.py"))
                .expect("formula path under env root");
        assert!(
            base_formula.exists(),
            "base cache must contain the formula sidecar:\n{}",
            base_formula.display()
        );
        assert_eq!(
            std::fs::read_to_string(&base_formula).unwrap(),
            "1 + 1\n",
            "base cached formula must match the pushed formula"
        );
    }
}
