use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied,
};
use crate::log::{Action, Log};
use crate::model::Engine;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

const KIND: &str = "engines";

/// Phase 1: list all engines from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<Engine>> {
    skip_on_permission_denied(
        ctx.client
            .list_engines(Some(progress.clone()))
            .await
            .context("listing engines"),
        KIND,
        progress,
    )
}

/// Phase 2: write listed engines to disk. `subset` selects which `(kind,
/// slug)` pairs are written; items outside the subset are skipped silently.
/// Returns `(count, conflicts)` of items written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    engines: Vec<Engine>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for e in &engines {
        let slug = match ctx.lockfile.slug_for_id(KIND, e.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&e.name, &used),
        };
        used.insert(slug.clone());

        if !subset.contains(&(KIND.to_string(), slug.clone())) {
            continue;
        }

        let result: Result<()> = (|| {
            // Each engine owns a directory: `engines/<slug>/`. The engine's
            // JSON lives at `engine.json` inside it, alongside `fields/`.
            let engine_dir = ctx.paths.engine_dir(&slug);
            std::fs::create_dir_all(&engine_dir)
                .with_context(|| format!("creating {}", engine_dir.display()))?;

            // Canonical on-disk bytes via KindCodec: redacts server-set
            // runtime fields (`agenda_id`) and strips `modified_at`.
            let value = serde_json::to_value(e)?;
            let codec = crate::snapshot::codec::codec(KIND).unwrap();
            let art = codec.disk_bytes(&value).context("serializing engine")?;
            let proposed = maybe_strip_overlay(
                art.json,
                ctx.overlay.as_ref().and_then(|o| codec.overlay(o, &slug)),
            )?;

            let local_path = engine_dir.join("engine.json");
            let base_hash = ctx
                .lockfile
                .objects
                .get(KIND)
                .and_then(|m| m.get(&slug))
                .and_then(|x| x.content_hash.clone());

            let proposed =
                crate::cli::pull::common::portabilize_proposed(&proposed, &*ctx.lockfile);
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

            record_object(
                ctx.lockfile,
                KIND,
                &slug,
                e.id,
                e.modified_at().map(|s| s.to_string()),
                Some(recorded_hash),
            );
            written += 1;
            Ok(())
        })();
        result?;
    }

    if written > 0 {
        progress.event(Action::Pull, &format!("engines ({written} pulled)"));
    }

    Ok((written, conflicts))
}
