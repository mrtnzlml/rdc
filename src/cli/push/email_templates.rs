use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::{strip_for_create, strip_patch_extra};
use crate::snapshot::writer::write_atomic;
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
/// local siblings that share a name (e.g. two custom "Annotation status change
/// - received" templates on the same queue) must adopt DISTINCT remote ids.
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
                            content_hash: None,
                            secrets_hash: None,
                        },
                    );
                    let created_bytes =
                        crate::cli::pull::common::portabilize_proposed(&created_art.json, lockfile);
                    let created_hash =
                        combined_hash(&created_bytes, &created_art.sidecars, lockfile);
                    write_atomic(template_path, &created_bytes).with_context(|| {
                        format!("writing post-create canonical form for '{lockfile_key}'")
                    })?;
                    lockfile.upsert(
                        "email_templates",
                        lockfile_key,
                        ObjectEntry {
                            id: created.id,
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
        crate::snapshot::refs::resolve_value(&mut payload, lockfile);
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
        let remote_bytes = remote_art.json;
        let remote_combined = combined_hash(&remote_bytes, &remote_art.sidecars, lockfile);
        let mut payload_to_send = payload_template;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, template_path, &remote_bytes, env)? {
                PushDriftOutcome::Patch { payload_override } => {
                    if let Some(bytes) = payload_override {
                        let mut ov: serde_json::Value = serde_json::from_slice(&bytes)
                            .with_context(|| {
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
                    write_atomic(template_path, &remote_bytes).with_context(|| {
                        format!("adopting remote into {}", template_path.display())
                    })?;
                    lockfile.upsert(
                        "email_templates",
                        lockfile_key,
                        ObjectEntry {
                            id,
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
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("email_template/{lockfile_key}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
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
}
