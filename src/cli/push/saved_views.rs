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
    let mut drift_saved_views: Option<Vec<crate::model::SavedView>> = None;

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. The two are NOT partitioned into "all creates, then all
    // updates": a saved view's refs are resolved against the lockfile AS IT STANDS
    // when that saved view is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that used to stay unresolved,
    // and change what this command sends (`push::hooks` carries an observable
    // instance of exactly that). So `changes` is still walked in slug order and
    // each MAXIMAL RUN of consecutive updates is fanned out, with a create
    // acting as a barrier.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (slug, path) in changes {
        // Missing lockfile entry → new saved view, POST.
        if lockfile
            .objects
            .get("saved_views")
            .and_then(|m| m.get(slug.as_str()))
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
                &mut drift_saved_views,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;

            let disk_bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "saved_views");
            // Checked AFTER `strip_for_create`, not before: a brand-new
            // object's own `url` is conventionally a self-referential
            // `rdc://saved_views/<own-slug>` (what `rdc migrate` and a
            // hand-scaffolded new-object file both write) and can never
            // resolve before the POST that creates it — but `url` is a
            // universal server field `strip_for_create` removes regardless,
            // so it must never trip this guard. See `ensure_no_residual_refs`.
            ensure_no_residual_refs(slug, &payload)?;
            let result = client
                .create_saved_view(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /saved_views (creating '{slug}')"));
            let created = result?;
            let codec = crate::snapshot::codec::codec("saved_views").unwrap();
            let created_art = codec
                .disk_bytes(&serde_json::to_value(&created).context("serializing created saved view")?)
                .context("codec disk_bytes for created saved view")?;
            // Register the new saved view's id NOW so its own `url` portabilizes to
            // `rdc://`. Concrete env URLs must never touch disk, even transiently
            // (an interrupted sync whose portabilize post-pass never runs would
            // freeze them into the snapshot).
            lockfile.upsert(
                "saved_views",
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
            crate::state::base_cache::write_disk_and_cache(paths, path, &created_bytes)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("saved_view/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        batch.push((slug, path));
    }

    // Flush the trailing run.
    let (batched_pushed, batched_skipped) = push_update_batch(
        paths,
        client,
        lockfile,
        interactive,
        &mut batch,
        &mut drift_saved_views,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// Fan out one maximal run of consecutive saved view UPDATES, then apply the results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem and every prompt. `batch` is drained.
///
/// `drift_saved_views` is the caller's one-per-push cache of the fresh saved-view list, so
/// several runs still cost a single `GET /saved_views` — and a push whose updates all
/// lack a `content_hash` still costs none.
#[allow(clippy::too_many_arguments)]
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    drift_saved_views: &mut Option<Vec<crate::model::SavedView>>,
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
    // at least one update can actually reach the drift check. The invariant:
    // the list is fetched if and only if some item gets PAST the
    // `content_hash` guard, so a run whose entries all lack a hash issues no
    // list call at all. It stays caller-owned so several runs of this function
    // share the single fetch.
    let needs_drift_check = updates.iter().any(|(slug, _)| {
        lockfile
            .objects
            .get("saved_views")
            .and_then(|m| m.get(slug.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if drift_saved_views.is_none() && needs_drift_check {
        *drift_saved_views = Some(
            client
                .list_saved_views(Some(progress.clone()))
                .await
                .context("listing saved views to verify no drift before push")?,
        );
    }
    // Empty only when nothing in this run can consult it: an entry with no
    // `content_hash` returns `Prepared::Skipped` before the list is ever
    // touched, and by construction that is then every entry in the run.
    let remote_saved_views: &[crate::model::SavedView] = drift_saved_views.as_deref().unwrap_or(&[]);

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref = remote_saved_views;
        prepare_all(updates.iter().copied(), |(slug, path)| async move {
            // Read BEFORE the `content_hash` guard, exactly as the old
            // sequential loop did: an unreadable file is an error even for an
            // entry that would otherwise be skipped.
            let disk_bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let entry = lf
                .objects
                .get("saved_views")
                .and_then(|m| m.get(slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("saved_view/{slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lf);
            let payload_view: crate::model::SavedView = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied saved view '{slug}'"))?;

            let Some(remote_view) = remote_ref.iter().find(|v| v.id == id) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("saved_view/{slug} (remote id {id} missing)"),
                });
            };
            let remote_art = remote_artifact(remote_view)?;
            if combined_hash(&remote_art.json, &remote_art.sidecars, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                // Deliberately NOT residual-ref-checked on this branch: a
                // drifted item is about to be handed to `push_one_drifted`,
                // which re-reads and re-resolves the file from scratch and may
                // resolve to Adopt/Skip, which never sends this (or any)
                // payload at all. Checking here would bail the whole push on
                // an item the interactive resolver might never have sent.
                return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
            }

            // Strip server-managed fields from `extra` so the PATCH matches the
            // CREATE contract.
            let mut payload_to_send = payload_view;
            strip_patch_extra(&mut payload_to_send.extra, "saved_views", false);
            // This IS the final payload for a clean (non-drifted) update — the
            // only branch of this closure that reaches an actual PATCH — so
            // the residual-ref guard is checked here, right before it is sent.
            // See `ensure_no_residual_refs`.
            let payload_value = serde_json::to_value(&payload_to_send)
                .context("serializing saved view for residual-ref check")?;
            ensure_no_residual_refs(slug, &payload_value)?;
            let updated = client
                .update_saved_view(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /saved_views/{id}"))?;
            Ok(Prepared::Patched {
                slug: slug.clone(),
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
    for (item, (slug_in, path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one saved view's response over another saved view's file.
        if let Ok(p) = &item {
            debug_assert_eq!(p.slug(), slug_in.as_str());
        }
        match item {
            // NOT `?`: by the time the apply stage runs, every clean PATCH in
            // the batch has already landed server-side. Returning early here
            // would leave the REMAINING items' completed PATCHes unrecorded —
            // the exact inconsistency D10 exists to shrink. So the loop always
            // runs to the end and the first error is returned afterwards.
            Ok(Prepared::Patched { slug, updated }) => {
                match write_back(paths, lockfile, &slug, path, &updated) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("saved_view/{slug}"));
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
                    path,
                    remote_saved_views,
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

/// Saved views do NOT participate in deferred relink. Dropping `queues_filter`
/// to `[]` would silently widen a view scoped to a few queues into one visible
/// to the WHOLE organization (that is the API's own semantics for an empty
/// filter), and `query` is required on POST so deferring it fails the create
/// outright. So every payload this driver actually sends is checked here,
/// after `resolve_value` (and after any subsequent stripping — the same
/// ordering `push` and `push_one_drifted` each apply before calling this): an
/// unresolved `rdc://` ref stops the push with a message naming it, rather
/// than sending a body that is quietly wrong, or (on the drift-resolution
/// path) than letting the server reject it with an opaque "Invalid hyperlink"
/// `400`.
///
/// One shared helper rather than the check pasted at each of this driver's
/// three call sites (the create path, the clean-update concurrent stage, and
/// the drift-resolution PATCH), so the message can never drift between them.
fn ensure_no_residual_refs(slug: &str, payload: &serde_json::Value) -> Result<()> {
    let residual = crate::snapshot::refs::residual_rdc_refs(payload);
    if !residual.is_empty() {
        anyhow::bail!(
            "saved view '{slug}' references objects that do not exist in this env: {}. \
             Create them first, or override `query`/`queues_filter` for this env in \
             overlay.toml.",
            residual.join(", ")
        );
    }
    Ok(())
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

/// The canonical on-disk artifact for a remote saved view, as the drift check
/// and the drift prompt both need it. One function so the bytes the concurrent
/// stage hashes and the bytes the prompt shows are the same bytes — a second
/// `codec.disk_bytes(...)` call site could diverge and make the prompt describe
/// a drift the check did not find.
fn remote_artifact(remote: &crate::model::SavedView) -> Result<crate::snapshot::codec::DiskArtifact> {
    let codec = crate::snapshot::codec::codec("saved_views").unwrap();
    codec
        .disk_bytes(
            &serde_json::to_value(remote).context("serializing remote saved view for drift check")?,
        )
        .context("codec disk_bytes for remote saved view")
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// plus the lockfile entry.
///
/// The single write-back path for this kind, shared by the clean PATCH and the
/// drift-prompt PATCH, so disk / base cache / lockfile can only ever advance
/// together. It deliberately does NOT emit the `Action::Patch` event: that is
/// left to each call site, which knows whether it is reporting a clean push or
/// a resolved drift.
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    slug: &str,
    path: &std::path::Path,
    updated: &crate::model::SavedView,
) -> Result<()> {
    let codec = crate::snapshot::codec::codec("saved_views").unwrap();
    let updated_art = codec
        .disk_bytes(
            &serde_json::to_value(updated).context("serializing updated saved view for disk write")?,
        )
        .context("codec disk_bytes for updated saved view")?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the saved view is lockfile-pinned, so its self-url resolves to rdc://).
    let updated_bytes =
        crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
    let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, path, &updated_bytes)
        .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;

    lockfile.upsert(
        "saved_views",
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

/// Resolve one drifted saved view interactively and, on `Patch`, send it.
///
/// Re-reads the local file, `resolve_value`, `resolve_push_drift`, then either
/// PATCHes (through the same `update_saved_view` + `write_back` as the clean
/// path), adopts the remote, or skips. It is called ONLY from the sequential
/// stage — that is the invariant that matters: `resolve_push_drift` blocks on
/// stdin, so a concurrent call site would interleave two prompts on one
/// terminal. Returns `(pushed, skipped)` deltas.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    slug: &str,
    path: &std::path::Path,
    remote_saved_views: &[crate::model::SavedView],
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let entry = lockfile
        .objects
        .get("saved_views")
        .and_then(|m| m.get(slug))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let disk_bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    crate::snapshot::refs::resolve_value(&mut payload, lockfile);
    let payload_view: crate::model::SavedView = serde_json::from_value(payload)
        .with_context(|| format!("deserializing overlay-applied saved view '{slug}'"))?;

    let Some(remote_view) = remote_saved_views.iter().find(|v| v.id == id) else {
        progress.event(
            Action::Skip,
            &format!("saved_view/{slug} (remote id {id} missing)"),
        );
        return Ok((0, 1));
    };
    let remote_art = remote_artifact(remote_view)?;
    let remote_bytes = remote_art.json;
    let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
    let mut payload_to_send = payload_view;

    // Drift detected. Spec §7.3 step 5: prompt on TTY; fall back to legacy
    // skip+warn otherwise.
    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(interactive, path, &remote_bytes, env)? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited saved view '{slug}'"))?;
                crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited saved view '{slug}'"))?;
            }
            // Fall through to PATCH below.
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the saved view is lockfile-pinned; self-url resolves).
            let remote_bytes =
                crate::cli::pull::common::portabilize_proposed(&remote_bytes, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, path, &remote_bytes)
                .with_context(|| format!("adopting remote into {}", path.display()))?;
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id,
                    modified_at: remote_view.modified_at().map(|s| s.to_string()),
                    modified_by: remote_view.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Warn,
                &format!("saved_view/{slug} adopted remote (drift)"),
            );
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("saved_view/{slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
        }
    }

    // Strip server-managed fields from `extra` so the PATCH matches the
    // CREATE contract.
    strip_patch_extra(&mut payload_to_send.extra, "saved_views", false);
    // Both `resolve_value` calls above (the initial read, and — only if the
    // user edited the file interactively — the re-read override) each
    // resolve their OWN draft of `payload_to_send`; only one draft is ever
    // the one reaching this point, since `Adopt`/`Skip` both return above
    // without using `payload_to_send` at all. So the residual-ref guard is
    // checked once, here, on whichever draft won — not right after either
    // individual `resolve_value` call, which would incorrectly bail before
    // the user ever saw the drift prompt on a push that was going to Adopt
    // or Skip and never send this payload. See `ensure_no_residual_refs`.
    let payload_value = serde_json::to_value(&payload_to_send)
        .context("serializing saved view for residual-ref check")?;
    ensure_no_residual_refs(slug, &payload_value)?;
    let result = client
        .update_saved_view(id, &payload_to_send, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /saved_views/{id}"));
    let updated = result?;

    write_back(paths, lockfile, slug, path, &updated)?;
    progress.event(Action::Patch, &format!("saved_view/{slug}"));
    Ok((1, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec D9: clean saved view updates PATCH concurrently. Four saved views whose
    /// PATCHes each take 200ms cost ~800ms in series and ~200-400ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_saved_views_patches_updates_concurrently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let saved_views_dir = paths.saved_views_dir();
        std::fs::create_dir_all(&saved_views_dir).unwrap();

        let slugs = ["v-a", "v-b", "v-c", "v-d"];
        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 500 + i as u64;
            // `shared` / `queues_filter` / `query` are typed `SavedView` fields
            // with no `skip_serializing_if`, so `list_saved_views` deserializing
            // this same body and re-serializing it for the drift check emits
            // them even when absent from the wire body. Setting them here
            // (rather than relying on the model's own defaults) keeps this
            // hand-built fixture's `disk_bytes` byte-identical to that
            // round-tripped one, which is what the drift check compares
            // against `base` below.
            let local = serde_json::json!({
                "url": format!("rdc://saved_views/{slug}"),
                "name": slug,
                "shared": true,
                "queues_filter": [],
                "query": { "$and": [] },
                "organization": format!("{api}/organizations/1"),
            });
            std::fs::write(
                saved_views_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/saved_views/{id}"),
                "name": slug,
                "shared": true,
                "queues_filter": [],
                "query": { "$and": [] },
                "organization": format!("{api}/organizations/1"),
            });
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("saved_views").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), saved_views_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/saved_views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 500 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/saved_views/{id}")))
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

    /// The create barrier and the caller-owned drift cache, pinned together.
    ///
    /// `changes` interleaves update / create / update in slug order, so the
    /// push splits into two runs with a POST between them. Two properties
    /// matter and neither is exercised by the all-updates concurrency test
    /// above:
    ///
    ///   1. The create is a BARRIER. `a-update` is prepared (and PATCHed)
    ///      before the POST, and `z-update` only afterwards — slug order is
    ///      preserved across the whole `changes` map, not just within a run.
    ///      Hoisting every create ahead of every update would resolve refs
    ///      that must still defer (see `push::hooks`, where that is
    ///      observable).
    ///   2. N runs still cost ONE `GET /saved_views`. That is the entire reason
    ///      the drift list is threaded through as `&mut Option<Vec<SavedView>>`
    ///      rather than being a local of the batch function; a per-run fetch
    ///      would issue one request per barrier instead of one per push.
    #[tokio::test]
    async fn push_saved_views_barriers_on_a_create_and_lists_only_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let saved_views_dir = paths.saved_views_dir();
        std::fs::create_dir_all(&saved_views_dir).unwrap();

        // `shared` / `queues_filter` / `query` are typed `SavedView` fields with
        // no `skip_serializing_if`; see the identical note in
        // `push_saved_views_patches_updates_concurrently` for why they must be
        // present here too so the drift check's round-tripped hash matches.
        let view_json = |slug: &str, id: Option<u64>| {
            let url = match id {
                Some(id) => format!("{api}/saved_views/{id}"),
                None => format!("rdc://saved_views/{slug}"),
            };
            let mut v = serde_json::json!({
                "url": url,
                "name": slug,
                "shared": true,
                "queues_filter": [],
                "query": { "$and": [] },
                "organization": format!("{api}/organizations/1"),
            });
            if let Some(id) = id {
                v.as_object_mut()
                    .unwrap()
                    .shift_insert(0, "id".to_string(), serde_json::json!(id));
            }
            v
        };

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        // Two tracked saved views (PATCH path) with a NEW saved view sorting between them.
        for (slug, id) in [("a-update", 510u64), ("z-update", 512u64)] {
            std::fs::write(
                saved_views_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&view_json(slug, None)).unwrap(),
            )
            .unwrap();
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("saved_views").unwrap();
            let art = codec.disk_bytes(&view_json(slug, Some(id))).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), saved_views_dir.join(format!("{slug}.json")));
        }
        // No lockfile entry -> create. Sorts between the two updates.
        std::fs::write(
            saved_views_dir.join("m-create.json"),
            serde_json::to_vec_pretty(&view_json("m-create", None)).unwrap(),
        )
        .unwrap();
        changes.insert("m-create".to_string(), saved_views_dir.join("m-create.json"));

        // The drift list never contains the saved view created mid-push: it is
        // fetched once, before the POST, and a created object has no
        // `content_hash` to drift against anyway.
        Mock::given(method("GET"))
            .and(path("/api/v1/saved_views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null },
                "results": [view_json("a-update", Some(510)), view_json("z-update", Some(512))]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/saved_views"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(view_json("m-create", Some(511))),
            )
            .mount(&server)
            .await;
        for (slug, id) in [("a-update", 510u64), ("z-update", 512u64)] {
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/saved_views/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(view_json(slug, Some(id))))
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!((pushed, skipped), (3, 0));

        let reqs = server.received_requests().await.unwrap_or_default();
        let stream: Vec<String> = reqs
            .iter()
            .filter(|r| r.url.path().starts_with("/api/v1/saved_views"))
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        assert_eq!(
            stream,
            vec![
                "GET /api/v1/saved_views".to_string(),
                "PATCH /api/v1/saved_views/510".to_string(),
                "POST /api/v1/saved_views".to_string(),
                "PATCH /api/v1/saved_views/512".to_string(),
            ],
            "the create must sit BETWEEN the two updates, and the drift list \
             must be fetched exactly once for both runs",
        );
    }

    /// The OTHER end of the hoisted fetch: a push in which no update carries a
    /// `content_hash` must issue NO list call at all, because no item can reach
    /// the drift check. `needs_drift_check` is what keeps the hoist guard and
    /// the per-item guard (`drift_base`) the same predicate; were the hoist
    /// unconditional, this push would spend a `GET /saved_views` on a list
    /// nothing reads.
    #[tokio::test]
    async fn push_saved_views_makes_no_request_when_no_update_has_a_content_hash() {
        use wiremock::MockServer;

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let saved_views_dir = paths.saved_views_dir();
        std::fs::create_dir_all(&saved_views_dir).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        // Both are UPDATES (each has a lockfile entry) but neither carries a
        // `content_hash`, so neither can reach the drift check. No mocks are
        // mounted, so any request at all would both fail and be recorded.
        for (i, slug) in ["v-a", "v-b"].iter().enumerate() {
            let local = serde_json::json!({
                "url": format!("rdc://saved_views/{slug}"),
                "name": slug,
                "organization": format!("{api}/organizations/1"),
            });
            std::fs::write(
                saved_views_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            lockfile.upsert(
                "saved_views",
                slug,
                ObjectEntry {
                    id: 520 + i as u64,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), saved_views_dir.join(format!("{slug}.json")));
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");

        assert_eq!((pushed, skipped), (0, 2), "both saved views are skipped");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no update can reach the drift check, so nothing may be requested",
        );
    }

    #[test]
    fn ensure_no_residual_refs_bails_on_a_dangling_ref() {
        let payload = serde_json::json!({
            "name": "mine",
            "queues_filter": ["rdc://queues/does-not-exist"],
        });
        let err = ensure_no_residual_refs("mine", &payload)
            .expect_err("a dangling ref must stop the push");
        let msg = err.to_string();
        assert!(msg.contains("mine"), "{msg}");
        assert!(msg.contains("rdc://queues/does-not-exist"), "{msg}");
    }

    #[test]
    fn ensure_no_residual_refs_passes_a_fully_resolved_payload() {
        let payload = serde_json::json!({
            "name": "mine",
            "queues_filter": ["https://acme.rossum.app/api/v1/queues/1"],
        });
        ensure_no_residual_refs("mine", &payload)
            .expect("a fully-resolved payload must pass");
    }

    /// The guard in `push_one_drifted` is checked ONCE, on whichever draft of
    /// `payload_to_send` survives the drift-resolution match — not right
    /// after either individual `resolve_value` call. Pin that placement: a
    /// drifted item whose local file ALSO carries a dangling ref must still
    /// just Skip (the ordinary non-interactive drift outcome) rather than
    /// bailing the whole push, because a non-interactive drift resolution is
    /// always `Skip` and never reaches `payload_to_send` at all. Placing the
    /// guard right after the first `resolve_value` instead would regress
    /// this: it would bail before the user (on a TTY) ever saw the drift
    /// prompt, even though `Adopt`/`Skip` never send that payload.
    #[tokio::test]
    async fn push_saved_views_skips_a_drifted_item_with_a_dangling_ref_without_bailing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let saved_views_dir = paths.saved_views_dir();
        std::fs::create_dir_all(&saved_views_dir).unwrap();

        let slug = "drifted-with-ref";
        let id = 530u64;
        // The dangling ref would fail `ensure_no_residual_refs` if it were
        // ever checked for this item — which non-interactively it must not
        // be, since the drift resolves to Skip before `payload_to_send` is
        // ever built from this file.
        let local = serde_json::json!({
            "url": format!("rdc://saved_views/{slug}"),
            "name": slug,
            "shared": true,
            "queues_filter": ["rdc://queues/does-not-exist"],
            "query": { "$and": [] },
            "organization": format!("{api}/organizations/1"),
        });
        std::fs::write(
            saved_views_dir.join(format!("{slug}.json")),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // Base recorded from the object's PRE-drift remote shape.
        let base_remote = serde_json::json!({
            "id": id,
            "url": format!("{api}/saved_views/{id}"),
            "name": slug,
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [] },
            "organization": format!("{api}/organizations/1"),
        });
        lockfile.upsert(
            "saved_views",
            slug,
            ObjectEntry {
                id,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let codec = crate::snapshot::codec::codec("saved_views").unwrap();
        let art = codec.disk_bytes(&base_remote).unwrap();
        let base = combined_hash(&art.json, &art.sidecars, &lockfile);
        lockfile.upsert(
            "saved_views",
            slug,
            ObjectEntry {
                id,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );
        let mut changes = BTreeMap::new();
        changes.insert(slug.to_string(), saved_views_dir.join(format!("{slug}.json")));

        // The remote returned at push time has since changed (drift): a
        // different `name` than the one `base` was computed from.
        let mut drifted_remote = base_remote.clone();
        drifted_remote["name"] = serde_json::json!("renamed on the server");
        let list = serde_json::json!({
            "pagination": { "next": null },
            "results": [drifted_remote],
        });
        Mock::given(method("GET"))
            .and(path("/api/v1/saved_views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list))
            .mount(&server)
            .await;
        // No PATCH mock: a non-interactive Skip must never send one. Any
        // PATCH request at all would fail (no mock registered for it) and
        // be recorded below regardless.

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("a non-interactive drift must Skip, never bail on the dangling ref");

        assert_eq!((pushed, skipped), (0, 1));
        let patched: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == http::Method::PATCH)
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(patched.is_empty(), "the drifted item must never be PATCHed, saw {patched:?}");
    }

    /// Pins the concurrent-stage (clean, non-drifted update) guard call site.
    /// A tracked saved view whose local `queues_filter` holds a ref to a
    /// queue that does not exist in this env's lockfile must stop the push --
    /// naming the ref -- rather than PATCHing a body that would silently
    /// widen the view to the WHOLE organization (the API's own semantics for
    /// an empty `queues_filter`). Deleting or relocating the
    /// `ensure_no_residual_refs` call this driver makes right before
    /// `update_saved_view` would let this test pass with a real PATCH sent;
    /// asserting on both halves -- the error naming the ref, AND the absence
    /// of any PATCH request -- catches that an inverted or relocated guard
    /// cannot slip through on the error check alone.
    #[tokio::test]
    async fn push_saved_views_bails_on_a_dangling_ref_in_a_clean_update() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let saved_views_dir = paths.saved_views_dir();
        std::fs::create_dir_all(&saved_views_dir).unwrap();

        let slug = "mine";
        let id = 540u64;
        // No "queues" entry for this slug exists in the lockfile, so
        // `resolve_value` cannot rewrite the ref and it survives to the final
        // payload as a literal `rdc://` string.
        let local = serde_json::json!({
            "url": format!("rdc://saved_views/{slug}"),
            "name": slug,
            "shared": true,
            "queues_filter": ["rdc://queues/does-not-exist"],
            "query": { "$and": [] },
            "organization": format!("{api}/organizations/1"),
        });
        std::fs::write(
            saved_views_dir.join(format!("{slug}.json")),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // Remote is CLEAN (no drift): its shape matches exactly what `base`
        // is computed from below, so the item reaches the ordinary "clean
        // update" tail of the concurrent closure -- never the drift /
        // `NeedsPrompt` path (which a DIFFERENT test already pins).
        let remote = serde_json::json!({
            "id": id,
            "url": format!("{api}/saved_views/{id}"),
            "name": slug,
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [] },
            "organization": format!("{api}/organizations/1"),
        });
        lockfile.upsert(
            "saved_views",
            slug,
            ObjectEntry {
                id,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let codec = crate::snapshot::codec::codec("saved_views").unwrap();
        let art = codec.disk_bytes(&remote).unwrap();
        let base = combined_hash(&art.json, &art.sidecars, &lockfile);
        lockfile.upsert(
            "saved_views",
            slug,
            ObjectEntry {
                id,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );
        let mut changes = BTreeMap::new();
        changes.insert(slug.to_string(), saved_views_dir.join(format!("{slug}.json")));

        let list = serde_json::json!({ "pagination": { "next": null }, "results": [remote] });
        Mock::given(method("GET"))
            .and(path("/api/v1/saved_views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list))
            .mount(&server)
            .await;
        // No PATCH mock: the guard must stop the push before any PATCH -- one
        // sent regardless would fail (no mock registered) and be recorded
        // below either way.

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let err = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect_err("a dangling ref in a clean update must stop the push");
        let msg = err.to_string();
        assert!(msg.contains(slug), "{msg}");
        assert!(msg.contains("rdc://queues/does-not-exist"), "{msg}");

        let patched: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == http::Method::PATCH)
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(patched.is_empty(), "no PATCH may be sent, saw {patched:?}");
    }

    /// Pins the create-path guard call site (mirrors the clean-update test
    /// above). A brand-new saved view whose `queues_filter` holds a ref to a
    /// queue that does not exist in this env's lockfile must stop the push
    /// rather than POST a body that would silently widen the view to the
    /// WHOLE organization.
    #[tokio::test]
    async fn push_saved_views_bails_on_a_dangling_ref_in_a_create() {
        use wiremock::MockServer;

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let saved_views_dir = paths.saved_views_dir();
        std::fs::create_dir_all(&saved_views_dir).unwrap();

        let slug = "mine";
        let local = serde_json::json!({
            "url": format!("rdc://saved_views/{slug}"),
            "name": slug,
            "shared": true,
            "queues_filter": ["rdc://queues/does-not-exist"],
            "query": { "$and": [] },
            "organization": format!("{api}/organizations/1"),
        });
        std::fs::write(
            saved_views_dir.join(format!("{slug}.json")),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();

        // No lockfile entry -> create. No mocks are mounted at all: the
        // guard must stop the push before any request, POST included.
        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        changes.insert(slug.to_string(), saved_views_dir.join(format!("{slug}.json")));

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let err = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect_err("a dangling ref in a create must stop the push");
        let msg = err.to_string();
        assert!(msg.contains(slug), "{msg}");
        assert!(msg.contains("rdc://queues/does-not-exist"), "{msg}");

        let posted: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == http::Method::POST)
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(posted.is_empty(), "no POST may be sent, saw {posted:?}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no request of any kind may be sent before the guard runs",
        );
    }
}
