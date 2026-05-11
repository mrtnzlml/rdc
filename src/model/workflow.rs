use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum workflow. Org-level orchestration for queue-to-queue transitions.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Workflow {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Workflow {
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
            "id": 700,
            "url": "https://x.rossum.app/api/v1/workflows/700",
            "name": "AP Approval Flow",
            "steps": [
                "https://x.rossum.app/api/v1/workflow_steps/1",
                "https://x.rossum.app/api/v1/workflow_steps/2"
            ],
            "modified_at": "2026-04-20T08:00:00Z",
            "queue": "https://x.rossum.app/api/v1/queues/100"
        });
        let w: Workflow = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(w.id, 700);
        assert_eq!(w.steps.len(), 2);
        let round_trip = serde_json::to_value(&w).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_steps_defaults_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/workflows/1",
            "name": "W"
        });
        let w: Workflow = serde_json::from_value(payload).unwrap();
        assert!(w.steps.is_empty());
    }
}
