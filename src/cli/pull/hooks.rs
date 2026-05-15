use super::common::{
    apply_pull_action, maybe_strip_overlay, record_object, skip_on_permission_denied,
    PullAction, PullCtx,
};
use crate::model::Hook;
use crate::progress::ProgressLog;
use crate::slug::slugify_unique;
use crate::snapshot::hook::{hook_code_extension, serialize_hook, write_hook_code};
use crate::state::hook_combined_hash;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Phase 1: list all hooks from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<ProgressLog>) -> Result<Vec<Hook>> {
    skip_on_permission_denied(
        ctx.client.list_hooks(Some(progress.clone())).await.context("listing hooks"),
        "hooks",
        progress,
    )
}

/// Phase 2: write listed hooks to disk. `subset` selects which `(kind, slug)`
/// pairs are written; items outside the subset are skipped silently. Returns
/// `(count, conflicts)` of items written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    hooks: Vec<Hook>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<ProgressLog>,
) -> Result<(usize, usize)> {
    let phase = progress.phase("pulling hooks");

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for hook in &hooks {
        let slug = match ctx.lockfile.slug_for_id("hooks", hook.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&hook.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        if !subset.contains(&("hooks".to_string(), slug.clone())) {
            continue;
        }

        let sp = phase.item(&hook.name);

        if !dir_created {
            std::fs::create_dir_all(ctx.paths.hooks_dir())
                .with_context(|| format!("creating {}", ctx.paths.hooks_dir().display()))?;
            dir_created = true;
        }

        let (proposed_json, proposed_code) = serialize_hook(hook)?;
        // Strip overlay-managed paths from the JSON (spec §9.3). Code in
        // <slug>.py is the canonical form for hook code, so strip
        // doesn't touch the .py side; users rarely overlay `config.code`.
        let proposed_json = maybe_strip_overlay(
            proposed_json,
            ctx.overlay.as_ref().and_then(|o| o.hook(&slug)),
        )?;

        // Derive the sidecar extension from the hook's runtime — Node.js
        // hooks land in `<slug>.js`, Python (and any unknown runtime) in
        // `<slug>.py`. The detection is centralized in
        // `snapshot::hook::hook_code_extension`.
        let ext = hook_code_extension(hook);
        let local_path = ctx.paths.hooks_dir().join(format!("{slug}.json"));
        let code_path = ctx.paths.hooks_dir().join(format!("{slug}.{ext}"));
        let stale_code_path = ctx
            .paths
            .hooks_dir()
            .join(format!("{slug}.{}", if ext == "py" { "js" } else { "py" }));
        let pre_local_json = if local_path.exists() {
            Some(std::fs::read(&local_path)
                .with_context(|| format!("reading {}", local_path.display()))?)
        } else {
            None
        };
        // Read whichever sidecar happens to exist on disk for the local
        // hash. The runtime-derived one wins if present; otherwise the
        // other extension still contributes — same defensive fallback as
        // `read_hook_value`.
        let pre_local_code = if code_path.exists() {
            Some(std::fs::read_to_string(&code_path)
                .with_context(|| format!("reading {}", code_path.display()))?)
        } else if stale_code_path.exists() {
            Some(std::fs::read_to_string(&stale_code_path)
                .with_context(|| format!("reading {}", stale_code_path.display()))?)
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
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone(), ctx.interactive, progress, ctx.paths.env(), base_hash.as_deref())?;
                if let Some(code) = &proposed_code {
                    write_hook_code(&ctx.paths.hooks_dir(), &slug, code, ext)
                        .with_context(|| format!("writing hook code for '{}'", hook.name))?;
                } else if code_path.exists() {
                    std::fs::remove_file(&code_path)
                        .with_context(|| format!("removing stale {}", code_path.display()))?;
                }
                // Always sweep a sidecar with the other extension —
                // runtime may have just changed, leaving a stale file.
                if stale_code_path.exists() {
                    std::fs::remove_file(&stale_code_path)
                        .with_context(|| format!("removing stale {}", stale_code_path.display()))?;
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

                let json_outcome = crate::cli::resolve::resolve_combined_file(
                    1, total,
                    &local_path,
                    local_json,
                    &proposed_json,
                    ctx.interactive && symmetric,
                    ctx.paths.env(),
                )?;

                // Track preserve-base intent across both sub-files of the
                // combined-hash entity — if either side asks for it, the
                // whole entity's lockfile entry must stay pinned to the
                // prior base (or fall back to the local combined hash
                // when no prior base exists, mirroring `shadow_file_conflict`).
                let mut preserve_base = json_outcome.is_preserve_base();

                let (resolved_json, resolved_code) = if symmetric {
                    let resolved_json = json_outcome.into_bytes();
                    let resolved_code = if let (Some(loc), Some(rem)) =
                        (&pre_local_code, &proposed_code)
                    {
                        let code_outcome = crate::cli::resolve::resolve_combined_file(
                            2, total,
                            &code_path,
                            loc.as_bytes(),
                            rem.as_bytes(),
                            ctx.interactive,
                            ctx.paths.env(),
                        )?;
                        preserve_base |= code_outcome.is_preserve_base();
                        let bytes = code_outcome.into_bytes();
                        Some(String::from_utf8(bytes)
                            .with_context(|| format!("hook code resolved bytes for '{}' are not UTF-8", hook.name))?)
                    } else {
                        None
                    };
                    (resolved_json, resolved_code)
                } else {
                    // Asymmetric — fall back to shadow for the sidecar side.
                    // Shadow uses the runtime-derived extension so the editor
                    // gets the right syntax highlighting on a `.js` vs `.py`.
                    // The JSON side already got the non-interactive shadow
                    // treatment (see the `ctx.interactive && symmetric`
                    // argument to `resolve_combined_file`); the sidecar
                    // side mirrors that here. Either way the conflict is
                    // unresolved → preserve the prior lockfile base.
                    if let Some(remote_code_str) = &proposed_code {
                        let env = ctx.paths.env();
                        let code_remote_path = ctx
                            .paths
                            .hooks_dir()
                            .join(format!("{slug}.{ext}.{env}"));
                        crate::snapshot::writer::write_atomic(&code_remote_path, remote_code_str.as_bytes())?;
                    }
                    preserve_base = true;
                    (json_outcome.into_bytes(), pre_local_code.clone())
                };

                if preserve_base {
                    // At least one sub-file asked for preserve-base —
                    // pin the lockfile to the prior base so the next
                    // pull/sync re-classifies as a conflict. Fall back
                    // to the freshly-computed combined hash only when no
                    // prior base exists (defensive — conflicts presuppose
                    // a prior base).
                    match base_hash.as_deref() {
                        Some(prior) => prior.to_string(),
                        None => hook_combined_hash(&resolved_json, &resolved_code),
                    }
                } else {
                    hook_combined_hash(&resolved_json, &resolved_code)
                }
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
        sp.finish_ok("");
        written += 1;
    }

    Ok((written, conflicts))
}
