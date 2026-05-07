use crate::api::RossumClient;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
) -> Result<(usize, usize)> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut remote_cache: std::collections::HashMap<u64, crate::model::EmailTemplate> =
        std::collections::HashMap::new();

    for ws_entry in std::fs::read_dir(&workspaces_dir)
        .with_context(|| format!("reading {}", workspaces_dir.display()))?
    {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }

        for q_entry in std::fs::read_dir(&queues_dir)
            .with_context(|| format!("reading {}", queues_dir.display()))?
        {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            let templates_dir = paths.queue_email_templates_dir(&ws_slug, &q_slug);
            if !templates_dir.exists() {
                continue;
            }

            for t_entry in std::fs::read_dir(&templates_dir)
                .with_context(|| format!("reading {}", templates_dir.display()))?
            {
                let t_entry = t_entry?;
                let name = t_entry.file_name().to_string_lossy().to_string();
                let Some(template_slug) = name.strip_suffix(".json") else { continue };
                if template_slug.ends_with(".remote") {
                    continue;
                }
                let lockfile_key = format!("{ws_slug}/{q_slug}/{template_slug}");
                let template_path = templates_dir.join(format!("{template_slug}.json"));

                let disk_bytes = std::fs::read(&template_path)
                    .with_context(|| format!("reading {}", template_path.display()))?;
                let local_combined = content_hash(&disk_bytes);

                let entry = lockfile.objects.get("email_templates").and_then(|m| m.get(&lockfile_key));
                let Some(entry) = entry else {
                    eprintln!("warning: email template '{lockfile_key}' — no lockfile entry, skipping");
                    skipped += 1;
                    continue;
                };
                let Some(base) = &entry.content_hash else {
                    eprintln!("warning: email template '{lockfile_key}' — lockfile entry has no content_hash, skipping");
                    skipped += 1;
                    continue;
                };
                if &local_combined == base {
                    continue;
                }

                let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                    .with_context(|| format!("parsing {}", template_path.display()))?;
                let overlay_paths = overlay.as_ref().and_then(|ov| ov.email_template(&lockfile_key));
                if let Some(p) = overlay_paths {
                    apply_overrides(&mut payload, p);
                }
                let payload_template: crate::model::EmailTemplate = serde_json::from_value(payload)
                    .with_context(|| format!("deserializing overlay-applied email template '{lockfile_key}'"))?;

                let id = entry.id;
                if remote_cache.is_empty() {
                    let remotes = client.list_email_templates().await
                        .context("listing email templates to verify no drift before push")?;
                    for r in remotes {
                        remote_cache.insert(r.id, r);
                    }
                }
                let Some(remote_template) = remote_cache.get(&id).cloned() else {
                    eprintln!("warning: email template '{lockfile_key}' — id {id} not found on remote, skipping");
                    skipped += 1;
                    continue;
                };
                let mut remote_bytes = serde_json::to_vec_pretty(&remote_template)
                    .context("serializing remote email template")?;
                remote_bytes.push(b'\n');
                let remote_bytes = maybe_strip_overlay(remote_bytes, overlay_paths)?;
                let remote_combined = content_hash(&remote_bytes);
                let mut payload_to_send = payload_template;
                if &remote_combined != base {
                    use crate::cli::resolve::{resolve_push_drift, PushDriftOutcome};
                    match resolve_push_drift(interactive, &template_path, &remote_bytes)? {
                        PushDriftOutcome::Patch { payload_override } => {
                            if let Some(bytes) = payload_override {
                                payload_to_send = serde_json::from_slice(&bytes)
                                    .with_context(|| format!("re-deserializing edited email template '{lockfile_key}'"))?;
                            }
                        }
                        PushDriftOutcome::Adopt => {
                            write_atomic(&template_path, &remote_bytes)
                                .with_context(|| format!("adopting remote into {}", template_path.display()))?;
                            lockfile.upsert(
                                "email_templates",
                                &lockfile_key,
                                ObjectEntry {
                                    id,
                                    url: Some(remote_template.url.clone()),
                                    modified_at: remote_template.modified_at().map(|s| s.to_string()),
                                    content_hash: Some(remote_combined),
                                },
                            );
                            skipped += 1;
                            continue;
                        }
                        PushDriftOutcome::Skip => {
                            eprintln!(
                                "warning: email template '{lockfile_key}' — remote has changed since last pull, skipping push (run `rdc pull` first)"
                            );
                            skipped += 1;
                            continue;
                        }
                    }
                }

                let updated = client.update_email_template(id, &payload_to_send).await
                    .with_context(|| format!("PATCH /email_templates/{id}"))?;

                let mut updated_bytes = serde_json::to_vec_pretty(&updated)
                    .context("serializing updated email template")?;
                updated_bytes.push(b'\n');
                let updated_bytes = maybe_strip_overlay(updated_bytes, overlay_paths)?;
                let updated_hash = content_hash(&updated_bytes);
                write_atomic(&template_path, &updated_bytes)
                    .with_context(|| format!("writing post-push canonical form for email template '{lockfile_key}'"))?;

                lockfile.upsert(
                    "email_templates",
                    &lockfile_key,
                    ObjectEntry {
                        id: updated.id,
                        url: Some(updated.url.clone()),
                        modified_at: updated.modified_at().map(|s| s.to_string()),
                        content_hash: Some(updated_hash),
                    },
                );
                pushed += 1;
            }
        }
    }

    Ok((pushed, skipped))
}
