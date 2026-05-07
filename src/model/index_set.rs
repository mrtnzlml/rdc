use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The set of indexes on an MDH collection. Regular MongoDB indexes and Atlas
/// Search indexes have different shapes; both are stored as opaque `Value`s
/// for forward-compat (definitions can change without breaking round-trip).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct IndexSet {
    /// Regular MongoDB indexes (b-tree, hashed, geo, etc.)
    #[serde(default)]
    pub regular: Vec<Value>,
    /// Atlas Search indexes (full-text indexes with mappings + analyzers)
    #[serde(default)]
    pub search: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_with_both_kinds() {
        let payload = json!({
            "regular": [
                { "name": "ix_vendor_id", "key": { "vendor_id": 1 }, "unique": true }
            ],
            "search": [
                { "name": "vendor_search", "definition": { "mappings": { "dynamic": true } } }
            ]
        });
        let s: IndexSet = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(s.regular.len(), 1);
        assert_eq!(s.search.len(), 1);
        let round_trip = serde_json::to_value(&s).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_arrays_default_to_empty() {
        let s: IndexSet = serde_json::from_value(json!({})).unwrap();
        assert!(s.regular.is_empty());
        assert!(s.search.is_empty());
    }
}
