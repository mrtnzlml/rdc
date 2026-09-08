//! The per-kind table the fake routes and shapes responses from.
//!
//! One row per Rossum kind `rdc` touches. `defaults` fills in the fields the
//! model declares WITHOUT a serde default — omit one and the typed client
//! fails to deserialize the fake's own response, which is a confusing way to
//! learn you forgot a field.

use serde_json::{json, Map, Value};

/// What a `defaults` fn is allowed to know about the org it is filling in for.
pub struct OrgCtx {
    pub org_url: String,
}

#[allow(dead_code)]
pub struct KindSpec {
    /// The path segment, e.g. `"queues"` in `/api/v1/queues`.
    pub path: &'static str,
    /// Whether `POST /<path>` is accepted.
    pub creatable: bool,
    /// Whether `GET /<path>/{id}` is accepted. Labels have no detail endpoint
    /// (`tests/live/scenarios/round_trip.rs:120`).
    pub detail_get: bool,
    /// Keys stripped from LIST responses only.
    pub list_omits: &'static [&'static str],
    /// Fill in server-assigned and required-but-absent fields.
    pub defaults: fn(&mut Map<String, Value>, &OrgCtx),
}

fn ensure(o: &mut Map<String, Value>, key: &str, v: Value) {
    o.entry(key.to_string()).or_insert(v);
}

fn no_defaults(_o: &mut Map<String, Value>, _c: &OrgCtx) {}

fn org_owned(o: &mut Map<String, Value>, c: &OrgCtx) {
    ensure(o, "organization", json!(c.org_url));
}

fn workspace_defaults(o: &mut Map<String, Value>, c: &OrgCtx) {
    org_owned(o, c);
    ensure(o, "queues", json!([]));
}

fn schema_defaults(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "queues", json!([]));
    ensure(o, "content", json!([]));
}

/// A real pulled queue carries these server-owned arrays — see the captured
/// body in `testdata/live/snapshot/**/queue.json`. `pull::queues::refresh_backrefs`
/// exists precisely because `hooks`/`rules` change when a child is created, so
/// a fake that never grew them would leave that path unexercised.
fn queue_defaults(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "hooks", json!([]));
    ensure(o, "rules", json!([]));
    ensure(o, "webhooks", json!([]));
    ensure(o, "users", json!([]));
    ensure(o, "locale", json!("en_GB"));
}

/// `email` is server-assigned. The real address is globally unique; the fake
/// derives it from `email_prefix` so it is unique per run for free (the seeder
/// prefixes `email_prefix` with the run id).
fn inbox_defaults(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "queues", json!([]));
    if o.get("email").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        let prefix = o
            .get("email_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("inbox")
            .to_string();
        o.insert("email".into(), json!(format!("{prefix}@fake.rossum.invalid")));
    }
}

fn hook_defaults(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "type", json!("function"));
    ensure(o, "queues", json!([]));
    ensure(o, "events", json!([]));
    ensure(o, "config", json!({}));
}

fn queues_owned(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "queues", json!([]));
}

/// Every kind the fake answers for. Kinds with `creatable: false` exist so a
/// sync's list of them returns an empty envelope instead of a 404.
pub const MODELLED: &[KindSpec] = &[
    KindSpec { path: "workspaces", creatable: true, detail_get: true, list_omits: &[], defaults: workspace_defaults },
    KindSpec { path: "queues", creatable: true, detail_get: true, list_omits: &[], defaults: queue_defaults },
    KindSpec { path: "schemas", creatable: true, detail_get: true, list_omits: &["content"], defaults: schema_defaults },
    KindSpec { path: "inboxes", creatable: true, detail_get: true, list_omits: &[], defaults: inbox_defaults },
    KindSpec { path: "hooks", creatable: true, detail_get: true, list_omits: &[], defaults: hook_defaults },
    KindSpec { path: "rules", creatable: true, detail_get: true, list_omits: &[], defaults: queues_owned },
    KindSpec { path: "labels", creatable: true, detail_get: false, list_omits: &[], defaults: org_owned },
    KindSpec { path: "email_templates", creatable: true, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "engines", creatable: true, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "engine_fields", creatable: true, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "saved_views", creatable: true, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "workflows", creatable: false, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "workflow_steps", creatable: false, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "users", creatable: false, detail_get: true, list_omits: &[], defaults: no_defaults },
    KindSpec { path: "hook_templates", creatable: false, detail_get: true, list_omits: &[], defaults: no_defaults },
];

pub fn spec(path: &str) -> Option<&'static KindSpec> {
    MODELLED.iter().find(|k| k.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake must answer every list endpoint a sync visits, or the pull
    /// aborts on a 404 that means nothing to the reader.
    #[test]
    fn every_list_endpoint_a_sync_visits_is_modelled() {
        for path in [
            "hooks", "workspaces", "queues", "inboxes", "rules", "labels",
            "engines", "engine_fields", "workflows", "workflow_steps",
            "email_templates", "saved_views",
        ] {
            assert!(spec(path).is_some(), "unmodelled list endpoint: /{path}");
        }
    }

    #[test]
    fn kind_paths_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for k in MODELLED {
            assert!(seen.insert(k.path), "duplicate kind row: {}", k.path);
        }
    }
}
