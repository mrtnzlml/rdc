use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum inbox. Each inbox is attached to one queue (1:1) and provides an
/// email-ingestion endpoint.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Inbox {
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    pub id: u64,
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    pub url: String,
    pub name: String,
    // `email` is server-assigned. `migrate` strips it (env-specific), leaving
    // it empty on a migrated inbox; skip serializing an empty value so a PATCH
    // OMITS it (the Rossum API rejects `email: ""` with "may not be blank" and
    // preserves the remote's own address when the field is absent).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    /// URL of the queue this inbox is attached to.
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl Inbox {
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
            "id": 813566,
            "url": "https://x.rossum.app/api/v1/inboxes/813566",
            "name": "Cost Invoices Inbox",
            "email": "cost-invoices@org.rossum.app",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-04-10T09:00:00Z",
            "filters": []
        });
        let inbox: Inbox = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(inbox.id, 813566);
        assert_eq!(inbox.email, "cost-invoices@org.rossum.app");
        assert_eq!(inbox.queues.len(), 1);
        let round_trip = serde_json::to_value(&inbox).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn empty_email_is_omitted_on_serialize() {
        // `migrate` strips an inbox's env-specific `email`, so a migrated
        // inbox deserializes with `email == ""`. Serializing it for a PATCH
        // must OMIT `email` entirely: sending `email: ""` makes the Rossum
        // API reject the whole PATCH ("email: This field may not be blank"),
        // whereas omitting it leaves the remote's server-assigned address
        // intact.
        let inbox = Inbox {
            id: 0,
            url: String::new(),
            name: "Cost Invoices Inbox".into(),
            email: String::new(),
            queues: vec!["rdc://queues/cost-invoices".into()],
            extra: IndexMap::new(),
        };
        let v = serde_json::to_value(&inbox).unwrap();
        assert!(
            v.get("email").is_none(),
            "empty email must be omitted from the serialized payload, got: {v}"
        );
        // A non-empty email still serializes normally.
        let with_email = Inbox {
            email: "cost-invoices@org.rossum.app".into(),
            ..inbox
        };
        let v2 = serde_json::to_value(&with_email).unwrap();
        assert_eq!(
            v2.get("email").and_then(|e| e.as_str()),
            Some("cost-invoices@org.rossum.app")
        );
    }
}
