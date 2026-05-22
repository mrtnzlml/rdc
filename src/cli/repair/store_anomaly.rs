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

    // Construct the client; Tasks 5/6 will use it for the cures.
    let token = resolve_token(&cwd, env)?;
    let _client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing API client")?;
    let _lockfile = Lockfile::load(&paths.lockfile())
        .with_context(|| format!("loading lockfile from {}", paths.lockfile().display()))?;
    let _ = yes; // wired in Task 5

    // Task 5 implements Cure B; Task 6 implements Cure A.
    Err(anyhow!(
        "the per-hook prompt is implemented in the next task; for now, use --check to list anomalies"
    ))
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
