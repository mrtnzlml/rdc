//! Store-extension support for `rdc push` and `rdc deploy`. Centralises:
//!   - Effective `token_owner` resolution (per-hook overlay → defaults → None).
//!   - Template-URL resolution against the target cluster (later tasks).
//!   - Install-body construction (later tasks).
//!   - Interactive `token_owner` picker (later tasks).

use anyhow::{anyhow, Result};
use crate::overlay::Overlay;
use serde_json::Value;

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
}
