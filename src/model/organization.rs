use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum organization (one per env). The pull command fetches a single
/// organization per env (the one whose ID is in `rdc.toml`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Organization {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Organization {
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
            "id": 285704,
            "url": "https://x.rossum.app/api/v1/organizations/285704",
            "name": "Acme",
            "modified_at": "2026-03-01T08:00:00Z",
            "settings": { "ui_settings": { "language": "en" } },
            "users": ["https://x.rossum.app/api/v1/users/1"]
        });
        let org: Organization = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(org.id, 285704);
        assert_eq!(org.name, "Acme");
        assert_eq!(org.modified_at(), Some("2026-03-01T08:00:00Z"));
        let round_trip = serde_json::to_value(&org).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn modified_at_absent_returns_none() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/organizations/1",
            "name": "Min"
        });
        let org: Organization = serde_json::from_value(payload).unwrap();
        assert_eq!(org.modified_at(), None);
    }
}
