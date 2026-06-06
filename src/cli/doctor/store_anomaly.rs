//! `rdc doctor <env>` — fix hooks with
//! `extension_source: "rossum_store"` and `hook_template: null`.
//!
//! Two cures, picked interactively per hook:
//!
//! * **Convert to custom (Cure B)**: one `PATCH /hooks/<id>
//!   {"extension_source": "custom"}`. Hook id preserved; no
//!   rewiring; instant. Right answer when the rossum_store tag was
//!   added in error.
//!
//! * **Reinstall as store extension (Cure A)**: `POST /hooks/create`
//!   with the right template URL, `PATCH` the new hook to mirror
//!   the old settings, swap `run_after` URLs on every dependent,
//!   `DELETE` the old hook. New hook id. Right answer when the
//!   hook genuinely is a Store template instance.

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::model::Hook;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

/// Walk the local snapshot's `hooks/` directory and return every hook
/// with `extension_source: "rossum_store"` AND `hook_template: null`.
/// Returns `(slug, hook)` pairs sorted by slug.
pub fn find_anomalies(paths: &Paths) -> Result<Vec<(String, Hook)>> {
    let hooks_dir = paths.hooks_dir();
    let mut out = Vec::new();
    if !hooks_dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let slug = path.file_stem().and_then(|s| s.to_str())
            .unwrap_or("").to_string();
        let hook = crate::snapshot::hook::read_hook(&hooks_dir, &slug)?;
        if hook.is_store_extension() && hook.hook_template().is_none() {
            out.push((slug, hook));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

pub async fn run(env: &str, check: bool, yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let env_cfg = cfg.envs.get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);
    let log = Log::new(crate::cli::resolve::detect_color_mode(false));

    let anomalies = find_anomalies(&paths)?;
    if anomalies.is_empty() {
        log.event(Action::Info, &format!("no anomalous store-extension hooks in env '{env}'"));
        return Ok(());
    }

    log.event(Action::Info, &format!(
        "{} anomalous hook(s) in env '{env}':",
        anomalies.len()
    ));
    for (slug, hook) in &anomalies {
        log.event(Action::Info, &format!("  hooks/{slug}  (id {}, name {:?}, type {})",
            hook.id, hook.name, hook.hook_type));
    }

    if check {
        return Ok(());
    }

    let token = resolve_token(&cwd, env, &env_cfg.api_base).await?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing API client")?;
    let mut lockfile = Lockfile::load(&paths.lockfile())
        .with_context(|| format!("loading lockfile from {}", paths.lockfile().display()))?;

    let interactive = crate::cli::resolve::is_interactive(yes);

    let mut fixed = 0usize;
    let mut skipped = 0usize;
    for (slug, hook) in anomalies {
        let cure = crate::cli::resolve::prompt_anomaly_cure(&slug, &hook, interactive)?;
        match cure {
            crate::cli::resolve::AnomalyCure::Skip => {
                log.event(Action::Skip, &format!("hooks/{slug} (id {})", hook.id));
                skipped += 1;
            }
            crate::cli::resolve::AnomalyCure::Convert => {
                convert_to_custom(&client, &mut lockfile, &paths, &slug, &hook, &log).await?;
                // Persist after each successful cure so a mid-batch
                // failure on a later hook doesn't leave drift between
                // the (already-mutated) on-disk + remote state for this
                // hook and the lockfile.
                lockfile.save(&paths.lockfile())
                    .with_context(|| format!("saving lockfile to {}", paths.lockfile().display()))?;
                fixed += 1;
            }
            crate::cli::resolve::AnomalyCure::Reinstall => {
                reinstall_as_store_extension(&client, &mut lockfile, &paths, &slug, &hook, &log).await?;
                lockfile.save(&paths.lockfile())
                    .with_context(|| format!("saving lockfile to {}", paths.lockfile().display()))?;
                fixed += 1;
            }
        }
    }

    lockfile.save(&paths.lockfile())
        .with_context(|| format!("saving lockfile to {}", paths.lockfile().display()))?;

    log.event(Action::Doctor, &format!(
        "done env '{env}': {fixed} fixed, {skipped} skipped"
    ));
    Ok(())
}

/// Cure B — `PATCH /hooks/<id> {"extension_source": "custom"}`,
/// rewrite the local snapshot to match, update the lockfile entry.
/// Hook id is preserved; no rewiring needed.
async fn convert_to_custom(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    paths: &Paths,
    slug: &str,
    hook: &Hook,
    log: &std::sync::Arc<Log>,
) -> Result<()> {
    let body = serde_json::json!({"extension_source": "custom"});
    let updated = client
        .update_hook_value(hook.id, &body, Some(log.clone()))
        .await
        .with_context(|| format!("PATCH /hooks/{} (cure B for hooks/{slug})", hook.id))?;

    // Reuse the pull side's canonical serialization so disk + hash stay
    // aligned with what a future pull would write.
    let (json_bytes, code) = crate::snapshot::hook::serialize_hook(&updated)?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())?;
    let stripped = crate::cli::pull::common::maybe_strip_overlay(
        json_bytes,
        overlay.as_ref().and_then(|o| o.hook(slug)),
    )?;
    let hash = crate::state::hook_combined_hash(&stripped, &code);
    let local_path = paths.hooks_dir().join(format!("{slug}.json"));
    crate::snapshot::writer::write_atomic(&local_path, &stripped)
        .with_context(|| format!("writing post-cure snapshot to {}", local_path.display()))?;

    // The hook's `extension_source` lives in `extra` (flattened in the
    // model), so the typed update + serialize cycle above already
    // round-trips the new value. Lockfile entry id/url unchanged; only
    // content_hash + modified_at move.
    let prior = lockfile.objects.get("hooks").and_then(|m| m.get(slug)).cloned();
    lockfile.upsert(
        "hooks",
        slug,
        crate::state::ObjectEntry {
            id: updated.id,
            url: Some(updated.url.clone()),
            modified_at: updated.modified_at().map(|s| s.to_string()),
            content_hash: Some(hash),
            secrets_hash: prior.and_then(|p| p.secrets_hash),
        },
    );
    log.event(
        Action::Doctor,
        &format!("hooks/{slug} (id {}) \u{2192} converted to custom", updated.id),
    );
    Ok(())
}

/// Pick the single tgt template matching `(name, type, "rossum_store")`
/// for an anomalous hook. Errors describe what the user must do to
/// disambiguate. Mirrors `build_template_url_map` in
/// `cli::deploy::store_extensions` but for the single-env case where
/// the user is fixing an existing hook.
pub fn match_template<'a>(
    hook: &Hook,
    templates: &'a [crate::model::HookTemplate],
) -> Result<&'a crate::model::HookTemplate> {
    let key = (hook.name.as_str(), hook.hook_type.as_str(), "rossum_store");
    let matches: Vec<&crate::model::HookTemplate> = templates.iter()
        .filter(|t| (t.name.as_str(), t.template_type.as_str(), t.extension_source.as_str()) == key)
        .collect();
    match matches.len() {
        0 => Err(anyhow!(
            "template matching ({:?}, type={}, extension_source=rossum_store) is not available on this env. \
             Either install the template manually via the Rossum UI then re-run, or pick the Convert cure if the hook isn't really a Store template instance.",
            hook.name, hook.hook_type
        )),
        1 => Ok(matches[0]),
        n => Err(anyhow!(
            "ambiguous templates for ({:?}, type={}) on this env ({n} matches: {}). \
             Manual intervention required — DELETE the anomalous hook and re-install via the Rossum UI with the right template.",
            hook.name, hook.hook_type,
            matches.iter().map(|t| t.url.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Cure A — reinstall via POST /hooks/create, mirror old settings via
/// PATCH, swap run_after URLs on dependents, DELETE old hook, reconcile
/// local snapshot. New hook id; brief overlap.
async fn reinstall_as_store_extension(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    paths: &Paths,
    slug: &str,
    old_hook: &Hook,
    log: &std::sync::Arc<Log>,
) -> Result<()> {
    // 1. Match template by (name, type, "rossum_store").
    let templates = client.list_hook_templates(Some(log.clone())).await
        .context("listing hook templates for Cure A")?;
    let template = match_template(old_hook, &templates)?;

    // 2. List existing hooks for orphan check + dependent rewiring.
    let remote_hooks = client.list_hooks(Some(log.clone())).await
        .context("listing hooks for orphan check")?;
    let installed_id = match crate::cli::deploy::store_extensions::find_orphan(
        &remote_hooks, &old_hook.name, &template.url,
    ) {
        Some(orphan) if orphan.id != old_hook.id => {
            log.event(Action::Info, &format!(
                "hooks/{slug}: adopting orphan id {} (previous install was interrupted)",
                orphan.id
            ));
            orphan.id
        }
        _ => {
            let token_owner = old_hook.extra.get("token_owner").cloned()
                .unwrap_or(serde_json::Value::Null);
            let install_body = serde_json::json!({
                "name": old_hook.name,
                "hook_template": template.url,
                "events": old_hook.events,
                "queues": old_hook.queues,
                "token_owner": token_owner,
            });
            let installed = client.create_hook_via_install(&install_body, Some(log.clone())).await
                .with_context(|| format!("POST /hooks/create (Cure A for hooks/{slug})"))?;
            log.event(Action::Info, &format!(
                "hooks/{slug}: installed new id {} from template {:?}",
                installed.id, template.name
            ));
            installed.id
        }
    };

    // 3. PATCH the new hook with the old hook's mutable settings.
    let mut patch_body = serde_json::Map::new();
    for field in ["settings", "active", "run_after", "sideload", "description", "metadata"] {
        if let Some(v) = old_hook.extra.get(field).cloned() {
            patch_body.insert(field.to_string(), v);
        }
    }
    if !old_hook.config.is_null()
        && !old_hook.config.as_object().map(|m| m.is_empty()).unwrap_or(true)
    {
        patch_body.insert("config".into(), old_hook.config.clone());
    }
    let updated = client.update_hook_value(installed_id, &serde_json::Value::Object(patch_body), Some(log.clone())).await
        .with_context(|| format!("PATCH /hooks/{installed_id} (mirror old settings)"))?;

    // 4. Rewire dependents' run_after.
    // `old_hook` comes from the local snapshot, whose refs are portabilized to
    // `rdc://` form, but dependents reference the old hook by its remote URL.
    // Resolve the remote URL by id from the freshly-listed remote hooks.
    let old_url = remote_hooks
        .iter()
        .find(|h| h.id == old_hook.id)
        .map(|h| h.url.clone())
        .unwrap_or_else(|| old_hook.url.clone());
    let new_url = updated.url.clone();
    let mut rewired = 0usize;
    for h in &remote_hooks {
        if h.id == old_hook.id || h.id == installed_id {
            continue;
        }
        let run_after = h.extra.get("run_after")
            .and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if !run_after.iter().any(|v| v.as_str() == Some(&old_url)) {
            continue;
        }
        let new_run_after: Vec<serde_json::Value> = run_after.into_iter()
            .map(|v| if v.as_str() == Some(&old_url) {
                serde_json::Value::String(new_url.clone())
            } else { v })
            .collect();
        let body = serde_json::json!({ "run_after": new_run_after });
        client.update_hook_value(h.id, &body, Some(log.clone())).await
            .with_context(|| format!("PATCH /hooks/{} (rewiring run_after)", h.id))?;
        rewired += 1;
    }
    if rewired > 0 {
        log.event(Action::Info, &format!(
            "hooks/{slug}: rewired {rewired} dependent(s) to new URL"
        ));
    }

    // 5. DELETE the old hook. 404 is acceptable (orphan-adoption case).
    match client
        .delete_path(&format!("/hooks/{}", old_hook.id), Some(log.clone()))
        .await
    {
        Ok(_) => {}
        Err(e) if crate::api::anyhow_has_status(&e, 404) => {
            log.event(Action::Info, &format!(
                "hooks/{slug}: old id {} already gone", old_hook.id
            ));
        }
        Err(e) => return Err(e).with_context(|| format!("DELETE /hooks/{}", old_hook.id)),
    }

    // 6. Reconcile local snapshot for the slug.
    let (json_bytes, code) = crate::snapshot::hook::serialize_hook(&updated)?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())?;
    let stripped = crate::cli::pull::common::maybe_strip_overlay(
        json_bytes, overlay.as_ref().and_then(|o| o.hook(slug))
    )?;
    let hash = crate::state::hook_combined_hash(&stripped, &code);
    let local_path = paths.hooks_dir().join(format!("{slug}.json"));
    crate::snapshot::writer::write_atomic(&local_path, &stripped)
        .with_context(|| format!("writing post-reinstall snapshot to {}", local_path.display()))?;
    if let Some(code_str) = &code {
        let ext = crate::snapshot::hook::hook_code_extension(&updated);
        crate::snapshot::hook::write_hook_code(&paths.hooks_dir(), slug, code_str, ext)
            .with_context(|| format!("writing hook code for {slug}"))?;
    }
    let prior = lockfile.objects.get("hooks").and_then(|m| m.get(slug)).cloned();
    lockfile.upsert("hooks", slug, crate::state::ObjectEntry {
        id: updated.id,
        url: Some(updated.url.clone()),
        modified_at: updated.modified_at().map(|s| s.to_string()),
        content_hash: Some(hash),
        secrets_hash: prior.and_then(|p| p.secrets_hash),
    });

    // 7. Refresh local snapshot + lockfile for each rewired dependent.
    for h in &remote_hooks {
        if h.id == old_hook.id || h.id == installed_id {
            continue;
        }
        let run_after_had_old = h.extra.get("run_after")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some(&old_url)))
            .unwrap_or(false);
        if !run_after_had_old { continue; }
        let Some(dep_slug) = lockfile.slug_for_id("hooks", h.id).map(|s| s.to_string()) else { continue; };
        let fresh = client.get_hook(h.id, Some(log.clone())).await
            .with_context(|| format!("GET /hooks/{} (post-rewire refresh)", h.id))?;
        let (j, c) = crate::snapshot::hook::serialize_hook(&fresh)?;
        let s = crate::cli::pull::common::maybe_strip_overlay(
            j, overlay.as_ref().and_then(|o| o.hook(&dep_slug))
        )?;
        let hh = crate::state::hook_combined_hash(&s, &c);
        let dep_path = paths.hooks_dir().join(format!("{dep_slug}.json"));
        crate::snapshot::writer::write_atomic(&dep_path, &s)?;
        if let Some(code_str) = &c {
            let ext = crate::snapshot::hook::hook_code_extension(&fresh);
            crate::snapshot::hook::write_hook_code(&paths.hooks_dir(), &dep_slug, code_str, ext)?;
        }
        let dep_prior = lockfile.objects.get("hooks").and_then(|m| m.get(&dep_slug)).cloned();
        lockfile.upsert("hooks", &dep_slug, crate::state::ObjectEntry {
            id: fresh.id,
            url: Some(fresh.url.clone()),
            modified_at: fresh.modified_at().map(|s| s.to_string()),
            content_hash: Some(hh),
            secrets_hash: dep_prior.and_then(|p| p.secrets_hash),
        });
    }

    log.event(Action::Doctor, &format!(
        "hooks/{slug}: reinstalled (new id {}); old id {} removed; {} dependent(s) rewired",
        updated.id, old_hook.id, rewired,
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_anomalies_returns_only_store_extensions_missing_template() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join("envs/dev/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let write = |slug: &str, payload: serde_json::Value| {
            std::fs::write(
                hooks_dir.join(format!("{slug}.json")),
                serde_json::to_string_pretty(&payload).unwrap(),
            ).unwrap();
        };

        write("anomalous", json!({
            "id": 42, "url": "u", "name": "Broken", "type": "webhook",
            "queues": [], "events": [], "config": {"private": true},
            "extension_source": "rossum_store"
        }));
        write("healthy-store", json!({
            "id": 43, "url": "u", "name": "OK Store", "type": "webhook",
            "queues": [], "events": [], "config": {"private": true},
            "extension_source": "rossum_store",
            "hook_template": "https://x/api/v1/hook_templates/1"
        }));
        write("custom", json!({
            "id": 44, "url": "u", "name": "Custom Hook", "type": "function",
            "queues": [], "events": [], "config": {},
            "extension_source": "custom"
        }));

        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let out = find_anomalies(&paths).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "anomalous");
        assert_eq!(out[0].1.id, 42);
    }

    #[test]
    fn find_anomalies_returns_empty_when_no_hooks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        assert!(find_anomalies(&paths).unwrap().is_empty());
    }

    #[test]
    fn match_template_picks_unique_by_name_and_type() {
        use crate::model::HookTemplate;
        let templates: Vec<HookTemplate> = serde_json::from_value(json!([
            {"url": "https://x/api/v1/hook_templates/1", "name": "Master Data Hub",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://x/api/v1/hook_templates/2", "name": "Email Notifications",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let hook: Hook = serde_json::from_value(json!({
            "id": 1, "url": "u", "name": "Master Data Hub", "type": "webhook",
            "extension_source": "rossum_store"
        })).unwrap();
        let m = match_template(&hook, &templates).unwrap();
        assert_eq!(m.url, "https://x/api/v1/hook_templates/1");
    }

    #[test]
    fn match_template_errors_on_zero_matches() {
        use crate::model::HookTemplate;
        let templates: Vec<HookTemplate> = vec![];
        let hook: Hook = serde_json::from_value(json!({
            "id": 1, "url": "u", "name": "Mystery Hook", "type": "webhook",
            "extension_source": "rossum_store"
        })).unwrap();
        let err = match_template(&hook, &templates).unwrap_err();
        assert!(format!("{err:#}").contains("Mystery Hook"));
        assert!(format!("{err:#}").contains("not available"));
    }

    #[test]
    fn match_template_errors_on_ambiguous() {
        use crate::model::HookTemplate;
        let templates: Vec<HookTemplate> = serde_json::from_value(json!([
            {"url": "https://x/api/v1/hook_templates/1", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://x/api/v1/hook_templates/2", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let hook: Hook = serde_json::from_value(json!({
            "id": 1, "url": "u", "name": "MDH", "type": "webhook",
            "extension_source": "rossum_store"
        })).unwrap();
        let err = match_template(&hook, &templates).unwrap_err();
        assert!(format!("{err:#}").contains("ambiguous"));
    }
}
