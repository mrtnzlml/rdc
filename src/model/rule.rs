use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum rule. Attached to one or more queues; carries business-logic config.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Rule {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl Rule {
    pub fn modified_at(&self) -> Option<&str> {
        crate::model::modified_at(&self.extra)
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
            "id": 2597,
            "url": "https://x.rossum.app/api/v1/rules/2597",
            "name": "E-invoice Validation Warning",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-04-15T08:00:00Z",
            "trigger": "annotation_content",
            "rule_actions": []
        });
        let r: Rule = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(r.id, 2597);
        assert_eq!(r.name, "E-invoice Validation Warning");
        assert_eq!(r.queues.len(), 1);
        let round_trip = serde_json::to_value(&r).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_queues_defaults_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/rules/1",
            "name": "R"
        });
        let r: Rule = serde_json::from_value(payload).unwrap();
        assert!(r.queues.is_empty());
    }
}
