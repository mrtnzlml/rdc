use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object,
    skip_on_permission_denied,
};
use crate::log::{Action, Log};
use crate::model::WorkflowStep;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

const KIND: &str = "workflow_steps";

/// Phase 1: list all workflow steps from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<WorkflowStep>> {
    skip_on_permission_denied(
        ctx.client
            .list_workflow_steps(Some(progress.clone()))
            .await
            .context("listing workflow steps"),
        KIND,
        progress,
    )
}

/// Phase 2: write listed workflow steps to disk. Each step nests under
/// its parent workflow at `workflows/<workflow_slug>/steps/<step_slug>.json`.
/// Orphan steps (no workflow in the lockfile) are skipped with a warning.
///
/// Lockfile keys are namespaced as `<workflow_slug>/<step_slug>` so two
/// workflows can both carry a step with the same name and keep clean
/// per-workflow slugs (mirrors `pull::engine_fields` and
/// `pull::email_templates`). Legacy flat-key lockfile entries are auto-
/// rewritten on the first sync after upgrade.
///
/// `subset` selects which `(kind, composite_key)` pairs are written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    steps: Vec<WorkflowStep>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut per_workflow_used: HashMap<String, HashSet<String>> = HashMap::new();
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for s in &steps {
        let Some(workflow_slug) = ctx
            .lockfile
            .slug_for_url("workflows", &s.workflow)
            .map(|x| x.to_string())
        else {
            progress.event(
                Action::Skip,
                &format!(
                    "workflow step '{}' (id {}) — unknown workflow URL '{}'; skipping",
                    s.name, s.id, s.workflow
                ),
            );
            continue;
        };

        let used = per_workflow_used.entry(workflow_slug.clone()).or_default();
        let (step_slug, legacy_flat_key) = match ctx.lockfile.slug_for_id(KIND, s.id) {
            Some(existing) => {
                let existing = existing.to_string();
                if let Some(tail) = existing.strip_prefix(&format!("{workflow_slug}/")) {
                    (tail.to_string(), None)
                } else {
                    (existing.clone(), Some(existing))
                }
            }
            None => (slugify_unique(&s.name, used), None),
        };
        used.insert(step_slug.clone());
        let composite_key = format!("{workflow_slug}/{step_slug}");

        if !subset.contains(&(KIND.to_string(), composite_key.clone())) {
            continue;
        }

        let result: Result<()> = (|| {
            let steps_dir = ctx.paths.workflow_steps_dir(&workflow_slug);
            std::fs::create_dir_all(&steps_dir)
                .with_context(|| format!("creating {}", steps_dir.display()))?;

            // Canonical on-disk bytes via KindCodec: strips `modified_at`.
            // No overlay for workflow_steps.
            let value = serde_json::to_value(s)?;
            let art = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing workflow step")?;
            let proposed = art.json;

            let local_path = steps_dir.join(format!("{step_slug}.json"));
            let base_hash = ctx
                .lockfile
                .objects
                .get(KIND)
                .and_then(|m| {
                    m.get(&composite_key)
                        .or_else(|| legacy_flat_key.as_deref().and_then(|k| m.get(k)))
                })
                .and_then(|x| x.content_hash.clone());

            let (action, remote_hash) =
                decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
            if action == PullAction::Conflict {
                conflicts += 1;
            }
            let recorded_hash = apply_pull_action(
                action,
                &local_path,
                &proposed,
                remote_hash,
                ctx.interactive,
                progress,
                ctx.paths.env(),
                base_hash.as_deref(),
                Some(ctx.paths),
            )?;

            if let Some(old) = legacy_flat_key.as_deref()
                && old != composite_key
                && let Some(m) = ctx.lockfile.objects.get_mut(KIND)
            {
                m.remove(old);
            }
            record_object(
                ctx.lockfile,
                KIND,
                &composite_key,
                s.id,
                Some(s.url.clone()),
                s.modified_at().map(|x| x.to_string()),
                Some(recorded_hash),
            );
            written += 1;
            Ok(())
        })();
        result?;
    }

    if written > 0 {
        progress.event(Action::Pull, &format!("workflow_steps ({written} pulled)"));
    }

    Ok((written, conflicts))
}
