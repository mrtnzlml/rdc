//! Store-extension support shared by `rdc sync` push and `rdc doctor`.
//! Centralises:
//!   - The store-extension anomaly guard (`extension_source: rossum_store`
//!     must carry a `hook_template`).
//!   - Orphan detection for the two-call install (POST /hooks/create + PATCH).
//!   - Install-body construction for `POST /hooks/create`.

use crate::model::Hook;
use anyhow::{Result, anyhow};
use serde_json::Value;

/// Find a remote hook matching `(name, hook_template)`. Used after a
/// previously-failed two-call create to adopt the partial install instead
/// of POSTing again.
pub fn find_orphan<'a>(hooks: &'a [Hook], name: &str, template_url: &str) -> Option<&'a Hook> {
    hooks
        .iter()
        .find(|h| h.name == name && h.hook_template() == Some(template_url))
}

/// Defensive guard: a hook with `extension_source: "rossum_store"` must
/// always have `hook_template` set. Production data violates this when a
/// client PATCHes `extension_source` to `"rossum_store"` without going
/// through `POST /hooks/create` — the API silently drops `hook_template`
/// on direct write but accepts the marker, leaving the hook in this
/// broken state. The fix is `rdc doctor <env>`.
pub fn check_store_extension_anomaly(hook: &Hook, slug: &str, env: &str) -> Result<()> {
    if hook.is_store_extension() && hook.hook_template().is_none() {
        return Err(anyhow!(
            "hooks/{slug}.json (id {id}) on env '{env}': marked as store extension \
             (extension_source = rossum_store) but missing hook_template URL.\n\
             \n\
             Two fixes:\n\
               - Convert to custom (one PATCH, hook id preserved): the rossum_store\n\
                 tag was added in error; the hook isn't really a Store template instance.\n\
               - Reinstall as store extension (new hook id, dependents rewired): the\n\
                 hook genuinely is a Store template instance and the hook_template link\n\
                 should be restored.\n\
             \n\
             Run `rdc doctor {env}` to choose interactively.",
            id = hook.id
        ));
    }
    Ok(())
}

/// Extract `{name, hook_template, events, queues, token_owner}` from a
/// full hook body and return them as the `POST /hooks/create` payload.
/// Any field present but null counts as missing (matches the API).
pub fn build_install_body(full: &Value) -> Result<Value> {
    let obj = full
        .as_object()
        .ok_or_else(|| anyhow!("hook body is not a JSON object"))?;
    let mut out = serde_json::Map::new();
    for field in ["name", "hook_template", "events", "queues", "token_owner"] {
        let value = obj
            .get(field)
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                anyhow!("store extension is missing required field '{field}' for /hooks/create")
            })?
            .clone();
        out.insert(field.to_string(), value);
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            body["hook_template"].as_str().unwrap(),
            "https://elis/api/v1/hook_templates/39"
        );
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["queues"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["token_owner"].as_str().unwrap(),
            "https://elis/api/v1/users/938493"
        );
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
    fn check_anomaly_rejects_store_extension_without_template() {
        let payload = serde_json::json!({
            "id": 12345, "url": "u", "name": "x", "type": "webhook",
            "extension_source": "rossum_store"
        });
        let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
        let err = check_store_extension_anomaly(&hook, "broken-slug", "prod").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("broken-slug"), "names the slug: {msg}");
        assert!(msg.contains("prod"), "names the env: {msg}");
        assert!(msg.contains("12345"), "names the hook id: {msg}");
        assert!(msg.contains("hook_template"), "explains the problem: {msg}");
        assert!(
            msg.contains("rdc doctor prod"),
            "points at the doctor command: {msg}"
        );
        assert!(
            msg.contains("Convert to custom") || msg.contains("convert to custom"),
            "names Cure B: {msg}"
        );
        assert!(
            msg.contains("Reinstall") || msg.contains("reinstall"),
            "names Cure A: {msg}"
        );
    }

    #[test]
    fn check_anomaly_passes_for_regular_hook() {
        let payload = serde_json::json!({"id": 1, "url": "u", "name": "x", "type": "function", "extension_source": "custom"});
        let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
        assert!(check_store_extension_anomaly(&hook, "x", "dev").is_ok());
    }

    #[test]
    fn check_anomaly_passes_for_store_extension_with_template() {
        let payload = serde_json::json!({
            "id": 1, "url": "u", "name": "x", "type": "webhook",
            "extension_source": "rossum_store",
            "hook_template": "https://x/api/v1/hook_templates/1"
        });
        let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
        assert!(check_store_extension_anomaly(&hook, "x", "dev").is_ok());
    }

    #[test]
    fn find_orphan_matches_by_name_and_template() {
        use crate::model::Hook;
        let hooks: Vec<Hook> = vec![
            serde_json::from_value(serde_json::json!({
                "id": 100, "url": "u100", "name": "Master Data Hub", "type": "webhook",
                "extension_source": "rossum_store",
                "hook_template": "https://elis/api/v1/hook_templates/39"
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "id": 101, "url": "u101", "name": "Master Data Hub", "type": "webhook",
                "extension_source": "rossum_store",
                "hook_template": "https://elis/api/v1/hook_templates/27"
            }))
            .unwrap(),
        ];
        let orphan = find_orphan(
            &hooks,
            "Master Data Hub",
            "https://elis/api/v1/hook_templates/39",
        );
        assert_eq!(orphan.map(|h| h.id), Some(100));

        let none = find_orphan(
            &hooks,
            "No Such Hook",
            "https://elis/api/v1/hook_templates/39",
        );
        assert!(none.is_none());
    }
}
