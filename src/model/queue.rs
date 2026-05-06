use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum queue. Each queue belongs to a workspace and carries one schema
/// (and optionally one inbox).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Queue {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub workspace: String,
    pub schema: String,
    /// Optional inbox URL. Many queues do not have an inbox.
    #[serde(default)]
    pub inbox: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Queue {
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
            "id": 2137275,
            "url": "https://x.rossum.app/api/v1/queues/2137275",
            "name": "Cost Invoices (AT)",
            "workspace": "https://x.rossum.app/api/v1/workspaces/700852",
            "schema": "https://x.rossum.app/api/v1/schemas/1824379",
            "inbox": "https://x.rossum.app/api/v1/inboxes/813566",
            "modified_at": "2026-04-10T09:00:00Z",
            "settings": { "default_score_threshold": 0.8 }
        });
        let q: Queue = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(q.id, 2137275);
        assert_eq!(q.name, "Cost Invoices (AT)");
        assert_eq!(q.inbox.as_deref(), Some("https://x.rossum.app/api/v1/inboxes/813566"));
        let round_trip = serde_json::to_value(&q).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_inbox_defaults_to_none() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/queues/1",
            "name": "No Inbox",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/1"
        });
        let q: Queue = serde_json::from_value(payload).unwrap();
        assert!(q.inbox.is_none());
    }
}
