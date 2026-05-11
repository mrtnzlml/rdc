//! Helpers for the resource-creation push path.
//!
//! When `rdc push` sees a local file with no lockfile entry, it treats it as
//! a new object and POSTs it. The POST body is the user-authored JSON minus
//! the server-managed fields. Stripping them client-side keeps the request
//! clean — the user's placeholder `id: 0` / `url: ""` (or missing fields)
//! never reach the server.

use serde_json::Value;

/// Field names that the server assigns / computes on every kind. Always
/// stripped before POST regardless of kind.
const UNIVERSAL_SERVER_FIELDS: &[&str] = &[
    "id",
    "url",
    "created_at",
    "created_by",
    "modified_at",
    "modified_by",
    "status",
];

/// Field names the server computes per-kind from child relationships.
/// Stripped before POST so the request doesn't ship stale or empty
/// computed values.
fn kind_specific_strip(kind: &str) -> &'static [&'static str] {
    match kind {
        // server fills `queues` from each queue's `workspace` URL
        "workspaces" => &["queues"],
        // server fills `hooks`, `webhooks`, `rules` from each child's `queues` URL,
        // `inbox` is the back-ref from the inbox's `queues` URL, `counts` is
        // a runtime aggregate.
        "queues" => &["hooks", "webhooks", "rules", "inbox", "counts"],
        // server fills `queues` from each queue's `schema` URL
        "schemas" => &["queues"],
        // server assigns the inbox's email address
        "inboxes" => &["email"],
        // server-managed sub-resource on hooks
        "hooks" => &["test"],
        _ => &[],
    }
}

/// Mutate `body` to remove server-managed fields for the given kind.
/// Idempotent: calling twice is the same as once.
pub fn strip_for_create(body: &mut Value, kind: &str) {
    let Some(obj) = body.as_object_mut() else { return };
    for f in UNIVERSAL_SERVER_FIELDS {
        obj.remove(*f);
    }
    for f in kind_specific_strip(kind) {
        obj.remove(*f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_universal_fields() {
        let mut v = json!({
            "id": 42,
            "url": "https://x/api/v1/hooks/42",
            "name": "h",
            "type": "function",
            "events": [],
            "config": {},
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "u",
            "modified_at": "2026-01-02T00:00:00Z",
            "modified_by": "u",
            "status": "ready",
        });
        strip_for_create(&mut v, "hooks");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("url"));
        assert!(!obj.contains_key("created_at"));
        assert!(!obj.contains_key("created_by"));
        assert!(!obj.contains_key("modified_at"));
        assert!(!obj.contains_key("modified_by"));
        assert!(!obj.contains_key("status"));
        // User-meaningful fields preserved.
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("events"));
        assert!(obj.contains_key("config"));
    }

    #[test]
    fn strips_kind_specific_hooks_test_field() {
        let mut v = json!({
            "id": 0,
            "url": "",
            "name": "h",
            "test": {"some": "data"},
        });
        strip_for_create(&mut v, "hooks");
        assert!(!v.as_object().unwrap().contains_key("test"));
    }

    #[test]
    fn strips_workspace_server_fields() {
        let mut v = json!({
            "id": 1,
            "url": "u",
            "name": "ws",
            "organization": "https://x/api/v1/organizations/1",
            "queues": ["https://x/api/v1/queues/9"],
            "autopilot": true,
        });
        strip_for_create(&mut v, "workspaces");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("queues"));
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("url"));
        // Required user fields kept.
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("organization"));
        assert!(obj.contains_key("autopilot"));
    }

    #[test]
    fn strips_queue_computed_relationships() {
        let mut v = json!({
            "id": 0,
            "name": "q",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/9",
            "hooks": ["https://x/api/v1/hooks/1"],
            "webhooks": ["https://x/api/v1/webhooks/1"],
            "rules": [],
            "inbox": "https://x/api/v1/inboxes/1",
            "counts": {"to_review": 4},
            "automation_level": "never",
        });
        strip_for_create(&mut v, "queues");
        let obj = v.as_object().unwrap();
        for k in &["hooks", "webhooks", "rules", "inbox", "counts", "id"] {
            assert!(!obj.contains_key(*k), "should strip {k}");
        }
        for k in &["name", "workspace", "schema", "automation_level"] {
            assert!(obj.contains_key(*k), "should keep {k}");
        }
    }

    #[test]
    fn strips_inbox_email() {
        let mut v = json!({
            "id": 0,
            "name": "i",
            "email": "should-be-stripped@rossum.app",
            "queues": ["https://x/api/v1/queues/1"],
        });
        strip_for_create(&mut v, "inboxes");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("email"));
        assert!(obj.contains_key("queues"));
    }

    #[test]
    fn idempotent() {
        let mut v = json!({"id": 1, "url": "u", "name": "x"});
        strip_for_create(&mut v, "hooks");
        let after1 = v.clone();
        strip_for_create(&mut v, "hooks");
        assert_eq!(v, after1);
    }

    #[test]
    fn unknown_kind_only_strips_universal() {
        let mut v = json!({"id": 1, "url": "u", "name": "x", "queues": ["q"]});
        strip_for_create(&mut v, "unknown_kind");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("url"));
        // queues kept because no kind-specific rule matched
        assert!(obj.contains_key("queues"));
    }
}
