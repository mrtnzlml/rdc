//! Store-extension support for `rdc push` and `rdc deploy`. Centralises:
//!   - Effective `token_owner` resolution (per-hook overlay → defaults → None).
//!   - Template-URL resolution against the target cluster.
//!   - Install-body construction.
//!   - Interactive `token_owner` picker.
//!   - Bootstrap pre-pass: resolves template URLs + prompts for token_owner.

// Re-exported so deploy callers can import the picker from one place.
pub use crate::cli::resolve::{format_user_choices, prompt_token_owner};

use anyhow::{anyhow, Context, Result};
use crate::api::RossumClient;
use crate::mapping::Mapping;
use crate::model::{Hook, HookTemplate};
use crate::overlay::{write_store_extension_token_owner, Overlay};
use crate::progress::ProgressHandle;
use crate::state::Lockfile;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One bootstrap entry per store extension to be created.
#[derive(Debug, Clone)]
pub struct StorePlan {
    pub src_slug: String,
    pub tgt_slug: String,
    pub src_template_url: String,
    pub tgt_template_url: String,
    pub tgt_template_name: String,
    pub token_owner_url: String,
}

/// Build the bootstrap list. Side-effects:
///   - lists src + tgt `/hook_templates` (only when the in-memory map cache misses)
///   - lists tgt `/users` only if at least one missing `token_owner`
///   - prompts (TTY) or refuses (non-TTY) for missing `token_owner` values
///   - writes the chosen URLs to the tgt overlay file
///   - inserts resolved template URL pairs into `mapping.hook_templates`
///     (the caller is responsible for persisting the mapping)
pub async fn plan_store_extension_bootstrap(
    src_paths: &crate::paths::Paths,
    _tgt_paths: &crate::paths::Paths,
    src_client: &RossumClient,
    tgt_client: &RossumClient,
    _src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &mut Mapping,
    tgt_overlay_path: &Path,
    interactive: bool,
    self_user_id: Option<u64>,
    tgt_env_label: &str,
    progress: ProgressHandle,
) -> Result<Vec<StorePlan>> {
    // 1. Walk src snapshot hooks/, identify store-extension hooks that need
    //    bootstrap (no tgt lockfile entry).
    let hooks_dir = src_paths.hooks_dir();
    let mut needed: Vec<(String, crate::model::Hook)> = Vec::new();
    if hooks_dir.exists() {
        for entry in std::fs::read_dir(&hooks_dir)
            .with_context(|| format!("reading {}", hooks_dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let slug = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let hook = crate::snapshot::hook::read_hook(&hooks_dir, &slug)?;
            if !hook.is_store_extension() {
                continue;
            }
            check_store_extension_anomaly(&hook, &slug)?;
            // Auto-mapping: same slug on tgt by default.
            let tgt_slug = mapping.lookup_tgt_slug("hooks", &slug)
                .map(|s| s.to_string())
                .unwrap_or_else(|| slug.clone());
            // Skip if tgt already has it (this is an update path, not bootstrap).
            if tgt_lockfile.objects.get("hooks").and_then(|m| m.get(&tgt_slug)).is_some() {
                continue;
            }
            needed.push((tgt_slug, hook));
        }
    }
    if needed.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Resolve template URLs. List on cache miss only.
    let src_urls: BTreeSet<String> = needed.iter()
        .filter_map(|(_, h)| h.hook_template().map(|s| s.to_string()))
        .collect();
    let uncached: Vec<&str> = src_urls.iter()
        .filter(|u| !mapping.hook_templates.contains_key(*u))
        .map(|s| s.as_str())
        .collect();
    let tgt_templates: Vec<crate::model::HookTemplate> = if !uncached.is_empty() {
        let src_templates = src_client.list_hook_templates(progress.clone()).await
            .context("listing src hook templates")?;
        let tgt = tgt_client.list_hook_templates(progress.clone()).await
            .context("listing tgt hook templates")?;
        let pairs = build_template_url_map(&uncached, &src_templates, &tgt, tgt_env_label)?;
        mapping.hook_templates.extend(pairs);
        tgt
    } else {
        // Even with cache hits, we need tgt_templates for plan-output names.
        tgt_client.list_hook_templates(progress.clone()).await
            .context("listing tgt hook templates for plan output")?
    };

    // 3. Resolve token_owner per hook. Prompt at most once if user picks
    //    "apply to all"; that fills `default_url`, which short-circuits
    //    subsequent prompts in this run.
    let mut overlay = Overlay::load(tgt_overlay_path)?;
    let mut tgt_users: Option<Vec<crate::model::User>> = None;
    let mut default_url: Option<String> = overlay.as_ref()
        .and_then(|ov| ov.defaults.store_extension_token_owner.clone());
    let mut plans = Vec::new();
    for (tgt_slug, hook) in &needed {
        let src_slug = tgt_slug.clone(); // same-slug default; rename via mapping handled at step 1
        let src_template_url = hook.hook_template().expect("check_store_extension_anomaly guarantees hook_template is Some for store extensions").to_string();
        let tgt_template_url = mapping.hook_templates.get(&src_template_url)
            .ok_or_else(|| anyhow!("internal: template URL '{src_template_url}' missing from mapping after resolve"))?
            .clone();
        let tgt_template_name = tgt_templates.iter().find(|t| t.url == tgt_template_url)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "<unknown>".into());

        let resolved = effective_token_owner(overlay.as_ref(), tgt_slug)
            .map(|s| s.to_string())
            .or_else(|| default_url.clone());

        let token_owner_url = match resolved {
            Some(u) => u,
            None => {
                if !interactive {
                    return Err(anyhow!(
                        "deploy needs token_owner for store extension '{tgt_slug}' on {tgt_env_label}, but {} has no [hooks.{tgt_slug}] token_owner and no [defaults] store_extension_token_owner.\nRun 'rdc deploy <src> {tgt_env_label}' on a TTY once to pick interactively, or edit the overlay directly. Aborting before any remote writes.",
                        tgt_overlay_path.display()
                    ));
                }
                if tgt_users.is_none() {
                    tgt_users = Some(tgt_client.list_users(progress.clone()).await
                        .context("listing tgt users for token_owner picker")?);
                }
                let users = tgt_users.as_ref().expect("tgt_users was just populated above");
                let (chosen, apply_all) = match prompt_token_owner(tgt_slug, tgt_env_label, users, self_user_id)? {
                    Some(pair) => pair,
                    None => return Err(anyhow!("deploy aborted at token_owner picker")),
                };
                if apply_all {
                    write_store_extension_token_owner(tgt_overlay_path, None, &chosen)?;
                    default_url = Some(chosen.clone());
                } else {
                    write_store_extension_token_owner(tgt_overlay_path, Some(tgt_slug), &chosen)?;
                }
                // Reload overlay so subsequent lookups see the write.
                overlay = Overlay::load(tgt_overlay_path)?;
                chosen
            }
        };

        plans.push(StorePlan {
            src_slug,
            tgt_slug: tgt_slug.clone(),
            src_template_url,
            tgt_template_url,
            tgt_template_name,
            token_owner_url,
        });
    }
    Ok(plans)
}

/// Resolve the effective `token_owner` URL for a store extension on a
/// given environment. Order: per-hook overlay `token_owner` → overlay
/// `[defaults] store_extension_token_owner` → `None`.
pub fn effective_token_owner<'a>(overlay: Option<&'a Overlay>, slug: &str) -> Option<&'a str> {
    let overlay = overlay?;
    if let Some(per_hook) = overlay.hook(slug)
        .and_then(|m| m.get("token_owner"))
        .and_then(Value::as_str)
    {
        return Some(per_hook);
    }
    overlay.defaults.store_extension_token_owner.as_deref()
}

/// Find a remote hook matching `(name, hook_template)`. Used after a
/// previously-failed two-call create to adopt the partial install instead
/// of POSTing again.
pub fn find_orphan<'a>(hooks: &'a [Hook], name: &str, template_url: &str) -> Option<&'a Hook> {
    hooks.iter().find(|h| h.name == name && h.hook_template() == Some(template_url))
}

/// Defensive guard: a hook with `extension_source: "rossum_store"` must
/// always have `hook_template` set. Production data should never violate
/// this, but a hand-edited snapshot could.
pub fn check_store_extension_anomaly(hook: &Hook, slug: &str) -> Result<()> {
    if hook.is_store_extension() && hook.hook_template().is_none() {
        return Err(anyhow!(
            "hooks/{slug}.json: marked as store extension (extension_source = rossum_store) but missing hook_template URL — refusing to push"
        ));
    }
    Ok(())
}

/// Extract `{name, hook_template, events, queues, token_owner}` from a
/// full hook body and return them as the `POST /hooks/create` payload.
/// Any field present but null counts as missing (matches the API).
pub fn build_install_body(full: &Value) -> Result<Value> {
    let obj = full.as_object()
        .ok_or_else(|| anyhow!("hook body is not a JSON object"))?;
    let mut out = serde_json::Map::new();
    for field in ["name", "hook_template", "events", "queues", "token_owner"] {
        let value = obj.get(field)
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("store extension is missing required field '{field}' for /hooks/create"))?
            .clone();
        out.insert(field.to_string(), value);
    }
    Ok(Value::Object(out))
}

/// Build a `src_template_url → tgt_template_url` map by matching templates
/// on `(name, type, extension_source)`. Only templates appearing in
/// `needed_src_urls` are looked up — irrelevant templates are skipped to
/// keep the error surface focused.
pub fn build_template_url_map(
    needed_src_urls: &[&str],
    src_templates: &[HookTemplate],
    tgt_templates: &[HookTemplate],
    tgt_env_label: &str,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for src_url in needed_src_urls {
        let src = src_templates.iter().find(|t| t.url == *src_url)
            .ok_or_else(|| anyhow!(
                "internal: needed src template '{src_url}' not present in src template listing — pull the src env first"
            ))?;
        let key = (src.name.as_str(), src.template_type.as_str(), src.extension_source.as_str());
        let matches: Vec<&HookTemplate> = tgt_templates.iter()
            .filter(|t| (t.name.as_str(), t.template_type.as_str(), t.extension_source.as_str()) == key)
            .collect();
        match matches.len() {
            0 => return Err(anyhow!(
                "template '{}' is not available on {tgt_env_label}. Templates with install_action=request_access require Rossum sales to enable; copy templates may have been withdrawn. Install manually via the UI on {tgt_env_label}, then re-run rdc sync {tgt_env_label}.",
                src.name
            )),
            1 => { out.insert(src_url.to_string(), matches[0].url.clone()); }
            n => {
                let ids: Vec<&str> = matches.iter()
                    .map(|t| t.url.rsplit('/').next().unwrap_or("?"))
                    .collect();
                return Err(anyhow!(
                    "ambiguous templates for '{}' on {tgt_env_label} ({n} matches, ids {}); add a mapping under [hook_templates] in .rdc/map/<src>-to-{tgt_env_label}.toml.",
                    src.name,
                    ids.join(", ")
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{Defaults, Overlay};
    use std::collections::BTreeMap;

    fn ov_with(per_hook: Option<&str>, default_url: Option<&str>) -> Overlay {
        let mut hooks = BTreeMap::new();
        if let Some(url) = per_hook {
            let mut entry = BTreeMap::new();
            entry.insert("token_owner".into(), Value::String(url.into()));
            hooks.insert("master-data-hub".into(), entry);
        }
        Overlay {
            version: 1,
            hooks,
            rules: BTreeMap::new(),
            labels: BTreeMap::new(),
            schemas: BTreeMap::new(),
            queues: BTreeMap::new(),
            inboxes: BTreeMap::new(),
            email_templates: BTreeMap::new(),
            engines: BTreeMap::new(),
            engine_fields: BTreeMap::new(),
            defaults: Defaults {
                store_extension_token_owner: default_url.map(|s| s.into()),
            },
        }
    }

    #[test]
    fn per_hook_wins_over_defaults() {
        let ov = ov_with(Some("https://per-hook"), Some("https://default"));
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), Some("https://per-hook"));
    }

    #[test]
    fn falls_back_to_defaults_when_no_per_hook() {
        let ov = ov_with(None, Some("https://default"));
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), Some("https://default"));
    }

    #[test]
    fn returns_none_when_neither_set() {
        let ov = ov_with(None, None);
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), None);
    }

    #[test]
    fn returns_none_when_no_overlay() {
        assert_eq!(effective_token_owner(None, "master-data-hub"), None);
    }

    #[test]
    fn build_install_body_extracts_five_fields() {
        let full = serde_json::json!({
            "name": "Master Data Hub",
            "hook_template": "https://elis/api/v1/hook_templates/39",
            "events": ["annotation_content.initialize", "annotation_content.started"],
            "queues": ["https://elis/api/v1/queues/100", "https://elis/api/v1/queues/101"],
            "token_owner": "https://elis/api/v1/users/938493",
            "settings": { "configurations": ["customized"] },
            "active": false,
            "description": "must not appear in install body",
            "config": { "private": true }
        });
        let body = build_install_body(&full).unwrap();
        assert_eq!(body.as_object().unwrap().len(), 5);
        assert_eq!(body["name"].as_str().unwrap(), "Master Data Hub");
        assert_eq!(body["hook_template"].as_str().unwrap(), "https://elis/api/v1/hook_templates/39");
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["queues"].as_array().unwrap().len(), 2);
        assert_eq!(body["token_owner"].as_str().unwrap(), "https://elis/api/v1/users/938493");
        assert!(body.get("settings").is_none());
        assert!(body.get("description").is_none());
    }

    #[test]
    fn build_install_body_errors_when_required_field_missing() {
        let no_template = serde_json::json!({
            "name": "X", "events": [], "queues": [], "token_owner": "u"
        });
        assert!(build_install_body(&no_template).is_err());
    }

    #[test]
    fn check_anomaly_passes_for_regular_hook() {
        let payload = serde_json::json!({"id": 1, "url": "u", "name": "x", "type": "function", "extension_source": "custom"});
        let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
        assert!(check_store_extension_anomaly(&hook, "x").is_ok());
    }

    #[test]
    fn check_anomaly_passes_for_store_extension_with_template() {
        let payload = serde_json::json!({
            "id": 1, "url": "u", "name": "x", "type": "webhook",
            "extension_source": "rossum_store",
            "hook_template": "https://x/api/v1/hook_templates/1"
        });
        let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
        assert!(check_store_extension_anomaly(&hook, "x").is_ok());
    }

    #[test]
    fn check_anomaly_rejects_store_extension_without_template() {
        let payload = serde_json::json!({
            "id": 1, "url": "u", "name": "x", "type": "webhook",
            "extension_source": "rossum_store"
        });
        let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
        let err = check_store_extension_anomaly(&hook, "broken-slug").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("broken-slug"), "error should name the slug: {msg}");
        assert!(msg.contains("hook_template"), "error should explain the problem: {msg}");
    }

    #[test]
    fn find_orphan_matches_by_name_and_template() {
        use crate::model::Hook;
        let hooks: Vec<Hook> = vec![
            serde_json::from_value(serde_json::json!({
                "id": 100, "url": "u100", "name": "Master Data Hub", "type": "webhook",
                "extension_source": "rossum_store",
                "hook_template": "https://elis/api/v1/hook_templates/39"
            })).unwrap(),
            serde_json::from_value(serde_json::json!({
                "id": 101, "url": "u101", "name": "Master Data Hub", "type": "webhook",
                "extension_source": "rossum_store",
                "hook_template": "https://elis/api/v1/hook_templates/27"
            })).unwrap(),
        ];
        let orphan = find_orphan(&hooks, "Master Data Hub", "https://elis/api/v1/hook_templates/39");
        assert_eq!(orphan.map(|h| h.id), Some(100));

        let none = find_orphan(&hooks, "No Such Hook", "https://elis/api/v1/hook_templates/39");
        assert!(none.is_none());
    }

    #[test]
    fn build_template_url_map_pairs_by_name_type_source() {
        use crate::model::HookTemplate;
        let src: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
            {"url": "https://test/api/v1/hook_templates/39", "name": "Master Data Hub",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://test/api/v1/hook_templates/27", "name": "Email Notifications",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let tgt: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
            {"url": "https://prod/api/v1/hook_templates/41", "name": "Master Data Hub",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://prod/api/v1/hook_templates/27", "name": "Email Notifications",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let needed = ["https://test/api/v1/hook_templates/39",
                      "https://test/api/v1/hook_templates/27"];
        let map = build_template_url_map(&needed, &src, &tgt, "prod").unwrap();
        assert_eq!(map["https://test/api/v1/hook_templates/39"],
                   "https://prod/api/v1/hook_templates/41");
        assert_eq!(map["https://test/api/v1/hook_templates/27"],
                   "https://prod/api/v1/hook_templates/27");
    }

    #[test]
    fn build_template_url_map_errors_on_missing_tgt() {
        use crate::model::HookTemplate;
        let src: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
            {"url": "https://test/api/v1/hook_templates/39", "name": "Master Data Hub",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let tgt: Vec<HookTemplate> = vec![];
        let err = build_template_url_map(&["https://test/api/v1/hook_templates/39"], &src, &tgt, "prod").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Master Data Hub"));
        assert!(msg.contains("not available on prod"));
    }

    #[test]
    fn build_template_url_map_errors_on_ambiguous_tgt() {
        use crate::model::HookTemplate;
        let src: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
            {"url": "https://test/api/v1/hook_templates/39", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let tgt: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
            {"url": "https://prod/api/v1/hook_templates/41", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://prod/api/v1/hook_templates/42", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let err = build_template_url_map(&["https://test/api/v1/hook_templates/39"], &src, &tgt, "prod").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ambiguous"));
    }
}
