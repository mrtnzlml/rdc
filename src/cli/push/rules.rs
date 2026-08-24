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
            let (created_json, created_code) = serialize_rule(&created)?;
            // Register the new rule's id NOW so its own `url` (and any ref to an
            // already-created object) portabilizes to `rdc://`. Concrete env URLs
            // must never touch disk, even transiently (an interrupted sync whose
            // portabilize post-pass never runs would freeze them into the snapshot).
            lockfile.upsert(
                "rules",
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
            let created_hash = rule_combined_hash(&created_json, &created_code, lockfile);
            write_atomic(local_json_path, &created_json)
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
                    modified_by: created.modified_by().map(|s| s.to_string()),
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
        let (remote_json, remote_code) = serialize_rule(remote_rule)?;
        let remote_combined = rule_combined_hash(&remote_json, &remote_code, lockfile);
        let mut payload_to_send = payload_rule;
        if remote_combined != base {
            use crate::cli::resolve::{PushDriftOutcome, resolve_push_drift};
            match resolve_push_drift(interactive, local_json_path, &remote_json, env)? {
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
                    // Portabilize the adopted remote so concrete env URLs never
                    // land on disk (the rule is lockfile-pinned; refs resolve).
                    let remote_json =
                        crate::cli::pull::common::portabilize_proposed(&remote_json, lockfile);
                    write_atomic(local_json_path, &remote_json).with_context(|| {
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
                            modified_by: remote_rule.modified_by().map(|s| s.to_string()),
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

        // Refresh local file with the codec's canonical form.
        let (updated_json, updated_code) = serialize_rule(&updated)?;
        // Re-portabilize the server response so concrete env URLs never land on
        // disk (the rule is lockfile-pinned, so self + refs resolve to rdc://).
        let updated_json =
            crate::cli::pull::common::portabilize_proposed(&updated_json, lockfile);
        let updated_hash = rule_combined_hash(&updated_json, &updated_code, lockfile);
        crate::state::base_cache::write_disk_and_cache(
            paths,
            local_json_path,
            &updated_json,
        )
        .with_context(|| format!("writing post-push canonical form for '{slug}'"))?;
        if let Some(code) = &updated_code {
            write_rule_code(&rules_dir, slug, code)
                .with_context(|| format!("writing rule code for '{slug}'"))?;
            // Mirror the `.py` into the base cache (matching `pull::rules`) so a
            // later `BothDiverged` conflict can 3-way-merge the code against a
            // real base instead of falling back to a manual prompt.
            crate::state::base_cache::write(paths, &local_py_path, code.as_bytes())
                .with_context(|| format!("caching base rule code for '{slug}'"))?;
        } else {
            // Server dropped the trigger_condition; remove the stale .py from
            // disk AND the base cache so the snapshot stays canonical.
            if local_py_path.exists() {
                std::fs::remove_file(&local_py_path)
                    .with_context(|| format!("removing stale {}", local_py_path.display()))?;
            }
            crate::state::base_cache::forget(paths, &local_py_path)?;
        }

        lockfile.upsert(
            "rules",
            slug,
            ObjectEntry {
                id: updated.id,
                modified_at: updated.modified_at().map(|s| s.to_string()),
                modified_by: updated.modified_by().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
                secrets_hash: None,
            },
        );
        progress.event(Action::Patch, &format!("rule/{slug}"));
        pushed += 1;
    }

    Ok((pushed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::snapshot::rule::serialize_rule;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Regression: after a rule PATCH push, the extracted `trigger_condition`
    /// sidecar (`<slug>.py`) must be mirrored into the base cache — exactly as
    /// `pull::rules` does — so a later `BothDiverged` conflict can 3-way-merge
    /// the code against a real base. Before the fix the push wrote the `.py`
    /// only to the working tree, leaving the base cache without it.
    #[tokio::test]
    async fn push_patch_rule_caches_code_sidecar_to_base() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let rules_dir = paths.rules_dir();
        std::fs::create_dir_all(&rules_dir).unwrap();

        let local = serde_json::json!({
            "name": "My Rule",
            "url": "rdc://rules/my-rule",
            "queues": [],
            "trigger": "annotation_content",
            "rule_actions": []
        });
        std::fs::write(
            rules_dir.join("my-rule.json"),
            serde_json::to_vec_pretty(&local).unwrap(),
        )
        .unwrap();
        std::fs::write(rules_dir.join("my-rule.py"), b"amount > 0\n").unwrap();

        let mut lockfile = Lockfile {
            api_base: api.clone(),
            ..Lockfile::default()
        };
        // Register the id before computing base so the self-url portabilizes.
        lockfile.upsert(
            "rules",
            "my-rule",
            ObjectEntry {
                id: 700,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        // Remote rule carries the trigger_condition (extracted to the .py).
        let remote = serde_json::json!({
            "id": 700,
            "url": format!("{api}/rules/700"),
            "name": "My Rule",
            "queues": [],
            "trigger": "annotation_content",
            "rule_actions": [],
            "trigger_condition": "amount > 0\n"
        });
        let remote_rule: crate::model::Rule = serde_json::from_value(remote.clone()).unwrap();
        let (rj, rc) = serialize_rule(&remote_rule).unwrap();
        let base = rule_combined_hash(&rj, &rc, &lockfile);
        lockfile.upsert(
            "rules",
            "my-rule",
            ObjectEntry {
                id: 700,
                modified_at: None,
                modified_by: None,
                content_hash: Some(base),
                secrets_hash: None,
            },
        );

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": null }, "results": [remote.clone()]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/rules/700"))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress =
            std::sync::Arc::new(crate::log::Log::new(crate::cli::resolve::ColorMode::Plain));
        let mut changes = BTreeMap::new();
        changes.insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        let (pushed, _skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        assert_eq!(pushed, 1, "the rule should be patched");

        let code_path = rules_dir.join("my-rule.py");
        let base_py = crate::state::base_cache::cache_mirror(&paths, &code_path)
            .expect("code path is under env root");
        assert!(
            base_py.exists(),
            "base cache must contain the rule code sidecar:\n{}",
            base_py.display()
        );
        assert_eq!(
            std::fs::read_to_string(&base_py).unwrap(),
            "amount > 0\n",
            "base cached code must match the pushed code"
        );
    }
}
