use crate::api::{RossumClient, anyhow_has_status};
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
    let mut drift_engines: Option<Vec<crate::model::Engine>> = None;

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. The two are NOT partitioned into "all creates, then all
    // updates": an engine's refs are resolved against the lockfile AS IT STANDS
    // when that engine is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that had to DEFER, collapsing
    // the documented push-PATCH + relink-PATCH pair into one PATCH with a
    // different body (`push::hooks` carries an observable instance of exactly
    // that). So `changes` is still walked in slug order and each MAXIMAL RUN of
    // consecutive updates is fanned out, with a create acting as a barrier.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (slug, path) in changes {

        // Missing lockfile entry → new engine, POST.
        if lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get(slug.as_str()))
            .is_none()
        {
            // Close the pending run first: every update sorting BEFORE this
            // create must be prepared against a lockfile that does not yet
            // know the id this POST is about to assign.
            let (batched_pushed, batched_skipped, read_only) = push_update_batch(
                paths,
                client,
                lockfile,
                interactive,
                &mut batch,
                &mut drift_engines,
                relink,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;
            if read_only {
                // A PATCH came back 405. The old loop's `break` ended the whole
                // driver right there, creates included; keep that.
                return Ok((pushed, skipped));
            }

            let disk_bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            let deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
            strip_for_create(&mut payload, "engines");
            let create_result = client
                .create_engine(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /engines (creating '{slug}')"));
            let created = match create_result {
                Ok(c) => c,
                Err(e) if anyhow_has_status(&e, 405) || anyhow_has_status(&e, 403) => {
                    let code = if anyhow_has_status(&e, 403) { "403" } else { "405" };
                    progress.event(Action::Skip, &format!("engine/{slug} (create {code} — engines not writable on this plan)"));
                    skipped += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            // Canonical on-disk bytes via KindCodec: redacts `agenda_id` and
            // strips hidden fields — matching exactly what pull produces.
            let codec = crate::snapshot::codec::codec("engines").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created engine")?)
                .context("codec disk_bytes for created engine")?;
            // Register the new engine's id NOW so its own `url` (and any ref to
            // an already-created object) portabilizes to `rdc://`. Concrete env
            // URLs must never touch disk, even transiently (an interrupted sync
            // whose portabilize post-pass never runs would freeze them in).
            lockfile.upsert(
                "engines",
                slug,
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
            write_atomic(path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            lockfile.upsert(
                "engines",
                slug,
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
                    kind: "engines".to_string(),
                    slug: slug.clone(),
                    path: path.clone(),
                    fields: deferred,
                });
            }
            progress.event(Action::Post, &format!("engine/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        batch.push((slug, path));
    }

    // Flush the trailing run.
    let (batched_pushed, batched_skipped, _read_only) = push_update_batch(
        paths,
        client,
        lockfile,
        interactive,
        &mut batch,
        &mut drift_engines,
        relink,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// What one engine's concurrent stage carries across to its apply stage.
enum EnginePatched {
    /// The PATCH went through. `deferred` holds the fields
    /// `resolve_value_deferring` held BACK from it; it rides along rather than
    /// being recomputed on the sequential side, because recomputing would need
    /// the local file re-read and re-resolved against a lockfile that later
    /// items have since mutated.
    Updated {
        updated: crate::model::Engine,
        deferred: Vec<(String, serde_json::Value)>,
    },
    /// The PATCH came back 405: engines are read-only on this plan. The old
    /// sequential loop emitted ONE skip line and then `break`ed out of the
    /// whole driver; the apply stage reproduces both, which is why this rides
    /// `Prepared::Patched` rather than `Prepared::Skipped` — `Skipped` carries
    /// only a transcript line, and a transcript line is not a control signal.
    ///
    /// This is a widening of the SUCCESS path, not the error-path exception
    /// the plan's Global Constraints document for Tasks 9-12 ("Do not change
    /// what a command requests"): this match arm returns `Ok`, not `Err`, and
    /// by the time the first 405 comes back `prepare_all` has already
    /// dispatched every item in the batch — so this driver can issue N
    /// rejected PATCHes where the old sequential loop's `break` sent exactly
    /// one. It is the case that actually occurs in practice (engines are
    /// 403/405 on the test sandbox — see the sibling 403-or-405 check on the
    /// create path above). Impact is bounded and harmless: every extra
    /// request is still paced by the same token bucket as everything else,
    /// rejects rather than mutating anything, and the apply stage still
    /// emits exactly one skip line for the whole run regardless of how many
    /// PATCHes actually went out.
    ReadOnly,
}

/// Fan out one maximal run of consecutive engine UPDATES, then apply the
/// results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem, `relink` and every prompt. `batch` is
/// drained.
///
/// `drift_engines` is the caller's one-per-push cache of the fresh engine list,
/// so several runs still cost a single `GET /engines` — and a push whose updates
/// all lack a `content_hash` still costs none.
///
/// The third return value is the 405 stop signal: `true` once a PATCH has come
/// back "read-only on this plan", which the caller turns back into the old
/// loop's `break`.
#[allow(clippy::too_many_arguments)]
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    drift_engines: &mut Option<Vec<crate::model::Engine>>,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize, bool)> {
    use crate::cli::push::concurrent::{Prepared, prepare_all};

    let updates = std::mem::take(batch);
    if updates.is_empty() {
        return Ok((0, 0, false));
    }
    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut read_only = false;

    // Drift-check list, hoisted to ONE fetch before the batch — but only when
    // at least one update can actually reach the drift check. The old lazy
    // `remote_cache` was populated by the first item that got PAST the
    // `content_hash` guard, so a run of entries that all lack a hash made no
    // list call at all; keep that exactly, and keep it caller-owned so several
    // runs share the single fetch.
    let needs_drift_check = updates.iter().any(|(slug, _)| {
        lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get(slug.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if drift_engines.is_none() && needs_drift_check {
        *drift_engines = Some(
            client
                .list_engines(Some(progress.clone()))
                .await
                .context("listing engines to verify no drift before push")?,
        );
    }
    // Empty only when nothing in this run can consult it: an entry with no
    // `content_hash` returns `Prepared::Skipped` before the list is ever
    // touched, and by construction that is then every entry in the run.
    let remote_engines: &[crate::model::Engine] = drift_engines.as_deref().unwrap_or(&[]);

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref = remote_engines;
        prepare_all(updates.iter().copied(), |(slug, path)| async move {
            // Read BEFORE the `content_hash` guard, exactly as the old
            // sequential loop did: an unreadable file is an error even for an
            // entry that would otherwise be skipped.
            let disk_bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let entry = lf
                .objects
                .get("engines")
                .and_then(|m| m.get(slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("engine/{slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            let mut deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lf);
            // `update_engine` sends a typed `Engine`; only `url` is modeled among
            // the ref-bearing fields, and it must not be sent empty.
            crate::cli::push::relink::restore_undeferrable("engines", &mut payload, &mut deferred);
            let payload_engine: crate::model::Engine = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied engine '{slug}'"))?;

            let Some(remote_engine) = remote_ref.iter().find(|e| e.id == id) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("engine/{slug} (remote id {id} missing)"),
                });
            };
            let remote_art = remote_artifact(remote_engine)?;
            if combined_hash(&remote_art.json, &remote_art.sidecars, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
            }

            // A PATCH must not echo server-managed fields. `agenda_id` is a
            // read-only, per-env identifier that Rossum refreshes on training;
            // echoing the redacted sentinel back is ignored at best and
            // overwrites/400s the engine's identifier at worst. Strip it (and the
            // other server fields) off `extra`, matching the CREATE contract.
            let mut payload_to_send = payload_engine;
            strip_patch_extra(&mut payload_to_send.extra, "engines", false);
            let patch_result = client
                .update_engine(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /engines/{id}"));
            let updated = match patch_result {
                Ok(u) => u,
                Err(e) if anyhow_has_status(&e, 405) => {
                    return Ok(Prepared::Patched {
                        slug: slug.clone(),
                        updated: EnginePatched::ReadOnly,
                    });
                }
                Err(e) => return Err(e),
            };
            Ok(Prepared::Patched {
                slug: slug.clone(),
                updated: EnginePatched::Updated { updated, deferred },
            })
        })
        .await
    };

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem, `relink` and every prompt. Every
    //     completed PATCH is recorded even if a sibling failed (spec D10),
    //     then the first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (slug_in, path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one engine's response over another engine's file.
        if let Ok(p) = &item {
            debug_assert_eq!(p.slug(), slug_in.as_str());
        }
        match item {
            // NOT `?`: by the time the apply stage runs, every clean PATCH in
            // the batch has already landed server-side. Returning early here
            // would leave the REMAINING items' completed PATCHes unrecorded —
            // the exact inconsistency D10 exists to shrink, and worse than the
            // old sequential loop, which never sent those requests at all.
            Ok(Prepared::Patched {
                slug,
                updated: EnginePatched::Updated { updated, deferred },
            }) => match write_back(paths, lockfile, relink, &slug, path, &updated, deferred) {
                Ok(()) => {
                    progress.event(Action::Patch, &format!("engine/{slug}"));
                    pushed += 1;
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            },
            // 405. The old loop emitted this line once and stopped; a run that
            // is entirely 405s would otherwise repeat it per item, so only the
            // FIRST one speaks and the rest are swallowed exactly as `break`
            // swallowed them.
            Ok(Prepared::Patched {
                slug,
                updated: EnginePatched::ReadOnly,
            }) => {
                if !read_only {
                    progress.event(
                        Action::Skip,
                        &format!("engine/{slug} (PATCH 405 — engines read-only on this plan)"),
                    );
                    skipped += 1;
                    read_only = true;
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
            //
            // `read_only` is the one exception, and it is not a suppression of
            // work already done: a drifted item's PATCH has NOT been sent yet,
            // and the old loop had already stopped by this point. Prompting for
            // an edit the plan cannot accept would be worse than silence.
            Ok(Prepared::NeedsPrompt { slug }) => {
                if read_only {
                    continue;
                }
                match push_one_drifted(
                    paths,
                    client,
                    lockfile,
                    interactive,
                    &slug,
                    path,
                    remote_engines,
                    relink,
                    progress,
                    env,
                )
                .await
                {
                    Ok((p, s, ro)) => {
                        pushed += p;
                        skipped += s;
                        read_only |= ro;
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

    Ok((pushed, skipped, read_only))
}

/// Whether this entry can reach the drift check at all — and, if so, the base
/// the remote is checked against.
///
/// The hoisted list fetch and the per-item guard inside the concurrent stage
/// MUST agree on this predicate. If the hoist guard were ever narrower than the
/// per-item one, an item would consult an empty list and silently take the
/// "remote id missing" skip instead of a real drift check — no error, just
/// wrong. One expression, called from both, so they cannot drift apart.
fn drift_base(entry: &ObjectEntry) -> Option<&str> {
    entry.content_hash.as_deref()
}

/// The canonical on-disk artifact for a remote engine, as the drift check and
/// the drift prompt both need it. Lifted verbatim from the old loop's
/// `codec.disk_bytes(...)` block: the same KindCodec path the pull driver uses,
/// so the hash matches the lockfile baseline (redacts `agenda_id`, strips
/// hidden fields).
fn remote_artifact(remote: &crate::model::Engine) -> Result<crate::snapshot::codec::DiskArtifact> {
    let codec = crate::snapshot::codec::codec("engines").unwrap();
    codec
        .disk_bytes(
            &serde_json::to_value(remote).context("serializing remote engine for drift check")?,
        )
        .context("codec disk_bytes for remote engine")
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// the lockfile entry, and the deferred-relink record.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let codec = ...` down to and including the `relink.push(...)`, with
/// `updated` taken by reference and the `progress.event(Action::Patch, ...)`
/// line left behind at the call site so the caller controls when it fires.
#[allow(clippy::too_many_arguments)]
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    slug: &str,
    path: &std::path::Path,
    updated: &crate::model::Engine,
    deferred: Vec<(String, serde_json::Value)>,
) -> Result<()> {
    // Post-PATCH disk write via KindCodec so `agenda_id` is redacted to
    // the sentinel (bug c fix: previously used raw to_vec_pretty, which
    // re-emitted the live agenda_id into engine.json after each PATCH).
    let codec = crate::snapshot::codec::codec("engines").unwrap();
    let updated_art = codec
        .disk_bytes(
            &serde_json::to_value(updated).context("serializing updated engine for disk write")?,
        )
        .context("codec disk_bytes for updated engine")?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the engine is lockfile-pinned, so self + refs resolve to rdc://).
    let updated_bytes =
        crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
    let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, path, &updated_bytes)
        .with_context(|| format!("writing post-push canonical form for engine '{slug}'"))?;

    lockfile.upsert(
        "engines",
        slug,
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
            kind: "engines".to_string(),
            slug: slug.to_string(),
            path: path.to_path_buf(),
            fields: deferred,
        });
    }
    Ok(())
}

/// Resolve one drifted engine interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value_deferring` + `restore_undeferrable`,
/// `resolve_push_drift`, then either PATCH (via the same `update_engine` +
/// `write_back`), adopt the remote, or skip. It runs only on the sequential
/// stage, so `resolve_push_drift`'s prompt can never interleave with another
/// item's. Returns `(pushed, skipped, read_only)`.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    slug: &str,
    path: &std::path::Path,
    remote_engines: &[crate::model::Engine],
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize, bool)> {
    let entry = lockfile
        .objects
        .get("engines")
        .and_then(|m| m.get(slug))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let disk_bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    let mut deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
    // `update_engine` sends a typed `Engine`; only `url` is modeled among
    // the ref-bearing fields, and it must not be sent empty.
    crate::cli::push::relink::restore_undeferrable("engines", &mut payload, &mut deferred);
    let payload_engine: crate::model::Engine = serde_json::from_value(payload)
        .with_context(|| format!("deserializing overlay-applied engine '{slug}'"))?;

    let Some(remote_engine) = remote_engines.iter().find(|e| e.id == id) else {
        progress.event(
            Action::Skip,
            &format!("engine/{slug} (remote id {id} missing)"),
        );
        return Ok((0, 1, false));
    };
    let remote_art = remote_artifact(remote_engine)?;
    let remote_bytes = remote_art.json;
    let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
    let mut payload_to_send = payload_engine;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(interactive, path, &remote_bytes, env)? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited engine '{slug}'"))?;
                deferred = crate::snapshot::refs::resolve_value_deferring(&mut ov, lockfile);
                crate::cli::push::relink::restore_undeferrable("engines", &mut ov, &mut deferred);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited engine '{slug}'"))?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the engine is lockfile-pinned; refs resolve).
            let remote_bytes =
                crate::cli::pull::common::portabilize_proposed(&remote_bytes, lockfile);
            write_atomic(path, &remote_bytes)
                .with_context(|| format!("adopting remote into {}", path.display()))?;
            lockfile.upsert(
                "engines",
                slug,
                ObjectEntry {
                    id,
                    modified_at: remote_engine.modified_at().map(|s| s.to_string()),
                    modified_by: remote_engine.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Warn,
                &format!("engine/{slug} adopted remote (drift)"),
            );
            return Ok((0, 1, false));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("engine/{slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1, false));
        }
    }

    // A PATCH must not echo server-managed fields (see the concurrent stage).
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
            return Ok((0, 1, true));
        }
        Err(e) => {
            return Err(e);
        }
    };

    write_back(paths, lockfile, relink, slug, path, &updated, deferred)?;
    progress.event(Action::Patch, &format!("engine/{slug}"));
    Ok((1, 0, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec D9: clean engine updates PATCH concurrently. Four engines whose
    /// PATCHes each take 200ms cost ~800ms in series and ~200-400ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_engines_patches_updates_concurrently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        let slugs = ["e-a", "e-b", "e-c", "e-d"];
        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 700 + i as u64;
            let dir = paths.engine_dir(slug);
            std::fs::create_dir_all(&dir).unwrap();
            let local = serde_json::json!({
                "url": format!("rdc://engines/{slug}"),
                "name": slug,
            });
            let engine_path = dir.join("engine.json");
            std::fs::write(&engine_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/engines/{id}"),
                "name": slug,
            });
            lockfile.upsert(
                "engines",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("engines").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "engines",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), engine_path);
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/engines"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 700 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/engines/{id}")))
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
        let (pushed, skipped) = push(
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
    /// An engine whose body still names a not-yet-created object keeps that
    /// field OUT of the PATCH and hands it to the relink pass. The network
    /// stage is where `resolve_value_deferring` runs and the apply stage is
    /// where `relink` is owned, so the deferred list has to ride across in
    /// `EnginePatched` — recomputing it on the far side would re-resolve
    /// against a lockfile later items have since mutated.
    #[tokio::test]
    async fn push_engines_carries_deferred_refs_to_the_relink_pass() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let dir = paths.engine_dir("e-a");
        std::fs::create_dir_all(&dir).unwrap();

        // `learning_source` names a queue that does not exist in the lockfile,
        // so the ref cannot resolve and the field defers.
        let local = serde_json::json!({
            "url": "rdc://engines/e-a",
            "name": "e-a",
            "learning_source": "rdc://queues/not-yet",
        });
        let engine_path = dir.join("engine.json");
        std::fs::write(&engine_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "engines",
            "e-a",
            ObjectEntry {
                id: 700,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let remote = serde_json::json!({
            "id": 700,
            "url": format!("{api}/engines/700"),
            "name": "e-a",
            "learning_source": "rdc://queues/not-yet",
        });
        let codec = crate::snapshot::codec::codec("engines").unwrap();
        let art = codec.disk_bytes(&remote).unwrap();
        let base = combined_hash(&art.json, &art.sidecars, &lockfile);
        lockfile.upsert(
            "engines",
            "e-a",
            ObjectEntry {
                id: 700,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );
        let mut changes = BTreeMap::new();
        changes.insert("e-a".to_string(), engine_path);

        Mock::given(method("GET"))
            .and(path("/api/v1/engines"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/engines/700"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 700,
                "url": format!("{api}/engines/700"),
                "name": "e-a",
            })))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");

        assert_eq!(pushed, 1);
        assert_eq!(relink.len(), 1, "expected one deferred relink: {relink:?}");
        assert_eq!(relink[0].kind, "engines");
        assert_eq!(relink[0].slug, "e-a");
        assert_eq!(
            relink[0].fields,
            vec![(
                "learning_source".to_string(),
                serde_json::json!("rdc://queues/not-yet")
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
            body.get("learning_source").is_none(),
            "the deferred field must be absent from the PATCH body: {body}",
        );
    }
}
