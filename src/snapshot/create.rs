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
        // a runtime aggregate. `users` and `workflows` are likewise reverse
        // membership lists (every entry references this queue from the *other*
        // side), and a cross-env PATCH can't rewrite their src URLs reliably
        // because users/workflows aren't deployable kinds in rdc.
        "queues" => &["hooks", "webhooks", "rules", "inbox", "counts", "users", "workflows"],
        // server fills `queues` from each queue's `schema` URL
        "schemas" => &["queues"],
        // server assigns the inbox's email address
        "inboxes" => &["email"],
        // server-managed sub-resource on hooks
        "hooks" => &["test"],
        // `triggers` references a sub-resource kind (`/api/v1/triggers/<id>`)
        // that rdc doesn't pull or deploy; sending src trigger URLs to tgt
        // 400s with "Invalid hyperlink", so strip them. The remote keeps its
        // own triggers, which is the conservative outcome.
        "email_templates" => &["triggers"],
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

/// Per-kind fields that get *redacted* on pull — the key stays visible
/// in the on-disk JSON but the value is replaced with [`REDACTED_VALUE_SENTINEL`],
/// so noisy server-computed runtime data doesn't pollute git diffs.
///
/// Different intent from `kind_specific_strip` above: that removes a
/// key entirely from outgoing POST/PATCH bodies; this rewrites an
/// *incoming* value into a constant so the on-disk bytes are stable
/// across syncs. The two lists may overlap (queue's `counts` appears
/// in both — stripped from outbound payloads because the server
/// rejects it on PATCH, and redacted in inbound payloads because it
/// churns every time a document changes status), but they're
/// independent and the duplication is intentional.
///
/// Add a new field here when a runtime aggregate (or other server-set
/// "live" data) shows up in `git diff` noise.
fn redact_on_pull(kind: &str) -> &'static [&'static str] {
    match kind {
        "queues" => &["counts"],
        _ => &[],
    }
}

/// The sentinel string that replaces redacted values on disk. Chosen
/// to be human-readable so anyone (or any agent) opening queue.json
/// sees both the field's existence and a one-line explanation, with
/// no need to consult external docs.
pub const REDACTED_VALUE_SENTINEL: &str = "<refreshed live in Rossum; not synced by rdc>";

/// Mutate `body` to redact noisy server-set fields per [`redact_on_pull`].
/// Each redacted key's value is replaced by [`REDACTED_VALUE_SENTINEL`];
/// keys that aren't present are left alone (no insertion). Idempotent.
pub fn redact_for_disk(body: &mut Value, kind: &str) {
    let Some(obj) = body.as_object_mut() else { return };
    for field in redact_on_pull(kind) {
        if obj.contains_key(*field) {
            obj.insert(
                (*field).to_string(),
                Value::String(REDACTED_VALUE_SENTINEL.to_string()),
            );
        }
    }
}

/// Like `strip_for_create`, but also strips `organization` — used for
/// **cross-env PATCH bodies and cross-env idempotency comparisons**, where
/// the src snapshot's `organization` URL belongs to the src org and would
/// either be 400'd by the API or distort byte-equality against the tgt
/// remote (whose `organization` belongs to the tgt org).
///
/// Same field set as `strip_for_create` (so creates inside an env still get
/// to specify the org), plus `organization`.
pub fn strip_for_cross_env_patch(body: &mut Value, kind: &str) {
    strip_for_create(body, kind);
    let Some(obj) = body.as_object_mut() else { return };
    obj.remove("organization");
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

    #[test]
    fn redact_for_disk_replaces_queue_counts_with_sentinel() {
        let mut v = json!({
            "id": 1,
            "name": "q",
            "counts": {"importing": 5, "to_review": 2, "exported": 100},
        });
        redact_for_disk(&mut v, "queues");
        assert_eq!(
            v["counts"],
            Value::String(REDACTED_VALUE_SENTINEL.to_string())
        );
        // Other fields untouched.
        assert_eq!(v["id"], json!(1));
        assert_eq!(v["name"], json!("q"));
    }

    #[test]
    fn redact_for_disk_noop_when_counts_absent() {
        let mut v = json!({"id": 1, "name": "q"});
        let before = v.clone();
        redact_for_disk(&mut v, "queues");
        assert_eq!(v, before, "should not introduce a counts key");
    }

    #[test]
    fn redact_for_disk_noop_for_other_kinds() {
        let mut v = json!({"counts": {"importing": 5}, "name": "x"});
        let before = v.clone();
        redact_for_disk(&mut v, "hooks");
        redact_for_disk(&mut v, "schemas");
        redact_for_disk(&mut v, "workspaces");
        assert_eq!(v, before);
    }

    #[test]
    fn redact_for_disk_is_idempotent() {
        let mut v = json!({"counts": {"importing": 5}, "name": "x"});
        redact_for_disk(&mut v, "queues");
        let after_first = v.clone();
        redact_for_disk(&mut v, "queues");
        assert_eq!(v, after_first);
    }
}
