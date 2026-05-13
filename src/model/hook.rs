use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Hook {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(rename = "type")]
    pub hook_type: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub config: Value,
    /// Any field we don't model explicitly is preserved here for round-trip fidelity.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Hook {
    /// The server-set `modified_at` timestamp, if present. Currently lives in
    /// the forward-compat `extra` map; this accessor isolates that detail.
    pub fn modified_at(&self) -> Option<&str> {
        crate::model::modified_at(&self.extra)
    }

    /// Returns the `extension_source` value if present and a string —
    /// `"rossum_store"` for store extensions, `"custom"` for user-created
    /// hooks, or `None` if the field is absent or null on the wire.
    pub fn extension_source(&self) -> Option<&str> {
        self.extra.get("extension_source").and_then(|v| v.as_str())
    }

    /// Returns `hook_template` (a URL) if present and a string. For regular
    /// hooks this is `null` on the wire and yields `None` here.
    pub fn hook_template(&self) -> Option<&str> {
        self.extra.get("hook_template").and_then(|v| v.as_str())
    }

    /// True iff this hook came from the Rossum store and must be created via
    /// `POST /hooks/create` rather than the regular `POST /hooks/`.
    pub fn is_store_extension(&self) -> bool {
        self.extension_source() == Some("rossum_store")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "id": 856489,
            "url": "https://example.rossum.app/api/v1/hooks/856489",
            "name": "Validator: invoices",
            "type": "function",
            "queues": ["https://example.rossum.app/api/v1/queues/2137275"],
            "events": ["annotation_content"],
            "config": { "code": "print('hi')", "runtime": "python3.12" },
            "modified_at": "2026-04-01T10:00:00Z",
            "future_field_we_have_not_modeled": { "nested": [1, 2, 3] }
        });

        let hook: Hook = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(hook.id, 856489);
        assert_eq!(hook.name, "Validator: invoices");
        assert_eq!(hook.hook_type, "function");

        // Re-serialize; unknown fields must survive byte-identically.
        let round_trip = serde_json::to_value(&hook).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_optional_lists_default_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://example/api/v1/hooks/1",
            "name": "Minimal",
            "type": "webhook"
        });
        let hook: Hook = serde_json::from_value(payload).unwrap();
        assert!(hook.queues.is_empty());
        assert!(hook.events.is_empty());
    }

    #[test]
    fn modified_at_accessor() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/hooks/1",
            "name": "T",
            "type": "function",
            "modified_at": "2026-04-01T10:00:00Z"
        });
        let hook: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(hook.modified_at(), Some("2026-04-01T10:00:00Z"));

        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/hooks/1",
            "name": "T",
            "type": "function"
        });
        let hook: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(hook.modified_at(), None);
    }

    #[test]
    fn extension_source_reads_from_extra() {
        let payload = json!({
            "id": 1, "url": "u", "name": "n", "type": "webhook",
            "extension_source": "rossum_store",
            "hook_template": "https://x/api/v1/hook_templates/39"
        });
        let h: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(h.extension_source(), Some("rossum_store"));
        assert_eq!(h.hook_template(), Some("https://x/api/v1/hook_templates/39"));
        assert!(h.is_store_extension());
    }

    #[test]
    fn extension_source_custom_is_not_store_extension() {
        let payload = json!({
            "id": 1, "url": "u", "name": "n", "type": "function",
            "extension_source": "custom",
            "hook_template": Value::Null
        });
        let h: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(h.extension_source(), Some("custom"));
        assert_eq!(h.hook_template(), None);
        assert!(!h.is_store_extension());
    }

    #[test]
    fn extension_source_absent_is_none() {
        let payload = json!({"id": 1, "url": "u", "name": "n", "type": "function"});
        let h: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(h.extension_source(), None);
        assert!(!h.is_store_extension());
    }

    #[test]
    fn extension_source_explicit_null_yields_none() {
        // `extension_source: null` on the wire is distinct from "field absent",
        // but `extension_source()` returns `None` for both since the value isn't
        // a string. Verifies the null-vs-absent equivalence at the accessor layer.
        let payload = json!({
            "id": 1, "url": "u", "name": "n", "type": "function",
            "extension_source": Value::Null,
            "hook_template": Value::Null
        });
        let h: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(h.extension_source(), None);
        assert_eq!(h.hook_template(), None);
        assert!(!h.is_store_extension());
    }
}
