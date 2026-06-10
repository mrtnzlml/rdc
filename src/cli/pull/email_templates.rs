use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object,
    skip_on_permission_denied,
};
use crate::log::{Action, Log};
use crate::model::EmailTemplate;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

const KIND: &str = "email_templates";

/// Phase 1: list all email templates from the API.
/// Note: the orphan-skipping logic (templates without a known queue_location)
/// lives in `process`, where ctx.queue_locations is fully populated.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<EmailTemplate>> {
    skip_on_permission_denied(
        ctx.client
            .list_email_templates(Some(progress.clone()))
            .await
            .context("listing email templates"),
        KIND,
        progress,
    )
}

/// Phase 2: write listed email templates to disk.
///
/// Templates are queue-scoped in the live API (each carries a `queue` URL),
/// so the snapshot nests them under the owning queue:
///
/// ```text
/// envs/<env>/workspaces/<ws>/queues/<q>/email-templates/<slug>.json
/// ```
///
/// Lockfile keys are namespaced as `<ws_slug>/<q_slug>/<template_slug>` so
/// per-template slugs don't collide across queues (most queues carry the
/// same five built-in templates).
///
/// ctx.queue_locations must already be populated by queues::process before
/// this is called.
///
/// `subset` selects which `(kind, slug)` pairs are written, where `slug`
/// is the compound `<ws>/<q>/<tpl>` lockfile key; items outside the subset
/// are skipped silently.
///
/// Returns `(count, conflicts)` of items written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    templates: Vec<EmailTemplate>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut per_queue_used_slugs: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut count = 0usize;
    let mut conflicts = 0usize;

    for t in &templates {
        let queue_url = match &t.queue {
            Some(u) => u,
            None => continue,
        };

        let Some((ws_slug, q_slug)) = ctx.queue_locations.get(queue_url).cloned() else {
            continue;
        };

        let used = per_queue_used_slugs
            .entry((ws_slug.clone(), q_slug.clone()))
            .or_default();
        let template_slug = match ctx.lockfile.slug_for_id(KIND, t.id) {
            // Existing key is `<ws>/<q>/<template>`; take the last segment.
            Some(existing) => existing.rsplit('/').next().unwrap_or(existing).to_string(),
            None => slugify_unique(&t.name, used),
        };
        used.insert(template_slug.clone());
        let lockfile_key = format!("{ws_slug}/{q_slug}/{template_slug}");

        if !subset.contains(&(KIND.to_string(), lockfile_key.clone())) {
            continue;
        }

        let result: Result<()> = (|| {
            let dir = ctx.paths.queue_email_templates_dir(&ws_slug, &q_slug);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

            // Canonical on-disk bytes via KindCodec: strips `modified_at`.
            let value = serde_json::to_value(t)?;
            let art = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing email template")?;
            let proposed = art.json;

            let local_path = dir.join(format!("{template_slug}.json"));
            let base_hash = ctx
                .lockfile
                .objects
                .get(KIND)
                .and_then(|m| m.get(&lockfile_key))
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
                &lockfile_key,
                t.id,
                t.modified_at().map(|s| s.to_string()),
                Some(recorded_hash),
            );
            count += 1;
            Ok(())
        })();
        result?;
    }

    if count > 0 {
        progress.event(Action::Pull, &format!("email_templates ({count} pulled)"));
    }

    Ok((count, conflicts))
}
