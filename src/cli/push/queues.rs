use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::{strip_for_create, strip_patch_extra};
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
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // Drift-check list, fetched at most ONCE for the whole push and owned here
    // so every run shares the single request. Populated lazily by the first run
    // that actually has an item able to reach the drift check, so a push whose
    // updates all lack a `content_hash` still makes no list call at all — see
    // [`push_update_batch`].
    //
    // `Option<HashMap>`, not the old bare `HashMap` with an `is_empty()`
    // sentinel: an organization with no queues at all made that sentinel
    // re-issue `GET /queues` for every single item. "Fetched and empty" and
    // "not fetched" are different states and the type now says so.
    let mut drift_queues: Option<std::collections::HashMap<u64, crate::model::Queue>> = None;

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. The two are NOT partitioned into "all creates, then all
    // updates": a queue's refs are resolved against the lockfile AS IT STANDS
    // when that queue is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that had to DEFER, collapsing
    // the documented push-PATCH + relink-PATCH pair into one PATCH with a
    // different body (`push::hooks` carries an observable instance of exactly
    // that). So `changes` is still walked in slug order and each MAXIMAL RUN of
    // consecutive updates is fanned out, with a create acting as a barrier.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (q_slug, queue_path) in changes {

        // Missing lockfile entry → new queue, POST. User must already have
        // POSTed the referenced workspace + schema (linear push); if not,
        // the server will reject with a clear error.
        if lockfile
            .objects
            .get("queues")
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
                &mut drift_queues,
                relink,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;

            let disk_bytes = std::fs::read(queue_path)
                .with_context(|| format!("reading {}", queue_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", queue_path.display()))?;
            let mut deferred =
                crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
            // A queue's `workspace`/`schema` are mandatory links that cannot be
            // deferred: dropping one from the body does not postpone it, it
            // sends a create the API refuses (`400 schema: This field is
            // required.`) — and it refuses it *after* the workspaces and
            // schemas phases have already written to the env. Restoring the
            // still-`rdc://` value hands the pre-send guard
            // (`api::ensure_no_residual_refs`) a ref to name, so the failure
            // says which reference is dangling instead of which key the server
            // wanted. The two UPDATE paths have always done this; only the
            // create path did not, which is why the one case that reaches the
            // wire unresolvable is the one that reports worst.
            crate::cli::push::relink::restore_undeferrable("queues", &mut payload, &mut deferred);
            strip_for_create(&mut payload, "queues");
            let create_result = client
                .create_queue(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /queues (creating '{q_slug}')"));
            let created = create_result?;
            // Canonical on-disk bytes via KindCodec: redacts `counts` and
            // strips hidden fields — matching exactly what pull produces.
            let codec = crate::snapshot::codec::codec("queues").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created queue")?)
                .context("codec disk_bytes for created queue")?;
            // Register the new queue's id NOW so its own `url` (and any ref to
            // an already-created object) portabilizes to `rdc://`. Concrete env
            // URLs must never touch disk, even transiently (an interrupted sync
            // whose portabilize post-pass never runs would freeze them in).
            lockfile.upsert(
                "queues",
                q_slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let created_bytes =
                crate::cli::pull::common::portabilize_proposed(&created_art.json, lockfile);
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, queue_path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{q_slug}'"))?;
            lockfile.upsert(
                "queues",
                q_slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            if !deferred.is_empty() {
                relink.push(crate::cli::push::relink::DeferredRelink {
                    kind: "queues".to_string(),
                    slug: q_slug.clone(),
                    path: queue_path.clone(),
                    fields: deferred,
                });
            }
            progress.event(Action::Post, &format!("queue/{q_slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        batch.push((q_slug, queue_path));
    }

    // Flush the trailing run.
    let (batched_pushed, batched_skipped) = push_update_batch(
        paths,
        client,
        lockfile,
        interactive,
        &mut batch,
        &mut drift_queues,
        relink,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// What one queue's concurrent stage carries across to its apply stage.
///
/// `deferred` holds the fields `resolve_value_deferring` held BACK from the
/// PATCH; it rides along rather than being recomputed on the sequential side,
/// because recomputing would need the local file re-read and re-resolved
/// against a lockfile that later items have since mutated.
struct QueuePatched {
    updated: crate::model::Queue,
    deferred: Vec<(String, serde_json::Value)>,
}

/// Fan out one maximal run of consecutive queue UPDATES, then apply the results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem, `relink` and every prompt. `batch` is
/// drained.
///
/// `drift_queues` is the caller's one-per-push cache of the fresh queue list, so
/// several runs still cost a single `GET /queues` — and a push whose updates all
/// lack a `content_hash` still costs none.
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    drift_queues: &mut Option<std::collections::HashMap<u64, crate::model::Queue>>,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
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

    // Drift-check list, hoisted to ONE fetch before the batch — but only when
    // at least one update can actually reach the drift check. The old lazy
    // `remote_cache` was populated by the first item that got PAST the
    // `content_hash` guard, so a run of entries that all lack a hash made no
    // list call at all; keep that exactly, and keep it caller-owned so several
    // runs share the single fetch.
    let needs_drift_check = updates.iter().any(|(slug, _)| {
        lockfile
            .objects
            .get("queues")
            .and_then(|m| m.get(slug.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if drift_queues.is_none() && needs_drift_check {
        let remotes = client
            .list_queues(Some(progress.clone()))
            .await
            .context("listing queues to verify no drift before push")?;
        *drift_queues = Some(remotes.into_iter().map(|r| (r.id, r)).collect());
    }
    // Empty only when nothing in this run can consult it: an entry with no
    // `content_hash` returns `Prepared::Skipped` before the list is ever
    // touched, and by construction that is then every entry in the run.
    let empty = std::collections::HashMap::new();
    let remote_queues: &std::collections::HashMap<u64, crate::model::Queue> =
        drift_queues.as_ref().unwrap_or(&empty);

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref = remote_queues;
        prepare_all(updates.iter().copied(), |(q_slug, queue_path)| async move {
            // Read BEFORE the `content_hash` guard, exactly as the old
            // sequential loop did: an unreadable file is an error even for an
            // entry that would otherwise be skipped.
            let disk_bytes = std::fs::read(queue_path)
                .with_context(|| format!("reading {}", queue_path.display()))?;
            let entry = lf
                .objects
                .get("queues")
                .and_then(|m| m.get(q_slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: q_slug.clone(),
                    event: format!("queue/{q_slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", queue_path.display()))?;
            let mut deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lf);
            // `update_queue` sends a typed `Queue`: an absent `workspace`/`schema`/
            // `url` would go out as `null` (= "clear it"), so those never defer.
            crate::cli::push::relink::restore_undeferrable("queues", &mut payload, &mut deferred);
            let payload_queue: crate::model::Queue = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied queue '{q_slug}'"))?;

            let Some(remote_queue) = remote_ref.get(&id) else {
                return Ok(Prepared::Skipped {
                    slug: q_slug.clone(),
                    event: format!("queue/{q_slug} (remote id {id} missing)"),
                });
            };
            let remote_art = remote_artifact(remote_queue)?;
            if combined_hash(&remote_art.json, &remote_art.sidecars, lf) != base
                && !drift_is_server_derived_only(paths, queue_path, &remote_art.json, base, lf)
            {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                return Ok(Prepared::NeedsPrompt {
                    slug: q_slug.clone(),
                });
            }

            // Strip server-managed fields from `extra` so the PATCH matches the
            // CREATE contract. Critically, `rir_url` is a per-cluster internal
            // service URL the API 400s ("Invalid URL") if echoed back, and
            // `counts` is redacted to the sentinel on disk.
            let mut payload_to_send = payload_queue;
            strip_patch_extra(&mut payload_to_send.extra, "queues", false);
            let updated = client
                .update_queue(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /queues/{id}"))?;
            crate::cli::push::warn_ignored(
                progress,
                &format!("queue/{q_slug}"),
                &payload_to_send,
                remote_queue,
                &updated,
            );
            Ok(Prepared::Patched {
                slug: q_slug.clone(),
                updated: QueuePatched { updated, deferred },
            })
        })
        .await
    };

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem, `relink` and every prompt. Every
    //     completed PATCH is recorded even if a sibling failed (spec D10),
    //     then the first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (slug_in, queue_path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one queue's response over another queue's file.
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
                match write_back(paths, lockfile, relink, &slug, queue_path, updated) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("queue/{slug}"));
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
                    queue_path,
                    remote_queues,
                    relink,
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
/// The hoisted list fetch and the per-item guard inside the concurrent stage
/// MUST agree on this predicate. If the hoist guard were ever narrower than the
/// per-item one, an item would consult an empty map and silently take the
/// "remote id missing" skip instead of a real drift check — no error, just
/// wrong. One expression, called from both, so they cannot drift apart.
fn drift_base(entry: &ObjectEntry) -> Option<&str> {
    entry.content_hash.as_deref()
}

/// Whether the remote moved away from the recorded base ONLY in fields the
/// server derives and a PATCH never sends (`rules`, `hooks`, `webhooks`,
/// `inbox`, …; see `is_server_stripped`). Such drift cannot be clobbered by
/// this push, so it must not block it.
///
/// The common cause is this very sync: it deletes a rule or hook before it
/// patches the queues, and the server drops the child from `queue.rules` /
/// `queue.hooks`. Without this check a queue edited in the same cycle was
/// skipped as "remote changed", and the edit only reached the server on the
/// next sync.
///
/// Needs the base bytes, so it reads the base cache — and trusts it only when
/// it hashes to the lockfile's `base`. Without a trustworthy base it answers
/// `false` and the caller treats the drift as real.
fn drift_is_server_derived_only(
    paths: &Paths,
    queue_path: &std::path::Path,
    remote_json: &[u8],
    base: &str,
    lf: &Lockfile,
) -> bool {
    let Ok(Some(base_bytes)) = crate::state::base_cache::read(paths, queue_path) else {
        return false;
    };
    if combined_hash(&base_bytes, &[], lf) != base {
        return false;
    }
    let authored = |bytes: &[u8]| -> Option<serde_json::Value> {
        let canonical = crate::snapshot::noise::canonicalize_for_hash(bytes, lf);
        let mut v: serde_json::Value = serde_json::from_slice(&canonical).ok()?;
        v.as_object_mut()?
            .retain(|k, _| !crate::snapshot::create::is_server_stripped("queues", k));
        Some(v)
    };
    matches!((authored(&base_bytes), authored(remote_json)), (Some(a), Some(b)) if a == b)
}

/// The canonical on-disk artifact for a remote queue, as the drift check and
/// the drift prompt both need it. Lifted verbatim from the old loop's
/// `codec.disk_bytes(...)` block: the same KindCodec path the pull driver uses,
/// so the hash matches the lockfile baseline (redacts `counts`, strips hidden
/// fields).
fn remote_artifact(remote: &crate::model::Queue) -> Result<crate::snapshot::codec::DiskArtifact> {
    let codec = crate::snapshot::codec::codec("queues").unwrap();
    codec
        .disk_bytes(
            &serde_json::to_value(remote).context("serializing remote queue for drift check")?,
        )
        .context("codec disk_bytes for remote queue")
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// the lockfile entry, and the deferred-relink record.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let codec = ...` down to and including the `relink.push(...)`, with the
/// `progress.event(Action::Patch, ...)` line left behind at the call site so the
/// caller controls when it fires.
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    q_slug: &str,
    queue_path: &std::path::Path,
    patched: QueuePatched,
) -> Result<()> {
    let QueuePatched { updated, deferred } = patched;

    let codec = crate::snapshot::codec::codec("queues").unwrap();
    let updated_art = codec
        .disk_bytes(
            &serde_json::to_value(&updated).context("serializing updated queue for disk write")?,
        )
        .context("codec disk_bytes for updated queue")?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the queue is lockfile-pinned, so self + refs resolve to rdc://).
    let updated_bytes =
        crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
    let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, queue_path, &updated_bytes)
        .with_context(|| format!("writing post-push canonical form for queue '{q_slug}'"))?;

    lockfile.upsert(
        "queues",
        q_slug,
        ObjectEntry {
            id: updated.id,
            modified_at: updated.modified_at().map(|s| s.to_string()),
            modified_by: updated.modified_by().map(|s| s.to_string()),
            content_hash: Some(updated_hash),
            secrets_hash: None,
        },
    );
    if !deferred.is_empty() {
        relink.push(crate::cli::push::relink::DeferredRelink {
            kind: "queues".to_string(),
            slug: q_slug.to_string(),
            path: queue_path.to_path_buf(),
            fields: deferred,
        });
    }
    Ok(())
}

/// Resolve one drifted queue interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value_deferring` + `restore_undeferrable`,
/// `resolve_push_drift`, then either PATCH (via the same `update_queue` +
/// `write_back`), adopt the remote, or skip. It runs only on the sequential
/// stage, so `resolve_push_drift`'s prompt can never interleave with another
/// item's. Returns `(pushed, skipped)` deltas.
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    q_slug: &str,
    queue_path: &std::path::Path,
    remote_queues: &std::collections::HashMap<u64, crate::model::Queue>,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let entry = lockfile
        .objects
        .get("queues")
        .and_then(|m| m.get(q_slug))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let disk_bytes = std::fs::read(queue_path)
        .with_context(|| format!("reading {}", queue_path.display()))?;
    let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
        .with_context(|| format!("parsing {}", queue_path.display()))?;
    let mut deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
    // `update_queue` sends a typed `Queue`: an absent `workspace`/`schema`/
    // `url` would go out as `null` (= "clear it"), so those never defer.
    crate::cli::push::relink::restore_undeferrable("queues", &mut payload, &mut deferred);
    let payload_queue: crate::model::Queue = serde_json::from_value(payload)
        .with_context(|| format!("deserializing overlay-applied queue '{q_slug}'"))?;

    let Some(remote_queue) = remote_queues.get(&id).cloned() else {
        progress.event(
            Action::Skip,
            &format!("queue/{q_slug} (remote id {id} missing)"),
        );
        return Ok((0, 1));
    };
    let remote_art = remote_artifact(&remote_queue)?;
    let remote_bytes = remote_art.json;
    let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
    let mut payload_to_send = payload_queue;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(
        interactive,
        crate::cli::resolve::ObjectRef { kind: "queues", slug: q_slug },
        queue_path, &remote_bytes,
        env,
        progress,
    )? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited queue '{q_slug}'"))?;
                deferred = crate::snapshot::refs::resolve_value_deferring(&mut ov, lockfile);
                crate::cli::push::relink::restore_undeferrable("queues", &mut ov, &mut deferred);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited queue '{q_slug}'"))?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the queue is lockfile-pinned; refs resolve).
            let remote_bytes =
                crate::cli::pull::common::portabilize_proposed(&remote_bytes, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, queue_path, &remote_bytes)
                .with_context(|| format!("adopting remote into {}", queue_path.display()))?;
            lockfile.upsert(
                "queues",
                q_slug,
                ObjectEntry {
                    id,
                    modified_at: remote_queue.modified_at().map(|s| s.to_string()),
                    modified_by: remote_queue.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Warn,
                &format!("queue/{q_slug} adopted remote (drift)"),
            );
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("queue/{q_slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
        }
    }

    // Strip server-managed fields from `extra` so the PATCH matches the
    // CREATE contract (see the concurrent stage).
    strip_patch_extra(&mut payload_to_send.extra, "queues", false);
    let patch_result = client
        .update_queue(id, &payload_to_send, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /queues/{id}"));
    let updated = patch_result?;

    crate::cli::push::warn_ignored(
        progress,
        &format!("queue/{q_slug}"),
        &payload_to_send,
        &remote_queue,
        &updated,
    );
    write_back(
        paths,
        lockfile,
        relink,
        q_slug,
        queue_path,
        QueuePatched { updated, deferred },
    )?;
    progress.event(Action::Patch, &format!("queue/{q_slug}"));
    Ok((1, 0))
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolve_value_rewrites_rdc_refs_to_env_urls() {
        use crate::state::{Lockfile, ObjectEntry};
        // api_base is set so the env URL can be DERIVED from id (v3).
        let mut lf = Lockfile {
            api_base: "https://example.rossum.app/api/v1".to_string(),
            ..Lockfile::default()
        };
        lf.upsert(
            "workspaces",
            "main",
            ObjectEntry {
                id: 7,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let mut payload = serde_json::json!({ "name": "Q", "workspace": "rdc://workspaces/main" });
        crate::snapshot::refs::resolve_value(&mut payload, &lf);
        assert_eq!(
            payload["workspace"],
            "https://example.rossum.app/api/v1/workspaces/7"
        );
    }


    /// Spec D9: clean queue updates PATCH concurrently. Four queues whose
    /// PATCHes each take 200ms cost ~800ms in series and ~200-400ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_queues_patches_updates_concurrently() {
        use crate::paths::Paths;
        use crate::snapshot::codec::combined_hash;
        use crate::state::{Lockfile, ObjectEntry};
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // The owning workspace must be lockfile-pinned so `rdc://workspaces/main`
        // resolves on the way out and portabilizes on the way back in.
        lockfile.upsert(
            "workspaces",
            "main",
            ObjectEntry {
                id: 7,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let slugs = ["q-a", "q-b", "q-c", "q-d"];
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 800 + i as u64;
            let dir = paths.queue_dir("main", slug);
            std::fs::create_dir_all(&dir).unwrap();
            let local = serde_json::json!({
                "url": format!("rdc://queues/{slug}"),
                "name": slug,
                "workspace": "rdc://workspaces/main",
                "schema": serde_json::Value::Null,
            });
            let queue_path = dir.join("queue.json");
            std::fs::write(&queue_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/queues/{id}"),
                "name": slug,
                "workspace": format!("{api}/workspaces/7"),
                "schema": serde_json::Value::Null,
            });
            lockfile.upsert(
                "queues",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("queues").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "queues",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), queue_path);
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/queues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 800 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/queues/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(list["results"][i].clone())
                        .set_delay(std::time::Duration::from_millis(200)),
                )
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let start = std::time::Instant::now();
        let (pushed, skipped) = super::push(
            &paths, &client, &mut lockfile, false, &changes, &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(650),
            "four 200ms PATCHes must overlap; sequential would be >= 800ms, took {elapsed:?}",
        );
    }

    /// Deferred refs must survive the concurrent/sequential boundary.
    ///
    /// A queue whose body still names a not-yet-created object keeps that field
    /// OUT of the PATCH and hands it to the relink pass. The network stage is
    /// where `resolve_value_deferring` runs and the apply stage is where
    /// `relink` is owned, so the deferred list has to ride across in
    /// `QueuePatched` — recomputing it on the far side would re-resolve against
    /// a lockfile later items have since mutated.
    #[tokio::test]
    async fn push_queues_carries_deferred_refs_to_the_relink_pass() {
        use crate::paths::Paths;
        use crate::snapshot::codec::combined_hash;
        use crate::state::{Lockfile, ObjectEntry};
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let dir = paths.queue_dir("main", "q-a");
        std::fs::create_dir_all(&dir).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "workspaces",
            "main",
            ObjectEntry {
                id: 7,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // `engine` names an engine that does not exist in the lockfile, so the
        // ref cannot resolve and the field defers. `workspace` also carries a
        // ref, but it is undeferrable (a typed `Queue` would send it as null),
        // so it must stay in the body.
        let local = serde_json::json!({
            "url": "rdc://queues/q-a",
            "name": "q-a",
            "workspace": "rdc://workspaces/main",
            "schema": serde_json::Value::Null,
            "engine": "rdc://engines/not-yet",
        });
        let queue_path = dir.join("queue.json");
        std::fs::write(&queue_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();

        lockfile.upsert(
            "queues",
            "q-a",
            ObjectEntry {
                id: 800,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let remote = serde_json::json!({
            "id": 800,
            "url": format!("{api}/queues/800"),
            "name": "q-a",
            "workspace": format!("{api}/workspaces/7"),
            "schema": serde_json::Value::Null,
            "engine": "rdc://engines/not-yet",
        });
        let codec = crate::snapshot::codec::codec("queues").unwrap();
        let art = codec.disk_bytes(&remote).unwrap();
        let base = combined_hash(&art.json, &art.sidecars, &lockfile);
        lockfile.upsert(
            "queues",
            "q-a",
            ObjectEntry {
                id: 800,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );
        let mut changes = BTreeMap::new();
        changes.insert("q-a".to_string(), queue_path);

        Mock::given(method("GET"))
            .and(path("/api/v1/queues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/queues/800"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 800,
                "url": format!("{api}/queues/800"),
                "name": "q-a",
                "workspace": format!("{api}/workspaces/7"),
                "schema": serde_json::Value::Null,
            })))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let (pushed, _skipped) = super::push(
            &paths, &client, &mut lockfile, false, &changes, &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");

        assert_eq!(pushed, 1);
        assert_eq!(relink.len(), 1, "expected one deferred relink: {relink:?}");
        assert_eq!(relink[0].kind, "queues");
        assert_eq!(relink[0].slug, "q-a");
        assert_eq!(
            relink[0].fields,
            vec![(
                "engine".to_string(),
                serde_json::json!("rdc://engines/not-yet")
            )],
        );

        let body: serde_json::Value = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .find(|r| r.method == wiremock::http::Method::PATCH)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("a PATCH must have been sent");
        assert!(
            body.get("engine").is_none(),
            "the deferred field must be absent from the PATCH body: {body}",
        );
        assert_eq!(
            body["workspace"],
            serde_json::json!(format!("{api}/workspaces/7")),
            "an undeferrable ref must stay in the body: {body}",
        );
    }
}
