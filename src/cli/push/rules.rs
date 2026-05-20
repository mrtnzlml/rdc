use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::{ResourceOp, ResourceOutcome, SyncRenderer};
use crate::snapshot::create::strip_for_create;
use crate::snapshot::rule::{read_rule_value, serialize_rule, write_rule_code};
use crate::snapshot::writer::write_atomic;
use crate::state::{rule_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<dyn SyncRenderer>,
    env: &str,
) -> Result<(usize, usize)> {
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let rules_dir = paths.rules_dir();
    progress.phase("pushing rules");
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let mut remote_rules: Option<Vec<crate::model::Rule>> = None;

    for (slug, local_json_path) in changes {
        let local_py_path = rules_dir.join(format!("{slug}.py"));
        let overlay_paths = overlay.as_ref().and_then(|ov| ov.rule(slug));

        // CREATE — no lockfile entry yet.
        if lockfile.objects.get("rules").and_then(|m| m.get(slug.as_str())).is_none() {
            let mut payload = read_rule_value(&rules_dir, slug)
                .with_context(|| format!("reading local rule '{slug}' for create"))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "rules");
            progress.resource_started("rules", slug, ResourceOp::Post);
            let create_result = client.create_rule(&payload, Some(progress.clone())).await
                .with_context(|| format!("POST /rules (creating '{slug}')"));
            let create_outcome = match &create_result {
                Ok(_) => ResourceOutcome::Ok,
                Err(e) => ResourceOutcome::Failed(e.to_string()),
            };
            progress.resource_finished("rules", slug, create_outcome);
            let created = create_result?;
            let (created_json_full, created_code) = serialize_rule(&created)?;
            let created_json_stripped = maybe_strip_overlay(created_json_full, overlay_paths)?;
            let created_hash = rule_combined_hash(&created_json_stripped, &created_code);
            write_atomic(local_json_path, &created_json_stripped)
                .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
            if let Some(code) = &created_code {
                write_rule_code(&rules_dir, slug, code)
                    .with_context(|| format!("writing rule code for '{slug}'"))?;
            }
            lockfile.upsert(
                "rules",
                slug,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.warn_line(&format!("[ok] rules/{slug} POST (id {})", created.id));
            pushed += 1;
            continue;
        }

        // UPDATE — read JSON+.py, splice, drift-check, PATCH.
        let entry = lockfile.objects.get("rules").and_then(|m| m.get(slug.as_str())).unwrap();
        let Some(base) = &entry.content_hash else {
            progress.warn_line(&format!("! rules/{slug} lockfile entry has no content_hash, skipping"));
            skipped += 1;
            continue;
        };
        let base = base.clone();
        let id = entry.id;

        let mut payload = read_rule_value(&rules_dir, slug)
            .with_context(|| format!("reading local rule '{slug}'"))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_rule: crate::model::Rule = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied rule '{slug}'"))?;

        // Drift check.
        if remote_rules.is_none() {
            remote_rules = Some(client.list_rules(Some(progress.clone())).await
                .context("listing rules to verify no drift before push")?);
        }
        let remote_list = remote_rules.as_ref().unwrap();
        let Some(remote_rule) = remote_list.iter().find(|r| r.id == id) else {
            progress.warn_line(&format!("! rules/{slug} id {id} not found on remote, skipping"));
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = serialize_rule(remote_rule)?;
        let remote_json_stripped = maybe_strip_overlay(remote_json_full, overlay_paths)?;
        let remote_combined = rule_combined_hash(&remote_json_stripped, &remote_code);
        let mut payload_to_send = payload_rule;
        if &remote_combined != &base {
            use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
            match resolve_push_drift(interactive, local_json_path, &remote_json_stripped, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes)
                            .with_context(|| format!("re-deserializing edited rule '{slug}'"))?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(local_json_path, &remote_json_stripped)
                        .with_context(|| format!("adopting remote into {}", local_json_path.display()))?;
                    if let Some(code) = &remote_code {
                        write_rule_code(&rules_dir, slug, code)
                            .with_context(|| format!("adopting remote rule code for '{slug}'"))?;
                    } else if local_py_path.exists() {
                        std::fs::remove_file(&local_py_path)
                            .with_context(|| format!("removing stale {}", local_py_path.display()))?;
                    }
                    lockfile.upsert(
                        "rules",
                        slug,
                        ObjectEntry {
                            id,
                            url: Some(remote_rule.url.clone()),
                            modified_at: remote_rule.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.warn_line(&format!("! rules/{slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.warn_line(&format!("! rules/{slug} remote has changed since last sync, skipping push (run `rdc sync` first)"));
                    skipped += 1;
                    continue;
                }
            }
        }

        progress.resource_started("rules", slug, ResourceOp::Patch);
        let patch_result = client.update_rule(id, &payload_to_send, Some(progress.clone())).await
            .with_context(|| format!("PATCH /rules/{id}"));
        let patch_outcome = match &patch_result {
            Ok(_) => ResourceOutcome::Ok,
            Err(e) => ResourceOutcome::Failed(e.to_string()),
        };
        progress.resource_finished("rules", slug, patch_outcome);
        let updated = patch_result?;

        // Refresh local file with the post-strip canonical form.
        let (updated_json_full, updated_code) = serialize_rule(&updated)?;
        let updated_json_stripped = maybe_strip_overlay(updated_json_full, overlay_paths)?;
        let updated_hash = rule_combined_hash(&updated_json_stripped, &updated_code);
        write_atomic(local_json_path, &updated_json_stripped)
            .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
        if let Some(code) = &updated_code {
            write_rule_code(&rules_dir, slug, code)
                .with_context(|| format!("writing rule code for '{slug}'"))?;
        } else if local_py_path.exists() {
            // Server dropped the trigger_condition; remove the stale .py.
            std::fs::remove_file(&local_py_path)
                .with_context(|| format!("removing stale {}", local_py_path.display()))?;
        }

        lockfile.upsert(
            "rules",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.warn_line(&format!("[ok] rules/{slug} PATCH"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
