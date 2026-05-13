//! Store-extension support for `rdc push` and `rdc deploy`. Centralises:
//!   - Effective `token_owner` resolution (per-hook overlay → defaults → None).
//!   - Template-URL resolution against the target cluster (later tasks).
//!   - Install-body construction (later tasks).
//!   - Interactive `token_owner` picker (later tasks).

pub use crate::cli::resolve::{prompt_token_owner, render_token_owner_picker};

use anyhow::{anyhow, Result};
use crate::model::{Hook, HookTemplate};
use crate::overlay::Overlay;
use serde_json::Value;
use std::collections::BTreeMap;

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
                "template '{}' is not available on {tgt_env_label}. Templates with install_action=request_access require Rossum sales to enable; copy templates may have been withdrawn. Install manually via the UI on {tgt_env_label}, then re-run rdc pull {tgt_env_label}.",
                src.name
            )),
            1 => { out.insert(src_url.to_string(), matches[0].url.clone()); }
            n => {
                let ids: Vec<&str> = matches.iter()
                    .map(|t| t.url.rsplit('/').next().unwrap_or("?"))
                    .collect();
                return Err(anyhow!(
                    "ambiguous templates for '{}' on {tgt_env_label} ({n} matches, ids {}); add a mapping under [hook_templates] in .rdc/map/<src>→{tgt_env_label}.toml.",
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
