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

    // Drift bodies, keyed by SCHEMA ID and owned here so the whole push shares
    // them. Two queue slugs can point at one shared schema, and the old
    // sequential loop's cache made that pair cost a single `GET /schemas/{id}`
    // for the entire push — across a create barrier included. Keeping the cache
    // at this level keeps exactly that; see [`push_update_batch`], which fills
    // it by DISTINCT id before each run.
    let mut remote_cache: std::collections::HashMap<u64, crate::model::Schema> =
        std::collections::HashMap::new();

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. The two are NOT partitioned into "all creates, then all
    // updates": a schema's refs are resolved against the lockfile AS IT STANDS
    // when that schema is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that used to stay unresolved,
    // and change what this command sends (`push::hooks` carries an observable
    // instance of exactly that). So `changes` is still walked in slug order and
    // each MAXIMAL RUN of consecutive updates is fanned out, with a create
    // acting as a barrier.
    //
    // A DUPLICATE SCHEMA ID is a barrier too, and only this driver needs one:
    // two queue slugs can resolve to ONE schema, so both would PATCH
    // `/schemas/{id}` inside the same fanned-out run — concurrently, leaving the
    // final server state as whichever request the server happened to apply
    // last. The sequential loop was deterministic (slug order, last slug wins).
    // `batch_ids` tracks the ids already accumulated so the second slug closes
    // the run and starts its own, restoring that ordering by construction. It
    // is cleared wherever `batch` is drained, and the ids it holds cannot go
    // stale: the only thing that mutates the lockfile mid-walk is a create,
    // which flushes first.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    let mut batch_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for (q_slug, schema_path) in changes {
        // Missing lockfile entry → new schema, POST.
        if lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(q_slug.as_str()))
            .is_none()
        {
            // Close the pending run first: every update sorting BEFORE this
            // create must be prepared against a lockfile that does not yet
            // know the id this POST is about to assign.
            let (batched_pushed, batched_skipped) = push_update_batch(
                paths,
                client,
                lockfile,
                interactive,
                &mut batch,
                &mut remote_cache,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;
            batch_ids.clear();

            // queue_dir is the parent of schema.json
            let queue_dir = schema_path
                .parent()
                .with_context(|| format!("schema path has no parent: {}", schema_path.display()))?;
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

        // Duplicate-id barrier (see the note above `batch`). Close the pending
        // run so this slug's PATCH is strictly AFTER the PATCH of the earlier
        // slug that shares its schema, exactly as the sequential loop ordered
        // them. The drift GET is not repeated: `remote_cache` is owned by this
        // function, so the new run finds the id already cached.
        let id = lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(q_slug.as_str()))
            .map(|e| e.id)
            .expect("not a create, so the entry exists");
        if !batch_ids.insert(id) {
            let (batched_pushed, batched_skipped) = push_update_batch(
                paths,
                client,
                lockfile,
                interactive,
                &mut batch,
                &mut remote_cache,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;
            batch_ids.clear();
            batch_ids.insert(id);
        }

        batch.push((q_slug, schema_path));
    }

    // Flush the trailing run.
    let (batched_pushed, batched_skipped) = push_update_batch(
        paths,
        client,
        lockfile,
        interactive,
        &mut batch,
        &mut remote_cache,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// Fan out one maximal run of consecutive schema UPDATES, then apply the results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem and every prompt. `batch` is drained.
///
/// There is no whole-kind list to hoist here — the drift check is a
/// `GET /schemas/{id}` per item. But unlike `workspaces`/`inboxes`, TWO queue
/// slugs can resolve to ONE schema, so the GET cannot simply move inside each
/// item's future: that would re-fetch a shared schema once per slug. The
/// distinct ids are fetched concurrently into `remote_cache` first, and the
/// concurrent stage then only reads it. Same request count as the sequential
/// `remote_cache`, now overlapped instead of serialized.
#[allow(clippy::too_many_arguments)]
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    remote_cache: &mut std::collections::HashMap<u64, crate::model::Schema>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    use crate::cli::push::concurrent::{Prepared, prepare_all};

    let updates = std::mem::take(batch);
    if updates.is_empty() {
        return Ok((0, 0));
    }
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // Prefetch the drift bodies this run needs, by DISTINCT id and skipping
    // anything an earlier run already cached.
    //
    // Only ids that can actually REACH the drift check are collected: the old
    // lazy cache was populated by the first item that got past the
    // `content_hash` guard, so a run whose entries all lack a hash made no
    // request at all. `drift_base` is the single expression both this filter
    // and the per-item guard below go through, so the two cannot disagree — if
    // this filter were the narrower of the two, an item would find no cached
    // body and the `expect` below would fire.
    let mut ids: Vec<u64> = updates
        .iter()
        .filter_map(|(q_slug, _)| {
            lockfile
                .objects
                .get("schemas")
                .and_then(|m| m.get(q_slug.as_str()))
                .and_then(|e| drift_base(e).map(|_| e.id))
        })
        .filter(|id| !remote_cache.contains_key(id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if !ids.is_empty() {
        use futures::stream::{StreamExt, TryStreamExt};
        let fetched: Vec<(u64, crate::model::Schema)> = futures::stream::iter(ids)
            .map(|id| async move {
                let s = client
                    .get_schema(id, Some(progress.clone()))
                    .await
                    .with_context(|| format!("fetching schema {id} to verify drift before push"))?;
                Ok::<_, anyhow::Error>((id, s))
            })
            .buffered(crate::cli::push::concurrent::PUSH_FANOUT)
            .try_collect()
            .await?;
        remote_cache.extend(fetched);
    }
    // Immutable for the rest of the run: the concurrent stage reads it, and so
    // does `push_one_drifted`, which is why a drifted schema still costs no
    // extra request.
    let cache: &std::collections::HashMap<u64, crate::model::Schema> = &*remote_cache;

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let cache_ref = cache;
        prepare_all(updates.iter().copied(), |(q_slug, schema_path)| async move {
            // queue_dir is the parent of schema.json, computed before the
            // `content_hash` guard exactly as the old loop head did.
            let queue_dir = schema_path
                .parent()
                .with_context(|| format!("schema path has no parent: {}", schema_path.display()))?;
            let entry = lf
                .objects
                .get("schemas")
                .and_then(|m| m.get(q_slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: q_slug.clone(),
                    event: format!("schema/{q_slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            // Read raw Value (formulas spliced inline), deserialize for the
            // PATCH body.
            let mut payload = read_schema_value(queue_dir)
                .with_context(|| format!("reading local schema for queue '{q_slug}'"))?;
            crate::snapshot::refs::resolve_value(&mut payload, lf);
            let payload_schema: crate::model::Schema = serde_json::from_value(payload)
                .with_context(|| format!("deserializing schema '{q_slug}'"))?;

            let remote_schema = cache_ref
                .get(&id)
                .expect("every id that can reach the drift check was prefetched");
            let (remote_json, remote_formulas) = serialize_schema(remote_schema)?;
            if schema_combined_hash(&remote_json, &remote_formulas, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                return Ok(Prepared::NeedsPrompt {
                    slug: q_slug.clone(),
                });
            }

            // Strip server-managed fields from `extra` so the PATCH matches the
            // CREATE contract (e.g. the server-computed `queues` back-ref).
            let mut payload_to_send = payload_schema;
            strip_patch_extra(&mut payload_to_send.extra, "schemas", false);
            let updated = client
                .update_schema(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /schemas/{id}"))?;
            Ok(Prepared::Patched {
                slug: q_slug.clone(),
                updated,
            })
        })
        .await
    };

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem and every prompt. Every completed
    //     PATCH is recorded even if a sibling failed (spec D10), then the
    //     first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (slug_in, schema_path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one schema's response into another queue's directory.
        if let Ok(p) = &item {
            debug_assert_eq!(p.slug(), slug_in.as_str());
        }
        match item {
            // NOT `?`: by the time the apply stage runs, every clean PATCH in
            // the batch has already landed server-side. Returning early here
            // would leave the REMAINING items' completed PATCHes unrecorded —
            // the exact inconsistency D10 exists to shrink, and worse than the
            // old sequential loop, which never sent those requests at all.
            Ok(Prepared::Patched { slug, updated }) => {
                match write_back(paths, lockfile, &slug, schema_path, &updated) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("schema/{slug}"));
                        pushed += 1;
                    }
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }
            Ok(Prepared::Skipped { event, .. }) => {
                progress.event(Action::Skip, &event);
                skipped += 1;
            }
            // Deliberate (inherited from `push::rules`): this arm still runs
            // when an earlier item already failed. Suppressing the prompt once
            // `first_error` is set would leave a drifted item neither prompted
            // nor recorded. Also NOT `?`, for the same reason as above.
            Ok(Prepared::NeedsPrompt { slug }) => {
                match push_one_drifted(
                    paths,
                    client,
                    lockfile,
                    interactive,
                    &slug,
                    schema_path,
                    cache,
                    progress,
                    env,
                )
                .await
                {
                    Ok((p, s)) => {
                        pushed += p;
                        skipped += s;
                    }
                    Err(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_error {
        return Err(e);
    }

    Ok((pushed, skipped))
}

/// Whether this entry can reach the drift check at all — and, if so, the base
/// the remote is checked against.
///
/// The prefetch's id filter and the per-item guard inside the concurrent stage
/// MUST agree on this predicate. If the prefetch filter were ever narrower than
/// the per-item one, an item would find no prefetched body at all. One
/// expression, called from both, so they cannot drift apart.
fn drift_base(entry: &ObjectEntry) -> Option<&str> {
    entry.content_hash.as_deref()
}

/// Write one PATCH response back: canonical form (schema JSON + formula
/// sidecars) to disk and the base cache, plus the lockfile entry.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let (updated_json, updated_formulas) = serialize_schema(&updated)?;` down to
/// and including the `lockfile.upsert("schemas", ...)` call, with `updated`
/// taken by reference and the `progress.event(Action::Patch, ...)` line left
/// behind at the call site so the caller controls when it fires.
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    q_slug: &str,
    schema_path: &std::path::Path,
    updated: &crate::model::Schema,
) -> Result<()> {
    let queue_dir = schema_path
        .parent()
        .with_context(|| format!("schema path has no parent: {}", schema_path.display()))?;
    let (updated_json, updated_formulas) = serialize_schema(updated)?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the schema is lockfile-pinned, so self + `queues` resolve to rdc://).
    let updated_json = crate::cli::pull::common::portabilize_proposed(&updated_json, lockfile);
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
    Ok(())
}

/// Resolve one drifted schema interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local schema, `resolve_value`, `resolve_push_drift`, then either PATCH (via
/// the same `update_schema` + `write_back`), adopt the remote, or skip. It runs
/// only on the sequential stage, so `resolve_push_drift`'s prompt can never
/// interleave with another item's. `remote_cache` is the batch's prefetched
/// drift bodies, so this path costs no extra request. Returns
/// `(pushed, skipped)` deltas.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    q_slug: &str,
    schema_path: &std::path::Path,
    remote_cache: &std::collections::HashMap<u64, crate::model::Schema>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let queue_dir = schema_path
        .parent()
        .with_context(|| format!("schema path has no parent: {}", schema_path.display()))?;
    let entry = lockfile
        .objects
        .get("schemas")
        .and_then(|m| m.get(q_slug))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let mut payload = read_schema_value(queue_dir)
        .with_context(|| format!("reading local schema for queue '{q_slug}'"))?;
    crate::snapshot::refs::resolve_value(&mut payload, lockfile);
    let payload_schema: crate::model::Schema = serde_json::from_value(payload)
        .with_context(|| format!("deserializing schema '{q_slug}'"))?;

    let remote_schema = remote_cache
        .get(&id)
        .expect("the concurrent stage only reports drift for a prefetched id");
    let (remote_json, remote_formulas) = serialize_schema(remote_schema)?;
    let remote_combined = schema_combined_hash(&remote_json, &remote_formulas, lockfile);
    let mut payload_to_send = payload_schema;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(
        interactive,
        crate::cli::resolve::ObjectRef { kind: "schemas", slug: q_slug },
        schema_path, &remote_json,
        env,
        progress,
    )? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
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
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("schema/{q_slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
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

    write_back(paths, lockfile, q_slug, schema_path, &updated)?;
    progress.event(Action::Patch, &format!("schema/{q_slug}"));
    Ok((1, 0))
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

    /// Seed schemas that are UPDATES: local `schema.json`, lockfile entry and a
    /// recorded base computed from the remote body the server will hand back.
    /// `slugs` pairs each queue slug with its schema id, so two slugs can be
    /// given the SAME id — which is exactly the dedup case.
    fn seed_schemas(
        tmp: &tempfile::TempDir,
        api: &str,
        slugs: &[(&str, u64)],
    ) -> (
        Paths,
        Lockfile,
        BTreeMap<String, std::path::PathBuf>,
        Vec<serde_json::Value>,
    ) {
        let paths = Paths::for_env(tmp.path(), "dev");
        let mut lockfile = Lockfile {
            api_base: api.to_string(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        // Two passes: register every id BEFORE any base is hashed, so the
        // reverse id→slug map the hash canonicalizer uses is already stable
        // (it matters when two slugs share one id).
        for (slug, id) in slugs {
            lockfile.upsert(
                "schemas",
                slug,
                ObjectEntry {
                    id: *id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
        }
        let mut remotes = Vec::new();
        for (slug, id) in slugs {
            let queue_dir = paths.env_root().join(format!("workspaces/w/queues/{slug}"));
            std::fs::create_dir_all(&queue_dir).unwrap();
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://schemas/{slug}"),
                "queues": [],
                "content": []
            });
            std::fs::write(
                queue_dir.join("schema.json"),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            // Keyed by ID, not by slug: when two queue slugs share one schema
            // there is only ONE remote body, and both slugs' recorded bases
            // must be that body's hash.
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/schemas/{id}"),
                "name": format!("schema-{id}"),
                "queues": [],
                "content": []
            });
            let remote_schema: crate::model::Schema =
                serde_json::from_value(remote.clone()).unwrap();
            let (rj, rc) = serialize_schema(&remote_schema).unwrap();
            let base = schema_combined_hash(&rj, &rc, &lockfile);
            lockfile.upsert(
                "schemas",
                slug,
                ObjectEntry {
                    id: *id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), queue_dir.join("schema.json"));
            remotes.push(remote);
        }
        (paths, lockfile, changes, remotes)
    }

    /// Write a local schema with NO lockfile entry, so the driver treats it as
    /// a CREATE, and register it in `changes`.
    fn seed_create(
        paths: &Paths,
        changes: &mut BTreeMap<String, std::path::PathBuf>,
        slug: &str,
    ) {
        let queue_dir = paths.env_root().join(format!("workspaces/w/queues/{slug}"));
        std::fs::create_dir_all(&queue_dir).unwrap();
        let body = serde_json::json!({
            "name": slug,
            "url": format!("rdc://schemas/{slug}"),
            "queues": [],
            "content": []
        });
        std::fs::write(
            queue_dir.join("schema.json"),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();
        changes.insert(slug.to_string(), queue_dir.join("schema.json"));
    }

    async fn mount_get_and_patch(
        server: &MockServer,
        id: u64,
        get_body: serde_json::Value,
        patch_body: serde_json::Value,
        delay: std::time::Duration,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/schemas/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(get_body)
                    .set_delay(delay),
            )
            .mount(server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v1/schemas/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(patch_body)
                    .set_delay(delay),
            )
            .mount(server)
            .await;
    }

    /// Spec D9: the per-item drift GET plus its PATCH is two round trips per
    /// slug. Four schemas at 150ms per call cost ~1.2s in series; overlapped
    /// they cost roughly one slug's worth.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_schemas_overlaps_the_per_item_drift_get_and_patch() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, remotes) = seed_schemas(
            &tmp,
            &api,
            &[("q-a", 800), ("q-b", 801), ("q-c", 802), ("q-d", 803)],
        );
        for (i, remote) in remotes.iter().enumerate() {
            mount_get_and_patch(
                &server,
                800 + i as u64,
                remote.clone(),
                remote.clone(),
                std::time::Duration::from_millis(150),
            )
            .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "the four GET+PATCH pairs must overlap; sequential would be >= 1.2s, took {elapsed:?}",
        );
    }

    /// Request-count parity, and the property most likely to regress: the
    /// sequential loop cached drift GETs BY SCHEMA ID, so two queue slugs
    /// pointing at one shared schema paid exactly ONE `GET /schemas/{id}`.
    /// Fanning the GET out inside each item's future would re-fetch that
    /// schema once per slug. Prefetching the DISTINCT ids keeps the count.
    ///
    /// It also pins the ORDER of the two PATCHes, which the fan-out would
    /// otherwise leave to the server: both slugs write the same schema id, so
    /// the last writer decides the final remote state. The duplicate-id barrier
    /// makes `q-b` (last in `BTreeMap` order) the last writer deterministically,
    /// exactly as the sequential loop did. The two local bodies differ by
    /// `name`, so the received request bodies say which went first.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_schemas_fetches_a_shared_schema_exactly_once() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        // Both queue slugs resolve to schema 800.
        let (paths, mut lockfile, changes, remotes) =
            seed_schemas(&tmp, &api, &[("q-a", 800), ("q-b", 800)]);
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/800"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remotes[0].clone()))
            .mount(&server)
            .await;
        // A slow PATCH turns "were these serialized?" into wall-clock evidence:
        // two 200ms PATCHes cost ~400ms back-to-back and ~200ms overlapped.
        Mock::given(method("PATCH"))
            .and(path("/api/v1/schemas/800"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(remotes[0].clone())
                    .set_delay(std::time::Duration::from_millis(200)),
            )
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        let elapsed = start.elapsed();
        assert_eq!((pushed, skipped), (2, 0));
        assert!(
            elapsed >= std::time::Duration::from_millis(350),
            "the two PATCHes to the SAME schema id must not overlap — whichever \
             the server applied last would decide the remote state; took {elapsed:?}",
        );

        // Label each PATCH with the `name` it carried, so the stream shows not
        // just how many requests went out but WHICH body won.
        let seq: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let line = format!("{} {}", r.method, r.url.path());
                if r.method == "PATCH" {
                    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                    format!("{line} name={}", body["name"].as_str().unwrap())
                } else {
                    line
                }
            })
            .collect();
        assert_eq!(
            seq,
            vec![
                "GET /api/v1/schemas/800".to_string(),
                "PATCH /api/v1/schemas/800 name=q-a".to_string(),
                "PATCH /api/v1/schemas/800 name=q-b".to_string(),
            ],
            "a schema shared by two slugs must cost exactly one drift GET, and \
             its two PATCHes must be SERIALIZED in slug order so the last \
             writer is deterministic: {seq:?}",
        );
    }

    /// The dedup must survive a create barrier too. The drift cache is owned by
    /// [`push`], not by one batch, so `q-a` (before the create) and `q-z`
    /// (after it) — both pointing at schema 800 — still share the single GET
    /// the sequential loop's whole-push `remote_cache` gave them.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_schemas_shares_the_drift_cache_across_a_create_barrier() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, mut changes, remotes) =
            seed_schemas(&tmp, &api, &[("q-a", 800), ("q-z", 800)]);
        seed_create(&paths, &mut changes, "q-m");
        mount_get_and_patch(
            &server,
            800,
            remotes[0].clone(),
            remotes[0].clone(),
            std::time::Duration::ZERO,
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/schemas"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 899,
                "url": format!("{api}/schemas/899"),
                "name": "q-m",
                "queues": [],
                "content": []
            })))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!((pushed, skipped), (3, 0));

        let seq: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        assert_eq!(
            seq,
            vec![
                "GET /api/v1/schemas/800".to_string(),
                "PATCH /api/v1/schemas/800".to_string(),
                "POST /api/v1/schemas".to_string(),
                "PATCH /api/v1/schemas/800".to_string(),
            ],
            "the update before the create is sent first, the trailing run is \
             still flushed, and the shared schema is fetched only once: {seq:?}",
        );
    }

    /// Request-count parity: the old lazy `remote_cache` was populated by the
    /// first update that got PAST the `content_hash` guard, so a push in which
    /// every entry lacks a hash made NO request at all. The prefetch must keep
    /// that exactly.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_schemas_makes_no_request_when_no_update_has_a_content_hash() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, _remotes) =
            seed_schemas(&tmp, &api, &[("q-a", 800), ("q-b", 801)]);
        // Strip the recorded base, keeping the ids: each is still an UPDATE
        // but neither can reach the drift check. No mocks are mounted, so any
        // request would both fail and show up in `received_requests`.
        for (slug, id) in [("q-a", 800u64), ("q-b", 801)] {
            lockfile.upsert(
                "schemas",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!((pushed, skipped), (0, 2), "both schemas are skipped");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no update can reach the drift check, so nothing may be requested",
        );
    }

    /// Spec D9: a drifted item is never PATCHed on the concurrent path, and the
    /// sequential drift pass reads the same prefetched body — so a drifted
    /// schema still costs exactly ONE `GET /schemas/{id}`.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_schemas_never_patches_a_drifted_item_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, remotes) =
            seed_schemas(&tmp, &api, &[("q-a", 800), ("q-b", 801), ("q-c", 802)]);
        for (i, remote) in remotes.iter().enumerate() {
            let id = 800 + i as u64;
            let get_body = if i == 1 {
                let mut drifted = remote.clone();
                drifted["name"] = serde_json::json!("changed remotely");
                drifted
            } else {
                remote.clone()
            };
            mount_get_and_patch(
                &server,
                id,
                get_body,
                remote.clone(),
                std::time::Duration::ZERO,
            )
            .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!((pushed, skipped), (2, 1), "the drifted schema is skipped");

        let seq: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        assert!(
            !seq.contains(&"PATCH /api/v1/schemas/801".to_string()),
            "the drifted schema must never be PATCHed, saw {seq:?}"
        );
        assert_eq!(
            seq.iter().filter(|r| *r == "GET /api/v1/schemas/801").count(),
            1,
            "the drift prompt must reuse the prefetched body: {seq:?}"
        );
    }
}
