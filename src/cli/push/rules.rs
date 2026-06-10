use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;

use crate::snapshot::create::{strip_for_create, strip_patch_extra};
use crate::snapshot::rule::{read_rule_value, serialize_rule, write_rule_code};
use crate::snapshot::writer::write_atomic;
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

    let mut remote_rules: Option<Vec<crate::model::Rule>> = None;

    for (slug, local_json_path) in changes {
        let local_py_path = rules_dir.join(format!("{slug}.py"));

        // CREATE — no lockfile entry yet.
        if lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(slug.as_str()))
            .is_none()
        {
            let mut payload = read_rule_value(&rules_dir, slug)
                .with_context(|| format!("reading local rule '{slug}' for create"))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            strip_for_create(&mut payload, "rules");
            let create_result = client
                .create_rule(&payload, Some(progress.clone()))
                .await
                .with_context(|| format!("POST /rules (creating '{slug}')"));
            let created = create_result?;
            let (created_json_full, created_code) = serialize_rule(&created)?;
            let created_json_stripped = created_json_full;
            let created_hash = rule_combined_hash(&created_json_stripped, &created_code, lockfile);
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
                    modified_at: created.modified_at().map(|s| s.to_string()),
                    content_hash: Some(created_hash),
                    secrets_hash: None,
                },
            );
            progress.event(Action::Post, &format!("rule/{slug} id={}", created.id));
            pushed += 1;
            continue;
        }

        // UPDATE — read JSON+.py, splice, drift-check, PATCH.
        let entry = lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(slug.as_str()))
            .unwrap();
        let Some(base) = &entry.content_hash else {
            progress.event(Action::Skip, &format!("rule/{slug} (no content_hash)"));
            skipped += 1;
            continue;
        };
        let base = base.clone();
        let id = entry.id;

        let mut payload = read_rule_value(&rules_dir, slug)
            .with_context(|| format!("reading local rule '{slug}'"))?;
        crate::snapshot::refs::resolve_value(&mut payload, lockfile);
        let payload_rule: crate::model::Rule = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied rule '{slug}'"))?;

        // Drift check.
        if remote_rules.is_none() {
            remote_rules = Some(
                client
                    .list_rules(Some(progress.clone()))
                    .await
                    .context("listing rules to verify no drift before push")?,
            );
        }
        let remote_list = remote_rules.as_ref().unwrap();
        let Some(remote_rule) = remote_list.iter().find(|r| r.id == id) else {
            progress.event(
                Action::Skip,
                &format!("rule/{slug} (remote id {id} missing)"),
            );
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = serialize_rule(remote_rule)?;
        let remote_json_stripped = remote_json_full;
        let remote_combined = rule_combined_hash(&remote_json_stripped, &remote_code, lockfile);
        let mut payload_to_send = payload_rule;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, local_json_path, &remote_json_stripped, env)? {
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
                    write_atomic(local_json_path, &remote_json_stripped).with_context(|| {
                        format!("adopting remote into {}", local_json_path.display())
                    })?;
                    if let Some(code) = &remote_code {
                        write_rule_code(&rules_dir, slug, code)
                            .with_context(|| format!("adopting remote rule code for '{slug}'"))?;
                    } else if local_py_path.exists() {
                        std::fs::remove_file(&local_py_path).with_context(|| {
                            format!("removing stale {}", local_py_path.display())
                        })?;
                    }
                    lockfile.upsert(
                        "rules",
                        slug,
                        ObjectEntry {
                            id,
                            modified_at: remote_rule.modified_at().map(|s| s.to_string()),
                            content_hash: Some(remote_combined),
                            secrets_hash: None,
                        },
                    );
                    progress.event(Action::Warn, &format!("rule/{slug} adopted remote (drift)"));
                    skipped += 1;
                    continue;
                }
                PushDriftOutcome::Skip => {
                    progress.event(
                        Action::Skip,
                        &format!("rule/{slug} (remote changed; rdc sync first)"),
                    );
                    skipped += 1;
                    continue;
                }
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

        // Refresh local file with the post-strip canonical form.
        let (updated_json_full, updated_code) = serialize_rule(&updated)?;
        let updated_json_stripped = updated_json_full;
        let updated_hash = rule_combined_hash(&updated_json_stripped, &updated_code, lockfile);
        crate::state::base_cache::write_disk_and_cache(
            paths,
            local_json_path,
            &updated_json_stripped,
        )
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
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("rule/{slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}
