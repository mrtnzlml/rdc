use super::common::{
    apply_pull_action, maybe_strip_overlay, record_object, skip_on_permission_denied,
    PullAction, PullCtx,
};
use crate::log::{Action, Log};
use crate::model::Rule;
use crate::slug::slugify_unique;
use crate::snapshot::rule::{serialize_rule, write_rule_code};
use crate::state::rule_combined_hash;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Phase 1: list all rules from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<Rule>> {
    skip_on_permission_denied(
        ctx.client.list_rules(Some(progress.clone())).await.context("listing rules"),
        "rules",
        progress,
    )
}

/// Phase 2: write listed rules to disk. Rules carry an optional
/// `trigger_condition` (Python). When present we write it to a sibling
/// `<slug>.py` and use a combined hash for three-way merge. Mirrors
/// the hooks pull driver.
///
/// `subset` selects which `(kind, slug)` pairs are written; items outside
/// the subset are skipped silently.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    rules: Vec<Rule>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for r in &rules {
        let slug = match ctx.lockfile.slug_for_id("rules", r.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&r.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        if !subset.contains(&("rules".to_string(), slug.clone())) {
            continue;
        }

        let result: Result<()> = (|| {

        if !dir_created {
            std::fs::create_dir_all(ctx.paths.rules_dir())
                .with_context(|| format!("creating {}", ctx.paths.rules_dir().display()))?;
            dir_created = true;
        }

        let (proposed_json, proposed_code) = serialize_rule(r)?;
        let proposed_json = maybe_strip_overlay(
            proposed_json,
            ctx.overlay.as_ref().and_then(|o| o.rule(&slug)),
        )?;

        let local_path = ctx.paths.rules_dir().join(format!("{slug}.json"));
        let py_path = ctx.paths.rules_dir().join(format!("{slug}.py"));
        let pre_local_json = if local_path.exists() {
            Some(std::fs::read(&local_path)
                .with_context(|| format!("reading {}", local_path.display()))?)
        } else {
            None
        };
        let pre_local_code = if py_path.exists() {
            Some(std::fs::read_to_string(&py_path)
                .with_context(|| format!("reading {}", py_path.display()))?)
        } else {
            None
        };

        let remote_combined_hash = rule_combined_hash(&proposed_json, &proposed_code);

        let base_hash = ctx
            .lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        let action = match (base_hash.as_deref(), &pre_local_json) {
            (None, _) => PullAction::Write,
            (_, None) => PullAction::Write,
            (Some(base), Some(local_json)) => {
                let local_combined = rule_combined_hash(local_json, &pre_local_code);
                let local_matches = local_combined == base;
                let remote_matches = remote_combined_hash == base;
                match (local_matches, remote_matches) {
                    (true, _) => PullAction::Write,
                    (false, true) => PullAction::KeepLocal,
                    (false, false) => PullAction::Conflict,
                }
            }
        };

        if action == PullAction::Conflict {
            conflicts += 1;
        }

        let recorded_hash = match action {
            PullAction::Write => {
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone(), ctx.interactive, progress, ctx.paths.env(), base_hash.as_deref())?;
                if let Some(code) = &proposed_code {
                    write_rule_code(&ctx.paths.rules_dir(), &slug, code)
                        .with_context(|| format!("writing rule code for '{}'", r.name))?;
                } else if py_path.exists() {
                    std::fs::remove_file(&py_path)
                        .with_context(|| format!("removing stale {}", py_path.display()))?;
                }
                remote_combined_hash
            }
            PullAction::KeepLocal => {
                let local_json = pre_local_json.as_ref().unwrap();
                rule_combined_hash(local_json, &pre_local_code)
            }
            PullAction::NoChange => remote_combined_hash,
            PullAction::Conflict => {
                let local_json = pre_local_json.as_ref().unwrap();
                let symmetric = matches!((&pre_local_code, &proposed_code), (Some(_), Some(_)))
                    || matches!((&pre_local_code, &proposed_code), (None, None));
                let total = if symmetric && pre_local_code.is_some() { 2 } else { 1 };

                let json_outcome = crate::cli::resolve::resolve_combined_file(
                    1, total,
                    &local_path,
                    local_json,
                    &proposed_json,
                    ctx.interactive && symmetric,
                    ctx.paths.env(),
                )?;

                // Same preserve-base intent tracking as `pull::hooks`.
                let mut preserve_base = json_outcome.is_preserve_base();

                let (resolved_json, resolved_code) = if symmetric {
                    let resolved_json = json_outcome.into_bytes();
                    let resolved_code = if let (Some(loc), Some(rem)) =
                        (&pre_local_code, &proposed_code)
                    {
                        let code_outcome = crate::cli::resolve::resolve_combined_file(
                            2, total,
                            &py_path,
                            loc.as_bytes(),
                            rem.as_bytes(),
                            ctx.interactive,
                            ctx.paths.env(),
                        )?;
                        preserve_base |= code_outcome.is_preserve_base();
                        let bytes = code_outcome.into_bytes();
                        Some(String::from_utf8(bytes)
                            .with_context(|| format!("rule code resolved bytes for '{}' are not UTF-8", r.name))?)
                    } else {
                        None
                    };
                    (resolved_json, resolved_code)
                } else {
                    // Asymmetric — fall back to shadow for the .py side.
                    // Unresolved → preserve the prior lockfile base.
                    if let Some(remote_code_str) = &proposed_code {
                        let env = ctx.paths.env();
                        let py_remote_path = ctx.paths.rules_dir().join(format!("{slug}.py.{env}"));
                        crate::snapshot::writer::write_atomic(&py_remote_path, remote_code_str.as_bytes())?;
                    }
                    preserve_base = true;
                    (json_outcome.into_bytes(), pre_local_code.clone())
                };

                if preserve_base {
                    match base_hash.as_deref() {
                        Some(prior) => prior.to_string(),
                        None => rule_combined_hash(&resolved_json, &resolved_code),
                    }
                } else {
                    rule_combined_hash(&resolved_json, &resolved_code)
                }
            }
        };

        record_object(
            ctx.lockfile,
            "rules",
            &slug,
            r.id,
            Some(r.url.clone()),
            r.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        written += 1;
        Ok(())
        })();
        result?;
    }

    if written > 0 {
        progress.event(Action::Pull, &format!("rules ({written} pulled)"));
    }

    Ok((written, conflicts))
}
