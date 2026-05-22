use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum MDH collection (dataset) metadata. Captures the structural attributes
/// of a dataset; row data is intentionally NOT included per spec §11.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Collection {
    pub name: String,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "name": "vendors",
            "size": 42,
            "options": { "capped": false }
        });
        let c: Collection = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(c.name, "vendors");
        let round_trip = serde_json::to_value(&c).unwrap();
        assert_eq!(round_trip, payload);
    }
}
