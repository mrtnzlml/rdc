use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::secrets::{HookSecrets, load_hook_secrets};
use crate::snapshot::create::strip_for_create;
use crate::snapshot::hook::{
    hook_code_extension, hook_code_extension_from_value, read_hook_value, serialize_hook,
    write_hook_code,
};
use crate::snapshot::writer::write_atomic;
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

    // Lazily-fetched fresh hook list, used exclusively for the pre-PATCH
    // drift check below. The orphan check uses `catalog_hooks` (Phase-1
    // data) instead, so this cache is no longer shared between the
    // create and update paths.
    let mut drift_hooks: Option<Vec<crate::model::Hook>> = None;

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
            // Read + portabilize refs once; reused by both paths.
            let mut payload = read_hook_value(&hooks_dir, slug)
                .with_context(|| format!("reading local hook '{slug}' for create"))?;
            // Two-phase relink: resolve what we can; defer top-level fields whose
            // rdc:// refs target a hook not yet created (e.g. `run_after` pointing
            // at another new hook). The relink pass PATCHes them once all hooks
            // exist. The patch path below does the same.
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
                    content_hash: None,
                    secrets_hash: None,
                },
            );
            let created_json =
                crate::cli::pull::common::portabilize_proposed(&created_json, lockfile);
            let created_hash = hook_combined_hash(&created_json, &created_code, lockfile);
            let created_ext = hook_code_extension(&created);
            write_atomic(local_json_path, &created_json)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            if let Some(code) = &created_code {
                write_hook_code(&hooks_dir, slug, code, created_ext)
                    .with_context(|| format!("writing hook code for '{slug}'"))?;
            }
            // Sweep any stale sidecar with the *other* extension that may
            // have been left over from a previous runtime.
            let other_created_ext = if created_ext == "py" { "js" } else { "py" };
            let stale_created = hooks_dir.join(format!("{slug}.{other_created_ext}"));
            if stale_created.exists() {
                std::fs::remove_file(&stale_created)
                    .with_context(|| format!("removing stale {}", stale_created.display()))?;
            }
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id: created.id,
                    modified_at: created.modified_at().map(|s| s.to_string()),
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

        let entry = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("hook/{slug} (no content_hash)"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let id = entry.id;

        // Read raw Value (with the sidecar code spliced in) BEFORE typed
        // deserialize.
        let mut payload = read_hook_value(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;
        // The on-disk sidecar is whatever the local JSON declared.
        let local_ext = hook_code_extension_from_value(&payload);
        // Two-phase relink, same as the create path above and as
        // `queues`/`engines` do on BOTH of their paths. Hooks are pushed in
        // slug order, so an already-deployed hook whose `run_after` names one
        // created later in this same pass is an ordinary forward reference —
        // resolving eagerly here left an `rdc://` in the body and the
        // unresolved-ref guard aborted the entire cycle, permanently wedging
        // any env where a tracked hook points at a not-yet-created one.
        let mut deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("deserializing hook '{slug}'"))?;

        // Drift check: fetch remote, serialize, hash. Compare to base.
        // The list is cached across iterations within this loop so a batch
        // of N updates only pays one list call here.
        if drift_hooks.is_none() {
            drift_hooks = Some(
                client
                    .list_hooks(Some(progress.clone()))
                    .await
                    .context("listing hooks to verify no drift before push")?,
            );
        }
        let remote_list = drift_hooks
            .as_ref()
            .expect("drift_hooks was just populated above");
        let Some(remote_hook) = remote_list.iter().find(|h| h.id == id) else {
            progress.event(
                Action::Skip,
                &format!("hook/{slug} (remote id {id} missing)"),
            );
            skipped += 1;
            continue;
        };
        let (remote_json, remote_code) = serialize_hook(remote_hook)?;
        let remote_combined = hook_combined_hash(&remote_json, &remote_code, lockfile);
        let mut payload_to_send = payload_hook;
        if remote_combined != base {
            // Drift detected. The hook is a combined-hash kind (json + py);
            // the resolver prompt shows json bytes for the diff (most
            // common case). On Adopt, we write both .json and .py from
            // the remote so disk + lockfile stay aligned.
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, local_json_path, &remote_json, env)? {
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
                    let remote_json =
                        crate::cli::pull::common::portabilize_proposed(&remote_json, lockfile);
                    write_atomic(local_json_path, &remote_json).with_context(|| {
                        format!("adopting remote into {}", local_json_path.display())
                    })?;
                    // Adopt uses the remote runtime to decide the
                    // sidecar extension — the remote is now the source
                    // of truth. Sweep any sidecar of the other
                    // extension so disk stays canonical.
                    let remote_ext = hook_code_extension(remote_hook);
                    if let Some(code) = &remote_code {
                        write_hook_code(&hooks_dir, slug, code, remote_ext)
                            .with_context(|| format!("adopting remote hook code for '{slug}'"))?;
                    } else {
                        let primary = hooks_dir.join(format!("{slug}.{remote_ext}"));
                        if primary.exists() {
                            std::fs::remove_file(&primary)
                                .with_context(|| format!("removing stale {}", primary.display()))?;
                        }
                    }
                    let other_remote_ext = if remote_ext == "py" { "js" } else { "py" };
                    let stale = hooks_dir.join(format!("{slug}.{other_remote_ext}"));
                    if stale.exists() {
                        std::fs::remove_file(&stale)
                            .with_context(|| format!("removing stale {}", stale.display()))?;
                    }
                    let _ = local_ext; // unused on adopt path; the remote ext drives layout
                    // Adopt is a content-side reconciliation (remote → local).
                    // The secrets we last pushed are unaffected; carry the
                    // previous lockfile `secrets_hash` forward so the next
                    // sync doesn't think they changed.
                    let prior_secrets_hash = lockfile
                        .objects
                        .get("hooks")
                        .and_then(|m| m.get(slug.as_str()))
                        .and_then(|e| e.secrets_hash.clone());
                    lockfile.upsert(
                        "hooks",
                        slug,
                        ObjectEntry {
                            id,
                            modified_at: remote_hook.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: prior_secrets_hash,
                        },
                    );
                    progress.event(Action::Warn, &format!("hook/{slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("hook/{slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
            }
        }

        // Build a Value form of the typed payload so secrets (which
        // have no place on the typed `Hook` model) can ride this PATCH.
        let mut body = serde_json::to_value(&payload_to_send)
            .with_context(|| format!("serializing hook '{slug}' for PATCH"))?;
        // A deferred field must not ride this PATCH at all. Removing the key
        // from the Value is not enough on its own: `queues` is a MODELED field
        // on `Hook`, so the typed round-trip re-materializes it as `[]` and the
        // PATCH would detach the hook from every queue until the relink lands —
        // permanently if the relink never resolves. Omitting the key leaves the
        // remote's current value untouched, which is what deferral means.
        if let Some(obj) = body.as_object_mut() {
            for (field, _) in &deferred {
                obj.remove(field);
            }
        }
        // `status` is a read-only server health field that's redacted to the
        // sentinel on disk; strip it (and the other server fields) so the
        // PATCH body matches the CREATE contract instead of echoing the
        // sentinel back. Done before secret injection so secrets survive.
        strip_for_create(&mut body, "hooks");
        let updated_secrets_hash = inject_hook_secrets(&mut body, slug, &hook_secrets);
        let patch_result = client
            .update_hook_value(id, &body, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /hooks/{id}"));
        let updated = patch_result?;

        // Refresh local file with the codec's canonical form (matches
        // what next pull would write) and update lockfile to match.
        let (updated_json, updated_code) = serialize_hook(&updated)?;
        // Re-portabilize the server response so concrete env URLs never land on
        // disk. The hook is already lockfile-pinned, so its `url` and every
        // cross-ref resolve back to `rdc://`.
        let updated_json =
            crate::cli::pull::common::portabilize_proposed(&updated_json, lockfile);
        let updated_hash = hook_combined_hash(&updated_json, &updated_code, lockfile);
        let updated_ext = hook_code_extension(&updated);
        crate::state::base_cache::write_disk_and_cache(
            paths,
            local_json_path,
            &updated_json,
        )
        .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
        let code_path = hooks_dir.join(format!("{slug}.{updated_ext}"));
        if let Some(code) = &updated_code {
            write_hook_code(&hooks_dir, slug, code, updated_ext)
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
        let _ = local_ext; // PATCH path: post-PATCH ext drives layout

        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: Some(updated_secrets_hash),
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
        progress.event(Action::Patch, &format!("hook/{slug}"));
        pushed += 1;
    }

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
            ObjectEntry { id: 100, modified_at: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "hooks",
            "upstream",
            ObjectEntry { id: 400, modified_at: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, content_hash: None, secrets_hash: None },
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
            ObjectEntry { id: 500, modified_at: None, content_hash: Some(base), secrets_hash: None },
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
            ObjectEntry { id: 100, modified_at: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 500, modified_at: None, content_hash: None, secrets_hash: None },
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
            ObjectEntry { id: 500, modified_at: None, content_hash: Some(base), secrets_hash: None },
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
            ObjectEntry { id: 500, modified_at: None, content_hash: None, secrets_hash: None },
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
            ObjectEntry { id: 500, modified_at: None, content_hash: Some(base), secrets_hash: None },
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
}
