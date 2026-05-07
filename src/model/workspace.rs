use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum workspace. Each env has 0..N workspaces, each holding queues.
/// The workspace itself is just metadata; queues are pulled separately
/// and nested under `envs/<env>/workspaces/<slug>/queues/<slug>/`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Workspace {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub organization: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Workspace {
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
            "id": 700852,
            "url": "https://x.rossum.app/api/v1/workspaces/700852",
            "name": "Invoices AP",
            "organization": "https://x.rossum.app/api/v1/organizations/285704",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-03-15T11:00:00Z",
            "metadata": { "tag": "ap" }
        });
        let ws: Workspace = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(ws.id, 700852);
        assert_eq!(ws.name, "Invoices AP");
        assert_eq!(ws.organization, "https://x.rossum.app/api/v1/organizations/285704");
        assert_eq!(ws.queues.len(), 1);
        assert_eq!(ws.modified_at(), Some("2026-03-15T11:00:00Z"));
        let round_trip = serde_json::to_value(&ws).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_queues_defaults_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/workspaces/1",
            "name": "Min",
            "organization": "https://x/api/v1/organizations/1"
        });
        let ws: Workspace = serde_json::from_value(payload).unwrap();
        assert!(ws.queues.is_empty());
    }
}
