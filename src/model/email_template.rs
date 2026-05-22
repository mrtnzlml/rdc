use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

/// Rossum email template. Each template belongs to a single queue (the live
/// API field is singular `queue`, not `queues`). Templates are not org-wide;
/// every queue carries its own set (e.g. annotation-status-change-confirmed,
/// default-rejection-template).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EmailTemplate {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    pub name: String,
    pub subject: String,
    #[serde(default)]
    pub queue: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl EmailTemplate {
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
            "id": 9001,
            "url": "https://x.rossum.app/api/v1/email_templates/9001",
            "name": "Rejection Notice",
            "subject": "Your invoice was rejected",
            "queue": "https://x.rossum.app/api/v1/queues/100",
            "modified_at": "2026-04-20T08:00:00Z",
            "body_template": "Hello,\n..."
        });
        let t: EmailTemplate = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(t.id, 9001);
        assert_eq!(t.subject, "Your invoice was rejected");
        assert_eq!(t.queue.as_deref(), Some("https://x.rossum.app/api/v1/queues/100"));
        let round_trip = serde_json::to_value(&t).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_queue_defaults_to_none() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/email_templates/1",
            "name": "Org-wide?",
            "subject": "Hi"
        });
        let t: EmailTemplate = serde_json::from_value(payload).unwrap();
        assert!(t.queue.is_none());
    }
}
