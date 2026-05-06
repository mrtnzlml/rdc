use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum engine field. Defines a single extractable field on an engine.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EngineField {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub engine: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl EngineField {
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
            "id": 501,
            "url": "https://x.rossum.app/api/v1/engine_fields/501",
            "name": "Invoice ID",
            "engine": "https://x.rossum.app/api/v1/engines/401",
            "modified_at": "2026-04-15T08:00:00Z",
            "field_type": "string"
        });
        let ef: EngineField = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(ef.id, 501);
        assert_eq!(ef.engine, "https://x.rossum.app/api/v1/engines/401");
        let round_trip = serde_json::to_value(&ef).unwrap();
        assert_eq!(round_trip, payload);
    }
}
