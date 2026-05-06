use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Hook {
    pub id: u64,
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
        self.extra.get("modified_at").and_then(|v| v.as_str())
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
}
