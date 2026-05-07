use super::common::{
    apply_pull_action, maybe_strip_overlay, record_object, skip_on_permission_denied,
    PullAction, PullCtx,
};
use crate::slug::slugify_unique;
use crate::snapshot::hook::{serialize_hook, write_hook_code};
use crate::state::hook_combined_hash;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all hooks. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let hooks = skip_on_permission_denied(
        ctx.client.list_hooks().await.context("listing hooks"),
        "hooks",
    )?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for hook in &hooks {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.hooks_dir())
                .with_context(|| format!("creating {}", ctx.paths.hooks_dir().display()))?;
            dir_created = true;
        }

        let slug = match ctx.lockfile.slug_for_id("hooks", hook.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&hook.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        let (proposed_json, proposed_code) = serialize_hook(hook)?;
        // Strip overlay-managed paths from the JSON (M26 / spec §9.3).
        // Code in <slug>.py is the canonical form for hook code, so strip
        // doesn't touch the .py side; users rarely overlay `config.code`.
        let proposed_json = maybe_strip_overlay(
            proposed_json,
            ctx.overlay.as_ref().and_then(|o| o.hook(&slug)),
        )?;

        let local_path = ctx.paths.hooks_dir().join(format!("{slug}.json"));
        let py_path = ctx.paths.hooks_dir().join(format!("{slug}.py"));
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

        let remote_combined_hash = hook_combined_hash(&proposed_json, &proposed_code);

        let base_hash = ctx
            .lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        let action = match (base_hash.as_deref(), &pre_local_json) {
            (None, _) => PullAction::Write,
            (_, None) => PullAction::Write,
            (Some(base), Some(local_json)) => {
                let local_combined = hook_combined_hash(local_json, &pre_local_code);
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
                // Write branch — the `interactive` flag is irrelevant (no
                // resolver path); pass `ctx.interactive` for consistency.
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone(), ctx.interactive)?;
                if let Some(code) = &proposed_code {
                    write_hook_code(&ctx.paths.hooks_dir(), &slug, code)
                        .with_context(|| format!("writing hook code for '{}'", hook.name))?;
                } else if py_path.exists() {
                    std::fs::remove_file(&py_path)
                        .with_context(|| format!("removing stale {}", py_path.display()))?;
                }
                remote_combined_hash
            }
            PullAction::KeepLocal => {
                let local_json = pre_local_json.as_ref().unwrap();
                hook_combined_hash(local_json, &pre_local_code)
            }
            PullAction::Conflict => {
                // Hooks are a combined-hash kind (json + py). The §8.3
                // resolver currently only handles single-file JSON conflicts,
                // so we force `interactive=false` here and stay on the
                // shadow-file path: <slug>.json.remote + <slug>.py.remote.
                // M33 (or later) extends the resolver to walk both files.
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone(), false)?;
                if let Some(code) = &proposed_code {
                    let py_remote_path = ctx.paths.hooks_dir().join(format!("{slug}.py.remote"));
                    crate::snapshot::writer::write_atomic(&py_remote_path, code.as_bytes())?;
                }
                let local_json = pre_local_json.as_ref().unwrap();
                hook_combined_hash(local_json, &pre_local_code)
            }
        };

        record_object(
            ctx.lockfile,
            "hooks",
            &slug,
            hook.id,
            Some(hook.url.clone()),
            hook.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((hooks.len(), conflicts))
}
