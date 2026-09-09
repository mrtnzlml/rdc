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

/// Whether an edge's field holds a single url or an array of them —
/// determines how `validate` and `graph` pull urls out of the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefShape {
    Single,
    Array,
}

/// The back-reference an edge's owner maintains on its target when the edge
/// is created or dropped. `Push` grows/shrinks an array field on the target
/// (`workspace.queues`, `queue.hooks`, `queue.rules`); `Set` is a scalar
/// field the real API also vacates entirely on removal rather than nulling
/// (`queue.inbox` — see `graph::remove_field`). Both carry the TARGET's field
/// name, which is not always the same string as the edge's own `field`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackRef {
    Push(&'static str),
    Set(&'static str),
}

/// One edge of the fake's object graph: a field that must resolve to a
/// specific target KIND, and the back-reference (if any) its owner maintains
/// on that target.
///
/// `owner: None` marks a UNIVERSAL edge — `queues` and `run_after` mean the
/// same thing (a list of queue urls, a list of hook urls) on whichever kind
/// carries them, so the ref-type check applies to every kind, not just ones
/// with a row of their own. A universal edge never carries a `back_ref`:
/// back-reference maintenance IS owner-specific (the whole reason
/// `inboxes`/`hooks`/`rules` each get their own `queues` row below, alongside
/// the universal one), so folding it into the universal row would either
/// apply a back-ref to owners that never had one, or require re-narrowing the
/// universal row per owner — the exact mistake this table exists to prevent.
pub struct Edge {
    pub owner: Option<&'static str>,
    pub field: &'static str,
    pub shape: RefShape,
    pub target: &'static str,
    /// Only meaningful when `owner` is `Some` — back-references are
    /// inherently owner-specific (the same `queues` field means three
    /// different things on `inboxes`, `hooks` and `rules`), which is exactly
    /// why the universal rows exist for the ref-TYPE check alone and never
    /// for back-reference maintenance. `edges_for` — the only thing
    /// `graph::relink`/`unlink` consult — filters on `owner == Some(...)`,
    /// so a universal row's `back_ref` is never read by anything: setting
    /// one produces no compiler error and no behavior change, just a value
    /// nobody looks at. `no_universal_row_carries_a_back_ref` below turns
    /// that dead-state possibility into an enforced invariant.
    pub back_ref: Option<BackRef>,
}

/// The complete edge table: ten owner-scoped edges plus the two universal
/// ones. Adding an edge — including the `saved_views.queues_filter` one
/// stage 2 needs next — is a one-line addition here; nothing else should ever
/// need touching.
pub const EDGES: &[Edge] = &[
    Edge { owner: Some("queues"), field: "workspace", shape: RefShape::Single, target: "workspaces", back_ref: Some(BackRef::Push("queues")) },
    Edge { owner: Some("queues"), field: "schema", shape: RefShape::Single, target: "schemas", back_ref: Some(BackRef::Push("queues")) },
    Edge { owner: Some("queues"), field: "engine", shape: RefShape::Single, target: "engines", back_ref: None },
    Edge { owner: Some("queues"), field: "generic_engine", shape: RefShape::Single, target: "engines", back_ref: None },
    Edge { owner: Some("email_templates"), field: "queue", shape: RefShape::Single, target: "queues", back_ref: None },
    Edge { owner: Some("labels"), field: "organization", shape: RefShape::Single, target: "organizations", back_ref: None },
    Edge { owner: Some("workspaces"), field: "organization", shape: RefShape::Single, target: "organizations", back_ref: None },
    Edge { owner: Some("inboxes"), field: "queues", shape: RefShape::Array, target: "queues", back_ref: Some(BackRef::Set("inbox")) },
    Edge { owner: Some("hooks"), field: "queues", shape: RefShape::Array, target: "queues", back_ref: Some(BackRef::Push("hooks")) },
    Edge { owner: Some("rules"), field: "queues", shape: RefShape::Array, target: "queues", back_ref: Some(BackRef::Push("rules")) },
    // Universal: checked for every kind, never back-ref'd — see the doc
    // comment on `Edge::owner`.
    Edge { owner: None, field: "queues", shape: RefShape::Array, target: "queues", back_ref: None },
    Edge { owner: None, field: "run_after", shape: RefShape::Array, target: "hooks", back_ref: None },
];

/// Owner-scoped edges declared on `owner` (single-url or array fields
/// alike) — used to check its refs and to drive `graph::relink`/`unlink`'s
/// back-reference maintenance. Excludes the universal rows; see
/// [`universal_edges`].
pub fn edges_for(owner: &str) -> impl Iterator<Item = &'static Edge> {
    EDGES.iter().filter(move |e| e.owner == Some(owner))
}

/// The edges that apply regardless of owner kind.
pub fn universal_edges() -> impl Iterator<Item = &'static Edge> {
    EDGES.iter().filter(|e| e.owner.is_none())
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

    /// The edge table must cover every edge the two derived behaviors used to
    /// hand-write, or one of them silently stops working. These are the ten
    /// owner-scoped edges as they stood at c47b3fe.
    #[test]
    fn the_edge_table_covers_every_edge_the_fake_used_to_hand_write() {
        for (owner, field, target) in [
            ("queues", "workspace", "workspaces"),
            ("queues", "schema", "schemas"),
            ("queues", "engine", "engines"),
            ("queues", "generic_engine", "engines"),
            ("email_templates", "queue", "queues"),
            ("labels", "organization", "organizations"),
            ("workspaces", "organization", "organizations"),
            ("inboxes", "queues", "queues"),
            ("hooks", "queues", "queues"),
            ("rules", "queues", "queues"),
        ] {
            assert!(
                edges_for(owner).any(|e| e.field == field && e.target == target),
                "edge table is missing {owner}.{field} -> {target}"
            );
        }
    }

    /// A universal row's `back_ref` is dead: `edges_for` — the only thing
    /// `graph::relink`/`unlink` consult — filters on `owner == Some(...)`,
    /// so nothing ever reads a `back_ref` set on an `owner: None` row.
    /// Setting one anyway would compile clean and change nothing, which is
    /// precisely the kind of silent trap a future edge-adder would not
    /// notice — so this enforces the invariant the doc comment on
    /// `Edge::back_ref` only describes.
    #[test]
    fn no_universal_row_carries_a_back_ref() {
        assert!(
            universal_edges().all(|e| e.back_ref.is_none()),
            "a universal row's back_ref is never read by any consumer \
             (see the doc comment on Edge::back_ref) — drop it"
        );
    }

    /// The two universal rows are deliberate, not an oversight: `queues` and
    /// `run_after` mean the same thing on whatever kind carries them, so the
    /// TYPE check must still apply to a kind with no row of its own.
    #[test]
    fn the_universal_ref_fields_are_still_universal() {
        for (field, target) in [("queues", "queues"), ("run_after", "hooks")] {
            assert!(
                universal_edges().any(|e| e.field == field && e.target == target),
                "the universal type check lost {field} -> {target}"
            );
        }
    }

    /// Only the four owners that maintained a back-reference before may carry
    /// one now — an accidental extra back-ref would mutate objects the real
    /// server does not touch.
    #[test]
    fn only_the_documented_owners_maintain_a_back_reference() {
        let with_back_ref: std::collections::BTreeSet<&str> = EDGES
            .iter()
            .filter(|e| e.back_ref.is_some() && e.owner.is_some())
            .map(|e| e.owner.unwrap())
            .collect();
        assert_eq!(
            with_back_ref,
            ["hooks", "inboxes", "queues", "rules"].into_iter().collect(),
        );
    }
}
