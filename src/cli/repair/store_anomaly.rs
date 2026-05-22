//! `rdc repair <env> --fix-store-anomaly` — repair hooks with
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

    let token = resolve_token(&cwd, env)?;
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
                fixed += 1;
            }
            crate::cli::resolve::AnomalyCure::Reinstall => {
                // Task 6 implements this.
                return Err(anyhow!(
                    "Reinstall (Cure A) is not yet implemented; pick [c] convert or [s] skip"
                ));
            }
        }
    }

    lockfile.save(&paths.lockfile())
        .with_context(|| format!("saving lockfile to {}", paths.lockfile().display()))?;

    log.event(Action::Repair, &format!(
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
        Action::Repair,
        &format!("hooks/{slug} (id {}) \u{2192} converted to custom", updated.id),
    );
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
}
