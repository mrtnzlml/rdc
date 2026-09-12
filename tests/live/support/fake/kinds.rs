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
    /// Whether the fake should stamp `modified_at` when it creates or
    /// patches an object of this kind. `false` only for `engines` and
    /// `engine_fields` — ORIGINALLY chosen not because this fake had
    /// observed that the real API omits the field on those two, but because
    /// `push::deletes::fetch_remote_modified_at` (`src/cli/push/deletes.rs`)
    /// deliberately DISCARDS whatever these two kinds' bodies carry: its
    /// `"engines"`/`"engine_fields"` arms end `.map(|_| None)` (lines
    /// 486/492), throwing the value away, where every other arm — e.g.
    /// `"labels"`/`"saved_views"` at lines 458/464 — keeps it via
    /// `.map(|x| x.modified_at()...)`. So rdc's own drift signal for these
    /// two kinds is existence-only BY CONSTRUCTION, regardless of what the
    /// wire actually sends. A fake that stamped a timestamp anyway would
    /// make `push::deletes::delete_one`'s comparison see `(None, Some(_))`
    /// — "one side has a timestamp the other doesn't" — and skip a delete
    /// this scenario expects to reach the server and be refused there
    /// instead. `has_modified_at: false` matches rdc's comparison contract,
    /// not, on its own, a claim about the real response body.
    ///
    /// It IS also a `quirks::QUIRKS` entry now — name
    /// `"an_engine_or_engine_field_carries_no_modified_at"`, a
    /// `Provenance::ChosenUnverified` row (`quirks.rs`). An earlier version
    /// of this comment argued the opposite — "deliberately NOT a
    /// `quirks::QUIRKS` entry: that registry is for behaviors of the real
    /// API, each backed by real, checkable evidence; this flag encodes an
    /// internal-consistency requirement... not an observed server fact" —
    /// but that rationale did not survive its own registry: the sibling row
    /// `modified_at_does_not_move_when_a_back_reference_grows` is EQUALLY an
    /// unobserved, chosen-not-proven fact (its own comment says the real
    /// behavior is unknown), and it was in `QUIRKS` from the start. Same
    /// category — "the fake had to pick an answer and the real API's
    /// behaviour is unknown" — deserves the same treatment, which is exactly
    /// what `Provenance::ChosenUnverified` is for. See that row's own doc
    /// comment for the evidence — not restated here, so this comment can't
    /// drift out of sync with it again.
    pub has_modified_at: bool,
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

/// A real pulled queue carries these server-owned arrays. The evidence is
/// `snapshot::noise::sort_url_arrays`, whose doc names them as exactly that —
/// "a queue's server-computed back-reference arrays (`hooks`, `webhooks`,
/// `rules`, `users`, `workflows`, `queues`, `run_after`, `triggers`, …)",
/// returned in an order the real API varies per env and endpoint. The
/// trailing `…` is that doc's own, not a truncation of it. NOT `testdata/live/snapshot/**/queue.json`, which this comment
/// used to call a captured body: `support::snapshot`'s module doc says that
/// tree is hand-authored and written straight to disk, so it shows what rdc
/// sends, not what the server answered. `pull::queues::refresh_backrefs`
/// exists precisely because `hooks`/`rules` change when a child is created, so
/// a fake that never grew them would leave that path unexercised.
fn queue_defaults(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "hooks", json!([]));
    ensure(o, "rules", json!([]));
    ensure(o, "webhooks", json!([]));
    ensure(o, "users", json!([]));
    ensure(o, "locale", json!("en_GB"));
    // Quirk `queue_create_materializes_automation_config_switched_off`
    // (`quirks::QUIRKS`) — see that row for the evidence and for why
    // `quality_spot_check_percentage`, the third member of
    // `cli::migrate::AUTOMATION_KEYS`, is deliberately NOT here.
    ensure(o, "automation_enabled", json!(false));
    ensure(o, "automation_level", json!("never"));
}

/// `email` is server-assigned: the real address is globally unique, and the
/// fake derives it from `email_prefix` so it is unique per run for free (the
/// seeder prefixes `email_prefix` with the run id). The DERIVATION — turning
/// an `email_prefix` that's actually present into an `email` — now lives
/// only in `quirks::normalize_write` (`derive_inbox_email`), called right
/// after this function by `state.rs::create_unchecked`, and again on every
/// `patch`. Keeping that derivation here too, the shape this function used
/// to have, is exactly the duplication the seam exists to close: a PATCH
/// that changed `email_prefix` would leave the create-time value stale
/// forever, which is what
/// `an_inbox_email_is_re_derived_when_its_prefix_changes` (`tests.rs`) pins
/// against.
///
/// What's left here is narrower: a body sent with NEITHER `email` nor
/// `email_prefix` has nothing for `derive_inbox_email` to derive FROM (it
/// deliberately never invents a prefix), so without this fallback such a
/// create would silently end up with no `email` at all — a real change in
/// behavior from before this seam existed, for a case nothing in this repo
/// currently tests but that the create-path-must-not-change requirement
/// still covers. This calls the same `quirks::inbox_email_for` formula
/// `derive_inbox_email` uses, so the two triggers (re-derive when a prefix
/// IS present; invent a fixed one when neither field is) can never disagree
/// on what an address for a given prefix looks like.
fn inbox_defaults(o: &mut Map<String, Value>, _c: &OrgCtx) {
    ensure(o, "queues", json!([]));
    let has_email = o.get("email").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty());
    let has_prefix = o.get("email_prefix").and_then(|v| v.as_str()).is_some();
    if !has_email && !has_prefix {
        o.insert("email".into(), json!(super::quirks::inbox_email_for("inbox")));
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
    KindSpec { path: "workspaces", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: workspace_defaults },
    KindSpec { path: "queues", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: queue_defaults },
    KindSpec { path: "schemas", creatable: true, detail_get: true, list_omits: &["content"], has_modified_at: true, defaults: schema_defaults },
    KindSpec { path: "inboxes", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: inbox_defaults },
    KindSpec { path: "hooks", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: hook_defaults },
    KindSpec { path: "rules", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: queues_owned },
    KindSpec { path: "labels", creatable: true, detail_get: false, list_omits: &[], has_modified_at: true, defaults: org_owned },
    KindSpec { path: "email_templates", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: no_defaults },
    KindSpec { path: "engines", creatable: true, detail_get: true, list_omits: &[], has_modified_at: false, defaults: no_defaults },
    KindSpec { path: "engine_fields", creatable: true, detail_get: true, list_omits: &[], has_modified_at: false, defaults: no_defaults },
    KindSpec { path: "saved_views", creatable: true, detail_get: true, list_omits: &[], has_modified_at: true, defaults: no_defaults },
    KindSpec { path: "workflows", creatable: false, detail_get: true, list_omits: &[], has_modified_at: true, defaults: no_defaults },
    KindSpec { path: "workflow_steps", creatable: false, detail_get: true, list_omits: &[], has_modified_at: true, defaults: no_defaults },
    KindSpec { path: "users", creatable: false, detail_get: true, list_omits: &[], has_modified_at: true, defaults: no_defaults },
    KindSpec { path: "hook_templates", creatable: false, detail_get: true, list_omits: &[], has_modified_at: true, defaults: no_defaults },
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

/// The complete edge table: eleven owner-scoped edges plus the two universal
/// ones. Adding an edge is a one-line addition here; nothing else should ever
/// need touching — `every_edge_in_the_table_is_load_bearing` below drives it
/// for free.
///
/// `saved_views.queues_filter`'s two halves are evidenced very differently,
/// so they are stated separately.
///
/// ARRAY-shaped: `model::SavedView::queues_filter` is a `Vec<String>`, and
/// that model is what every real `GET /saved_views` response is deserialized
/// into, so the wire shape is settled. The fixture
/// `testdata/live/snapshot/saved-views/rdc-it-{{RUN}}-view.json` agrees
/// (`"queues_filter": ["rdc://queues/..."]`) but is NOT independent
/// confirmation of it: `support::snapshot`'s module doc says that whole tree
/// is hand-authored and written straight to disk, never seeded and pulled, so
/// it evidences what rdc SENDS, never what a server returned.
///
/// `back_ref: None` is a CHOICE, and weaker. An earlier version of this
/// comment justified it as "a saved view doesn't own the queues it filters
/// on, the way a hook or rule owns the queues it runs against" — a principle
/// that does not discriminate: `email_templates.queue` also carries
/// `back_ref: None`, and a queue owns its email templates if it owns anything
/// (they are literally stored under it on disk). The honest position is
/// narrower: nothing in this repo shows a queue growing a `saved_views`
/// array, so the fake does not grow one. The nearest corroboration is
/// `snapshot::noise::sort_url_arrays`, whose doc enumerates the queue
/// back-reference arrays the real API returns in non-deterministic order —
/// `hooks`, `webhooks`, `rules`, `users`, `workflows`, `queues`, `run_after`,
/// `triggers` — with no `saved_views` among them. That list ends in an
/// ellipsis, so its silence is weak evidence, not proof.
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
    Edge { owner: Some("saved_views"), field: "queues_filter", shape: RefShape::Array, target: "queues", back_ref: None },
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

    /// `every_edge_in_the_table_is_load_bearing` below drives every row still
    /// IN `EDGES` and proves each one is actually consulted — but a row that
    /// has been deleted outright just shrinks that loop rather than failing
    /// it. For a `back_ref`-carrying row that deletion still fails loudly
    /// one module over (`only_the_documented_owners_maintain_a_back_reference`,
    /// plus graph.rs's hand-written creation tests); a non-`back_ref` row —
    /// e.g. `email_templates.queue` — had no such backstop, which is exactly
    /// what an empirical sabotage of this row confirmed while restoring this
    /// test. This pins the full row set — both owner-scoped edges and the
    /// two universal ones — against a hand-written list independent of the
    /// table, so a silent deletion of ANY row is caught here regardless of
    /// whether it carries a back-reference.
    ///
    /// Owner-scoped and universal rows are pinned in the same set rather
    /// than two: `edges_for` vs `universal_edges` split on `owner` because
    /// back-reference *maintenance* is owner-specific (that split is what
    /// `graph.rs` and the loop below rely on), but the property this test
    /// checks — "this exact row is still in `EDGES`" — doesn't care which
    /// bucket a row falls into, so one set covers both without duplicating
    /// the loop.
    ///
    /// A failure here means one of two things, and the message says which:
    /// either a row that should still be here is gone (restore it — this is
    /// the accidental-loss case this test exists for), or the model was
    /// changed on purpose (added or removed an edge deliberately) and this
    /// hand-written list simply needs updating to match — that is the
    /// correct fix in that case, not a bug.
    #[test]
    fn the_edge_table_still_models_every_edge_it_is_supposed_to() {
        let actual: std::collections::BTreeSet<(Option<&str>, &str, &str)> =
            EDGES.iter().map(|e| (e.owner, e.field, e.target)).collect();
        let expected: std::collections::BTreeSet<(Option<&str>, &str, &str)> = [
            (Some("queues"), "workspace", "workspaces"),
            (Some("queues"), "schema", "schemas"),
            (Some("queues"), "engine", "engines"),
            (Some("queues"), "generic_engine", "engines"),
            (Some("email_templates"), "queue", "queues"),
            (Some("labels"), "organization", "organizations"),
            (Some("workspaces"), "organization", "organizations"),
            (Some("inboxes"), "queues", "queues"),
            (Some("hooks"), "queues", "queues"),
            (Some("rules"), "queues", "queues"),
            (Some("saved_views"), "queues_filter", "queues"),
            (None, "queues", "queues"),
            (None, "run_after", "hooks"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            actual, expected,
            "EDGES no longer matches the pinned row set (shown above as a left/right \
             set diff). If a row you expect is missing from `actual`, something \
             deleted it by accident — restore it in EDGES; that's the bug this test \
             exists to catch (the load-bearing loop above cannot, since it only \
             drives rows still present). If you changed the model on purpose — you \
             meant to add or remove an edge — update the `expected` list here to \
             match; that is the correct fix, not a bug."
        );
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

    // ---- The load-bearing loop -------------------------------------------
    //
    // `no_universal_row_carries_a_back_ref` and
    // `only_the_documented_owners_maintain_a_back_reference` above pin real
    // invariants about the SHAPE of the table. What used to sit alongside
    // them — a pair of tests hand-listing every row and asserting it was
    // present — restated the table rather than proving anything reads it:
    // they would stay green even if `validate::on_write` or `graph::relink`
    // stopped consulting `EDGES` entirely. This loop replaces that pair. It
    // drives the FAKE itself, through the same `OrgState::create` every
    // scenario goes through, and asserts what each row actually claims.

    use super::super::state::OrgState;

    /// Which kind to `create` in order to exercise `edge`. An owner-scoped
    /// row creates an instance of its own owner. A universal row (`owner:
    /// None`) has no owner of its own to instantiate — see the doc comment
    /// on `Edge::owner` — so a driving kind is chosen by hand for each of
    /// the two that exist today: `queues` is deliberately driven against
    /// `workspaces`, which is NOT one of the three owner rows for that field
    /// (`inboxes`/`hooks`/`rules`), specifically to prove the universal
    /// check reaches a kind with no row of its own; `run_after` is, in
    /// practice, only ever carried by `hooks` (`src/snapshot/hook.rs`).
    /// A third universal row would need its own arm here — the panic says
    /// so rather than silently skipping it.
    fn driving_kind(edge: &Edge) -> &'static str {
        match edge.owner {
            Some(k) => k,
            None if edge.field == "queues" => "workspaces",
            None if edge.field == "run_after" => "hooks",
            None => panic!(
                "no driving kind chosen for the new universal edge on {:?} — \
                 add one alongside `driving_kind`'s other two arms",
                edge.field
            ),
        }
    }

    /// A minimal, otherwise-valid create body for `kind`, before the edge
    /// under test overwrites its own one field. Only `queues` has a
    /// create-time requirement `validate::on_write` enforces regardless of
    /// which edge is under test (rule 2: a schema); every other kind here
    /// creates cleanly off just a name, so a wrong-kind url is the ONLY
    /// reason any of these bodies gets refused.
    fn minimal_body(kind: &str, ws_url: &str, schema_url: &str) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert("name".to_string(), json!(format!("z-{kind}")));
        match kind {
            "queues" => {
                body.insert("workspace".to_string(), json!(ws_url));
                body.insert("schema".to_string(), json!(schema_url));
            }
            // Not required by the fake, but a real inbox never lacks one;
            // `inbox_defaults` only invents an address when BOTH `email` and
            // `email_prefix` are absent, and this keeps the body realistic.
            "inboxes" => {
                body.insert("email_prefix".to_string(), json!("z"));
            }
            _ => {}
        }
        body
    }

    /// A real, EXISTING url of some kind other than `target` — proving the
    /// check is a TYPE check, not mere presence
    /// (`OrgState::resolves_kind`'s doc comment, and
    /// `a_ref_of_the_wrong_kind_is_refused_even_though_it_resolves` in
    /// `state.rs`). `ws_url` is wrong for every target except `workspaces`
    /// itself, where `schema_url` steps in.
    fn wrong_kind_url(ws_url: &str, schema_url: &str, target: &str) -> String {
        if target == "workspaces" { schema_url.to_string() } else { ws_url.to_string() }
    }

    /// For `edge`, drive a create that puts a wrong-kind url in its field and
    /// assert the fake refuses it — the behavior `validate::on_write`'s rule
    /// 1 derives from every row in `EDGES`.
    fn assert_ref_type_is_enforced(edge: &Edge) {
        let mut st = OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1);
        let ws = st.create("workspaces", json!({ "name": "ws" })).unwrap();
        let sc = st.create("schemas", json!({ "name": "sc" })).unwrap();
        let ws_url = ws["url"].as_str().unwrap().to_string();
        let sc_url = sc["url"].as_str().unwrap().to_string();

        let kind = driving_kind(edge);
        let mut body = minimal_body(kind, &ws_url, &sc_url);
        let bad_url = wrong_kind_url(&ws_url, &sc_url, edge.target);
        let value = match edge.shape {
            RefShape::Single => json!(bad_url),
            RefShape::Array => json!([bad_url]),
        };
        body.insert(edge.field.to_string(), value);

        let err = st.create(kind, Value::Object(body)).expect_err(&format!(
            "edge {:?}.{} -> {} accepted a wrong-kind url — the ref-type \
             check for this row is not being reached",
            edge.owner, edge.field, edge.target
        ));
        assert_eq!(
            err.status, 400,
            "edge {:?}.{} -> {}: wrong-kind url got status {} instead of a 400",
            edge.owner, edge.field, edge.target, err.status
        );
        assert!(
            format!("{:?}", err.body).contains("Invalid hyperlink"),
            "edge {:?}.{} -> {}: refused for the wrong reason: {:?}",
            edge.owner,
            edge.field,
            edge.target,
            err.body
        );
    }

    /// A fresh, real instance of `target` to attach a back-reference to.
    /// Only the three kinds that are ever the TARGET of a `back_ref` row
    /// need an arm here (`only_the_documented_owners_maintain_a_back_reference`
    /// pins the owner side; this is the target side) — a future back-ref row
    /// aimed at a new target kind needs its own arm, and the panic says so
    /// rather than silently building a nonsense body.
    fn create_target(
        st: &mut OrgState,
        target: &'static str,
        ws_url: &str,
        schema_url: &str,
    ) -> (u64, String) {
        let body = match target {
            "workspaces" => json!({ "name": "target-ws" }),
            "schemas" => json!({ "name": "target-schema" }),
            "queues" => json!({ "name": "target-queue", "workspace": ws_url, "schema": schema_url }),
            other => panic!(
                "assert_back_ref_is_maintained has no target-builder for {other:?} \
                 — add one alongside the new back_ref row"
            ),
        };
        let created = st.create(target, body).expect("target create must succeed");
        (created["id"].as_u64().unwrap(), created["url"].as_str().unwrap().to_string())
    }

    /// The correct back-reference for `(owner, target)`, pinned independent
    /// of whatever `EDGES` currently says. Without this, a renamed
    /// `back_ref` field (e.g. `queue.inbox` corrupted to `queue.inbox_wrong`)
    /// would be invisible to a check that reads the field name to look up
    /// from the SAME row it is verifying: `graph::relink` and the assertion
    /// below would both derive "which field" from the one (broken) row and
    /// agree with each other — a fake `state.rs` demonstrated live, driving
    /// this exact rename, in the course of writing this loop. Only an
    /// answer that does NOT come from the row can catch that, which is what
    /// this is; the graph.rs hand-written tests
    /// (`creating_an_inbox_sets_its_queues_inbox` and siblings) already
    /// pin the same facts by hand, one row at a time — this is the same
    /// oracle, applied to every row through the loop instead.
    fn expected_back_ref(owner: &str, target: &str) -> BackRef {
        match (owner, target) {
            ("queues", "workspaces") => BackRef::Push("queues"),
            ("queues", "schemas") => BackRef::Push("queues"),
            ("inboxes", "queues") => BackRef::Set("inbox"),
            ("hooks", "queues") => BackRef::Push("hooks"),
            ("rules", "queues") => BackRef::Push("rules"),
            (o, t) => panic!(
                "expected_back_ref has no known-good answer for {o}.* -> {t} \
                 — add one alongside the new back_ref row"
            ),
        }
    }

    /// For `edge` (which must carry a `back_ref`), first check the row's
    /// declared `back_ref` against the independent, known-good answer above,
    /// then create a VALID instance of its owner pointing at a fresh target
    /// and assert the back-reference really appears on that target
    /// afterward — the behavior `graph::relink` derives from every
    /// `back_ref` in `EDGES`.
    fn assert_back_ref_is_maintained(edge: &Edge) {
        let mut st = OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1);
        let ws = st.create("workspaces", json!({ "name": "ws" })).unwrap();
        let sc = st.create("schemas", json!({ "name": "sc" })).unwrap();
        let ws_url = ws["url"].as_str().unwrap().to_string();
        let sc_url = sc["url"].as_str().unwrap().to_string();

        let (target_id, target_url) = create_target(&mut st, edge.target, &ws_url, &sc_url);

        let owner = edge.owner.expect(
            "back_ref is only ever set on an owner-scoped row — \
             no_universal_row_carries_a_back_ref enforces this",
        );
        let expected = expected_back_ref(owner, edge.target);
        assert_eq!(
            edge.back_ref,
            Some(expected),
            "edge {owner}.{} -> {}: back_ref is {:?}, but the known-good answer is {:?} \
             — this field name was renamed to something the target does not really carry",
            edge.field,
            edge.target,
            edge.back_ref,
            expected
        );

        let mut body = minimal_body(owner, &ws_url, &sc_url);
        let value = match edge.shape {
            RefShape::Single => json!(target_url),
            RefShape::Array => json!([target_url]),
        };
        body.insert(edge.field.to_string(), value);
        let created = st.create(owner, Value::Object(body)).unwrap_or_else(|e| {
            panic!(
                "edge {owner}.{} -> {}: a VALID create was refused: {:?}",
                edge.field, edge.target, e.body
            )
        });
        let created_url = created["url"].as_str().unwrap().to_string();

        let target_after = st.get(edge.target, target_id).expect("target still exists");
        match expected {
            BackRef::Push(field) => {
                let arr = target_after.get(field).and_then(Value::as_array).unwrap_or_else(|| {
                    panic!(
                        "edge {owner}.{} -> {}: back_ref Push({field:?}) names a field the \
                         target doesn't have: {target_after:?}",
                        edge.field, edge.target
                    )
                });
                assert!(
                    arr.contains(&json!(created_url)),
                    "edge {owner}.{} -> {}: back_ref Push({field:?}) never appeared on the \
                     target: {arr:?}",
                    edge.field,
                    edge.target
                );
            }
            BackRef::Set(field) => {
                assert_eq!(
                    target_after.get(field),
                    Some(&json!(created_url)),
                    "edge {owner}.{} -> {}: back_ref Set({field:?}) never appeared on the target: {target_after:?}",
                    edge.field,
                    edge.target
                );
            }
        }
    }

    /// The permanent proof that `EDGES` is actually READ, not merely
    /// declared: for every row still IN the table, drive the fake and assert
    /// what the row claims — the ref-type check refuses a wrong-kind url,
    /// and, when the row names a `back_ref`, that back-reference really
    /// grows on the target after a real create, at the field name an
    /// INDEPENDENT answer (`expected_back_ref`) says is correct. If
    /// `validate.rs` or `graph.rs` stops actually consulting a row that is
    /// still declared here — the more likely regression, since deleting a
    /// row outright is a diff anyone reviewing `EDGES` would see — the
    /// corresponding assertion fails HERE, naming the row. With one
    /// exception, worth knowing before trusting a green run too far.
    ///
    /// The REF-TYPE half is masked for the three `*.queues` owner rows
    /// (`inboxes`, `hooks`, `rules`). `validate::on_write` iterates every
    /// edge whose `owner` is `None` or equal to the kind being written, so a
    /// wrong-kind url in one of those bodies is refused by the universal
    /// `(None, "queues", "queues")` row just as well as by the owner's own —
    /// `assert_ref_type_is_enforced` would still pass if `validate` stopped
    /// consulting the owner row entirely. Verified by driving the fake with
    /// exactly those three rows filtered out of `on_write`'s iteration: the
    /// loop stayed green. Every other row is unmasked, because no universal
    /// row carries its field name (`workspace`, `schema`, `engine`,
    /// `generic_engine`, `queue`, `organization`, `queues_filter`).
    ///
    /// Those three rows are still proven load-bearing here — by the OTHER
    /// half. `graph::relink`/`unlink` walk `kinds::edges_for`, which is
    /// owner-scoped only and never sees a universal row, so
    /// `assert_back_ref_is_maintained` fails the moment one of them stops
    /// being consulted; verified the same way, by skipping those three rows
    /// in `relink` and watching the loop fail naming `inboxes.queues`.
    ///
    /// A row's outright DELETION is a narrower case this loop cannot itself
    /// catch: it can only assert about rows still present in `EDGES`, so
    /// removing one just shrinks the loop rather than failing it. The
    /// backstop for that is
    /// `the_edge_table_still_models_every_edge_it_is_supposed_to` above,
    /// which pins the full row set against a hand-written list and therefore
    /// covers EVERY row — owner-scoped or universal, `back_ref`-carrying or
    /// not, `email_templates.queue` included.
    ///
    /// Two narrower backstops also fire for some rows, and neither is
    /// general — stated exactly, because "a deleted row fails loudly
    /// somewhere else too" is easy to over-read:
    /// `only_the_documented_owners_maintain_a_back_reference` compares the
    /// set of OWNERS, so it only notices a deletion that leaves an owner
    /// with no `back_ref` row at all. That covers `inboxes.queues`,
    /// `hooks.queues` and `rules.queues`, but NOT `queues.workspace` or
    /// `queues.schema`: delete either and `"queues"` is still in the set via
    /// the surviving sibling. graph.rs's hand-written tests cover those two
    /// (`creating_a_queue_grows_its_workspace_and_schema` asserts both
    /// back-references, `patching_a_queues_workspace_moves_the_back_ref` the
    /// first). All of it verified by deleting each row in turn and reading
    /// off which tests actually went red.
    #[test]
    fn every_edge_in_the_table_is_load_bearing() {
        for edge in EDGES {
            assert_ref_type_is_enforced(edge);
            if edge.back_ref.is_some() {
                assert_back_ref_is_maintained(edge);
            }
        }
    }
}
