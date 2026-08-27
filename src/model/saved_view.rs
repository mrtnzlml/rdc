use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rossum saved view — a stored annotation-dashboard filter.
///
/// rdc manages SHARED views only; `cli::pull::saved_views::list` drops the rest
/// and the design doc's section B explains why that filter is a safety boundary
/// rather than a convenience.
///
/// Field declaration order IS the on-disk key order: `serde_json` is built with
/// `preserve_order`, so `to_value` emits fields in this order and the codec
/// writes them out unchanged. Scalars first, the `query` blob last, so a diff of
/// the interesting fields stays readable.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct SavedView {
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    pub id: u64,
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub queues_filter: Vec<String>,
    #[serde(default)]
    pub query: Value,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl SavedView {
    pub fn modified_at(&self) -> Option<&str> {
        crate::model::modified_at(&self.extra)
    }

    pub fn modified_by(&self) -> Option<&str> {
        crate::model::modified_by(&self.extra)
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
            "id": 42,
            "url": "https://acme.rossum.app/api/v1/saved_views/42",
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["https://acme.rossum.app/api/v1/queues/100"],
            "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
            "organization": "https://acme.rossum.app/api/v1/organizations/1",
            "created_by": "https://acme.rossum.app/api/v1/users/7",
            "created_at": "2026-08-01T08:00:00Z",
            "modified_at": "2026-08-02T09:00:00Z",
            "modified_by": "https://acme.rossum.app/api/v1/users/8"
        });
        let v: SavedView = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(v.id, 42);
        assert_eq!(v.name, "Awaiting approval");
        assert!(v.shared);
        assert_eq!(v.queues_filter.len(), 1);
        assert_eq!(v.modified_at(), Some("2026-08-02T09:00:00Z"));
        // The forward-compat bucket must keep every key the struct does not name.
        let round_trip = serde_json::to_value(&v).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn null_id_and_url_deserialize_as_defaults() {
        // A hand-scaffolded new-object file carries nulls; the create path
        // strips both before POST anyway.
        let payload = json!({
            "id": null, "url": null, "name": "New view",
            "query": { "$and": [] }
        });
        let v: SavedView = serde_json::from_value(payload).unwrap();
        assert_eq!(v.id, 0);
        assert_eq!(v.url, "");
        assert!(!v.shared, "shared must default to false when absent");
        assert!(v.queues_filter.is_empty());
    }

    #[test]
    fn absent_query_defaults_to_null() {
        let payload = json!({ "id": 1, "url": "u", "name": "n" });
        let v: SavedView = serde_json::from_value(payload).unwrap();
        assert_eq!(v.query, serde_json::Value::Null);
    }
}
