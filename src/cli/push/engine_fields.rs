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
    let mut drift_fields: Option<Vec<crate::model::EngineField>> = None;

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. The two are NOT partitioned into "all creates, then all
    // updates": a field's refs are resolved against the lockfile AS IT STANDS
    // when that field is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that used to stay unresolved,
    // and change what this command sends (`push::hooks` carries an observable
    // instance of exactly that). So `changes` is still walked in slug order and
    // each MAXIMAL RUN of consecutive updates is fanned out, with a create
    // acting as a barrier.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (slug, path) in changes {

        // Missing lockfile entry → new engine field, POST.
        if lockfile
            .objects
            .get("engine_fields")
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
                &mut drift_fields,
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
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "engine_fields");
            let create_result = client
                .create_engine_field(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /engine_fields (creating '{slug}')"));
            let created = create_result?;
            let codec = crate::snapshot::codec::codec("engine_fields").unwrap();
            let created_art = codec
                .disk_bytes(
                    &serde_json::to_value(&created).context("serializing created engine field")?,
                )
                .context("codec disk_bytes for created engine field")?;
            // Register the new engine field's id NOW so its own `url` (and its
            // `engine` ref) portabilizes to `rdc://`. Concrete env URLs must never
            // touch disk, even transiently (an interrupted sync whose portabilize
            // post-pass never runs would freeze them into the snapshot).
            lockfile.upsert(
                "engine_fields",
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
                "engine_fields",
                slug,
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
                &format!("engine_field/{slug} id={}", created.id),
            );
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
        &mut drift_fields,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// What one engine field's concurrent stage carries across to its apply stage.
enum FieldPatched {
    /// The PATCH went through; this is what the apply stage writes back.
    Updated(crate::model::EngineField),
    /// The PATCH came back 405: `engine_fields` are read-only on this plan.
    /// The old sequential loop emitted ONE skip line and then `break`ed out of
    /// the whole driver; the apply stage reproduces both, which is why this
    /// rides `Prepared::Patched` rather than `Prepared::Skipped` — `Skipped`
    /// carries only a transcript line, and a transcript line is not a control
    /// signal.
    ReadOnly,
}

/// Fan out one maximal run of consecutive engine-field UPDATES, then apply the
/// results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem and every prompt. `batch` is drained.
///
/// `drift_fields` is the caller's one-per-push cache of the fresh engine-field
/// list, so several runs still cost a single `GET /engine_fields` — and a push
/// whose updates all lack a `content_hash` still costs none.
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
    drift_fields: &mut Option<Vec<crate::model::EngineField>>,
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
            .get("engine_fields")
            .and_then(|m| m.get(slug.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if drift_fields.is_none() && needs_drift_check {
        *drift_fields = Some(
            client
                .list_engine_fields(Some(progress.clone()))
                .await
                .context("listing engine fields to verify no drift before push")?,
        );
    }
    // Empty only when nothing in this run can consult it: an entry with no
    // `content_hash` returns `Prepared::Skipped` before the list is ever
    // touched, and by construction that is then every entry in the run.
    let remote_fields: &[crate::model::EngineField] = drift_fields.as_deref().unwrap_or(&[]);

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref = remote_fields;
        prepare_all(updates.iter().copied(), |(slug, path)| async move {
            // Read BEFORE the `content_hash` guard, exactly as the old
            // sequential loop did: an unreadable file is an error even for an
            // entry that would otherwise be skipped.
            let disk_bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let entry = lf
                .objects
                .get("engine_fields")
                .and_then(|m| m.get(slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("engine_field/{slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lf);
            let payload_field: crate::model::EngineField = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied engine field '{slug}'"))?;

            let Some(remote_field) = remote_ref.iter().find(|f| f.id == id) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("engine_field/{slug} (remote id {id} missing)"),
                });
            };
            let remote_art = remote_artifact(remote_field)?;
            if combined_hash(&remote_art.json, &remote_art.sidecars, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
            }

            // Strip server-managed fields from `extra` so the PATCH matches the
            // CREATE contract. (`name` is immutable cross-env but editable
            // within-env, so within-env strip leaves it intact.)
            let mut payload_to_send = payload_field;
            strip_patch_extra(&mut payload_to_send.extra, "engine_fields", false);
            let patch_result = client
                .update_engine_field(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /engine_fields/{id}"));
            let updated = match patch_result {
                Ok(u) => u,
                Err(e) if anyhow_has_status(&e, 405) => {
                    return Ok(Prepared::Patched {
                        slug: slug.clone(),
                        updated: FieldPatched::ReadOnly,
                    });
                }
                Err(e) => return Err(e),
            };
            Ok(Prepared::Patched {
                slug: slug.clone(),
                updated: FieldPatched::Updated(updated),
            })
        })
        .await
    };

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem and every prompt. Every completed
    //     PATCH is recorded even if a sibling failed (spec D10), then the
    //     first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (slug_in, path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one field's response over another field's file.
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
                updated: FieldPatched::Updated(updated),
            }) => match write_back(paths, lockfile, &slug, path, &updated) {
                Ok(()) => {
                    progress.event(Action::Patch, &format!("engine_field/{slug}"));
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
                updated: FieldPatched::ReadOnly,
            }) => {
                if !read_only {
                    progress.event(
                        Action::Skip,
                        &format!(
                            "engine_field/{slug} (PATCH 405 — engine_fields read-only on this plan)"
                        ),
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
                    remote_fields,
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

/// The canonical on-disk artifact for a remote engine field, as the drift check
/// and the drift prompt both need it. Lifted verbatim from the old loop's
/// `codec.disk_bytes(...)` block.
fn remote_artifact(
    remote: &crate::model::EngineField,
) -> Result<crate::snapshot::codec::DiskArtifact> {
    let codec = crate::snapshot::codec::codec("engine_fields").unwrap();
    codec
        .disk_bytes(
            &serde_json::to_value(remote)
                .context("serializing remote engine field for drift check")?,
        )
        .context("codec disk_bytes for remote engine field")
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// plus the lockfile entry.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let codec = ...` down to and including the
/// `lockfile.upsert("engine_fields", ...)` call, with `updated` taken by
/// reference and the `progress.event(Action::Patch, ...)` line left behind at
/// the call site so the caller controls when it fires.
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    slug: &str,
    path: &std::path::Path,
    updated: &crate::model::EngineField,
) -> Result<()> {
    let codec = crate::snapshot::codec::codec("engine_fields").unwrap();
    let updated_art = codec
        .disk_bytes(
            &serde_json::to_value(updated)
                .context("serializing updated engine field for disk write")?,
        )
        .context("codec disk_bytes for updated engine field")?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the field is lockfile-pinned, so self + `engine` resolve to rdc://).
    let updated_bytes =
        crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
    let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, path, &updated_bytes).with_context(
        || format!("writing post-push canonical form for engine field '{slug}'"),
    )?;

    lockfile.upsert(
        "engine_fields",
        slug,
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

/// Resolve one drifted engine field interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value`, `resolve_push_drift`, then either PATCH (via the
/// same `update_engine_field` + `write_back`), adopt the remote, or skip. It
/// runs only on the sequential stage, so `resolve_push_drift`'s prompt can never
/// interleave with another item's. Returns `(pushed, skipped, read_only)`.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    slug: &str,
    path: &std::path::Path,
    remote_fields: &[crate::model::EngineField],
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize, bool)> {
    let entry = lockfile
        .objects
        .get("engine_fields")
        .and_then(|m| m.get(slug))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let disk_bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    crate::snapshot::refs::resolve_value(&mut payload, lockfile);
    let payload_field: crate::model::EngineField = serde_json::from_value(payload)
        .with_context(|| format!("deserializing overlay-applied engine field '{slug}'"))?;

    let Some(remote_field) = remote_fields.iter().find(|f| f.id == id) else {
        progress.event(
            Action::Skip,
            &format!("engine_field/{slug} (remote id {id} missing)"),
        );
        return Ok((0, 1, false));
    };
    let remote_art = remote_artifact(remote_field)?;
    let remote_bytes = remote_art.json;
    let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
    let mut payload_to_send = payload_field;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(interactive, path, &remote_bytes, env)? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited engine field '{slug}'"))?;
                crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited engine field '{slug}'"))?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the field is lockfile-pinned; self + engine resolve).
            let remote_bytes =
                crate::cli::pull::common::portabilize_proposed(&remote_bytes, lockfile);
            write_atomic(path, &remote_bytes)
                .with_context(|| format!("adopting remote into {}", path.display()))?;
            lockfile.upsert(
                "engine_fields",
                slug,
                ObjectEntry {
                    id,
                    modified_at: remote_field.modified_at().map(|s| s.to_string()),
                    modified_by: remote_field.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Warn,
                &format!("engine_field/{slug} adopted remote (drift)"),
            );
            return Ok((0, 1, false));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("engine_field/{slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1, false));
        }
    }

    // Strip server-managed fields from `extra` so the PATCH matches the
    // CREATE contract. (`name` is immutable cross-env but editable
    // within-env, so within-env strip leaves it intact.)
    strip_patch_extra(&mut payload_to_send.extra, "engine_fields", false);
    let patch_result = client
        .update_engine_field(id, &payload_to_send, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /engine_fields/{id}"));
    let updated = match patch_result {
        Ok(u) => u,
        Err(e) if anyhow_has_status(&e, 405) => {
            progress.event(
                Action::Skip,
                &format!("engine_field/{slug} (PATCH 405 — engine_fields read-only on this plan)"),
            );
            return Ok((0, 1, true));
        }
        Err(e) => {
            return Err(e);
        }
    };

    write_back(paths, lockfile, slug, path, &updated)?;
    progress.event(Action::Patch, &format!("engine_field/{slug}"));
    Ok((1, 0, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec D9: clean engine-field updates PATCH concurrently. Four fields
    /// whose PATCHes each take 200ms cost ~800ms in series and ~200-400ms
    /// fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_engine_fields_patches_updates_concurrently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let fields_dir = paths.engine_fields_dir("e-main");
        std::fs::create_dir_all(&fields_dir).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // The owning engine must be lockfile-pinned so `rdc://engines/e-main`
        // resolves on the way out and portabilizes on the way back in.
        lockfile.upsert(
            "engines",
            "e-main",
            ObjectEntry {
                id: 900,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let slugs = ["f-a", "f-b", "f-c", "f-d"];
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 600 + i as u64;
            let key = format!("e-main/{slug}");
            let local = serde_json::json!({
                "url": format!("rdc://engine_fields/{key}"),
                "name": slug,
                "engine": "rdc://engines/e-main",
            });
            std::fs::write(
                fields_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/engine_fields/{id}"),
                "name": slug,
                "engine": format!("{api}/engines/900"),
            });
            lockfile.upsert(
                "engine_fields",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("engine_fields").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "engine_fields",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(key, fields_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/engine_fields"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 600 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/engine_fields/{id}")))
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
        let start = std::time::Instant::now();
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
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

    /// The 405 stop, preserved across the fan-out.
    ///
    /// `engine_fields` are read-only on some plans: the PATCH comes back 405.
    /// The old sequential loop emitted ONE skip line and `break`ed out of the
    /// whole driver, so nothing after it — including a CREATE — was attempted.
    /// Fanning the run out means both PATCHes are dispatched before anything is
    /// known, but the transcript and the stop must still look the same: one
    /// skip line, and no POST.
    #[tokio::test]
    async fn push_engine_fields_stops_the_whole_driver_on_a_405() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let fields_dir = paths.engine_fields_dir("e-main");
        std::fs::create_dir_all(&fields_dir).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "engines",
            "e-main",
            ObjectEntry {
                id: 900,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in ["f-a", "f-b"].iter().enumerate() {
            let id = 600 + i as u64;
            let key = format!("e-main/{slug}");
            let local = serde_json::json!({
                "url": format!("rdc://engine_fields/{key}"),
                "name": slug,
                "engine": "rdc://engines/e-main",
            });
            std::fs::write(
                fields_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/engine_fields/{id}"),
                "name": slug,
                "engine": format!("{api}/engines/900"),
            });
            lockfile.upsert(
                "engine_fields",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("engine_fields").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "engine_fields",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(key, fields_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        // A create sorting AFTER both updates: the 405 must stop the driver
        // before this is ever POSTed.
        std::fs::write(
            fields_dir.join("z-new.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "url": "rdc://engine_fields/e-main/z-new",
                "name": "z-new",
                "engine": "rdc://engines/e-main",
            }))
            .unwrap(),
        )
        .unwrap();
        changes.insert(
            "e-main/z-new".to_string(),
            fields_dir.join("z-new.json"),
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/engine_fields"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": remotes
            })))
            .mount(&server)
            .await;
        for id in [600u64, 601] {
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/engine_fields/{id}")))
                .respond_with(ResponseTemplate::new(405))
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/api/v1/engine_fields"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 602,
                "url": format!("{api}/engine_fields/602"),
                "name": "z-new",
                "engine": format!("{api}/engines/900"),
            })))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("a 405 is a skip, not an error");

        assert_eq!(
            (pushed, skipped),
            (0, 1),
            "one skip line for the read-only plan, and nothing else counted",
        );
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(
            !reqs.iter().any(|r| r.method == wiremock::http::Method::POST),
            "the 405 must stop the driver before the create is attempted",
        );
    }
}
