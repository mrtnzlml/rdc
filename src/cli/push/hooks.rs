use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::SyncRenderer;
use crate::snapshot::create::strip_for_create;
use crate::snapshot::hook::{
    hook_code_extension, hook_code_extension_from_value, read_hook_value, serialize_hook,
    write_hook_code,
};
use crate::snapshot::writer::write_atomic;
use crate::secrets::{load_hook_secrets, HookSecrets};
use crate::state::{hook_combined_hash, hook_secrets_hash, Lockfile, ObjectEntry};
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
    let empty = BTreeMap::<String, String>::new();
    let kv = secrets.for_slug(slug).unwrap_or(&empty);
    let hash = hook_secrets_hash(kv);
    if !kv.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "secrets".to_string(),
                serde_json::to_value(kv).expect("BTreeMap<String,String> serializes"),
            );
        }
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
    progress: &Arc<dyn SyncRenderer>,
    env: &str,
) -> Result<(usize, usize)> {
    // Load overlay if present. Overlay drives both the outbound payload
    // (apply_overrides) AND the strip applied to remote bytes for hashing
    // — so disk-bytes (already stripped) and post-strip remote bytes can
    // both be compared against `lockfile.content_hash`.
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

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
        let empty = BTreeMap::<String, String>::new();
        for slug in hook_secrets.slugs() {
            let local = hook_secrets.for_slug(slug).unwrap_or(&empty);
            let local_hash = hook_secrets_hash(local);
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
    progress.phase("pushing hooks");
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // Lazily-fetched fresh hook list, used exclusively for the pre-PATCH
    // drift check below. The orphan check uses `catalog_hooks` (Phase-1
    // data) instead, so this cache is no longer shared between the
    // create and update paths.
    let mut drift_hooks: Option<Vec<crate::model::Hook>> = None;

    for (slug, local_json_path) in changes {
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.hook(slug));

        // Missing lockfile entry = new hook → POST. Local file becomes the
        // create payload; server response (with id/url assigned) overwrites
        // disk; lockfile gets a fresh entry.
        if lockfile.objects.get("hooks").and_then(|m| m.get(slug.as_str())).is_none() {
            // Read + overlay-apply once; reused by both paths.
            let mut payload = read_hook_value(&hooks_dir, slug)
                .with_context(|| format!("reading local hook '{slug}' for create"))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }

            // Anomaly guard, then dispatch on extension type.
            let typed: crate::model::Hook = serde_json::from_value(payload.clone())
                .with_context(|| format!("deserializing hook '{slug}' for create"))?;
            crate::cli::deploy::store_extensions::check_store_extension_anomaly(&typed, slug)?;

            // Compute the secrets hash now (before injection) so the
            // lockfile entry written below carries the up-to-date value
            // regardless of which branch creates the hook.
            let created_secrets_hash =
                hook_secrets_hash(hook_secrets.for_slug(slug).unwrap_or(&BTreeMap::new()));

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
                let template_url = typed.hook_template().expect("check_store_extension_anomaly guarantees hook_template is Some for store extensions");
                let installed_id = match crate::cli::deploy::store_extensions::find_orphan(
                    catalog_hooks, &typed.name, template_url,
                ) {
                    Some(orphan) => {
                        progress.warn_line(&format!(
                            "hooks/{slug} (adopting orphan store-extension id {})",
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
                        progress.warn_line(&format!(
                            "hooks/{slug} (installed store extension id {})",
                            installed.id
                        ));
                        installed.id
                    }
                };
                let mut body = serde_json::to_value(&typed)
                    .with_context(|| format!("serializing hook '{slug}' for store-extension PATCH"))?;
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

            // Disk + lockfile write — same for both paths. The sidecar
            // extension is derived from the server's response runtime so
            // it stays canonical even if the local JSON declared a
            // different runtime.
            let (created_json_full, created_code) = serialize_hook(&created)?;
            let created_json_stripped = maybe_strip_overlay(created_json_full, overlay_paths)?;
            let created_hash = hook_combined_hash(&created_json_stripped, &created_code);
            let created_ext = hook_code_extension(&created);
            write_atomic(local_json_path, &created_json_stripped)
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
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: Some(created_secrets_hash),
                },
            );
            progress.warn_line(&format!("[ok] hooks/{slug} POST (id {})", created.id));
            pushed += 1;
            continue;
        }

        let entry = lockfile.objects.get("hooks").and_then(|m| m.get(slug.as_str())).unwrap();
        let Some(base) = &entry.content_hash else {
            progress.warn_line(&format!("! hooks/{slug} lockfile entry has no content_hash, skipping"));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let id = entry.id;

        // Read raw Value (with the sidecar code spliced in) so overlay
        // can re-add fields stripped by pull (spec §9.3) BEFORE typed
        // deserialize.
        let mut payload = read_hook_value(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;
        // Derive the local sidecar extension *before* applying overlay,
        // since overlay may mutate `config.runtime`. The on-disk sidecar
        // is whatever the local JSON declared.
        let local_ext = hook_code_extension_from_value(&payload);
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied hook '{slug}'"))?;

        // Drift check: fetch remote, serialize, strip same overlay paths,
        // hash. Compare to base (which was recorded post-strip on pull).
        // The list is cached across iterations within this loop so a batch
        // of N updates only pays one list call here.
        if drift_hooks.is_none() {
            drift_hooks = Some(
                client.list_hooks(Some(progress.clone())).await
                    .context("listing hooks to verify no drift before push")?,
            );
        }
        let remote_list = drift_hooks.as_ref().expect("drift_hooks was just populated above");
        let Some(remote_hook) = remote_list.iter().find(|h| h.id == id) else {
            progress.warn_line(&format!("! hooks/{slug} id {id} not found on remote, skipping"));
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = serialize_hook(remote_hook)?;
        let remote_json_stripped = maybe_strip_overlay(remote_json_full, overlay_paths)?;
        let remote_combined = hook_combined_hash(&remote_json_stripped, &remote_code);
        let mut payload_to_send = payload_hook;
        if remote_combined != base {
            // Drift detected. The hook is a combined-hash kind (json + py);
            // the resolver prompt shows json bytes for the diff (most
            // common case). On Adopt, we write both .json and .py from
            // the remote so disk + lockfile stay aligned.
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, local_json_path, &remote_json_stripped, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited hook '{slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(local_json_path, &remote_json_stripped)
                        .with_context(|| format!("adopting remote into {}", local_json_path.display()))?;
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
                            url: Some(remote_hook.url.clone()),
                            modified_at: remote_hook.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: prior_secrets_hash,
                        },
                    );
                    progress.warn_line(&format!("! hooks/{slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.warn_line(&format!("! hooks/{slug} remote has changed since last sync, skipping push (run `rdc sync` first)"));
                    skipped += 1;
                    continue;
                }
            }
        }

        // Build a Value form of the typed payload so secrets (which
        // have no place on the typed `Hook` model) can ride this PATCH.
        let mut body = serde_json::to_value(&payload_to_send)
            .with_context(|| format!("serializing hook '{slug}' for PATCH"))?;
        let updated_secrets_hash = inject_hook_secrets(&mut body, slug, &hook_secrets);
        let updated = client
            .update_hook_value(id, &body, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /hooks/{id}"))?;

        // Refresh local file with the post-strip canonical form (matches
        // what next pull would write) and update lockfile to match.
        let (updated_json_full, updated_code) = serialize_hook(&updated)?;
        let updated_json_stripped = maybe_strip_overlay(updated_json_full, overlay_paths)?;
        let updated_hash = hook_combined_hash(&updated_json_stripped, &updated_code);
        let updated_ext = hook_code_extension(&updated);
        write_atomic(local_json_path, &updated_json_stripped)
            .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
        if let Some(code) = &updated_code {
            write_hook_code(&hooks_dir, slug, code, updated_ext)
                .with_context(|| format!("writing hook code for '{slug}'"))?;
        }
        // Sweep a stale sidecar if the post-PATCH runtime differs from
        // what the local disk still carries.
        let other_updated_ext = if updated_ext == "py" { "js" } else { "py" };
        let stale_updated = hooks_dir.join(format!("{slug}.{other_updated_ext}"));
        if stale_updated.exists() {
            std::fs::remove_file(&stale_updated)
                .with_context(|| format!("removing stale {}", stale_updated.display()))?;
        }
        let _ = local_ext; // PATCH path: post-PATCH ext drives layout

        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: Some(updated_secrets_hash),
            },
        );
        progress.warn_line(&format!("[ok] hooks/{slug} PATCH"));
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
    let empty = BTreeMap::<String, String>::new();
    for slug in hook_secrets.slugs() {
        let local_kv = hook_secrets.for_slug(slug).unwrap_or(&empty);
        let local_hash = hook_secrets_hash(local_kv);
        let entry = lockfile.objects.get("hooks").and_then(|m| m.get(slug.as_str()));
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
        let updated = client
            .update_hook_value(entry.id, &body, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /hooks/{} (secrets for '{}')", entry.id, slug))?;
        // Carry forward the existing `content_hash`; only `secrets_hash`
        // and `modified_at` may have changed.
        let prior_content_hash = entry.content_hash.clone();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: prior_content_hash,
                secrets_hash: Some(local_hash),
            },
        );
        progress.warn_line(&format!("[ok] hooks/{slug} (secrets only) PATCH (secrets)"));
        secrets_pushed += 1;
    }
    if !secrets_warned.is_empty() {
        // One actionable line, not one per slug — the user typed them
        // in the same file so a list is the clear signal.
        eprintln!(
            "! secrets entries with no matching hook on env '{}': {}",
            env,
            secrets_warned.join(", ")
        );
    }

    Ok((pushed + secrets_pushed, skipped))
}
