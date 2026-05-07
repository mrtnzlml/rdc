use super::common::{
    apply_pull_action, maybe_strip_overlay, record_object, skip_on_permission_denied,
    PullAction, PullCtx,
};
use crate::progress::KindProgress;
use crate::slug::slugify_unique;
use crate::snapshot::hook::{serialize_hook, write_hook_code};
use crate::state::hook_combined_hash;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all hooks. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<(usize, usize)> {
    let hooks = skip_on_permission_denied(
        ctx.client.list_hooks(Some(progress)).await.context("listing hooks"),
        "hooks",
        progress,
    )?;
    progress.set_total(hooks.len() as u64);

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
        // Strip overlay-managed paths from the JSON (spec §9.3). Code in
        // <slug>.py is the canonical form for hook code, so strip
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
                // The `interactive` flag is irrelevant on Write (no resolver
                // path); pass `ctx.interactive` for consistency.
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone(), ctx.interactive, progress)?;
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
            PullAction::NoChange => {
                // Combined hash is already equal — no file writes needed.
                remote_combined_hash
            }
            PullAction::Conflict => {
                // Combined-hash conflict (spec §8.3). When both sides
                // have code, walk json and py separately so the user
                // resolves each. Asymmetric cases (one side has code, the
                // other doesn't) keep the shadow-file flow because
                // adding/removing a file isn't "[k]eep / [r]emote / [e]dit"
                // shaped — it's a Write/Delete decision the resolver
                // doesn't model in v1.
                let local_json = pre_local_json.as_ref().unwrap();
                let symmetric = matches!((&pre_local_code, &proposed_code), (Some(_), Some(_)))
                    || matches!((&pre_local_code, &proposed_code), (None, None));
                let total = if symmetric && pre_local_code.is_some() { 2 } else { 1 };

                let resolved_json = crate::cli::resolve::resolve_combined_file(
                    1, total,
                    &local_path,
                    local_json,
                    &proposed_json,
                    ctx.interactive && symmetric,
                )?;

                let resolved_code = if symmetric {
                    if let (Some(loc), Some(rem)) = (&pre_local_code, &proposed_code) {
                        let bytes = crate::cli::resolve::resolve_combined_file(
                            2, total,
                            &py_path,
                            loc.as_bytes(),
                            rem.as_bytes(),
                            ctx.interactive,
                        )?;
                        Some(String::from_utf8(bytes)
                            .with_context(|| format!("hook code resolved bytes for '{}' are not UTF-8", hook.name))?)
                    } else {
                        None
                    }
                } else {
                    // Asymmetric — fall back to shadow for the .py side.
                    if let Some(remote_code_str) = &proposed_code {
                        let py_remote_path = ctx.paths.hooks_dir().join(format!("{slug}.py.remote"));
                        crate::snapshot::writer::write_atomic(&py_remote_path, remote_code_str.as_bytes())?;
                    }
                    pre_local_code.clone()
                };

                hook_combined_hash(&resolved_json, &resolved_code)
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
        progress.tick();
    }

    Ok((hooks.len(), conflicts))
}
