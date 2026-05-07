use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::KindProgress;
use crate::snapshot::hook::{read_hook_value, serialize_hook, write_hook_code};
use crate::snapshot::writer::write_atomic;
use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &KindProgress,
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

        let entry = lockfile.objects.get("hooks").and_then(|m| m.get(slug.as_str()));
        let Some(entry) = entry else {
            progress.suspend(|| {
                eprintln!("warning: hooks/{slug}.json — no lockfile entry, skipping (push only updates existing objects)");
            });
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            progress.suspend(|| {
                eprintln!("warning: hooks/{slug}.json — lockfile entry has no content_hash, skipping");
            });
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let id = entry.id;

        // Read raw Value (with .py spliced in) so overlay can re-add fields
        // stripped by pull (spec §9.3) BEFORE typed deserialize.
        let mut payload = read_hook_value(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.hook(slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied hook '{slug}'"))?;

        // Drift check: fetch remote, serialize, strip same overlay paths,
        // hash. Compare to base (which was recorded post-strip on pull).
        if remote_hooks.is_none() {
            remote_hooks = Some(
                client.list_hooks(Some(progress)).await
                    .context("listing hooks to verify no drift before push")?,
            );
        }
        let remote_list = remote_hooks.as_ref().unwrap();
        let Some(remote_hook) = remote_list.iter().find(|h| h.id == id) else {
            progress.suspend(|| {
                eprintln!("warning: hooks/{slug}.json — id {id} not found on remote, skipping");
            });
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = serialize_hook(remote_hook)?;
        let remote_json_stripped = maybe_strip_overlay(remote_json_full, overlay_paths)?;
        let remote_combined = hook_combined_hash(&remote_json_stripped, &remote_code);
        let mut payload_to_send = payload_hook;
        if &remote_combined != &base {
            // Drift detected. The hook is a combined-hash kind (json + py);
            // the resolver prompt shows json bytes for the diff (most
            // common case). On Adopt, we write both .json and .py from
            // the remote so disk + lockfile stay aligned.
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, local_json_path, &remote_json_stripped)? {
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
                    progress.suspend(|| {
                        eprintln!(
                            "warning: hooks/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
                        );
                    });
                    skipped += 1;
                    continue;
                }
            }
        }

        let updated = client.update_hook(id, &payload_to_send, Some(progress)).await
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
        progress.tick();
        pushed += 1;
    }

    Ok((pushed, skipped))
}
