use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied, PullAction, PullCtx,
};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

/// Pull all email templates. Templates are queue-scoped in the live API
/// (each carries a `queue` URL), so the snapshot nests them under the
/// owning queue:
///
/// ```text
/// envs/<env>/workspaces/<ws>/queues/<q>/email-templates/<slug>.json
/// ```
///
/// Lockfile keys are namespaced as `<ws_slug>/<q_slug>/<template_slug>` so
/// per-template slugs don't collide across queues (most queues carry the
/// same five built-in templates).
///
/// Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let templates = skip_on_permission_denied(
        ctx.client.list_email_templates().await.context("listing email templates"),
        "email_templates",
    )?;

    let mut per_queue_used_slugs: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut count = 0usize;
    let mut conflicts = 0usize;

    for t in &templates {
        let queue_url = match &t.queue {
            Some(u) => u,
            None => {
                eprintln!(
                    "warning: skipping email template '{}' (id {}) — no queue (org-wide template not supported by snapshot layout)",
                    t.name, t.id
                );
                continue;
            }
        };

        let Some((ws_slug, q_slug)) = ctx.queue_locations.get(queue_url).cloned() else {
            eprintln!(
                "warning: skipping email template '{}' (id {}) — queue {} not in snapshot (orphan/filtered)",
                t.name, t.id, queue_url
            );
            continue;
        };

        let used = per_queue_used_slugs
            .entry((ws_slug.clone(), q_slug.clone()))
            .or_default();
        let template_slug = match ctx.lockfile.slug_for_id("email_templates", t.id) {
            // Existing key is `<ws>/<q>/<template>`; take the last segment.
            Some(existing) => existing
                .rsplit('/')
                .next()
                .unwrap_or(existing)
                .to_string(),
            None => slugify_unique(&t.name, used),
        };
        used.insert(template_slug.clone());
        let lockfile_key = format!("{ws_slug}/{q_slug}/{template_slug}");

        let dir = ctx.paths.queue_email_templates_dir(&ws_slug, &q_slug);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;

        let mut proposed = serde_json::to_vec_pretty(t).context("serializing email template")?;
        proposed.push(b'\n');
        let proposed = maybe_strip_overlay(
            proposed,
            ctx.overlay.as_ref().and_then(|o| o.email_template(&lockfile_key)),
        )?;

        let local_path = dir.join(format!("{template_slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("email_templates")
            .and_then(|m| m.get(&lockfile_key))
            .and_then(|x| x.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash)?;

        record_object(
            ctx.lockfile,
            "email_templates",
            &lockfile_key,
            t.id,
            Some(t.url.clone()),
            t.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        count += 1;
    }

    Ok((count, conflicts))
}
