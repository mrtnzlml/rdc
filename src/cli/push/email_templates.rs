use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::{strip_for_create, strip_patch_extra};
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// Choose which remote email template a local template should adopt, when the
/// local template has no lockfile entry yet. Among the remotes on the SAME
/// queue matching by `type` (non-`custom`) or `name` (`custom`), returns the
/// lowest-id candidate that is not already `claimed` by another local template
/// in this push. Returns `None` when every match is claimed (or there is no
/// match at all) — the caller then POSTs a fresh template.
///
/// The `claimed` gate is what makes multi-template-per-name queues safe: two
/// local siblings that share a name (e.g. two custom "Annotation status
/// change - received" templates on the same queue) must adopt DISTINCT remote
/// ids.
/// Without it, `find`-by-name collapses both onto the first match, orphaning
/// the sibling remote (deleted under `--allow-deletes` = data loss) and leaving
/// the surviving binding perpetually mismatched (churn). Selecting the lowest
/// unclaimed id — rather than an arbitrary `HashMap` iteration order — also
/// makes the pairing deterministic across runs and environments.
fn pick_adoption_id(
    remotes: &std::collections::HashMap<u64, crate::model::EmailTemplate>,
    local_queue: &Option<String>,
    local_type: Option<&str>,
    local_name: &str,
    claimed: &HashSet<u64>,
) -> Option<u64> {
    remotes
        .values()
        .filter(|r| {
            !claimed.contains(&r.id)
                && r.queue == *local_queue
                && match local_type {
                    Some(t) if t != "custom" => {
                        r.extra.get("type").and_then(|v| v.as_str()) == Some(t)
                    }
                    _ => r.name == local_name,
                }
        })
        .map(|r| r.id)
        .min()
}

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
    // Fetched at most ONCE for the whole push and SHARED by both branches: the
    // adoption matcher below and the drift check in [`push_update_batch`] read
    // the same list, and whichever runs first pays for it. `is_empty()` is the
    // "not populated yet" sentinel, exactly as before.
    let mut remote_cache: std::collections::HashMap<u64, crate::model::EmailTemplate> =
        std::collections::HashMap::new();

    // Remote ids already owned by a local template — either recorded in the
    // lockfile (a prior sync pinned them) or adopted earlier in THIS push.
    // The adoption matcher skips these so same-name siblings on one queue each
    // claim a DISTINCT remote id instead of collapsing onto the first match.
    let mut claimed: HashSet<u64> = lockfile
        .objects
        .get("email_templates")
        .map(|m| m.values().map(|e| e.id).collect())
        .unwrap_or_default();

    // Updates fan out (the two-stage shape in [`push_update_batch`]); the
    // adopt-or-create branch below stays strictly sequential and is left
    // ENTIRELY alone. It is stateful by construction — it threads `claimed` and
    // `remote_cache` across iterations so `pick_adoption_id` hands each local
    // sibling a distinct remote id, and it upserts the lockfile mid-branch.
    //
    // The two are also NOT partitioned into "all creates, then all updates": a
    // template's refs are resolved against the lockfile AS IT STANDS when that
    // template is prepared, so hoisting a create ahead of an earlier-sorting
    // update would resolve a ref that used to stay unresolved, and change what
    // this command sends (`push::hooks` carries an observable instance of
    // exactly that). So `changes` is still walked in slug order and each
    // MAXIMAL RUN of consecutive updates is fanned out, with an adopt-or-create
    // acting as a barrier.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    // slug (lockfile_key) = "ws_slug/q_slug/template_slug"
    for (lockfile_key, template_path) in changes {
        // Missing lockfile entry → try to adopt an existing remote template
        // (Rossum auto-creates typed defaults per queue; blind POST → 400 or
        // duplicate). Match on type+queue (or name+queue for custom types),
        // then PATCH local content into the adopted id. Fall through to POST
        // only when there is genuinely no matching remote template.
        if lockfile
            .objects
            .get("email_templates")
            .and_then(|m| m.get(lockfile_key.as_str()))
            .is_none()
        {
            // Close the pending run first: every update sorting BEFORE this
            // adopt-or-create must be prepared against a lockfile that does not
            // yet know the id it is about to pin.
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
            let disk_bytes = std::fs::read(template_path)
                .with_context(|| format!("reading {}", template_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", template_path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);

            // Identify the local template's queue + match key BEFORE stripping.
            let local: crate::model::EmailTemplate = serde_json::from_value(payload.clone())
                .with_context(|| {
                    format!("deserializing local email template '{lockfile_key}'")
                })?;
            let local_type = local.extra.get("type").and_then(|v| v.as_str());
            let local_queue = local.queue.clone();

            // Populate the remote cache once (shared with the PATCH branch).
            if remote_cache.is_empty() {
                for r in client
                    .list_email_templates(Some(progress.clone()))
                    .await
                    .context("listing email templates to adopt server-managed defaults")?
                {
                    remote_cache.insert(r.id, r);
                }
            }

            // Match an existing remote template on the SAME queue by `type`
            // (unless "custom") else by `name`. Rossum auto-creates default
            // templates per queue (some unique-typed → POST 400s; the custom
            // ones → silent duplicates), so adopt rather than POST. `claimed`
            // guarantees each local sibling takes a distinct remote id (see
            // `pick_adoption_id`); when a name-colliding sibling has no
            // unclaimed match left, `adopt` is `None` and we POST a fresh one.
            let adopt = pick_adoption_id(
                &remote_cache,
                &local_queue,
                local_type,
                &local.name,
                &claimed,
            )
            .and_then(|id| remote_cache.get(&id).cloned());

            if let Some(remote) = adopt {
                // Adopt the remote id into the lockfile, then PATCH local content.
                let id = remote.id;
                claimed.insert(id);
                let mut to_send = local.clone();
                strip_patch_extra(&mut to_send.extra, "email_templates", false);
                let updated = client
                    .update_email_template(id, &to_send, Some(progress.clone()))
                    .await
                    .with_context(|| {
                        format!("PATCH /email_templates/{id} (adopting existing)")
                    })?;
                let codec = crate::snapshot::codec::codec("email_templates").unwrap();
                let updated_art = codec
                    .disk_bytes(
                        &serde_json::to_value(&updated)
                            .context("serializing adopted email template")?,
                    )
                    .context("codec disk_bytes for adopted email template")?;
                // Register the adopted id NOW so the template's own `url` (and
                // its `queue` ref) portabilizes to `rdc://` — concrete env URLs
                // must never touch disk, even transiently.
                lockfile.upsert(
                    "email_templates",
                    lockfile_key,
                    ObjectEntry {
                        id,
                        modified_at: updated.modified_at().map(|s| s.to_string()),
                        modified_by: updated.modified_by().map(|s| s.to_string()),
                        content_hash: None,
                        secrets_hash: None,
                    },
                );
                let updated_json =
                    crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
                let updated_hash = combined_hash(&updated_json, &updated_art.sidecars, lockfile);
                crate::state::base_cache::write_disk_and_cache(paths, template_path, &updated_json)
                    .with_context(|| format!("writing adopted form for '{lockfile_key}'"))?;
                lockfile.upsert(
                    "email_templates",
                    lockfile_key,
                    ObjectEntry {
                        id,
                        modified_at: updated.modified_at().map(|s| s.to_string()),
                        modified_by: updated.modified_by().map(|s| s.to_string()),
                        content_hash: Some(updated_hash),
                        secrets_hash: None,
                    },
                );
                progress.event(
                    Action::Patch,
                    &format!("email_template/{lockfile_key} adopted existing id={id}"),
                );
                pushed += 1;
                continue;
            }

            // No existing match → POST as before (skip-and-continue on failure).
            strip_for_create(&mut payload, "email_templates");
            match client
                .create_email_template(&payload, Some(progress.clone()))
                .await
            {
                Ok(created) => {
                    claimed.insert(created.id);
                    let codec = crate::snapshot::codec::codec("email_templates").unwrap();
                    let created_art = codec
                        .disk_bytes(
                            &serde_json::to_value(&created)
                                .context("serializing created email template")?,
                        )
                        .context("codec disk_bytes for created email template")?;
                    // Register the new template's id NOW so its own `url` (and
                    // its `queue` ref) portabilizes to `rdc://` — concrete env
                    // URLs must never touch disk, even transiently.
                    lockfile.upsert(
                        "email_templates",
                        lockfile_key,
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
                    let created_hash =
                        combined_hash(&created_bytes, &created_art.sidecars, lockfile);
                    crate::state::base_cache::write_disk_and_cache(paths, template_path, &created_bytes).with_context(|| {
                        format!("writing post-create canonical form for '{lockfile_key}'")
                    })?;
                    lockfile.upsert(
                        "email_templates",
                        lockfile_key,
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
                        &format!("email_template/{lockfile_key} id={}", created.id),
                    );
                    pushed += 1;
                }
                Err(e) => {
                    // Skip-and-continue (mirror the DELETE driver): a stray 400
                    // (e.g. unique-type conflict) must not abort the whole push.
                    progress.event(
                        Action::Warn,
                        &format!(
                            "email_template/{lockfile_key} create failed (skipped): {e:#}"
                        ),
                    );
                    skipped += 1;
                }
            }
            continue;
        }

        batch.push((lockfile_key, template_path));
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

/// Fan out one maximal run of consecutive email-template UPDATES, then apply
/// the results.
///
/// The two-stage shape established by `push::rules`: a concurrent stage that
/// needs only `&Lockfile`, touches neither the working tree nor the lockfile and
/// never prompts, then a sequential apply stage in slug order that owns
/// `&mut Lockfile`, the filesystem and every prompt. `batch` is drained.
///
/// `remote_cache` is the caller's one-per-push template list, SHARED with the
/// adopt-or-create branch, so several runs plus any number of adoptions still
/// cost a single `GET /email_templates` — and a push whose updates all lack a
/// `content_hash` and whose creates all adopt nothing still costs none.
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    remote_cache: &mut std::collections::HashMap<u64, crate::model::EmailTemplate>,
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
    // at least one update can actually reach the drift check. The old lazy fill
    // happened at the first item that got PAST the `content_hash` guard, so a
    // run of entries that all lack a hash made no list call at all; keep that
    // exactly. `remote_cache` is the caller's, so the adopt-or-create branch
    // and every other run reuse whatever this fetch (or theirs) put there.
    let needs_drift_check = updates.iter().any(|(key, _)| {
        lockfile
            .objects
            .get("email_templates")
            .and_then(|m| m.get(key.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if remote_cache.is_empty() && needs_drift_check {
        let remotes = client
            .list_email_templates(Some(progress.clone()))
            .await
            .context("listing email templates to verify no drift before push")?;
        for r in remotes {
            remote_cache.insert(r.id, r);
        }
    }

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref: &std::collections::HashMap<u64, crate::model::EmailTemplate> =
            &*remote_cache;
        prepare_all(
            updates.iter().copied(),
            |(lockfile_key, template_path)| async move {
                // Read BEFORE the `content_hash` guard, exactly as the old
                // sequential loop did: an unreadable file is an error even for
                // an entry that would otherwise be skipped.
                let disk_bytes = std::fs::read(template_path)
                    .with_context(|| format!("reading {}", template_path.display()))?;
                let entry = lf
                    .objects
                    .get("email_templates")
                    .and_then(|m| m.get(lockfile_key.as_str()))
                    .expect("batched as an update, so the entry exists");
                let Some(base) = drift_base(entry) else {
                    return Ok(Prepared::Skipped {
                        slug: lockfile_key.clone(),
                        event: format!("email_template/{lockfile_key} (no content_hash)"),
                    });
                };
                let id = entry.id;

                let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                    .with_context(|| format!("parsing {}", template_path.display()))?;
                crate::snapshot::refs::resolve_value(&mut payload, lf);
                let payload_template: crate::model::EmailTemplate =
                    serde_json::from_value(payload).with_context(|| {
                        format!("deserializing overlay-applied email template '{lockfile_key}'")
                    })?;

                let Some(remote_template) = remote_ref.get(&id) else {
                    return Ok(Prepared::Skipped {
                        slug: lockfile_key.clone(),
                        event: format!(
                            "email_template/{lockfile_key} (remote id {id} missing)"
                        ),
                    });
                };
                let remote_art = remote_artifact(remote_template)?;
                if combined_hash(&remote_art.json, &remote_art.sidecars, lf) != base {
                    // Drift. NOT patched here — the sequential stage owns the
                    // prompt.
                    return Ok(Prepared::NeedsPrompt {
                        slug: lockfile_key.clone(),
                    });
                }

                // Strip server-managed fields from `extra` so the PATCH matches
                // the CREATE contract (e.g. the `triggers` sub-resource refs).
                let mut payload_to_send = payload_template;
                strip_patch_extra(&mut payload_to_send.extra, "email_templates", false);
                let updated = client
                    .update_email_template(id, &payload_to_send, Some(progress.clone()))
                    .await
                    .with_context(|| format!("PATCH /email_templates/{id}"))?;
                Ok(Prepared::Patched {
                    slug: lockfile_key.clone(),
                    updated,
                })
            },
        )
        .await
    };

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem and every prompt. Every completed
    //     PATCH is recorded even if a sibling failed (spec D10), then the
    //     first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (key_in, template_path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one template's response over another template's file.
        if let Ok(p) = &item {
            debug_assert_eq!(p.slug(), key_in.as_str());
        }
        match item {
            // NOT `?`: by the time the apply stage runs, every clean PATCH in
            // the batch has already landed server-side. Returning early here
            // would leave the REMAINING items' completed PATCHes unrecorded —
            // the exact inconsistency D10 exists to shrink, and worse than the
            // old sequential loop, which never sent those requests at all.
            Ok(Prepared::Patched { slug, updated }) => {
                match write_back(paths, lockfile, &slug, template_path, &updated) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("email_template/{slug}"));
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
                    template_path,
                    remote_cache,
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

/// The canonical on-disk artifact for a remote email template, as the drift
/// check and the drift prompt both need it. Lifted verbatim from the old loop's
/// `codec.disk_bytes(...)` block.
fn remote_artifact(
    remote: &crate::model::EmailTemplate,
) -> Result<crate::snapshot::codec::DiskArtifact> {
    let codec = crate::snapshot::codec::codec("email_templates").unwrap();
    codec
        .disk_bytes(
            &serde_json::to_value(remote)
                .context("serializing remote email template for drift check")?,
        )
        .context("codec disk_bytes for remote email template")
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// plus the lockfile entry.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let codec = ...` down to and including the
/// `lockfile.upsert("email_templates", ...)` call, with `updated` taken by
/// reference and the `progress.event(Action::Patch, ...)` line left behind at
/// the call site so the caller controls when it fires.
fn write_back(
    paths: &Paths,
    lockfile: &mut Lockfile,
    lockfile_key: &str,
    template_path: &std::path::Path,
    updated: &crate::model::EmailTemplate,
) -> Result<()> {
    let codec = crate::snapshot::codec::codec("email_templates").unwrap();
    let updated_art = codec
        .disk_bytes(
            &serde_json::to_value(updated)
                .context("serializing updated email template for disk write")?,
        )
        .context("codec disk_bytes for updated email template")?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the template is lockfile-pinned, so self + `queue` resolve to rdc://).
    let updated_bytes =
        crate::cli::pull::common::portabilize_proposed(&updated_art.json, lockfile);
    let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, template_path, &updated_bytes)
        .with_context(|| {
            format!("writing post-push canonical form for email template '{lockfile_key}'")
        })?;

    lockfile.upsert(
        "email_templates",
        lockfile_key,
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

/// Resolve one drifted email template interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value`, `resolve_push_drift`, then either PATCH (via the
/// same `update_email_template` + `write_back`), adopt the remote, or skip. It
/// runs only on the sequential stage, so `resolve_push_drift`'s prompt can never
/// interleave with another item's. Returns `(pushed, skipped)` deltas.
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    lockfile_key: &str,
    template_path: &std::path::Path,
    remote_cache: &std::collections::HashMap<u64, crate::model::EmailTemplate>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let entry = lockfile
        .objects
        .get("email_templates")
        .and_then(|m| m.get(lockfile_key))
        .expect("only reached for an item that was batched as an update");
    let id = entry.id;

    let disk_bytes = std::fs::read(template_path)
        .with_context(|| format!("reading {}", template_path.display()))?;
    let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
        .with_context(|| format!("parsing {}", template_path.display()))?;
    crate::snapshot::refs::resolve_value(&mut payload, lockfile);
    let payload_template: crate::model::EmailTemplate = serde_json::from_value(payload)
        .with_context(|| {
            format!("deserializing overlay-applied email template '{lockfile_key}'")
        })?;

    let Some(remote_template) = remote_cache.get(&id).cloned() else {
        progress.event(
            Action::Skip,
            &format!("email_template/{lockfile_key} (remote id {id} missing)"),
        );
        return Ok((0, 1));
    };
    let remote_art = remote_artifact(&remote_template)?;
    let remote_bytes = remote_art.json;
    let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
    let mut payload_to_send = payload_template;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(
        interactive,
        crate::cli::resolve::ObjectRef { kind: "email_templates", slug: lockfile_key },
        template_path, &remote_bytes,
        env,
        progress,
    )? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value =
                    serde_json::from_slice(&bytes).with_context(|| {
                        format!("re-deserializing edited email template '{lockfile_key}'")
                    })?;
                crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                payload_to_send = serde_json::from_value(ov).with_context(|| {
                    format!("re-deserializing edited email template '{lockfile_key}'")
                })?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the template is lockfile-pinned; refs resolve).
            let remote_bytes =
                crate::cli::pull::common::portabilize_proposed(&remote_bytes, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, template_path, &remote_bytes)
                .with_context(|| format!("adopting remote into {}", template_path.display()))?;
            lockfile.upsert(
                "email_templates",
                lockfile_key,
                ObjectEntry {
                    id,
                    modified_at: remote_template.modified_at().map(|s| s.to_string()),
                    modified_by: remote_template.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Warn,
                &format!("email_template/{lockfile_key} adopted remote (drift)"),
            );
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("email_template/{lockfile_key} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
        }
    }

    // Strip server-managed fields from `extra` so the PATCH matches the
    // CREATE contract (e.g. the `triggers` sub-resource refs).
    strip_patch_extra(&mut payload_to_send.extra, "email_templates", false);
    let patch_result = client
        .update_email_template(id, &payload_to_send, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /email_templates/{id}"));
    let updated = patch_result?;

    write_back(paths, lockfile, lockfile_key, template_path, &updated)?;
    progress.event(Action::Patch, &format!("email_template/{lockfile_key}"));
    Ok((1, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EmailTemplate;
    use indexmap::IndexMap;
    use std::collections::HashMap;

    fn tpl(id: u64, name: &str, queue: &str, ty: &str) -> EmailTemplate {
        let mut extra = IndexMap::new();
        extra.insert("type".to_string(), serde_json::Value::String(ty.to_string()));
        EmailTemplate {
            id,
            url: String::new(),
            name: name.to_string(),
            subject: String::new(),
            queue: Some(format!("https://x/api/v1/queues/{queue}")),
            extra,
        }
    }

    fn cache(templates: Vec<EmailTemplate>) -> HashMap<u64, EmailTemplate> {
        templates.into_iter().map(|t| (t.id, t)).collect()
    }

    #[test]
    fn picks_lowest_id_match_deterministically() {
        // Two custom templates share a name on the same queue. The pick must be
        // the lowest id regardless of HashMap iteration order.
        let q = Some("https://x/api/v1/queues/1".to_string());
        let remotes = cache(vec![
            tpl(200, "received", "1", "custom"),
            tpl(100, "received", "1", "custom"),
        ]);
        let got = pick_adoption_id(&remotes, &q, Some("custom"), "received", &HashSet::new());
        assert_eq!(got, Some(100));
    }

    #[test]
    fn excludes_claimed_so_same_name_siblings_get_distinct_ids() {
        // The heart of the collision fix: once the first sibling claims id 100,
        // the second sibling must adopt the OTHER id (200), never re-claim 100.
        let q = Some("https://x/api/v1/queues/1".to_string());
        let remotes = cache(vec![
            tpl(100, "received", "1", "custom"),
            tpl(200, "received", "1", "custom"),
        ]);
        let mut claimed = HashSet::new();

        let first = pick_adoption_id(&remotes, &q, Some("custom"), "received", &claimed);
        assert_eq!(first, Some(100));
        claimed.insert(first.unwrap());

        let second = pick_adoption_id(&remotes, &q, Some("custom"), "received", &claimed);
        assert_eq!(second, Some(200), "second sibling must not re-adopt the claimed id");
    }

    #[test]
    fn returns_none_when_every_match_is_claimed() {
        // Fewer remotes than local siblings → the extra sibling has no match →
        // caller POSTs a fresh template (restores the missing one) rather than
        // colliding onto an already-claimed id.
        let q = Some("https://x/api/v1/queues/1".to_string());
        let remotes = cache(vec![tpl(100, "received", "1", "custom")]);
        let claimed = HashSet::from([100]);
        let got = pick_adoption_id(&remotes, &q, Some("custom"), "received", &claimed);
        assert_eq!(got, None);
    }

    #[test]
    fn custom_type_matches_by_name_not_type() {
        // A custom template on the queue with a DIFFERENT name is not a match.
        let q = Some("https://x/api/v1/queues/1".to_string());
        let remotes = cache(vec![tpl(100, "some-other-name", "1", "custom")]);
        let got = pick_adoption_id(&remotes, &q, Some("custom"), "received", &HashSet::new());
        assert_eq!(got, None);
    }

    #[test]
    fn non_custom_type_matches_by_type_ignoring_name() {
        // Server default templates (non-custom) match by `type`, so a renamed
        // local still adopts the queue's typed default.
        let q = Some("https://x/api/v1/queues/1".to_string());
        let remotes = cache(vec![tpl(100, "renamed-locally", "1", "annotation_rejected")]);
        let got = pick_adoption_id(
            &remotes,
            &q,
            Some("annotation_rejected"),
            "does-not-matter",
            &HashSet::new(),
        );
        assert_eq!(got, Some(100));
    }

    #[test]
    fn does_not_match_templates_on_a_different_queue() {
        // Same name, different queue → never adopted across queue boundaries.
        let q = Some("https://x/api/v1/queues/1".to_string());
        let remotes = cache(vec![tpl(100, "received", "2", "custom")]);
        let got = pick_adoption_id(&remotes, &q, Some("custom"), "received", &HashSet::new());
        assert_eq!(got, None);
    }

    /// Spec D9: clean email-template updates PATCH concurrently. Four templates
    /// whose PATCHes each take 200ms cost ~800ms in series and ~200-400ms
    /// fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_email_templates_patches_updates_concurrently() {
        use crate::snapshot::codec::combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let dir = paths.queue_email_templates_dir("main", "invoices");
        std::fs::create_dir_all(&dir).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // The owning queue must be lockfile-pinned so `rdc://queues/invoices`
        // resolves on the way out and portabilizes on the way back in.
        lockfile.upsert(
            "queues",
            "invoices",
            ObjectEntry {
                id: 42,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let slugs = ["t-a", "t-b", "t-c", "t-d"];
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 300 + i as u64;
            let key = format!("main/invoices/{slug}");
            let local = serde_json::json!({
                "url": format!("rdc://email_templates/{key}"),
                "name": slug,
                "subject": "Hello",
                "queue": "rdc://queues/invoices",
            });
            let tpl_path = dir.join(format!("{slug}.json"));
            std::fs::write(&tpl_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/email_templates/{id}"),
                "name": slug,
                "subject": "Hello",
                "queue": format!("{api}/queues/42"),
            });
            lockfile.upsert(
                "email_templates",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("email_templates").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "email_templates",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(key, tpl_path);
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/email_templates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 300 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/email_templates/{id}")))
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

    /// This driver's "create" is really an ADOPT-OR-CREATE branch that
    /// shares `remote_cache` (and `claimed`) with the update batch — the
    /// most stateful un-batched branch of any push driver. A regression here
    /// could silently duplicate the `GET /email_templates` the hoisted drift
    /// check already paid for, or let the adopt-or-create run concurrently
    /// with an update it must act as a barrier against.
    ///
    /// Two tracked (update) templates with a NEW template — matching no
    /// remote by name, so `pick_adoption_id` returns `None` and it genuinely
    /// POSTs — sorting between them. The pre-create flush must PATCH the
    /// first update BEFORE the POST, the trailing flush must PATCH the
    /// second update AFTER it, and the whole push must cost exactly one
    /// `GET /email_templates` despite that list being consulted by both the
    /// adoption matcher and the update batch's drift check.
    #[tokio::test]
    async fn push_email_templates_barriers_on_a_create_and_lists_only_once() {
        use crate::snapshot::codec::combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let dir = paths.queue_email_templates_dir("main", "invoices");
        std::fs::create_dir_all(&dir).unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // The owning queue must be lockfile-pinned so `rdc://queues/invoices`
        // resolves on the way out and portabilizes on the way back in.
        lockfile.upsert(
            "queues",
            "invoices",
            ObjectEntry {
                id: 42,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let tpl_json = |slug: &str, id: Option<u64>| {
            let mut v = serde_json::json!({
                "url": match id {
                    Some(id) => format!("{api}/email_templates/{id}"),
                    None => format!("rdc://email_templates/main/invoices/{slug}"),
                },
                "name": slug,
                "subject": "Hello",
                "queue": match id {
                    Some(_) => format!("{api}/queues/42"),
                    None => "rdc://queues/invoices".to_string(),
                },
                "type": "custom",
            });
            if let Some(id) = id {
                v["id"] = serde_json::json!(id);
            }
            v
        };

        let mut changes = BTreeMap::new();
        for (slug, id) in [("a-update", 700u64), ("z-update", 702u64)] {
            let key = format!("main/invoices/{slug}");
            let tpl_path = dir.join(format!("{slug}.json"));
            std::fs::write(
                &tpl_path,
                serde_json::to_vec_pretty(&tpl_json(slug, None)).unwrap(),
            )
            .unwrap();
            lockfile.upsert(
                "email_templates",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let codec = crate::snapshot::codec::codec("email_templates").unwrap();
            let art = codec.disk_bytes(&tpl_json(slug, Some(id))).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert(
                "email_templates",
                &key,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(key, tpl_path);
        }
        // No lockfile entry -> adopt-or-create. Sorts between the two
        // updates. Name "m-create" matches neither remote template's name
        // ("a-update" / "z-update"), so `pick_adoption_id` finds nothing and
        // this genuinely POSTs.
        let create_path = dir.join("m-create.json");
        std::fs::write(
            &create_path,
            serde_json::to_vec_pretty(&tpl_json("m-create", None)).unwrap(),
        )
        .unwrap();
        changes.insert("main/invoices/m-create".to_string(), create_path);

        // The drift list never contains the template created mid-push — same
        // as the old loop, whose cache was also filled before the POST.
        Mock::given(method("GET"))
            .and(path("/api/v1/email_templates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null },
                "results": [tpl_json("a-update", Some(700)), tpl_json("z-update", Some(702))],
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/email_templates"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(tpl_json("m-create", Some(701))),
            )
            .mount(&server)
            .await;
        for (slug, id) in [("a-update", 700u64), ("z-update", 702u64)] {
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/email_templates/{id}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(tpl_json(slug, Some(id))),
                )
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
            .filter(|r| r.url.path().starts_with("/api/v1/email_templates"))
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        assert_eq!(
            stream,
            vec![
                "GET /api/v1/email_templates".to_string(),
                "PATCH /api/v1/email_templates/700".to_string(),
                "POST /api/v1/email_templates".to_string(),
                "PATCH /api/v1/email_templates/702".to_string(),
            ],
            "the adopt-or-create must sit BETWEEN the two updates, and the \
             drift list must be fetched exactly once for the whole push \
             despite being shared with the adoption matcher",
        );
        assert_eq!(
            stream
                .iter()
                .filter(|r| *r == "GET /api/v1/email_templates")
                .count(),
            1,
            "remote_cache is shared between the adopt-or-create branch and \
             the update batch; a second run must reuse it, not refetch it",
        );
    }
}
