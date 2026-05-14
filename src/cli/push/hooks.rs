use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::snapshot::create::strip_for_create;
use crate::snapshot::hook::{read_hook_value, serialize_hook, write_hook_code};
use crate::snapshot::writer::write_atomic;
use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<OverallProgress>,
    env: &str,
) -> Result<(usize, usize)> {
    // Load overlay if present. Overlay drives both the outbound payload
    // (apply_overrides) AND the strip applied to remote bytes for hashing
    // — so disk-bytes (already stripped) and post-strip remote bytes can
    // both be compared against `lockfile.content_hash`.
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let hooks_dir = paths.hooks_dir();
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let mut remote_hooks: Option<Vec<crate::model::Hook>> = None;

    for (slug, local_json_path) in changes {
        let local_py_path = hooks_dir.join(format!("{slug}.py"));
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

            let created = if typed.is_store_extension() {
                // Two-call create: orphan check → POST /hooks/create → PATCH.
                if remote_hooks.is_none() {
                    remote_hooks = Some(
                        client.list_hooks(Some(progress.clone())).await
                            .context("listing hooks for store-extension orphan check")?,
                    );
                }
                let remote = remote_hooks.as_ref().expect("remote_hooks was just populated above");
                let template_url = typed.hook_template().expect("check_store_extension_anomaly guarantees hook_template is Some for store extensions");
                let installed_id = match crate::cli::deploy::store_extensions::find_orphan(
                    remote, &typed.name, template_url,
                ) {
                    Some(orphan) => {
                        progress.println(format!(
                            "adopting orphan store-extension hooks/{slug} (id {})",
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
                        progress.println(format!(
                            "installed store extension hooks/{slug} (id {})",
                            installed.id
                        ));
                        installed.id
                    }
                };
                client
                    .update_hook(installed_id, &typed, Some(progress.clone()))
                    .await
                    .with_context(|| {
                        format!(
                            "PATCH /hooks/{installed_id} (reconciling store extension '{slug}')"
                        )
                    })?
            } else {
                // Regular hook: strip server-only fields, then POST.
                strip_for_create(&mut payload, "hooks");
                client
                    .create_hook(&payload, Some(progress.clone()))
                    .await
                    .with_context(|| format!("POST /hooks (creating '{slug}')"))?
            };

            // Disk + lockfile write — same for both paths.
            let (created_json_full, created_code) = serialize_hook(&created)?;
            let created_json_stripped = maybe_strip_overlay(created_json_full, overlay_paths)?;
            let created_hash = hook_combined_hash(&created_json_stripped, &created_code);
            write_atomic(local_json_path, &created_json_stripped)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            if let Some(code) = &created_code {
                write_hook_code(&hooks_dir, slug, code)
                    .with_context(|| format!("writing hook code for '{slug}'"))?;
            }
            lockfile.upsert(
                "hooks",
                slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                },
            );
            progress.println(format!("created hooks/{slug} (id {})", created.id));
            progress.tick(slug.as_str());
            pushed += 1;
            continue;
        }

        let entry = lockfile.objects.get("hooks").and_then(|m| m.get(slug.as_str())).unwrap();
        let Some(base) = &entry.content_hash else {
            progress.println(format!(
                "warning: hooks/{slug}.json — lockfile entry has no content_hash, skipping"
            ));
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let id = entry.id;

        // Read raw Value (with .py spliced in) so overlay can re-add fields
        // stripped by pull (spec §9.3) BEFORE typed deserialize.
        let mut payload = read_hook_value(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied hook '{slug}'"))?;

        // Drift check: fetch remote, serialize, strip same overlay paths,
        // hash. Compare to base (which was recorded post-strip on pull).
        if remote_hooks.is_none() {
            remote_hooks = Some(
                client.list_hooks(Some(progress.clone())).await
                    .context("listing hooks to verify no drift before push")?,
            );
        }
        let remote_list = remote_hooks.as_ref().expect("remote_hooks was just populated above");
        let Some(remote_hook) = remote_list.iter().find(|h| h.id == id) else {
            progress.println(format!(
                "warning: hooks/{slug}.json — id {id} not found on remote, skipping"
            ));
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
                    if let Some(code) = &remote_code {
                        write_hook_code(&hooks_dir, slug, code)
                            .with_context(|| format!("adopting remote hook code for '{slug}'"))?;
                    } else if local_py_path.exists() {
                        std::fs::remove_file(&local_py_path)
                            .with_context(|| format!("removing stale {}", local_py_path.display()))?;
                    }
                    lockfile.upsert(
                        "hooks",
                        slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_hook.url.clone()),
                            modified_at: remote_hook.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                        },
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.println(format!(
                        "warning: hooks/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
                    ));
                    skipped += 1;
                    continue;
                }
            }
        }

        let updated = client.update_hook(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /hooks/{id}"))?;

        // Refresh local file with the post-strip canonical form (matches
        // what next pull would write) and update lockfile to match.
        let (updated_json_full, updated_code) = serialize_hook(&updated)?;
        let updated_json_stripped = maybe_strip_overlay(updated_json_full, overlay_paths)?;
        let updated_hash = hook_combined_hash(&updated_json_stripped, &updated_code);
        write_atomic(local_json_path, &updated_json_stripped)
            .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
        if let Some(code) = &updated_code {
            write_hook_code(&hooks_dir, slug, code)
                .with_context(|| format!("writing hook code for '{slug}'"))?;
        }

        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
            },
        );
        progress.tick(slug.as_str());
        pushed += 1;
    }

    Ok((pushed, skipped))
}
