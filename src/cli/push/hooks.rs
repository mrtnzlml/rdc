use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::secrets::{HookSecrets, load_hook_secrets};
use crate::snapshot::create::strip_for_create;
use crate::snapshot::hook::{
    hook_code_extension, read_hook_value, serialize_hook, write_hook_code,
};
use crate::state::{Lockfile, ObjectEntry, hook_combined_hash, hook_secrets_hash};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Splice the local hook-secrets map for `slug` into a JSON body about
/// to be POSTed/PATCHed. Returns the hex hash of what was injected so
/// the caller can record it in the lockfile alongside the regular
/// `content_hash`. When no secrets are configured for the slug the
/// body is left untouched and the hash of an empty map is returned —
/// that's also the hash recorded for "this hook has no secrets",
/// distinguishing it from "we never tried to sync secrets" (`None`).
fn inject_hook_secrets(body: &mut Value, slug: &str, secrets: &HookSecrets) -> String {
    // `filled_kv_for_slug` strips any value equal to UNFILLED_SENTINEL
    // (rdc's pre-populated placeholder marker) so a half-edited
    // template never leaks a `"<unfilled>"` literal to the API.
    let kv = secrets.filled_kv_for_slug(slug);
    let hash = hook_secrets_hash(&kv);
    if !kv.is_empty()
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert(
            "secrets".to_string(),
            serde_json::to_value(&kv).expect("BTreeMap<String,String> serializes"),
        );
    }
    hash
}

/// `catalog_hooks` is the hook list the sync pipeline pulled during Phase
/// 1 (`list_remote`). It is used **only** for the store-extension orphan
/// check (Phase-1 freshness is sufficient: an orphan from a previously
/// interrupted sync was already committed to the server before this
/// cycle started, so the catalog will see it). The pre-PATCH drift
/// check below still does its own fresh `list_hooks` to preserve the
/// safety contract's "remote bytes at the moment of PATCH" guarantee.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    catalog_hooks: &[crate::model::Hook],
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    // Load hook secrets for this env (the gitignored `secrets/<env>.hook-secrets.json`).
    // Missing file → empty map, all injection sites become no-ops.
    let hook_secrets = load_hook_secrets(paths.root(), env)
        .with_context(|| format!("loading hook secrets for env '{env}'"))?;

    // Detect whether the secrets-only force-push pass at the bottom of
    // this function would do any work. Used to decide whether to open
    // the "pushing hooks" phase header at all — without this guard,
    // a sync with no hook changes AND no secret drift would still
    // print an empty section.
    let secrets_pass_has_work = || -> bool {
        for slug in hook_secrets.slugs() {
            let local = hook_secrets.filled_kv_for_slug(slug);
            let local_hash = hook_secrets_hash(&local);
            let lf_hash = lockfile
                .objects
                .get("hooks")
                .and_then(|m| m.get(slug.as_str()))
                .and_then(|e| e.secrets_hash.as_deref());
            if lf_hash != Some(local_hash.as_str()) {
                return true;
            }
        }
        false
    };
    if changes.is_empty() && !secrets_pass_has_work() {
        return Ok((0, 0));
    }

    let hooks_dir = paths.hooks_dir();
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // The pre-PATCH drift list: fetched at most once per push and shared by
    // every update batch below. Same lazy semantics as the old loop's cache —
    // the first item that gets PAST the `content_hash` guard pays for it, and
    // a push whose updates all lack a hash makes no list call at all. It stays
    // separate from `catalog_hooks` (Phase-1 data, used only for the
    // store-extension orphan check), so the safety contract's "remote bytes at
    // the moment of PATCH" guarantee is unchanged.
    let mut drift_hooks: Option<Vec<crate::model::Hook>> = None;

    // Updates fan out (Task 9's two-stage shape); creates stay strictly
    // sequential. But the two are NOT partitioned into "all creates, then all
    // updates" the way `push::rules` can afford to be. A hook's refs are
    // resolved against the lockfile AS IT STANDS when that hook is prepared,
    // so hoisting a create ahead of an earlier-sorting update would resolve a
    // ref that used to defer, and change what this command sends: an
    // already-deployed hook whose `run_after` names a hook created later in
    // the same pass would collapse the documented push-PATCH + relink-PATCH
    // pair into a single PATCH. `push::relink` and the integration test
    // `sync_push_hook_run_after_deferred_relink_on_the_patch_path` both pin
    // that ordering.
    //
    // So `changes` is still walked in slug order, and each MAXIMAL RUN of
    // consecutive updates is fanned out with a create acting as a barrier.
    // Within a run the concurrency is invisible: an update's write-back
    // rewrites only its OWN entry's hashes and its own files, and no sibling's
    // ref resolution or drift check reads those — ids, which are what refs
    // resolve through, never change on an update. The common steady-state
    // push (all updates, no creates) is one run, i.e. exactly Task 9's shape.
    let mut batch: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (slug, local_json_path) in changes {
        // Missing lockfile entry = new hook → POST. Local file becomes the
        // create payload; server response (with id/url assigned) overwrites
        // disk; lockfile gets a fresh entry.
        if lockfile
            .objects
            .get("hooks")
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
                &hooks_dir,
                &mut batch,
                &mut drift_hooks,
                &hook_secrets,
                relink,
                progress,
                env,
            )
            .await?;
            pushed += batched_pushed;
            skipped += batched_skipped;

            // Read + portabilize refs once; reused by both paths.
            let mut payload = read_hook_value(&hooks_dir, slug)
                .with_context(|| format!("reading local hook '{slug}' for create"))?;
            // Two-phase relink: resolve what we can; defer top-level fields whose
            // rdc:// refs target a hook not yet created (e.g. `run_after` pointing
            // at another new hook). The relink pass PATCHes them once all hooks
            // exist. `push_update_batch`'s patch path does the same.
            let deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);

            // Anomaly guard, then dispatch on extension type.
            let typed: crate::model::Hook = serde_json::from_value(payload.clone())
                .with_context(|| format!("deserializing hook '{slug}' for create"))?;
            crate::cli::deploy::store_extensions::check_store_extension_anomaly(&typed, slug, env)?;

            // Compute the secrets hash now (before injection) so the
            // lockfile entry written below carries the up-to-date value
            // regardless of which branch creates the hook. Filtered map
            // strips the sentinel so an unedited template doesn't shift
            // the hash and trigger a spurious force-push.
            let created_secrets_hash = hook_secrets_hash(&hook_secrets.filled_kv_for_slug(slug));

            let post_result: Result<crate::model::Hook> = async {
            let created = if typed.is_store_extension() {
                // Two-call create: orphan check → POST /hooks/create → PATCH.
                // The install endpoint takes a fixed minimal body; secrets
                // ride the subsequent PATCH instead.
                //
                // Orphan check reuses the catalog snapshot (Phase 1) rather
                // than refetching. An orphan is the trace of a previous
                // sync that was interrupted between POST /hooks/create and
                // the follow-up PATCH; that committed write predates this
                // cycle, so the Phase-1 list already saw it.
                let template_url_src = typed.hook_template().expect("check_store_extension_anomaly guarantees hook_template is Some for store extensions");
                // The snapshot carries the SOURCE env's hook_template URL, whose
                // host 404s here. Store templates are Rossum-global by id, so
                // retarget the host to THIS env (keeping the id) before install,
                // orphan-matching, and the reconcile PATCH. Falls back to the raw
                // URL if it isn't a `/hook_templates/<id>` URL.
                let template_url = crate::cli::deploy::store_extensions::retarget_hook_template(
                    template_url_src,
                    &lockfile.api_base,
                )
                .unwrap_or_else(|| template_url_src.to_string());
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert(
                        "hook_template".to_string(),
                        serde_json::Value::String(template_url.clone()),
                    );
                }
                let installed_id = match crate::cli::deploy::store_extensions::find_orphan(
                    catalog_hooks, &typed.name, &template_url,
                ) {
                    Some(orphan) => {
                        progress.event(Action::Info, &format!(
                            "hook/{slug} (adopting orphan store-extension id {})",
                            orphan.id
                        ));
                        orphan.id
                    }
                    None => {
                        let install_body =
                            crate::cli::deploy::store_extensions::build_install_body(&payload)?;
                        let installed = client
                            .create_hook_via_install(&install_body, Some(progress.clone()))
                            .await
                            .with_context(|| {
                                format!(
                                    "POST /hooks/create (installing store extension '{slug}')"
                                )
                            })?;
                        progress.event(Action::Info, &format!(
                            "hook/{slug} (installed store extension id {})",
                            installed.id
                        ));
                        installed.id
                    }
                };
                let mut body = serde_json::to_value(&typed)
                    .with_context(|| format!("serializing hook '{slug}' for store-extension PATCH"))?;
                // `typed` still holds the source-env hook_template; keep the
                // reconcile PATCH pointed at the retargeted (this-env) template.
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "hook_template".to_string(),
                        serde_json::Value::String(template_url.clone()),
                    );
                }
                // Strip server-managed fields (`status`, `test`, …) so the
                // PATCH matches the CREATE contract and never echoes the
                // redacted sentinel back to the API.
                strip_for_create(&mut body, "hooks");
                inject_hook_secrets(&mut body, slug, &hook_secrets);
                client
                    .update_hook_value(installed_id, &body, Some(progress.clone()))
                    .await
                    .with_context(|| {
                        format!(
                            "PATCH /hooks/{installed_id} (reconciling store extension '{slug}')"
                        )
                    })?
            } else {
                // Regular hook: strip server-only fields, inject secrets, POST.
                strip_for_create(&mut payload, "hooks");
                inject_hook_secrets(&mut payload, slug, &hook_secrets);
                client
                    .create_hook(&payload, Some(progress.clone()))
                    .await
                    .with_context(|| format!("POST /hooks (creating '{slug}')"))?
            };
            Ok(created)
            }.await;
            let created = post_result?;

            // Disk + lockfile write — same for both paths. The sidecar
            // extension is derived from the server's response runtime so
            // it stays canonical even if the local JSON declared a
            // different runtime.
            let (created_json, created_code) = serialize_hook(&created)?;
            // Register the new hook's id NOW so its own `url` (and any ref to an
            // already-created object) portabilizes to `rdc://` below. Concrete
            // env URLs must never be written to disk — not even transiently: an
            // interrupted sync (whose portabilize post-pass never runs) would
            // otherwise freeze the raw response's env URLs into the snapshot.
            lockfile.upsert(
                "hooks",
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
            let created_hash = hook_combined_hash(&created_json, &created_code, lockfile);
            let created_ext = hook_code_extension(&created);
            crate::state::base_cache::write_disk_and_cache(paths, local_json_path, &created_json)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            let created_code_path = hooks_dir.join(format!("{slug}.{created_ext}"));
            if let Some(code) = &created_code {
                write_hook_code(&hooks_dir, slug, code, created_ext)
                    .with_context(|| format!("writing hook code for '{slug}'"))?;
                // Mirror the code sidecar into the base cache, exactly as the
                // PATCH path does. Without it the cache holds the `.json` but
                // not the `.py`, so the first `BothDiverged` on this hook has
                // no base to 3-way-merge the code against.
                crate::state::base_cache::write(paths, &created_code_path, code.as_bytes())
                    .with_context(|| format!("caching base hook code for '{slug}'"))?;
            }
            // Sweep any stale sidecar with the *other* extension that may
            // have been left over from a previous runtime — from disk AND the
            // cache mirror.
            let other_created_ext = if created_ext == "py" { "js" } else { "py" };
            let stale_created = hooks_dir.join(format!("{slug}.{other_created_ext}"));
            if stale_created.exists() {
                std::fs::remove_file(&stale_created)
                    .with_context(|| format!("removing stale {}", stale_created.display()))?;
            }
            crate::state::base_cache::forget(paths, &stale_created)?;
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    modified_by: created.modified_by().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: Some(created_secrets_hash),
                },
            );
            if !deferred.is_empty() {
                relink.push(crate::cli::push::relink::DeferredRelink {
                    kind: "hooks".to_string(),
                    slug: slug.clone(),
                    path: local_json_path.clone(),
                    fields: deferred,
                });
            }
            progress.event(Action::Post, &format!("hook/{slug} id={}", created.id));
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
        &hooks_dir,
        &mut batch,
        &mut drift_hooks,
        &hook_secrets,
        relink,
        progress,
        env,
    )
    .await?;
    pushed += batched_pushed;
    skipped += batched_skipped;

    // Secrets-only force-push: a user can edit
    // `secrets/<env>.hook-secrets.json` without touching any hook JSON
    // or code. The main loop above only fires for hooks whose snapshot
    // bytes changed, so we'd miss the secrets-only edits without this
    // second pass. For every slug declared in the local secrets file,
    // re-hash and compare to the lockfile entry's `secrets_hash`; if
    // they differ, PATCH with just `{"secrets": {...}}` and bring the
    // lockfile entry up to date.
    //
    // Slugs that don't have a `hooks/<slug>.json` on disk are typos in
    // the secrets file and surface as warnings, not errors — a typo
    // shouldn't abort the sync, but it shouldn't ship secrets to the
    // wrong slug either (we just don't have a matching id to target).
    let mut secrets_pushed = 0usize;
    let mut secrets_warned: Vec<String> = Vec::new();
    for slug in hook_secrets.slugs() {
        // Sentinel-valued keys must not reach the API; the filtered
        // map drops them so a half-edited template doesn't PATCH a
        // literal `"<unfilled>"` into Rossum.
        let local_kv = hook_secrets.filled_kv_for_slug(slug);
        let local_hash = hook_secrets_hash(&local_kv);
        let entry = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug.as_str()));
        let Some(entry) = entry else {
            // No lockfile entry → either the hook hasn't been synced yet
            // (legitimate, will sync next) or the slug is a typo. Either
            // way we can't target a remote id, so just warn.
            secrets_warned.push(slug.clone());
            continue;
        };
        if entry.secrets_hash.as_deref() == Some(local_hash.as_str()) {
            continue; // already in sync
        }
        // PATCH with just `secrets` — Rossum's PATCH /hooks/<id>
        // accepts a partial body, so we don't need to send the whole
        // hook just to update one secret value.
        let body = serde_json::json!({ "secrets": local_kv });
        let secrets_result = client
            .update_hook_value(entry.id, &body, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /hooks/{} (secrets for '{}')", entry.id, slug));
        let updated = secrets_result?;
        // Carry forward the existing `content_hash`; only `secrets_hash`
        // and `modified_at` may have changed.
        let prior_content_hash = entry.content_hash.clone();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                modified_at: updated.modified_at().map(|s| s.to_string()),
                modified_by: updated.modified_by().map(|s| s.to_string()),
                content_hash: prior_content_hash,
                secrets_hash: Some(local_hash),
            },
        );
        progress.event(Action::Patch, &format!("hook/{slug} (secrets)"));
        secrets_pushed += 1;
    }
    if !secrets_warned.is_empty() {
        // One actionable line, not one per slug — the user typed them
        // in the same file so a list is the clear signal.
        progress.event(
            crate::log::Action::Warn,
            &format!(
                "secrets entries with no matching hook on env '{}': {}",
                env,
                secrets_warned.join(", "),
            ),
        );
    }

    Ok((pushed + secrets_pushed, skipped))
}

/// Fan out one maximal run of consecutive hook UPDATES, then apply the results.
///
/// Task 9's two-stage shape (`push::rules`), scoped to a run rather than to the
/// whole push: a concurrent stage that needs only `&Lockfile`, touches neither
/// the working tree nor the lockfile and never prompts, then a sequential apply
/// stage in slug order that owns `&mut Lockfile`, the filesystem, `relink` and
/// every prompt. `batch` is drained.
///
/// `drift_hooks` is the caller's one-per-push cache of the fresh hook list, so
/// several runs still cost a single `GET /hooks` — and a push whose updates all
/// lack a `content_hash` still costs none.
#[allow(clippy::too_many_arguments)]
async fn push_update_batch(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    hooks_dir: &std::path::Path,
    batch: &mut Vec<(&String, &std::path::PathBuf)>,
    drift_hooks: &mut Option<Vec<crate::model::Hook>>,
    hook_secrets: &HookSecrets,
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
    // cache was populated by the first item that got PAST the `content_hash`
    // guard, so a run of entries that all lack a hash made no list call at
    // all; keep that exactly, and keep it caller-owned so several runs share
    // the single fetch. The list is still FRESH (the fetch just no longer sits
    // behind the first item's PATCH) and still separate from `catalog_hooks`.
    let needs_drift_check = updates.iter().any(|(slug, _)| {
        lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug.as_str()))
            .and_then(drift_base)
            .is_some()
    });
    if drift_hooks.is_none() && needs_drift_check {
        *drift_hooks = Some(
            client
                .list_hooks(Some(progress.clone()))
                .await
                .context("listing hooks to verify no drift before push")?,
        );
    }
    // Empty only when nothing in this run can consult it: an entry with no
    // `content_hash` returns `Prepared::Skipped` before the list is ever
    // touched, and by construction that is then every entry in the run.
    let remote_hooks: &[crate::model::Hook] = drift_hooks.as_deref().unwrap_or(&[]);

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let prepared = {
        let lf: &Lockfile = &*lockfile;
        let remote_ref = remote_hooks;
        let dir_ref = hooks_dir;
        let secrets_ref = hook_secrets;
        prepare_all(updates.iter().copied(), |(slug, _path)| async move {
            let entry = lf
                .objects
                .get("hooks")
                .and_then(|m| m.get(slug.as_str()))
                .expect("partitioned as an update, so the entry exists");
            let Some(base) = drift_base(entry) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("hook/{slug} (no content_hash)"),
                });
            };
            let id = entry.id;

            // Read raw Value (with the sidecar code spliced in) BEFORE typed
            // deserialize.
            let mut payload = read_hook_value(dir_ref, slug)
                .with_context(|| format!("reading local hook '{slug}'"))?;
            // Two-phase relink, same as [`push`]'s create path and as
            // `queues`/`engines` do on BOTH of their paths. Hooks are pushed in
            // slug order, so an already-deployed hook whose `run_after` names one
            // created later in this same pass is an ordinary forward reference —
            // resolving eagerly here left an `rdc://` in the body and the
            // unresolved-ref guard aborted the entire cycle, permanently wedging
            // any env where a tracked hook points at a not-yet-created one.
            let deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lf);
            let payload_to_send: crate::model::Hook = serde_json::from_value(payload)
                .with_context(|| format!("deserializing hook '{slug}'"))?;

            // Drift check: find the remote in the hoisted list, serialize,
            // hash, compare to base.
            let Some(remote_hook) = remote_ref.iter().find(|h| h.id == id) else {
                return Ok(Prepared::Skipped {
                    slug: slug.clone(),
                    event: format!("hook/{slug} (remote id {id} missing)"),
                });
            };
            let (remote_json, remote_code) = serialize_hook(remote_hook)?;
            if hook_combined_hash(&remote_json, &remote_code, lf) != base {
                // Drift. NOT patched here — the sequential stage owns the
                // prompt, so `stdin_coord` stays the single stdin owner.
                return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
            }

            let (updated, secrets_hash) = send_patch(
                client,
                id,
                slug,
                &payload_to_send,
                &deferred,
                secrets_ref,
                progress,
            )
            .await?;
            Ok(Prepared::Patched {
                slug: slug.clone(),
                updated: HookPatched {
                    updated,
                    deferred,
                    secrets_hash: Some(secrets_hash),
                },
            })
        })
        .await
    };

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem, `relink` and every prompt. Every
    //     completed PATCH is recorded even if a sibling failed (spec D10),
    //     then the first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (slug_in, local_json_path)) in prepared.into_iter().zip(updates) {
        // `prepare_all` returns one result per item IN INPUT ORDER; this zip is
        // what pairs each result with its own file path, so pin that guarantee
        // where it is relied upon. A reordering primitive would silently write
        // one hook's response over another hook's file.
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
                match write_back(
                    paths,
                    hooks_dir,
                    lockfile,
                    relink,
                    &slug,
                    local_json_path,
                    updated,
                ) {
                    Ok(()) => {
                        progress.event(Action::Patch, &format!("hook/{slug}"));
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
                    hooks_dir,
                    &slug,
                    local_json_path,
                    remote_hooks,
                    hook_secrets,
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
    // The old sequential loop propagated with `?`, so a failed hook update
    // never reached the create that followed it, nor the secrets-only pass.
    // Keep that: bail out of the whole push, not just this run.
    if let Some(e) = first_error {
        return Err(e);
    }

    Ok((pushed, skipped))
}

/// What a hook's concurrent stage carries across to its apply stage.
///
/// A hook PATCH produces two pieces of state that are NOT recoverable from the
/// server response, so they ride along rather than being recomputed on the
/// sequential side (recomputing `deferred` would need the local file re-read
/// and re-resolved against a lockfile that later items have since mutated).
struct HookPatched {
    updated: crate::model::Hook,
    /// Fields held back from the PATCH by `resolve_value_deferring`; the apply
    /// stage turns these into `relink::DeferredRelink` entries.
    deferred: Vec<(String, Value)>,
    /// From `inject_hook_secrets`, for the lockfile entry. Always `Some` on the
    /// PATCH path — the injector returns the hash of an empty map when a hook
    /// has no secrets, which is itself meaningful ("no secrets", as opposed to
    /// `None`'s "never tried to sync secrets").
    secrets_hash: Option<String>,
}

/// Whether this entry can reach the drift check at all — and, if so, the base
/// the remote is checked against.
///
/// A hook entry carries TWO hashes and only `content_hash` gates this check.
/// `secrets_hash` tracks the separate `secrets/<env>.hook-secrets.json` file
/// and is what the secrets-only pass at the bottom of [`push`] compares; it
/// says nothing about whether the hook's snapshot bytes drifted remotely, and
/// the old sequential loop's guard (`let Some(base) = &entry.content_hash`)
/// never consulted it.
///
/// The hoisted list fetch and the per-item guard inside the concurrent stage
/// MUST agree on this predicate. If the hoist guard were ever narrower than the
/// per-item one, an item would consult an empty list and silently take the
/// "remote id missing" skip instead of a real drift check — no error, just
/// wrong. One expression, called from both, so they cannot drift apart.
fn drift_base(entry: &ObjectEntry) -> Option<&str> {
    entry.content_hash.as_deref()
}

/// Build the PATCH body for one hook and send it.
///
/// Shared by the concurrent stage and [`push_one_drifted`] so the two can never
/// disagree about what rides a hook PATCH. Lifted verbatim out of the old
/// update loop's tail, from `let mut body = serde_json::to_value(...)` down to
/// the `update_hook_value` call. Returns the server's response and the hash of
/// the injected secrets, for the lockfile entry.
async fn send_patch(
    client: &RossumClient,
    id: u64,
    slug: &str,
    payload_to_send: &crate::model::Hook,
    deferred: &[(String, Value)],
    hook_secrets: &HookSecrets,
    progress: &Arc<Log>,
) -> Result<(crate::model::Hook, String)> {
    // Build a Value form of the typed payload so secrets (which
    // have no place on the typed `Hook` model) can ride this PATCH.
    let mut body = serde_json::to_value(payload_to_send)
        .with_context(|| format!("serializing hook '{slug}' for PATCH"))?;
    // A deferred field must not ride this PATCH at all. Removing the key
    // from the Value is not enough on its own: `queues` is a MODELED field
    // on `Hook`, so the typed round-trip re-materializes it as `[]` and the
    // PATCH would detach the hook from every queue until the relink lands —
    // permanently if the relink never resolves. Omitting the key leaves the
    // remote's current value untouched, which is what deferral means.
    if let Some(obj) = body.as_object_mut() {
        for (field, _) in deferred {
            obj.remove(field);
        }
    }
    // `status` is a read-only server health field that's redacted to the
    // sentinel on disk; strip it (and the other server fields) so the
    // PATCH body matches the CREATE contract instead of echoing the
    // sentinel back. Done before secret injection so secrets survive.
    strip_for_create(&mut body, "hooks");
    let secrets_hash = inject_hook_secrets(&mut body, slug, hook_secrets);
    let updated = client
        .update_hook_value(id, &body, Some(progress.clone()))
        .await
        .with_context(|| format!("PATCH /hooks/{id}"))?;
    Ok((updated, secrets_hash))
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// the code sidecar (or its removal) plus the stale other-extension sweep, the
/// lockfile entry, and the deferred-relink record.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let (updated_json, updated_code) = serialize_hook(&updated)?;` down to and
/// including the `relink.push(...)`, with the `progress.event(Action::Patch, …)`
/// line left behind at the call site so the caller controls when it fires. The
/// sidecar extension comes from the server's response, never from what the
/// local JSON declared.
fn write_back(
    paths: &Paths,
    hooks_dir: &std::path::Path,
    lockfile: &mut Lockfile,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    slug: &str,
    local_json_path: &std::path::Path,
    patched: HookPatched,
) -> Result<()> {
    let HookPatched {
        updated,
        deferred,
        secrets_hash,
    } = patched;

    // Refresh local file with the codec's canonical form (matches
    // what next pull would write) and update lockfile to match.
    let (updated_json, updated_code) = serialize_hook(&updated)?;
    // Re-portabilize the server response so concrete env URLs never land on
    // disk. The hook is already lockfile-pinned, so its `url` and every
    // cross-ref resolve back to `rdc://`.
    let updated_json = crate::cli::pull::common::portabilize_proposed(&updated_json, lockfile);
    let updated_hash = hook_combined_hash(&updated_json, &updated_code, lockfile);
    let updated_ext = hook_code_extension(&updated);
    crate::state::base_cache::write_disk_and_cache(paths, local_json_path, &updated_json)
        .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
    let code_path = hooks_dir.join(format!("{slug}.{updated_ext}"));
    if let Some(code) = &updated_code {
        write_hook_code(hooks_dir, slug, code, updated_ext)
            .with_context(|| format!("writing hook code for '{slug}'"))?;
        // Mirror the code sidecar into the base cache so a later
        // `BothDiverged` conflict can 3-way-merge the code against a real
        // base — matching what `pull::hooks` does. Without this the base
        // cache holds the `.json` but not the `.py`, and the conflict
        // resolver falls back to a manual prompt (`base_cache::read` → None).
        crate::state::base_cache::write(paths, &code_path, code.as_bytes())
            .with_context(|| format!("caching base hook code for '{slug}'"))?;
    } else {
        // Post-PATCH the hook has no code (e.g. a function→webhook change):
        // drop the primary sidecar from disk and the base cache so the
        // snapshot stays canonical.
        if code_path.exists() {
            std::fs::remove_file(&code_path)
                .with_context(|| format!("removing stale {}", code_path.display()))?;
        }
        crate::state::base_cache::forget(paths, &code_path)?;
    }
    // Sweep a stale sidecar if the post-PATCH runtime differs from what the
    // local disk still carries — from disk AND the base cache mirror.
    let other_updated_ext = if updated_ext == "py" { "js" } else { "py" };
    let stale_updated = hooks_dir.join(format!("{slug}.{other_updated_ext}"));
    if stale_updated.exists() {
        std::fs::remove_file(&stale_updated)
            .with_context(|| format!("removing stale {}", stale_updated.display()))?;
    }
    crate::state::base_cache::forget(paths, &stale_updated)?;

    lockfile.upsert(
        "hooks",
        slug,
        ObjectEntry {
            id: updated.id,
            modified_at: updated.modified_at().map(|s| s.to_string()),
            modified_by: updated.modified_by().map(|s| s.to_string()),
            content_hash: Some(updated_hash),
            secrets_hash,
        },
    );
    if !deferred.is_empty() {
        relink.push(crate::cli::push::relink::DeferredRelink {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            path: local_json_path.to_path_buf(),
            fields: deferred,
        });
    }
    Ok(())
}

/// Resolve one drifted hook interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value_deferring`, `resolve_push_drift`, then either
/// PATCH (via the same `send_patch` + `write_back`), adopt the remote, or skip.
/// It runs only on the sequential stage, so `resolve_push_drift`'s prompt can
/// never interleave with another item's. Returns `(pushed, skipped)` deltas.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    hooks_dir: &std::path::Path,
    slug: &str,
    local_json_path: &std::path::Path,
    remote_hooks: &[crate::model::Hook],
    hook_secrets: &HookSecrets,
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    let entry = lockfile
        .objects
        .get("hooks")
        .and_then(|m| m.get(slug))
        .expect("only reached for an item that was partitioned as an update");
    let id = entry.id;

    let mut payload = read_hook_value(hooks_dir, slug)
        .with_context(|| format!("reading local hook '{slug}'"))?;
    let mut deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
    let payload_hook: crate::model::Hook = serde_json::from_value(payload)
        .with_context(|| format!("deserializing hook '{slug}'"))?;

    let Some(remote_hook) = remote_hooks.iter().find(|h| h.id == id) else {
        progress.event(
            Action::Skip,
            &format!("hook/{slug} (remote id {id} missing)"),
        );
        return Ok((0, 1));
    };
    let (remote_json, remote_code) = serialize_hook(remote_hook)?;
    let remote_combined = hook_combined_hash(&remote_json, &remote_code, lockfile);
    let mut payload_to_send = payload_hook;

    // Drift already established by the concurrent stage. The hook is a
    // combined-hash kind (json + py); the resolver prompt shows json bytes for
    // the diff (most common case). On Adopt, we write both .json and .py from
    // the remote so disk + lockfile stay aligned.
    use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
    match resolve_push_drift(
        interactive,
        crate::cli::resolve::ObjectRef { kind: "hooks", slug },
        local_json_path, &remote_json,
        env,
    )? {
        PushDriftOutcome::Patch { payload_override } => {
            if let Some(bytes) = payload_override {
                let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("re-deserializing edited hook '{slug}'"))?;
                // The edited body replaces the one deferral was computed
                // from, so recompute it (mirrors `push::queues`).
                deferred = crate::snapshot::refs::resolve_value_deferring(&mut ov, lockfile);
                payload_to_send = serde_json::from_value(ov)
                    .with_context(|| format!("re-deserializing edited hook '{slug}'"))?;
            }
        }
        PushDriftOutcome::Adopt => {
            // Portabilize the adopted remote so concrete env URLs never
            // land on disk (the hook is lockfile-pinned; self + refs resolve).
            let remote_json = crate::cli::pull::common::portabilize_proposed(&remote_json, lockfile);
            crate::state::base_cache::write_disk_and_cache(paths, local_json_path, &remote_json)
                .with_context(|| format!("adopting remote into {}", local_json_path.display()))?;
            // Adopt uses the remote runtime to decide the
            // sidecar extension — the remote is now the source
            // of truth. Sweep any sidecar of the other
            // extension so disk stays canonical.
            let remote_ext = hook_code_extension(remote_hook);
            let remote_code_path = hooks_dir.join(format!("{slug}.{remote_ext}"));
            if let Some(code) = &remote_code {
                write_hook_code(hooks_dir, slug, code, remote_ext)
                    .with_context(|| format!("adopting remote hook code for '{slug}'"))?;
                // Adopting makes the remote the base; mirror the sidecar so the
                // next conflict has one (same reason as the PATCH path).
                crate::state::base_cache::write(paths, &remote_code_path, code.as_bytes())
                    .with_context(|| format!("caching base hook code for '{slug}'"))?;
            } else {
                if remote_code_path.exists() {
                    std::fs::remove_file(&remote_code_path).with_context(|| {
                        format!("removing stale {}", remote_code_path.display())
                    })?;
                }
                crate::state::base_cache::forget(paths, &remote_code_path)?;
            }
            let other_remote_ext = if remote_ext == "py" { "js" } else { "py" };
            let stale = hooks_dir.join(format!("{slug}.{other_remote_ext}"));
            if stale.exists() {
                std::fs::remove_file(&stale)
                    .with_context(|| format!("removing stale {}", stale.display()))?;
            }
            // Adopt is a content-side reconciliation (remote → local).
            // The secrets we last pushed are unaffected; carry the
            // previous lockfile `secrets_hash` forward so the next
            // sync doesn't think they changed.
            let prior_secrets_hash = lockfile
                .objects
                .get("hooks")
                .and_then(|m| m.get(slug))
                .and_then(|e| e.secrets_hash.clone());
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id,
                    modified_at: remote_hook.modified_at().map(|s| s.to_string()),
                    modified_by: remote_hook.modified_by().map(|s| s.to_string()),
                    content_hash: Some(remote_combined),
                    secrets_hash: prior_secrets_hash,
                },
            );
            progress.event(Action::Warn, &format!("hook/{slug} adopted remote (drift)"));
            return Ok((0, 1));
        }
        PushDriftOutcome::Skip => {
            progress.event(
                Action::Skip,
                &format!("hook/{slug} (remote changed; rdc sync first)"),
            );
            return Ok((0, 1));
        }
    }

    let (updated, secrets_hash) = send_patch(
        client,
        id,
        slug,
        &payload_to_send,
        &deferred,
        hook_secrets,
        progress,
    )
    .await?;
    write_back(
        paths,
        hooks_dir,
        lockfile,
        relink,
        slug,
        local_json_path,
        HookPatched {
            updated,
            deferred,
            secrets_hash: Some(secrets_hash),
        },
    )?;
    progress.event(Action::Patch, &format!("hook/{slug}"));
    Ok((1, 0))
}

/// Predict the secrets-only pass for `--dry-run`, network-free. Classifies each
/// slug in the local hook-secrets file exactly as [`push`]'s secrets-only pass
/// does, so the dry-run count matches the real run (which counts secret-only
/// PATCHes in its total). Returns `(would_push, dangling)`, both sorted:
///   - `would_push`: slugs with a lockfile entry whose `secrets_hash` differs
///     from the local secret hash (including `None`, i.e. never pushed). The
///     sync content classifier can't see these — a hook's JSON/code is unchanged
///     while its secret (in the separate `secrets/<env>.hook-secrets.json`)
///     drifted — so without this the dry-run undercounts the push.
///   - `dangling`: slugs with NO lockfile entry — a typo or a slug left stale
///     after a rename; the real pass warns and skips them (no id to target).
pub fn plan_secret_pushes(
    hook_secrets: &HookSecrets,
    lockfile: &crate::state::Lockfile,
) -> (Vec<String>, Vec<String>) {
    let mut would_push = Vec::new();
    let mut dangling = Vec::new();
    for slug in hook_secrets.slugs() {
        let local_hash = hook_secrets_hash(&hook_secrets.filled_kv_for_slug(slug));
        match lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug.as_str()))
        {
            None => dangling.push(slug.clone()),
            Some(entry) => {
                if entry.secrets_hash.as_deref() != Some(local_hash.as_str()) {
                    would_push.push(slug.clone());
                }
            }
        }
    }
    would_push.sort();
    dangling.sort();
    (would_push, dangling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};

    fn secrets_with(entries: &[(&str, &str)]) -> HookSecrets {
        // Write a hook-secrets file and load it through the real reader.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        let hooks: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(slug, val)| (slug.to_string(), serde_json::json!({ "password": *val })))
            .collect();
        let body = serde_json::json!({ "hooks": hooks });
        std::fs::write(
            dir.path().join("secrets/dev.hook-secrets.json"),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
        load_hook_secrets(dir.path(), "dev").unwrap()
    }

    fn entry(id: u64, secrets_hash: Option<&str>) -> ObjectEntry {
        ObjectEntry {
            id,
            modified_at: None,
            modified_by: None,
            content_hash: None,
            secrets_hash: secrets_hash.map(str::to_string),
        }
    }

    #[test]
    fn plan_secret_pushes_flags_drift_and_dangling_but_not_in_sync() {
        let hs = secrets_with(&[
            ("in-sync", "v1"),
            ("drifted", "v2"),
            ("dangling", "v3"),
        ]);
        let insync_hash = hook_secrets_hash(&hs.filled_kv_for_slug("in-sync"));

        let mut lf = Lockfile::default();
        // in-sync: lockfile hash matches local -> no push.
        lf.upsert("hooks", "in-sync", entry(1, Some(&insync_hash)));
        // drifted: lockfile hash differs -> secret-only push (classifier misses it).
        lf.upsert("hooks", "drifted", entry(2, Some("stale-hash")));
        // dangling: no lockfile entry at all -> warning, not a push.

        let (would_push, dangling) = plan_secret_pushes(&hs, &lf);
        assert_eq!(would_push, vec!["drifted".to_string()]);
        assert_eq!(dangling, vec!["dangling".to_string()]);
    }

    #[test]
    fn plan_secret_pushes_treats_none_secrets_hash_as_drift() {
        // A hook that has never had its secret pushed (secrets_hash: None) but now
        // has a local secret must be planned as a push.
        let hs = secrets_with(&[("h", "v")]);
        let mut lf = Lockfile::default();
        lf.upsert("hooks", "h", entry(1, None));
        let (would_push, dangling) = plan_secret_pushes(&hs, &lf);
        assert_eq!(would_push, vec!["h".to_string()]);
        assert!(dangling.is_empty());
    }

    #[test]
    fn plan_secret_pushes_empty_when_no_secrets() {
        let hs = secrets_with(&[]);
        let (would_push, dangling) = plan_secret_pushes(&hs, &Lockfile::default());
        assert!(would_push.is_empty());
        assert!(dangling.is_empty());
    }

    /// Regression: pushing a hook must NEVER write concrete env URLs to disk.
    /// The post-POST/PATCH write-back serializes the server response (which
    /// carries concrete `https://…/hooks/<id>` URLs); it must be re-portabilized
    /// to `rdc://<kind>/<slug>` form before landing on disk. Otherwise an
    /// interrupted sync (whose portabilize post-pass never runs) freezes concrete
    /// env-specific URLs into the portable snapshot — silent data corruption.
    #[tokio::test]
    async fn push_create_hook_writes_portable_refs_not_concrete_urls() {
        use crate::paths::Paths;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        // Local hook on disk with portable rdc:// refs (self url + queue ref).
        let local = serde_json::json!({
            "name": "My Hook",
            "type": "function",
            "url": "rdc://hooks/my-hook",
            "queues": ["rdc://queues/q1"],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        });
        std::fs::write(
            hooks_dir.join("my-hook.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(hooks_dir.join("my-hook.py"), b"x = 1\n").unwrap();

        // Lockfile: queue q1 tracked so the create payload's rdc://queues/q1
        // resolves; no hooks/my-hook entry -> this is the create (POST) path.
        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "queues",
            "q1",
            ObjectEntry {
                id: 100,
                modified_at: None,
                modified_by: None,
                content_hash: Some("h".into()),
                secrets_hash: None,
            },
        );

        // POST /hooks returns the created hook with CONCRETE, env-specific urls.
        let created = serde_json::json!({
            "id": 500,
            "url": format!("{api}/hooks/500"),
            "name": "My Hook",
            "type": "function",
            "queues": [format!("{api}/queues/100")],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "x = 1\n" }
        });
        Mock::given(method("POST"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(201).set_body_json(created))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let mut changes = BTreeMap::new();
        changes.insert("my-hook".to_string(), hooks_dir.join("my-hook.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!(pushed, 1, "the hook should be created");

        let on_disk = std::fs::read_to_string(hooks_dir.join("my-hook.json")).unwrap();
        assert!(
            !on_disk.contains(&server.uri()),
            "concrete env URL must NEVER be written to disk:\n{on_disk}"
        );
        assert!(
            on_disk.contains("rdc://hooks/my-hook"),
            "hook self-url must be portable rdc://:\n{on_disk}"
        );
        assert!(
            on_disk.contains("rdc://queues/q1"),
            "queue membership must be portable rdc://:\n{on_disk}"
        );
    }

    /// Regression (PATCH path — the reported incident): editing an existing hook
    /// and pushing must NOT de-portabilize its refs. The PATCH response carries
    /// concrete env URLs (self `url`, `queues`, `run_after`); they must be
    /// rewritten to `rdc://` before the on-disk write, so a sync that later
    /// aborts (skipping the portabilize post-pass) never freezes them in.
    #[tokio::test]
    async fn push_patch_hook_keeps_portable_refs_not_concrete_urls() {
        use crate::paths::Paths;
        use crate::snapshot::hook::serialize_hook;
        use crate::state::hook_combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let local = serde_json::json!({
            "name": "My Hook",
            "type": "function",
            "url": "rdc://hooks/my-hook",
            "queues": ["rdc://queues/q1"],
            "run_after": ["rdc://hooks/upstream"],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        });
        std::fs::write(
            hooks_dir.join("my-hook.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(hooks_dir.join("my-hook.py"), b"x = 2\n").unwrap();

        // Lockfile: queue q1, the upstream hook, and my-hook itself all pinned so
        // every ref (and the self-url) resolves back to rdc://.
        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        lockfile.upsert(
            "queues",
            "q1",
            ObjectEntry { id: 100, modified_at: None, modified_by: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "hooks",
            "upstream",
            ObjectEntry { id: 400, modified_at: None, modified_by: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None },
        );

        // The remote hook (concrete urls). base := its canonical hash so the
        // drift check sees no remote drift and proceeds to PATCH.
        let remote = serde_json::json!({
            "id": 500,
            "url": format!("{api}/hooks/500"),
            "name": "My Hook",
            "type": "function",
            "queues": [format!("{api}/queues/100")],
            "run_after": [format!("{api}/hooks/400")],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "x = 2\n" }
        });
        let remote_hook: crate::model::Hook = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_hook(&remote_hook).unwrap();
        let base = hook_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, modified_by: None, content_hash: Some(base), secrets_hash: None },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote.clone()]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/hooks/500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let mut changes = BTreeMap::new();
        changes.insert("my-hook".to_string(), hooks_dir.join("my-hook.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!(pushed, 1, "the hook should be patched");

        let on_disk = std::fs::read_to_string(hooks_dir.join("my-hook.json")).unwrap();
        assert!(
            !on_disk.contains(&server.uri()),
            "concrete env URL must NEVER be written to disk:\n{on_disk}"
        );
        assert!(on_disk.contains("rdc://hooks/my-hook"), "self-url portable:\n{on_disk}");
        assert!(on_disk.contains("rdc://queues/q1"), "queue ref portable:\n{on_disk}");
        assert!(
            on_disk.contains("rdc://hooks/upstream"),
            "run_after ref portable:\n{on_disk}"
        );
    }

    /// Regression (the reported incident): PATCHing a hook whose `run_after`
    /// names a hook that does not exist in this env yet must DEFER that field,
    /// not abort the push. Hooks are pushed in slug order, so a forward
    /// reference — an already-deployed hook pointing at one created later in
    /// the same pass — is normal; the create path has always deferred it, but
    /// the patch path resolved eagerly and the unresolved-ref guard killed the
    /// whole cycle. `queues.rs`/`engines.rs` defer on both paths; hooks only
    /// did so on create.
    #[tokio::test]
    async fn push_patch_hook_defers_unresolvable_run_after_instead_of_failing() {
        use crate::paths::Paths;
        use crate::snapshot::hook::serialize_hook;
        use crate::state::hook_combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let local = serde_json::json!({
            "name": "My Hook",
            "type": "function",
            "url": "rdc://hooks/my-hook",
            "queues": ["rdc://queues/q1"],
            // `not-yet-created` sorts after `my-hook`: it is created later in
            // this same push pass, so it cannot resolve now.
            "run_after": ["rdc://hooks/not-yet-created"],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        });
        std::fs::write(
            hooks_dir.join("my-hook.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(hooks_dir.join("my-hook.py"), b"x = 2\n").unwrap();

        let mut lockfile = Lockfile { api_base: api.clone(), ..Lockfile::default() };
        lockfile.upsert(
            "queues",
            "q1",
            ObjectEntry { id: 100, modified_at: None, modified_by: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None },
        );

        let remote = serde_json::json!({
            "id": 500,
            "url": format!("{api}/hooks/500"),
            "name": "My Hook",
            "type": "function",
            "queues": [format!("{api}/queues/100")],
            "run_after": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "x = 2\n" }
        });
        let remote_hook: crate::model::Hook = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_hook(&remote_hook).unwrap();
        let base = hook_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, modified_by: None, content_hash: Some(base), secrets_hash: None },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote.clone()]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/hooks/500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let mut changes = BTreeMap::new();
        changes.insert("my-hook".to_string(), hooks_dir.join("my-hook.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("a forward run_after ref must not abort the push");
        assert_eq!(pushed, 1, "the hook should still be patched");

        // The deferred field must be recorded for the relink pass, with its
        // ORIGINAL rdc:// value so it can be re-resolved once the target exists.
        assert_eq!(relink.len(), 1, "expected one deferred relink: {relink:?}");
        assert_eq!(relink[0].kind, "hooks");
        assert_eq!(relink[0].slug, "my-hook");
        assert_eq!(
            relink[0].fields,
            vec![(
                "run_after".to_string(),
                serde_json::json!(["rdc://hooks/not-yet-created"])
            )]
        );

        // ...and must not ride the PATCH at all: sending `run_after: []` would
        // clear the remote's links rather than leave them for the relink.
        let reqs = server.received_requests().await.unwrap_or_default();
        let patch = reqs
            .iter()
            .find(|r| r.method == http::Method::PATCH)
            .expect("a PATCH must have been issued");
        let body: serde_json::Value = serde_json::from_slice(&patch.body).unwrap();
        assert!(
            body.get("run_after").is_none(),
            "deferred field must be absent from the PATCH body: {body}"
        );
    }

    /// The same deferral must not silently UNBIND a hook. `Hook.queues` is a
    /// modeled `Vec<String>`, so a deferred `queues` round-trips through the
    /// typed struct as `[]` — PATCHing that would detach the hook from every
    /// queue until the relink lands (and permanently if it never does). The
    /// key has to be dropped from the body, not emptied.
    #[tokio::test]
    async fn push_patch_hook_omits_a_deferred_queues_field_rather_than_emptying_it() {
        use crate::paths::Paths;
        use crate::snapshot::hook::serialize_hook;
        use crate::state::hook_combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let local = serde_json::json!({
            "name": "My Hook",
            "type": "function",
            "url": "rdc://hooks/my-hook",
            "queues": ["rdc://queues/never-created"],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        });
        std::fs::write(
            hooks_dir.join("my-hook.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(hooks_dir.join("my-hook.py"), b"x = 2\n").unwrap();

        let mut lockfile = Lockfile { api_base: api.clone(), ..Lockfile::default() };
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None },
        );

        let remote = serde_json::json!({
            "id": 500,
            "url": format!("{api}/hooks/500"),
            "name": "My Hook",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "x = 2\n" }
        });
        let remote_hook: crate::model::Hook = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_hook(&remote_hook).unwrap();
        let base = hook_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, modified_by: None, content_hash: Some(base), secrets_hash: None },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote.clone()]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/hooks/500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let mut changes = BTreeMap::new();
        changes.insert("my-hook".to_string(), hooks_dir.join("my-hook.json"));

        push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");

        let reqs = server.received_requests().await.unwrap_or_default();
        let patch = reqs
            .iter()
            .find(|r| r.method == http::Method::PATCH)
            .expect("a PATCH must have been issued");
        let body: serde_json::Value = serde_json::from_slice(&patch.body).unwrap();
        assert!(
            body.get("queues").is_none(),
            "a deferred `queues` must be omitted, never sent as []: {body}"
        );
    }

    /// Regression: after a hook PATCH push, the extracted code sidecar must be
    /// mirrored into the base cache — exactly as the pull path does (see
    /// `pull::hooks`, "Cache the code sidecar so a future 3-way merge") — so a
    /// later `BothDiverged` conflict can 3-way-merge the code against a real
    /// base. Before the fix the push wrote the `.py` only to the working tree,
    /// leaving the base cache with the hook `.json` but no `.py` (observed on a
    /// live env), which forces the conflict resolver to fall back to a manual
    /// prompt (`base_cache::read` → `None`).
    #[tokio::test]
    async fn push_patch_hook_caches_code_sidecar_to_base() {
        use crate::paths::Paths;
        use crate::snapshot::hook::serialize_hook;
        use crate::state::hook_combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let local = serde_json::json!({
            "name": "My Hook",
            "type": "function",
            "url": "rdc://hooks/my-hook",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        });
        std::fs::write(
            hooks_dir.join("my-hook.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(hooks_dir.join("my-hook.py"), b"x = 2\n").unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // Register the hook's id BEFORE computing the base hash so the remote
        // self-url portabilizes to rdc:// consistently (the hash is ref-aware).
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry {
                id: 500,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // The remote hook (concrete urls) carries the code. base := its combined
        // hash so the drift check sees no remote drift and proceeds to PATCH.
        let remote = serde_json::json!({
            "id": 500,
            "url": format!("{api}/hooks/500"),
            "name": "My Hook",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "x = 2\n" }
        });
        let remote_hook: crate::model::Hook = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_hook(&remote_hook).unwrap();
        let base = hook_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry {
                id: 500,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote.clone()]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/hooks/500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let mut changes = BTreeMap::new();
        changes.insert("my-hook".to_string(), hooks_dir.join("my-hook.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!(pushed, 1, "the hook should be patched");

        // The code sidecar must be mirrored into the base cache, byte-exact.
        let code_path = hooks_dir.join("my-hook.py");
        let base_py = crate::state::base_cache::cache_mirror(&paths, &code_path)
            .expect("code path is under env root");
        assert!(
            base_py.exists(),
            "base cache must contain the hook code sidecar:\n{}",
            base_py.display()
        );
        assert_eq!(
            std::fs::read_to_string(&base_py).unwrap(),
            "x = 2\n",
            "base cached code must match the pushed code"
        );
    }

    /// Spec D9/B5: hook PATCHes ran at 2.44 req/s against a 10 req/s bucket —
    /// the largest headroom on the write path. Four hooks whose PATCHes each
    /// take 300ms cost ~1.2s in series and ~300-600ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_hooks_patches_updates_concurrently() {
        use crate::paths::Paths;
        use crate::snapshot::hook::serialize_hook;
        use crate::state::hook_combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let slugs = ["h-a", "h-b", "h-c", "h-d"];
        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 900 + i as u64;
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://hooks/{slug}"),
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": { "url": "https://example.invalid/hook" }
            });
            std::fs::write(
                hooks_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/hooks/{id}"),
                "name": slug,
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": { "url": "https://example.invalid/hook" }
            });
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let remote_hook: crate::model::Hook = serde_json::from_value(remote.clone()).unwrap();
            let (rj, rc) = serialize_hook(&remote_hook).unwrap();
            let base = hook_combined_hash(&rj, &rc, &lockfile);
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), hooks_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 900 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/hooks/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(list["results"][i].clone())
                        .set_delay(std::time::Duration::from_millis(300)),
                )
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let start = std::time::Instant::now();
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "four 300ms PATCHes must overlap; sequential would be >= 1.2s, took {elapsed:?}",
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
    ///      before the POST, and `z-update` only afterwards — exactly the
    ///      order the old sequential loop used. Hoisting every create ahead of
    ///      every update would resolve refs that must still defer (see
    ///      `sync_push_hook_run_after_deferred_relink_on_the_patch_path`).
    ///   2. N runs still cost ONE `GET /hooks`. That is the entire reason the
    ///      drift list is threaded through as `&mut Option<Vec<Hook>>` rather
    ///      than being a local of the batch function; a per-run fetch would be
    ///      an extra request the old loop never made.
    #[tokio::test]
    async fn push_hooks_barriers_on_a_create_and_lists_only_once() {
        use crate::paths::Paths;
        use crate::snapshot::hook::serialize_hook;
        use crate::state::hook_combined_hash;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let hook_json = |slug: &str, id: Option<u64>| {
            let url = match id {
                Some(id) => format!("{api}/hooks/{id}"),
                None => format!("rdc://hooks/{slug}"),
            };
            let mut v = serde_json::json!({
                "url": url,
                "name": slug,
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": { "url": "https://example.invalid/hook" }
            });
            if let Some(id) = id {
                v["id"] = serde_json::json!(id);
            }
            v
        };

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        let mut changes = BTreeMap::new();
        // Two tracked hooks (PATCH path) with a NEW hook sorting between them.
        for (slug, id) in [("a-update", 910u64), ("z-update", 912u64)] {
            std::fs::write(
                hooks_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&hook_json(slug, None)).unwrap(),
            )
            .unwrap();
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let remote: crate::model::Hook =
                serde_json::from_value(hook_json(slug, Some(id))).unwrap();
            let (rj, rc) = serialize_hook(&remote).unwrap();
            let base = hook_combined_hash(&rj, &rc, &lockfile);
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(base),
                    secrets_hash: None,
                },
            );
            changes.insert(slug.to_string(), hooks_dir.join(format!("{slug}.json")));
        }
        // No lockfile entry -> create. Sorts between the two updates.
        std::fs::write(
            hooks_dir.join("m-create.json"),
            serde_json::to_vec_pretty(&hook_json("m-create", None)).unwrap(),
        )
        .unwrap();
        changes.insert("m-create".to_string(), hooks_dir.join("m-create.json"));

        // The drift list never contains the hook created mid-push — same as
        // the old loop, whose cache was also filled before the POST.
        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null },
                "results": [hook_json("a-update", Some(910)), hook_json("z-update", Some(912))]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/hooks"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(hook_json("m-create", Some(911))),
            )
            .mount(&server)
            .await;
        for (slug, id) in [("a-update", 910u64), ("z-update", 912u64)] {
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/hooks/{id}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(hook_json(slug, Some(id))),
                )
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut relink = Vec::new();
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!((pushed, skipped), (3, 0));

        let reqs = server.received_requests().await.unwrap_or_default();
        let stream: Vec<String> = reqs
            .iter()
            .filter(|r| r.url.path().starts_with("/api/v1/hooks"))
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        assert_eq!(
            stream,
            vec![
                "GET /api/v1/hooks".to_string(),
                "PATCH /api/v1/hooks/910".to_string(),
                "POST /api/v1/hooks".to_string(),
                "PATCH /api/v1/hooks/912".to_string(),
            ],
            "the create must sit BETWEEN the two updates, and the drift list \
             must be fetched exactly once for both runs",
        );
        assert_eq!(
            stream.iter().filter(|r| *r == "GET /api/v1/hooks").count(),
            1,
            "a second run must reuse the caller-owned drift list, not refetch it",
        );
    }
}
