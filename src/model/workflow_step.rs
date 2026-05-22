use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum workflow step. Belongs to a workflow; defines one stage in a
/// queue-to-queue transition.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct WorkflowStep {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    pub workflow: String,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl WorkflowStep {
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
            "id": 1,
            "url": "https://x.rossum.app/api/v1/workflow_steps/1",
            "name": "Manager Approval",
            "workflow": "https://x.rossum.app/api/v1/workflows/700",
            "modified_at": "2026-04-20T08:00:00Z",
            "step_type": "approval"
        });
        let s: WorkflowStep = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(s.id, 1);
        assert_eq!(s.workflow, "https://x.rossum.app/api/v1/workflows/700");
        let round_trip = serde_json::to_value(&s).unwrap();
        assert_eq!(round_trip, payload);
    }
}
