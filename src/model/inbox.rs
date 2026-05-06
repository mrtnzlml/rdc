use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum inbox. Each inbox is attached to one queue (1:1) and provides an
/// email-ingestion endpoint.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Inbox {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub email: String,
    /// URL of the queue this inbox is attached to.
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Inbox {
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
            "id": 813566,
            "url": "https://x.rossum.app/api/v1/inboxes/813566",
            "name": "Cost Invoices Inbox",
            "email": "cost-invoices@org.rossum.app",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-04-10T09:00:00Z",
            "filters": []
        });
        let inbox: Inbox = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(inbox.id, 813566);
        assert_eq!(inbox.email, "cost-invoices@org.rossum.app");
        assert_eq!(inbox.queues.len(), 1);
        let round_trip = serde_json::to_value(&inbox).unwrap();
        assert_eq!(round_trip, payload);
    }
}
