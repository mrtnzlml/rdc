use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::log::{Action, Log};
use crate::overlay::{Overlay, apply_overrides};
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
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: std::collections::HashMap<u64, crate::model::EmailTemplate> =
        std::collections::HashMap::new();

    // slug (lockfile_key) = "ws_slug/q_slug/template_slug"
    for (lockfile_key, template_path) in changes {
        let overlay_paths = overlay
            .as_ref()
            .and_then(|ov| ov.email_template(lockfile_key));

        // Missing lockfile entry → new email template, POST.
        if lockfile
            .objects
            .get("email_templates")
            .and_then(|m| m.get(lockfile_key.as_str()))
            .is_none()
        {
            let disk_bytes = std::fs::read(template_path)
                .with_context(|| format!("reading {}", template_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", template_path.display()))?;
            if let Some(p) = overlay_paths {
                apply_overrides(&mut payload, p);
            }
            strip_for_create(&mut payload, "email_templates");
            let create_result = client
                .create_email_template(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /email_templates (creating '{lockfile_key}')"));
            let created = create_result?;
            let codec = crate::snapshot::codec::codec("email_templates").unwrap();
            let created_art = codec
                .disk_bytes(
                    &serde_json::to_value(&created)
                        .context("serializing created email template")?,
                )
                .context("codec disk_bytes for created email template")?;
            let created_bytes = maybe_strip_overlay(created_art.json, overlay_paths)?;
            let created_hash = combined_hash(&created_bytes, &created_art.sidecars);
            write_atomic(template_path, &created_bytes).with_context(|| {
                format!("writing post-create canonical form for '{lockfile_key}'")
            })?;
            lockfile.upsert(
                "email_templates",
                lockfile_key,
                ObjectEntry {
                    id: created.id,
                    url: Some(created.url.clone()),
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(
                Action::Post,
                &format!("email_template/{lockfile_key} id={}", created.id),
            );
            pushed += 1;
            continue;
        }

        let disk_bytes = std::fs::read(template_path)
            .with_context(|| format!("reading {}", template_path.display()))?;
        let entry = lockfile
            .objects
            .get("email_templates")
            .and_then(|m| m.get(lockfile_key.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(
                Action::Skip,
                &format!("email_template/{lockfile_key} (no content_hash)"),
            );
            skipped += 1;
            continue;
        };
        let base = base.clone();

        let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
            .with_context(|| format!("parsing {}", template_path.display()))?;
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_template: crate::model::EmailTemplate = serde_json::from_value(payload)
            .with_context(|| {
                format!("deserializing overlay-applied email template '{lockfile_key}'")
            })?;

        let id = entry.id;
        if remote_cache.is_empty() {
            let remotes = client
                .list_email_templates(Some(progress.clone()))
                .await
                .context("listing email templates to verify no drift before push")?;
            for r in remotes {
                remote_cache.insert(r.id, r);
            }
        }
        let Some(remote_template) = remote_cache.get(&id).cloned() else {
            progress.event(
                Action::Skip,
                &format!("email_template/{lockfile_key} (remote id {id} missing)"),
            );
            skipped += 1;
            continue;
        };
        let codec = crate::snapshot::codec::codec("email_templates").unwrap();
        let remote_art = codec
            .disk_bytes(
                &serde_json::to_value(&remote_template)
                    .context("serializing remote email template for drift check")?,
            )
            .context("codec disk_bytes for remote email template")?;
        let remote_bytes = maybe_strip_overlay(remote_art.json, overlay_paths)?;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars);
        let mut payload_to_send = payload_template;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, template_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        payload_to_send = serde_json::from_slice(&bytes).with_context(|| {
                            format!("re-deserializing edited email template '{lockfile_key}'")
                        })?;
                    }
                }
                PushDriftOutcome::Adopt => {
                    write_atomic(template_path, &remote_bytes).with_context(|| {
                        format!("adopting remote into {}", template_path.display())
                    })?;
                    lockfile.upsert(
                        "email_templates",
                        lockfile_key,
                        ObjectEntry {
                            id,
                            url: Some(remote_template.url.clone()),
                            modified_at: remote_template.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.event(
                        Action::Warn,
                        &format!("email_template/{lockfile_key} adopted remote (drift)"),
                    );
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("email_template/{lockfile_key} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
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

        let codec = crate::snapshot::codec::codec("email_templates").unwrap();
        let updated_art = codec
            .disk_bytes(
                &serde_json::to_value(&updated)
                    .context("serializing updated email template for disk write")?,
            )
            .context("codec disk_bytes for updated email template")?;
        let updated_bytes = maybe_strip_overlay(updated_art.json, overlay_paths)?;
        let updated_hash = combined_hash(&updated_bytes, &updated_art.sidecars);
        crate::state::base_cache::write_disk_and_cache(paths, template_path, &updated_bytes)
            .with_context(|| {
                format!("writing post-push canonical form for email template '{lockfile_key}'")
            })?;

        lockfile.upsert(
            "email_templates",
            lockfile_key,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("email_template/{lockfile_key}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
