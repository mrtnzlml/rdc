use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum engine. Document-extraction model configuration.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Engine {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl Engine {
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
            "id": 401,
            "url": "https://x.rossum.app/api/v1/engines/401",
            "name": "Invoice Engine",
            "modified_at": "2026-04-15T08:00:00Z",
            "type": "extractor",
            "agenda_id": "invoices"
        });
        let e: Engine = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(e.id, 401);
        assert_eq!(e.name, "Invoice Engine");
        let round_trip = serde_json::to_value(&e).unwrap();
        assert_eq!(round_trip, payload);
    }
}
