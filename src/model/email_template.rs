use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum email template. Used to customize notification emails.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EmailTemplate {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub subject: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl EmailTemplate {
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
            "id": 9001,
            "url": "https://x.rossum.app/api/v1/email_templates/9001",
            "name": "Rejection Notice",
            "subject": "Your invoice was rejected",
            "queues": ["https://x.rossum.app/api/v1/queues/100"],
            "modified_at": "2026-04-20T08:00:00Z",
            "body_template": "Hello,\n..."
        });
        let t: EmailTemplate = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(t.id, 9001);
        assert_eq!(t.subject, "Your invoice was rejected");
        let round_trip = serde_json::to_value(&t).unwrap();
        assert_eq!(round_trip, payload);
    }
}
