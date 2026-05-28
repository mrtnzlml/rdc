use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied, PullAction, PullCtx,
};
use crate::log::{Action, Log};
use crate::model::EngineField;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Phase 1: list all engine fields from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<EngineField>> {
    skip_on_permission_denied(
        ctx.client.list_engine_fields(Some(progress.clone())).await.context("listing engine fields"),
        "engine_fields",
        progress,
    )
}

/// Phase 2: write listed engine fields to disk. Each field nests under
/// its parent engine at `engines/<engine_slug>/fields/<field_slug>.json`.
/// Orphan fields (no engine in the lockfile) are skipped with a warning
/// — same pattern as orphan queues.
///
/// Lockfile keys are namespaced as `<engine_slug>/<field_slug>` so two
/// engines can both carry a field called `Amount` and keep clean per-engine
/// slugs (mirrors the email_templates per-queue scoping). `slugify_unique`
/// runs against a per-engine `used` set, not a global one.
///
/// Legacy flat-key entries (lockfiles written before the composite-key
/// migration) are auto-rewritten on the first sync after upgrade: when
/// `slug_for_id` matches a flat entry for this field, the entry is moved
/// to the composite key and the field's content_hash baseline is preserved.
///
/// `subset` selects which `(kind, composite_key)` pairs are written;
/// items outside the subset are skipped silently.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    fields: Vec<EngineField>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut per_engine_used: HashMap<String, HashSet<String>> = HashMap::new();
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for f in &fields {
        let Some(engine_slug) = ctx.lockfile.slug_for_url("engines", &f.engine).map(|s| s.to_string()) else {
            progress.event(Action::Skip, &format!(
                "engine field '{}' (id {}) — unknown engine URL '{}'; skipping",
                f.name, f.id, f.engine
            ));
            continue;
        };

        let used = per_engine_used.entry(engine_slug.clone()).or_default();
        let (field_slug, legacy_flat_key) = match ctx.lockfile.slug_for_id("engine_fields", f.id) {
            Some(existing) => {
                let existing = existing.to_string();
                if let Some(tail) = existing.strip_prefix(&format!("{engine_slug}/")) {
                    (tail.to_string(), None)
                } else {
                    (existing.clone(), Some(existing))
                }
            }
            None => (slugify_unique(&f.name, used), None),
        };
        used.insert(field_slug.clone());
        let composite_key = format!("{engine_slug}/{field_slug}");

        if !subset.contains(&("engine_fields".to_string(), composite_key.clone())) {
            continue;
        }

        let result: Result<()> = (|| {

        let fields_dir = ctx.paths.engine_fields_dir(&engine_slug);
        std::fs::create_dir_all(&fields_dir)
            .with_context(|| format!("creating {}", fields_dir.display()))?;

        let mut proposed = serde_json::to_vec_pretty(f).context("serializing engine field")?;
        proposed.push(b'\n');
        let proposed = maybe_strip_overlay(
            proposed,
            ctx.overlay.as_ref().and_then(|o| o.engine_field(&composite_key)),
        )?;

        let local_path = fields_dir.join(format!("{field_slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("engine_fields")
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
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive, progress, ctx.paths.env(), base_hash.as_deref())?;

        if let Some(old) = legacy_flat_key.as_deref()
            && old != composite_key
            && let Some(m) = ctx.lockfile.objects.get_mut("engine_fields") {
            m.remove(old);
        }
        record_object(
            ctx.lockfile,
            "engine_fields",
            &composite_key,
            f.id,
            Some(f.url.clone()),
            f.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        written += 1;
        Ok(())
        })();
        result?;
    }

    if written > 0 {
        progress.event(Action::Pull, &format!("engine_fields ({written} pulled)"));
    }

    Ok((written, conflicts))
}
