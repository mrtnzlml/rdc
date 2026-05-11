use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum label. Categorizes annotations within an organization.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Label {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    pub organization: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Label {
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
            "id": 11,
            "url": "https://x.rossum.app/api/v1/labels/11",
            "name": "Priority High",
            "organization": "https://x.rossum.app/api/v1/organizations/285704",
            "modified_at": "2026-04-15T08:00:00Z",
            "color": "#ff0000"
        });
        let l: Label = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(l.id, 11);
        assert_eq!(l.name, "Priority High");
        let round_trip = serde_json::to_value(&l).unwrap();
        assert_eq!(round_trip, payload);
    }
}
