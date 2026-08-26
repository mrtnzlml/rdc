//! Push workspaces. Handles both CREATE (POST) and UPDATE (PATCH).
//! Workspaces sit at the top of the dependency tree — queues, schemas,
//! inboxes, email_templates all root from a workspace by URL — so this
//! driver runs first in the phase-2 dispatch.

use crate::api::RossumClient;
use crate::log::{Action, Log};
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
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. The two are NOT partitioned into "all creates, then all
    // updates": a workspace's refs are resolved against the lockfile AS IT
    // STANDS when that workspace is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that used to stay unresolved,
    // and change what this command sends (`push::hooks` carries an observable
    // instance of exactly that). So `changes` is still walked in slug order and
    // each MAXIMAL RUN of consecutive updates is fanned out, with a create
    // acting as a barrier.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (ws_slug, ws_path) in changes {
        // CREATE — no lockfile entry yet.
        if lockfile
            .objects
            .get("workspaces")
            .and_then(|m| m.get(ws_slug.as_str()))
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
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;

            let disk_bytes =
                std::fs::read(ws_path).with_context(|| format!("reading {}", ws_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", ws_path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "workspaces");
            let create_result = client
                .create_workspace(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /workspaces (creating '{ws_slug}')"));
            let created = create_result?;
            let codec = crate::snapshot::codec::codec("workspaces").unwrap();
            let created_art = codec
                .disk_bytes(
                    &serde_json::to_value(&created).context("serializing created workspace")?,
                )
                .context("codec disk_bytes for created workspace")?;
            // Register the new workspace's id NOW so its own `url` portabilizes
            // to `rdc://`. Concrete env URLs must never touch disk, even
            // transiently (an interrupted sync whose portabilize post-pass never
            // runs would freeze them into the snapshot).
            lockfile.upsert(
                "workspaces",
                ws_slug,
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
            write_atomic(ws_path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{ws_slug}'"))?;
            lockfile.upsert(
                "workspaces",
                ws_slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Post,
                &format!("workspace/{ws_slug} id={}", created.id),
            );
            pushed += 1;
            continue;
        }

        batch.push((ws_slug, ws_path));
    }

    // Flush the trailing run.
    let (batched_pushed, batched_skipped) = push_update_batch(
        paths,
        client,
        lockfile,
        interactive,
        &mut batch,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// Fan out one maximal run of consecutive workspace UPDATES, then apply the
/// results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem and every prompt. `batch` is drained.
///
/// Unlike the list-backed drivers there is no whole-kind fetch to hoist: the
/// drift check is a `GET /workspaces/{id}` per item, and one id per slug by
/// construction means there is nothing to dedup. That GET therefore lives
/// INSIDE the per-item future, where it overlaps with its siblings' GETs and
/// PATCHes — it is the round trip this split exists to hide.
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
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

    // The drift GET's body, parked for the sequential stage.
    //
    // `Prepared::NeedsPrompt` carries only a slug, but `resolve_push_drift`
    // needs the very body the drift check compared against — and re-fetching it
    // on the apply path would add a SECOND `GET /workspaces/{id}` that this
    // command never used to make. The list-backed drivers hand
    // `push_one_drifted` their hoisted list for exactly this reason; a
    // per-item-GET driver has no list, so each future parks its own body here
    // instead. Only a drifted item ever inserts. The lock is taken around the
    // insert alone and never held across an `.await`.
    let drift_bodies: std::sync::Mutex<
        std::collections::HashMap<String, crate::model::Workspace>,
    > = std::sync::Mutex::new(std::collections::HashMap::new());

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let bodies = &drift_bodies;
        prepare_all(updates.iter().copied(), |(ws_slug, ws_path)| async move {
            // The `content_hash` guard runs BEFORE the file is read, exactly as
            // the old sequential loop did: an entry with no recorded base is
            // skipped without the local file ever being touched.
            let entry = lf
                .objects
                .get("workspaces")
                .and_then(|m| m.get(ws_slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: ws_slug.clone(),
                    event: format!("workspace/{ws_slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            let disk_bytes = std::fs::read(ws_path)
                .with_context(|| format!("reading {}", ws_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", ws_path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lf);
            let payload_workspace: crate::model::Workspace = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied workspace '{ws_slug}'"))?;

            // Drift check — one GET per item, overlapped with its siblings'.
            let remote_workspace = client
                .get_workspace(id, Some(progress.clone()))
                .await
                .with_context(|| format!("fetching workspace {id} to verify drift before push"))?;
            let remote_art = remote_artifact(&remote_workspace)?;
            if combined_hash(&remote_art.json, &remote_art.sidecars, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                // Named binding + explicit `drop`, NOT a one-statement
                // temporary: `prepare_all` puts no `Send` bound on its future,
                // so a guard held across an `.await` would COMPILE. With the
                // guard named, `clippy::await_holding_lock` catches any future
                // edit that awaits before this drop.
                let mut guard = bodies.lock().expect("drift-body cache poisoned");
                guard.insert(ws_slug.clone(), remote_workspace);
                drop(guard);
                return Ok(Prepared::NeedsPrompt {
                    slug: ws_slug.clone(),
                });
            }

            // Strip server-managed fields from `extra` so the PATCH matches the
            // CREATE contract (the server-computed `queues` back-ref).
            let mut payload_to_send = payload_workspace;
            strip_patch_extra(&mut payload_to_send.extra, "workspaces", false);
            let updated = client
                .update_workspace(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /workspaces/{id}"))?;
            Ok(Prepared::Patched {
                slug: ws_slug.clone(),
                updated,
            })
        })
        .await
    };
    let drift_bodies = drift_bodies.into_inner().expect("drift-body cache poisoned");

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem and every prompt. Every completed
    //     PATCH is recorded even if a sibling failed (spec D10), then the
    //     first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (slug_in, ws_path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one workspace's response over another workspace's file.
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
                match write_back(paths, lockfile, &slug, ws_path, &updated) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("workspace/{slug}"));
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
                let remote_workspace = drift_bodies
                    .get(slug.as_str())
                    .expect("the concurrent stage parks a body before returning NeedsPrompt");
                match push_one_drifted(
                    paths,
                    client,
                    lockfile,
                    interactive,
                    &slug,
                    ws_path,
                    remote_workspace,
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
/// There is no hoisted list here for it to keep in step with, but the other
/// seven fanned-out drivers all read their base through this one expression and
/// the comparability is what makes them diff-able by eye.
fn drift_base(entry: &ObjectEntry) -> Option<&str> {
    entry.content_hash.as_deref()
}

/// The canonical on-disk artifact for a remote workspace, as the drift check and
/// the drift prompt both need it. Lifted verbatim from the old loop's
/// `codec.disk_bytes(...)` block.
fn remote_artifact(
    remote: &crate::model::Workspace,
) -> Result<crate::snapshot::codec::DiskArtifact> {
    let codec = crate::snapshot::codec::codec("workspaces").unwrap();
    codec
        .disk_bytes(
            &serde_json::to_value(remote).context("serializing remote workspace for drift check")?,
        )
        .context("codec disk_bytes for remote workspace")
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// plus the lockfile entry.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let codec = ...` down to and including the `lockfile.upsert("workspaces", ...)`
/// call, with `updated` taken by reference and the
/// `progress.event(Action::Patch, ...)` line left behind at the call site so the
/// caller controls when it fires.
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    ws_slug: &str,
    ws_path: &std::path::Path,
    updated: &crate::model::Workspace,
) -> Result<()> {
    let codec = crate::snapshot::codec::codec("workspaces").unwrap();
    let updated_art = codec
        .disk_bytes(
            &serde_json::to_value(updated)
                .context("serializing updated workspace for disk write")?,
        )
        .context("codec disk_bytes for updated workspace")?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the workspace is lockfile-pinned, so its self-url resolves to rdc://).
    let updated_bytes =
        crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
    let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, ws_path, &updated_bytes)
        .with_context(|| format!("writing post-push canonical form for '{ws_slug}'"))?;
    lockfile.upsert(
        "workspaces",
        ws_slug,
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

/// Resolve one drifted workspace interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value`, `resolve_push_drift`, then either PATCH (via the
/// same `update_workspace` + `write_back`), adopt the remote, or skip. It runs
/// only on the sequential stage, so `resolve_push_drift`'s prompt can never
/// interleave with another item's. `remote_workspace` is the body the concurrent
/// stage already fetched, so this path costs no extra request. Returns
/// `(pushed, skipped)` deltas.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    ws_slug: &str,
    ws_path: &std::path::Path,
    remote_workspace: &crate::model::Workspace,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let entry = lockfile
        .objects
        .get("workspaces")
        .and_then(|m| m.get(ws_slug))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let disk_bytes =
        std::fs::read(ws_path).with_context(|| format!("reading {}", ws_path.display()))?;
    let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
        .with_context(|| format!("parsing {}", ws_path.display()))?;
    crate::snapshot::refs::resolve_value(&mut payload, lockfile);
    let payload_workspace: crate::model::Workspace = serde_json::from_value(payload)
        .with_context(|| format!("deserializing overlay-applied workspace '{ws_slug}'"))?;

    let remote_art = remote_artifact(remote_workspace)?;
    let remote_bytes = remote_art.json;
    let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
    let mut payload_to_send = payload_workspace;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(interactive, ws_path, &remote_bytes, env)? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited workspace '{ws_slug}'"))?;
                crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited workspace '{ws_slug}'"))?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the workspace is lockfile-pinned; self-url resolves).
            let remote_bytes =
                crate::cli::pull::common::portabilize_proposed(&remote_bytes, lockfile);
            write_atomic(ws_path, &remote_bytes)
                .with_context(|| format!("adopting remote into {}", ws_path.display()))?;
            lockfile.upsert(
                "workspaces",
                ws_slug,
                ObjectEntry {
                    id,
                    modified_at: remote_workspace.modified_at().map(|s| s.to_string()),
                    modified_by: remote_workspace.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Warn,
                &format!("workspace/{ws_slug} adopted remote (drift)"),
            );
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("workspace/{ws_slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
        }
    }

    // Strip server-managed fields from `extra` so the PATCH matches the
    // CREATE contract (the server-computed `queues` back-ref).
    strip_patch_extra(&mut payload_to_send.extra, "workspaces", false);
    let patch_result = client
        .update_workspace(id, &payload_to_send, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /workspaces/{id}"));
    let updated = patch_result?;

    write_back(paths, lockfile, ws_slug, ws_path, &updated)?;
    progress.event(Action::Patch, &format!("workspace/{ws_slug}"));
    Ok((1, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Seed `n` workspaces that are UPDATES: local file, lockfile entry and a
    /// recorded base computed from the remote body the server will hand back.
    /// Returns the remote bodies so each test mounts exactly the responses it
    /// wants.
    fn seed_workspaces(
        tmp: &tempfile::TempDir,
        api: &str,
        slugs: &[&str],
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
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 300 + i as u64;
            let ws_dir = paths.workspace_dir(slug);
            std::fs::create_dir_all(&ws_dir).unwrap();
            let ws_path = ws_dir.join("workspace.json");
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://workspaces/{slug}"),
                "organization": format!("{api}/organizations/1"),
                "queues": []
            });
            std::fs::write(&ws_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/workspaces/{id}"),
                "name": slug,
                "organization": format!("{api}/organizations/1"),
                "queues": []
            });
            // Register the id BEFORE hashing so the self-url portabilizes.
            lockfile.upsert(
                "workspaces",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("workspaces").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "workspaces",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), ws_path);
            remotes.push(remote);
        }
        (paths, lockfile, changes, remotes)
    }

    /// Write a local workspace with NO lockfile entry, so the driver treats it
    /// as a CREATE, and register it in `changes`.
    fn seed_create(
        paths: &Paths,
        api: &str,
        changes: &mut BTreeMap<String, std::path::PathBuf>,
        slug: &str,
    ) {
        let ws_dir = paths.workspace_dir(slug);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_path = ws_dir.join("workspace.json");
        let body = serde_json::json!({
            "name": slug,
            "url": format!("rdc://workspaces/{slug}"),
            "organization": format!("{api}/organizations/1"),
            "queues": []
        });
        std::fs::write(&ws_path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        changes.insert(slug.to_string(), ws_path);
    }

    async fn mount_get_and_patch(
        server: &MockServer,
        id: u64,
        get_body: serde_json::Value,
        patch_body: serde_json::Value,
        delay: std::time::Duration,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/workspaces/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(get_body)
                    .set_delay(delay),
            )
            .mount(server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v1/workspaces/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(patch_body)
                    .set_delay(delay),
            )
            .mount(server)
            .await;
    }

    /// Spec D9: the per-item drift GET plus its PATCH is two round trips per
    /// slug. Four workspaces at 150ms per call cost ~1.2s in series; fanned
    /// out they cost roughly one slug's worth.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_workspaces_overlaps_the_per_item_drift_get_and_patch() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, remotes) =
            seed_workspaces(&tmp, &api, &["w-a", "w-b", "w-c", "w-d"]);
        for (i, remote) in remotes.iter().enumerate() {
            mount_get_and_patch(
                &server,
                300 + i as u64,
                remote.clone(),
                remote.clone(),
                std::time::Duration::from_millis(150),
            )
            .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "the four GET+PATCH pairs must overlap; sequential would be >= 1.2s, took {elapsed:?}",
        );
    }

    /// Ordering: `changes` is walked in slug order with creates acting as
    /// barriers, NOT partitioned into "all creates, then all updates". `w-a`
    /// (an update) sorts before `w-b` (a create), so its PATCH must go out
    /// BEFORE the POST — exactly the request stream the pre-refactor
    /// sequential loop produced. `w-m` sorts AFTER the create, so it lands in
    /// the TRAILING run: drop the unconditional flush after the loop and its
    /// PATCH vanishes with no request and no error at all.
    ///
    /// Both runs hold exactly one item, so the whole stream stays deterministic
    /// even though each run is fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_workspaces_keeps_slug_order_across_a_create_barrier() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, mut changes, remotes) = seed_workspaces(&tmp, &api, &["w-a", "w-m"]);
        seed_create(&paths, &api, &mut changes, "w-b");
        for (i, remote) in remotes.iter().enumerate() {
            mount_get_and_patch(
                &server,
                300 + i as u64,
                remote.clone(),
                remote.clone(),
                std::time::Duration::ZERO,
            )
            .await;
        }
        Mock::given(method("POST"))
            .and(path("/api/v1/workspaces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 399,
                "url": format!("{api}/workspaces/399"),
                "name": "w-b",
                "organization": format!("{api}/organizations/1"),
                "queues": []
            })))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
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
                "GET /api/v1/workspaces/300".to_string(),
                "PATCH /api/v1/workspaces/300".to_string(),
                "POST /api/v1/workspaces".to_string(),
                "GET /api/v1/workspaces/301".to_string(),
                "PATCH /api/v1/workspaces/301".to_string(),
            ],
            "the update sorting before the create must be sent before the POST, \
             and the one sorting after it must still be sent",
        );
    }

    /// Spec D9: a drifted item is never PATCHed on the concurrent path. It is
    /// deferred to the sequential pass, where non-interactive
    /// `resolve_push_drift` skips it — so its id must never appear in a PATCH,
    /// while its clean neighbours are patched normally.
    ///
    /// It also pins the request count on the drift path: the sequential stage
    /// reuses the body the concurrent stage already fetched, so a drifted
    /// workspace still costs exactly ONE `GET /workspaces/{id}`.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_workspaces_never_patches_a_drifted_item_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, remotes) =
            seed_workspaces(&tmp, &api, &["w-a", "w-b", "w-c"]);
        for (i, remote) in remotes.iter().enumerate() {
            let id = 300 + i as u64;
            // w-b's remote no longer matches the recorded base.
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
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");
        assert_eq!((pushed, skipped), (2, 1), "the drifted workspace is skipped");

        let seq: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        assert!(
            !seq.contains(&"PATCH /api/v1/workspaces/301".to_string()),
            "the drifted workspace must never be PATCHed, saw {seq:?}"
        );
        assert_eq!(
            seq.iter()
                .filter(|r| *r == "GET /api/v1/workspaces/301")
                .count(),
            1,
            "the drift prompt must reuse the body the concurrent stage fetched, \
             not re-fetch it: {seq:?}"
        );
        for id in [300u64, 302] {
            assert!(
                seq.contains(&format!("PATCH /api/v1/workspaces/{id}")),
                "the clean neighbours must still be patched, saw {seq:?}"
            );
        }
    }

    /// Spec D10, apply stage: an error raised while APPLYING one item must not
    /// strand the completed PATCHes of the items AFTER it. By the time the
    /// apply stage runs, every clean PATCH in the batch has already landed
    /// server-side, so returning early would leave w-c's `content_hash` stale
    /// for a change the server has accepted.
    ///
    /// w-b's write-back is what fails: its recorded path is outside the env
    /// tree, which `base_cache::write` refuses. Its PATCH still succeeds.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_workspaces_records_a_later_patch_when_an_earlier_apply_fails() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, mut changes, remotes) =
            seed_workspaces(&tmp, &api, &["w-a", "w-b", "w-c"]);
        let before: Vec<Option<String>> = ["w-a", "w-b", "w-c"]
            .iter()
            .map(|s| lockfile.objects["workspaces"][*s].content_hash.clone())
            .collect();
        for (i, remote) in remotes.iter().enumerate() {
            // Echo a changed name so a recorded write-back is observable.
            let mut patched = remote.clone();
            patched["name"] = serde_json::json!(format!("{} pushed", remote["name"].as_str().unwrap()));
            mount_get_and_patch(
                &server,
                300 + i as u64,
                remote.clone(),
                patched,
                std::time::Duration::ZERO,
            )
            .await;
        }
        // The concurrent stage reads the local file out of the workspace dir,
        // so w-b is still read and PATCHed normally; only the write-back
        // consults this path, and `base_cache::write` bails on anything
        // outside `env_root`.
        changes.insert(
            "w-b".to_string(),
            tmp.path().join("outside-env-tree").join("workspace.json"),
        );
        std::fs::create_dir_all(tmp.path().join("outside-env-tree")).unwrap();
        std::fs::copy(
            paths.workspace_dir("w-b").join("workspace.json"),
            tmp.path().join("outside-env-tree").join("workspace.json"),
        )
        .unwrap();

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let err = push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
            .await
            .expect_err("the failed write-back must propagate");
        assert!(
            format!("{err:#}").contains("env_root"),
            "error names the write-back failure: {err:#}"
        );

        let after: Vec<Option<String>> = ["w-a", "w-b", "w-c"]
            .iter()
            .map(|s| lockfile.objects["workspaces"][*s].content_hash.clone())
            .collect();
        assert_ne!(after[0], before[0], "the earlier item stays recorded");
        assert_ne!(
            after[2], before[2],
            "the LATER item's completed PATCH must still be recorded"
        );
        assert_eq!(
            after[1], before[1],
            "the un-applied item's base must not move"
        );
    }
}
