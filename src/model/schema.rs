use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum schema. Each queue has exactly one schema. The `content` array
/// holds the field definitions; we keep them as opaque `Value`s so unknown
/// field types and nested structures round-trip cleanly. The codec walks
/// `content` to extract formula fields' `formula` strings into sibling .py
/// files (mirroring the hook code-extraction pattern).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Schema {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub queues: Vec<String>,
    /// The schema content tree (sections, datapoints, formulas, etc.). Opaque
    /// in the model; the codec walks it.
    pub content: Vec<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Schema {
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
    fn round_trip_preserves_content() {
        let payload = json!({
            "id": 1824379,
            "url": "https://x.rossum.app/api/v1/schemas/1824379",
            "name": "Cost Invoices Schema",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "content": [
                {
                    "category": "section",
                    "id": "header",
                    "label": "Header",
                    "children": [
                        {
                            "category": "datapoint",
                            "id": "invoice_id",
                            "type": "string"
                        },
                        {
                            "category": "datapoint",
                            "id": "amount_total",
                            "type": "number",
                            "formula": "amount_due + amount_tax"
                        }
                    ]
                }
            ],
            "modified_at": "2026-04-10T09:00:00Z"
        });

        let s: Schema = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(s.id, 1824379);
        assert_eq!(s.content.len(), 1);
        let round_trip = serde_json::to_value(&s).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn empty_content_allowed() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/schemas/1",
            "name": "Empty",
            "queues": [],
            "content": []
        });
        let s: Schema = serde_json::from_value(payload).unwrap();
        assert!(s.content.is_empty());
    }
}
