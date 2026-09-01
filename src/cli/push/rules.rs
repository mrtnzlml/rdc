use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::snapshot::create::{strip_for_create, strip_patch_extra};
use crate::snapshot::rule::{read_rule_value, serialize_rule, write_rule_code};
use crate::state::{Lockfile, ObjectEntry, rule_combined_hash};
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
    let rules_dir = paths.rules_dir();
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // Drift-check list, fetched at most ONCE for the whole push and owned here
    // so every run shares the single request. Populated lazily by the first run
    // that actually has an item able to reach the drift check, so a push whose
    // updates all lack a `content_hash` still makes no list call at all — see
    // [`push_update_batch`].
    let mut drift_rules: Option<Vec<crate::model::Rule>> = None;

    // Updates fan out (the two-stage shape in [`push_update_batch`]); creates
    // stay strictly sequential because POST assigns ids that later items
    // resolve against. But the two are NOT partitioned into "all creates, then
    // all updates": a rule's refs are resolved against the lockfile AS IT
    // STANDS when that rule is prepared, so hoisting a create ahead of an
    // earlier-sorting update would resolve a ref that used to defer, and change
    // what this command sends. `push::hooks` carries an observable instance of
    // exactly that (a `run_after` forward reference collapsing its documented
    // push-PATCH + relink-PATCH pair into a single PATCH); a rule's refs
    // normally point at queues and labels rather than other rules, but
    // `resolve_value` walks every string, so an `rdc://rules/<slug>` ref in a
    // rule body hits it too.
    //
    // So `changes` is still walked in slug order, and each MAXIMAL RUN of
    // consecutive updates is fanned out with a create acting as a barrier.
    // Within a run the concurrency is invisible: an update's write-back
    // rewrites only its OWN entry's hashes and its own files, and no sibling's
    // ref resolution or drift check reads those — ids, which are what refs
    // resolve through, never change on an update. The common steady-state push
    // (all updates, no creates) is a single run, so the full win is unchanged.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (slug, local_json_path) in changes {
        // CREATE — no lockfile entry yet.
        if lockfile
            .objects
            .get("rules")
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
                &rules_dir,
                &mut batch,
                &mut drift_rules,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;

            let mut payload = read_rule_value(&rules_dir, slug)
                .with_context(|| format!("reading local rule '{slug}' for create"))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "rules");
            let create_result = client
                .create_rule(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /rules (creating '{slug}')"));
            let created = create_result?;
            let (created_json, created_code) = serialize_rule(&created)?;
            // Register the new rule's id NOW so its own `url` (and any ref to an
            // already-created object) portabilizes to `rdc://`. Concrete env URLs
            // must never touch disk, even transiently (an interrupted sync whose
            // portabilize post-pass never runs would freeze them into the snapshot).
            lockfile.upsert(
                "rules",
                slug,
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
            let created_hash = rule_combined_hash(&created_json, &created_code, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, local_json_path, &created_json)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            let created_py_path = rules_dir.join(format!("{slug}.py"));
            if let Some(code) = &created_code {
                write_rule_code(&rules_dir, slug, code)
                    .with_context(|| format!("writing rule code for '{slug}'"))?;
                // Mirror the sidecar into the base cache, as the PATCH path
                // does — otherwise a later code conflict has no merge base.
                crate::state::base_cache::write(paths, &created_py_path, code.as_bytes())
                    .with_context(|| format!("caching base rule code for '{slug}'"))?;
            } else {
                crate::state::base_cache::forget(paths, &created_py_path)?;
            }
            lockfile.upsert(
                "rules",
                slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("rule/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }
        batch.push((slug, local_json_path));
    }

    // Flush the trailing run.
    let (batched_pushed, batched_skipped) = push_update_batch(
        paths,
        client,
        lockfile,
        interactive,
        &rules_dir,
        &mut batch,
        &mut drift_rules,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    Ok((pushed, skipped))
}

/// Fan out one maximal run of consecutive rule UPDATES, then apply the results.
///
/// The two-stage shape: a concurrent stage that needs only `&Lockfile`, touches
/// neither the working tree nor the lockfile and never prompts, then a
/// sequential apply stage in slug order that owns `&mut Lockfile`, the
/// filesystem and every prompt. `batch` is drained.
///
/// `drift_rules` is the caller's one-per-push cache of the fresh rule list, so
/// several runs still cost a single `GET /rules` — and a push whose updates all
/// lack a `content_hash` still costs none.
#[allow(clippy::too_many_arguments)]
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    rules_dir: &std::path::Path,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    drift_rules: &mut Option<Vec<crate::model::Rule>>,
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
    // `remote_rules` cache was populated by the first item that got PAST the
    // `content_hash` guard, so a run of entries that all lack a hash made no
    // list call at all; keep that exactly, and keep it caller-owned so several
    // runs share the single fetch. Otherwise this is the same single request
    // the lazy cache used to make — it just no longer sits behind the first
    // item's PATCH.
    let needs_drift_check = updates.iter().any(|(slug, _)| {
        lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(slug.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if drift_rules.is_none() && needs_drift_check {
        *drift_rules = Some(
            client
                .list_rules(Some(progress.clone()))
                .await
                .context("listing rules to verify no drift before push")?,
        );
    }
    // Empty only when nothing in this run can consult it: an entry with no
    // `content_hash` returns `Prepared::Skipped` before the list is ever
    // touched, and by construction that is then every entry in the run.
    let remote_rules: &[crate::model::Rule] = drift_rules.as_deref().unwrap_or(&[]);

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref = remote_rules;
        let dir_ref = rules_dir;
        prepare_all(updates.iter().copied(), |(slug, _path)| async move {
            let entry = lf
                .objects
                .get("rules")
                .and_then(|m| m.get(slug.as_str()))
                .expect("batched as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("rule/{slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            let mut payload = read_rule_value(dir_ref, slug)
                .with_context(|| format!("reading local rule '{slug}'"))?;
            crate::snapshot::refs::resolve_value(&mut payload, lf);
            let payload_rule: crate::model::Rule = serde_json::from_value(payload)
                .with_context(|| format!("deserializing overlay-applied rule '{slug}'"))?;

            let Some(remote_rule) = remote_ref.iter().find(|r| r.id == id) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("rule/{slug} (remote id {id} missing)"),
                });
            };
            let (remote_json, remote_code) = serialize_rule(remote_rule)?;
            if rule_combined_hash(&remote_json, &remote_code, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the prompt.
                return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
            }

            // Strip server-managed fields from `extra` so the PATCH matches the
            // CREATE contract.
            let mut payload_to_send = payload_rule;
            strip_patch_extra(&mut payload_to_send.extra, "rules", false);
            let updated = client
                .update_rule(id, &payload_to_send, Some(progress.clone()))
                .await
                .with_context(|| format!("PATCH /rules/{id}"))?;
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
    for (item, (slug_in, local_json_path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one rule's response over another rule's file.
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
                match write_back(paths, rules_dir, lockfile, &slug, local_json_path, &updated) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("rule/{slug}"));
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
            // Deliberate, and inherited by Tasks 10-12: this arm still runs
            // when an earlier item already failed. Suppressing the prompt once
            // `first_error` is set would leave a drifted item neither prompted
            // nor recorded — worse than prompting on a run that will fail
            // anyway — and would contradict D10's principle that the apply
            // stage completes all the work it can before propagating.
            // Also NOT `?`, for the same reason as the `Patched` arm above.
            Ok(Prepared::NeedsPrompt { slug }) => {
                match push_one_drifted(
                    paths,
                    client,
                    lockfile,
                    interactive,
                    rules_dir,
                    &slug,
                    local_json_path,
                    remote_rules,
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
/// per-item one, an item would consult an empty list and silently take the
/// "remote id missing" skip instead of a real drift check — no error, just
/// wrong. One expression, called from both, so they cannot drift apart.
fn drift_base(entry: &ObjectEntry) -> Option<&str> {
    entry.content_hash.as_deref()
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// the `.py` sidecar (or its removal), and the lockfile entry.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let (updated_json, updated_code) = serialize_rule(&updated)?;` down to and
/// including the `lockfile.upsert("rules", ...)` call, with `updated` taken by
/// reference and the `progress.event(Action::Patch, ...)` line left behind at
/// the call site so the caller controls when it fires. The moved block's
/// `local_py_path` is recomputed here as `rules_dir.join(format!("{slug}.py"))`,
/// exactly as the loop head used to compute it.
fn write_back(
    paths: &Paths,
    rules_dir: &std::path::Path,
    lockfile: &mut Lockfile,
    slug: &str,
    local_json_path: &std::path::Path,
    updated: &crate::model::Rule,
) -> Result<()> {
    let local_py_path = rules_dir.join(format!("{slug}.py"));

    // Refresh local file with the codec's canonical form.
    let (updated_json, updated_code) = serialize_rule(updated)?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk (the rule is lockfile-pinned, so self + refs resolve to rdc://).
    let updated_json = crate::cli::pull::common::portabilize_proposed(&updated_json, lockfile);
    let updated_hash = rule_combined_hash(&updated_json, &updated_code, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, local_json_path, &updated_json)
        .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
    if let Some(code) = &updated_code {
        write_rule_code(rules_dir, slug, code)
            .with_context(|| format!("writing rule code for '{slug}'"))?;
        // Mirror the `.py` into the base cache (matching `pull::rules`) so a
        // later `BothDiverged` conflict can 3-way-merge the code against a
        // real base instead of falling back to a manual prompt.
        crate::state::base_cache::write(paths, &local_py_path, code.as_bytes())
            .with_context(|| format!("caching base rule code for '{slug}'"))?;
    } else {
        // Server dropped the trigger_condition; remove the stale .py from
        // disk AND the base cache so the snapshot stays canonical.
        if local_py_path.exists() {
            std::fs::remove_file(&local_py_path)
                .with_context(|| format!("removing stale {}", local_py_path.display()))?;
        }
        crate::state::base_cache::forget(paths, &local_py_path)?;
    }

    lockfile.upsert(
        "rules",
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

/// Resolve one drifted rule interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value`, `resolve_push_drift`, then either PATCH (via
/// the same `update_rule` + `write_back`), adopt the remote, or skip. It runs
/// only on the sequential stage, so `resolve_push_drift`'s prompt can never
/// interleave with another item's. Returns `(pushed, skipped)` deltas.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    rules_dir: &std::path::Path,
    slug: &str,
    local_json_path: &std::path::Path,
    remote_rules: &[crate::model::Rule],
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let local_py_path = rules_dir.join(format!("{slug}.py"));
    let entry = lockfile
        .objects
        .get("rules")
        .and_then(|m| m.get(slug))
        .expect("only reached for an item that was partitioned as an update");
    let id = entry.id;

    let mut payload = read_rule_value(rules_dir, slug)
        .with_context(|| format!("reading local rule '{slug}'"))?;
    crate::snapshot::refs::resolve_value(&mut payload, lockfile);
    let payload_rule: crate::model::Rule = serde_json::from_value(payload)
        .with_context(|| format!("deserializing overlay-applied rule '{slug}'"))?;

    let Some(remote_rule) = remote_rules.iter().find(|r| r.id == id) else {
        progress.event(
            Action::Skip,
            &format!("rule/{slug} (remote id {id} missing)"),
        );
        return Ok((0, 1));
    };
    let (remote_json, remote_code) = serialize_rule(remote_rule)?;
    let remote_combined = rule_combined_hash(&remote_json, &remote_code, lockfile);
    let mut payload_to_send = payload_rule;

    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(
        interactive,
        crate::cli::resolve::ObjectRef { kind: "rules", slug },
        local_json_path, &remote_json,
        env,
    )? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited rule '{slug}'"))?;
                crate::snapshot::refs::resolve_value(&mut ov, lockfile);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited rule '{slug}'"))?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the rule is lockfile-pinned; refs resolve).
            let remote_json = crate::cli::pull::common::portabilize_proposed(&remote_json, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, local_json_path, &remote_json)
                .with_context(|| format!("adopting remote into {}", local_json_path.display()))?;
            if let Some(code) = &remote_code {
                write_rule_code(rules_dir, slug, code)
                    .with_context(|| format!("adopting remote rule code for '{slug}'"))?;
                crate::state::base_cache::write(paths, &local_py_path, code.as_bytes())
                    .with_context(|| format!("caching base rule code for '{slug}'"))?;
            } else {
                if local_py_path.exists() {
                    std::fs::remove_file(&local_py_path)
                        .with_context(|| format!("removing stale {}", local_py_path.display()))?;
                }
                crate::state::base_cache::forget(paths, &local_py_path)?;
            }
            lockfile.upsert(
                "rules",
                slug,
                ObjectEntry {
                    id,
                    modified_at: remote_rule.modified_at().map(|s| s.to_string()),
                    modified_by: remote_rule.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Warn, &format!("rule/{slug} adopted remote (drift)"));
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("rule/{slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
        }
    }

    // Strip server-managed fields from `extra` so the PATCH matches the
    // CREATE contract.
    strip_patch_extra(&mut payload_to_send.extra, "rules", false);
    let patch_result = client
        .update_rule(id, &payload_to_send, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /rules/{id}"));
    let updated = patch_result?;

    write_back(paths, rules_dir, lockfile, slug, local_json_path, &updated)?;
    progress.event(Action::Patch, &format!("rule/{slug}"));
    Ok((1, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::snapshot::rule::serialize_rule;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Regression: after a rule PATCH push, the extracted `trigger_condition`
    /// sidecar (`<slug>.py`) must be mirrored into the base cache — exactly as
    /// `pull::rules` does — so a later `BothDiverged` conflict can 3-way-merge
    /// the code against a real base. Before the fix the push wrote the `.py`
    /// only to the working tree, leaving the base cache without it.
    #[tokio::test]
    async fn push_patch_rule_caches_code_sidecar_to_base() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let rules_dir = paths.rules_dir();
        std::fs::create_dir_all(&rules_dir).unwrap();

        let local = serde_json::json!({
            "name": "My Rule",
            "url": "rdc://rules/my-rule",
            "queues": [],
            "trigger": "annotation_content",
            "rule_actions": []
        });
        std::fs::write(
            rules_dir.join("my-rule.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(rules_dir.join("my-rule.py"), b"amount > 0\n").unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // Register the id before computing base so the self-url portabilizes.
        lockfile.upsert(
            "rules",
            "my-rule",
            ObjectEntry {
                id: 700,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // Remote rule carries the trigger_condition (extracted to the .py).
        let remote = serde_json::json!({
            "id": 700,
            "url": format!("{api}/rules/700"),
            "name": "My Rule",
            "queues": [],
            "trigger": "annotation_content",
            "rule_actions": [],
            "trigger_condition": "amount > 0\n"
        });
        let remote_rule: crate::model::Rule = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_rule(&remote_rule).unwrap();
        let base = rule_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "rules",
            "my-rule",
            ObjectEntry {
                id: 700,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote.clone()]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/rules/700"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut changes = BTreeMap::new();
        changes.insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!(pushed, 1, "the rule should be patched");

        let code_path = rules_dir.join("my-rule.py");
        let base_py = crate::state::base_cache::cache_mirror(&paths, &code_path)
            .expect("code path is under env root");
        assert!(
            base_py.exists(),
            "base cache must contain the rule code sidecar:\n{}",
            base_py.display()
        );
        assert_eq!(
            std::fs::read_to_string(&base_py).unwrap(),
            "amount > 0\n",
            "base cached code must match the pushed code"
        );
    }

    /// Shared fixture: `n` rules already in the lockfile with a matching base,
    /// so every one of them is a clean UPDATE. Returns (paths, lockfile,
    /// changes, the remote list body).
    fn seed_rules(
        tmp: &tempfile::TempDir,
        api: &str,
        slugs: &[&str],
    ) -> (Paths, Lockfile, BTreeMap<String, std::path::PathBuf>, serde_json::Value) {
        let paths = Paths::for_env(tmp.path(), "dev");
        let rules_dir = paths.rules_dir();
        std::fs::create_dir_all(&rules_dir).unwrap();
        let mut lockfile = Lockfile { api_base: api.to_string(), ..Lockfile::default() };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 700 + i as u64;
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://rules/{slug}"),
                "queues": [],
                "trigger": "annotation_content",
                "rule_actions": []
            });
            std::fs::write(
                rules_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            lockfile.upsert("rules", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: None, secrets_hash: None,
            });
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/rules/{id}"),
                "name": slug,
                "queues": [],
                "trigger": "annotation_content",
                "rule_actions": []
            });
            let remote_rule: crate::model::Rule =
                serde_json::from_value(remote.clone()).unwrap();
            let (rj, rc) = serialize_rule(&remote_rule).unwrap();
            let base = rule_combined_hash(&rj, &rc, &lockfile);
            lockfile.upsert("rules", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: Some(base), secrets_hash: None,
            });
            changes.insert(slug.to_string(), rules_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({
            "pagination": { "next": null }, "results": remotes
        });
        (paths, lockfile, changes, list)
    }

    /// Write a local rule with NO lockfile entry, so the driver treats it as a
    /// CREATE, and register it in `changes`.
    fn seed_create(
        paths: &Paths,
        changes: &mut BTreeMap<String, std::path::PathBuf>,
        slug: &str,
    ) {
        let rules_dir = paths.rules_dir();
        let body = serde_json::json!({
            "name": slug,
            "url": format!("rdc://rules/{slug}"),
            "queues": [],
            "trigger": "annotation_content",
            "rule_actions": []
        });
        std::fs::write(
            rules_dir.join(format!("{slug}.json")),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();
        changes.insert(slug.to_string(), rules_dir.join(format!("{slug}.json")));
    }

    async fn mount_rules_list(server: &MockServer, list: &serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(server)
            .await;
    }

    async fn mount_rule_create(server: &MockServer, api: &str, slug: &str, id: u64) {
        Mock::given(method("POST"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id,
                "url": format!("{api}/rules/{id}"),
                "name": slug,
                "queues": [],
                "trigger": "annotation_content",
                "rule_actions": []
            })))
            .mount(server)
            .await;
    }

    /// Request-count parity: the old lazy `remote_rules` cache was populated
    /// by the first update that got PAST the `content_hash` guard, so a batch
    /// in which every entry lacks a hash made NO list call at all. The hoisted
    /// fetch must keep that exactly — no `content_hash` anywhere means no
    /// request of any kind.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_makes_no_request_when_no_update_has_a_content_hash() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, _list) = seed_rules(&tmp, &api, &["r-a", "r-b"]);
        // Strip the recorded base from both entries, keeping their ids: each is
        // still an UPDATE (it has a lockfile entry), but neither can reach the
        // drift check. No mocks are mounted, so any request at all would both
        // fail and show up in `received_requests`.
        for (i, slug) in ["r-a", "r-b"].iter().enumerate() {
            lockfile.upsert(
                "rules",
                slug,
                ObjectEntry {
                    id: 700 + i as u64,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");

        assert_eq!((pushed, skipped), (0, 2), "both rules are skipped");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no update can reach the drift check, so nothing may be requested",
        );
    }

    /// Spec D10, apply stage: an error raised while APPLYING one item must not
    /// strand the completed PATCHes of the items AFTER it. By the time the
    /// apply stage runs, every clean PATCH in the batch has already landed
    /// server-side, so returning early would leave r-c's `content_hash` stale
    /// for a change the server has accepted — worse than the old sequential
    /// loop, which never sent r-c's request at all.
    ///
    /// r-b's write-back is what fails: its recorded path is outside the env
    /// tree, which `base_cache::write` refuses. Its PATCH still succeeds.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_records_a_later_patch_when_an_earlier_apply_fails() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, mut changes, list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c"]);
        let before: Vec<Option<String>> = ["r-a", "r-b", "r-c"]
            .iter()
            .map(|s| lockfile.objects["rules"][*s].content_hash.clone())
            .collect();
        // The concurrent stage reads the local file out of `rules_dir`, so r-b
        // is still read and PATCHed normally; only the write-back consults this
        // path, and `base_cache::write` bails on anything outside `env_root`.
        changes.insert(
            "r-b".to_string(),
            tmp.path().join("outside-env-tree").join("r-b.json"),
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..3usize {
            let id = 700 + i as u64;
            // Echo a changed name so a recorded write-back is observable (see
            // `push_rules_records_completed_patches_when_one_fails`).
            let mut body = list["results"][i].clone();
            body["name"] =
                serde_json::json!(format!("{} pushed", body["name"].as_str().unwrap()));
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let err = push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
            .await
            .expect_err("the failed write-back must propagate");
        assert!(
            format!("{err:#}").contains("env_root"),
            "error names the write-back failure: {err:#}"
        );

        let after: Vec<Option<String>> = ["r-a", "r-b", "r-c"]
            .iter()
            .map(|s| lockfile.objects["rules"][*s].content_hash.clone())
            .collect();
        assert_ne!(after[0], before[0], "the earlier item stays recorded");
        assert_ne!(
            after[2], before[2],
            "the LATER item's completed PATCH must still be recorded"
        );
        assert_eq!(after[1], before[1], "the un-applied item's base must not move");
    }

    /// Ordering: `changes` is walked in slug order with creates acting as
    /// barriers, NOT partitioned into "all creates, then all updates". `r-a`
    /// (an update) sorts before `r-z` (a create), so its PATCH must go out
    /// BEFORE the POST — exactly the request stream the pre-refactor
    /// sequential loop produced.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_keeps_slug_order_across_a_create_barrier() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, mut changes, list) = seed_rules(&tmp, &api, &["r-a"]);
        seed_create(&paths, &mut changes, "r-z");
        mount_rules_list(&server, &list).await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/rules/700"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list["results"][0].clone()))
            .mount(&server)
            .await;
        mount_rule_create(&server, &api, "r-z", 799).await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");
        assert_eq!((pushed, skipped), (2, 0));

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
                "GET /api/v1/rules".to_string(),
                "PATCH /api/v1/rules/700".to_string(),
                "POST /api/v1/rules".to_string(),
            ],
            "the update must be sent before the create that sorts after it",
        );
    }

    /// The reason the ordering above is load-bearing and not cosmetic: refs are
    /// resolved against the lockfile AS IT STANDS when an item is prepared.
    ///
    /// Rules DO make this observable, contrary to the assumption that they only
    /// reference queues and labels — `resolve_value` walks every string, so an
    /// `rdc://rules/r-z` ref in `r-a`'s body resolves through the same path.
    /// Prepared before the POST (correct, and what the sequential loop did),
    /// `r-z` is absent from the lockfile, the ref cannot resolve, and the
    /// client's pre-flight unresolved-ref guard refuses to send — so no PATCH
    /// and no POST reach the server. Prepared after the POST registered `r-z`'s
    /// id, the very same push would instead succeed and ship a concrete env
    /// URL. That difference is precisely what partitioning would have hidden.
    ///
    /// (Rules resolve eagerly; `push::hooks` defers such a field instead. That
    /// asymmetry is pre-existing behaviour, untouched here.)
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_resolves_an_update_against_the_pre_create_lockfile() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, mut changes, list) = seed_rules(&tmp, &api, &["r-a"]);
        let rules_dir = paths.rules_dir();
        seed_create(&paths, &mut changes, "r-z");

        // r-a names the rule that r-z is about to create, and sorts first.
        let mut local: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rules_dir.join("r-a.json")).unwrap()).unwrap();
        local["description"] = serde_json::json!("rdc://rules/r-z");
        std::fs::write(
            rules_dir.join("r-a.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();

        mount_rules_list(&server, &list).await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/rules/700"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list["results"][0].clone()))
            .mount(&server)
            .await;
        mount_rule_create(&server, &api, "r-z", 799).await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let err = push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
            .await
            .expect_err("the ref cannot resolve yet, so the guard must refuse");
        assert!(
            format!("{err:#}").contains("rdc://rules/r-z"),
            "the guard must name the unresolved ref: {err:#}"
        );
        let methods: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.method.to_string())
            .collect();
        assert!(
            !methods.iter().any(|m| m == "POST"),
            "r-z must not have been created before r-a was prepared, saw {methods:?}"
        );
    }

    /// Spec D9: clean updates PATCH concurrently. Four rules whose PATCHes each
    /// take 200ms cost ~800ms in series and ~200-400ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_patches_updates_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c", "r-d"]);

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for (i, slug) in ["r-a", "r-b", "r-c", "r-d"].iter().enumerate() {
            let id = 700 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(list["results"][i].clone())
                        .set_delay(std::time::Duration::from_millis(200)),
                )
                .mount(&server)
                .await;
            let _ = slug;
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
            elapsed < std::time::Duration::from_millis(650),
            "four 200ms PATCHes must overlap; sequential would be >= 800ms, took {elapsed:?}",
        );
    }

    /// Spec D9: a drifted item is never PATCHed on the concurrent path. It is
    /// deferred to the sequential pass, where non-interactive `resolve_push_drift`
    /// skips it — so its id must never appear in a PATCH, while its clean
    /// neighbours are patched normally.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_never_patches_a_drifted_item_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, mut list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c"]);
        // r-b (id 701) drifted: the remote now carries a name the recorded
        // base never saw, so its combined hash no longer matches.
        list["results"][1]["name"] = serde_json::json!("changed remotely");

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in [0usize, 2] {
            let id = 700 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(list["results"][i].clone()))
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");

        assert_eq!((pushed, skipped), (2, 1), "the drifted rule is skipped");
        let patched: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == http::Method::PATCH)
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(
            !patched.iter().any(|p| p.ends_with("/rules/701")),
            "the drifted rule must never be PATCHed, saw {patched:?}"
        );
        assert_eq!(patched.len(), 2, "only the two clean rules are patched");
    }

    /// Spec D10: when one item's PATCH fails, every PATCH that DID complete is
    /// still recorded before the error propagates. The sequential loop aborted
    /// with the failing item's siblings unrecorded; this must be strictly
    /// better, not worse.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_records_completed_patches_when_one_fails() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c"]);
        let before: Vec<Option<String>> = ["r-a", "r-b", "r-c"]
            .iter()
            .map(|s| lockfile.objects["rules"][*s].content_hash.clone())
            .collect();

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in [0usize, 2] {
            let id = 700 + i as u64;
            // The response must differ from the pre-PATCH remote: the recorded
            // `content_hash` is the hash of what the server echoed back, so if
            // the echo were byte-identical to the base, "was this PATCH
            // recorded?" would be unobservable. A server echoing the state it
            // just wrote (here, a normalized name) is exactly this case.
            let mut body = list["results"][i].clone();
            body["name"] =
                serde_json::json!(format!("{} pushed", body["name"].as_str().unwrap()));
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        // 400 is NOT retriable, so this fails immediately instead of burning
        // the retry budget.
        Mock::given(method("PATCH"))
            .and(path("/api/v1/rules/701"))
            .respond_with(ResponseTemplate::new(400).set_body_string("nope"))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let err = push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
            .await
            .expect_err("the failing PATCH must propagate");
        assert!(format!("{err:#}").contains("701"), "error names the failed rule: {err:#}");

        let after: Vec<Option<String>> = ["r-a", "r-b", "r-c"]
            .iter()
            .map(|s| lockfile.objects["rules"][*s].content_hash.clone())
            .collect();
        assert_ne!(after[0], before[0], "r-a's completed PATCH must be recorded");
        assert_ne!(after[2], before[2], "r-c's completed PATCH must be recorded");
        assert_eq!(after[1], before[1], "the failed rule's base must not move");
    }
}
