# Stateful fake org — Stage 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the live scenario suite a second, offline backend — a stateful
fake Rossum organization — and get `fake_round_trip_core` passing in a plain
`cargo test`, so convergence is asserted offline for the first time.

**Architecture:** A `wiremock` catch-all responder backed by
`Arc<Mutex<OrgState>>` serves the 28 endpoints `rdc` calls, applying writes and
answering reads from its own state. `FakeOrg` hands out an `EnvCreds`, which is
all the existing harness needs — `ProjectFixture` already builds a project from
one. Every scenario body is extracted to a function over `&LiveConfig` and gets
two wrappers: `fake_*` (runs in `cargo test`) and `live_*` (`#[ignore]`,
unchanged).

**Tech Stack:** Rust, `wiremock` 0.6.5 (dev-dep, already present), `serde_json`,
`chrono` (already a regular dep, available to integration tests), `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`

## Global Constraints

- **No production-code changes.** Everything in this plan lives under `tests/`.
  If a task seems to need a change under `src/`, stop and report it — that is a
  finding, not a step.
- **The `live_*` wrappers keep `#[ignore = "live: needs RDC_LIVE_* env"]` and
  the same `LiveConfig::from_env()` early return.** `cargo test --test live --
  --ignored` must keep selecting exactly the live set (README.md:522).
- **Fake tests must be deterministic.** Own port and own state per test, a
  logical clock instead of the wall clock, zero sleeps, no randomness beyond the
  existing `RunId`. `cargo test --locked` is the weekly release gate
  (`.github/workflows/weekly-release.yaml:141`), so a flaky fake blocks releases.
- **Every quirk carries `Proven by: tests/live/scenarios/<file>.rs::<test_fn>`**
  and the guard test in Task 5 enforces it.
- **Strict validation:** the fake rejects what the server rejects.
- **Causality, not latency:** a pending queue delete resolves after one further
  request, never after a wall-clock wait.
- **No customer names or customer-specific identifiers** anywhere — source,
  tests, fixtures, or commit messages. Use `fake`, `main`, `invoices`.
- **Commit after every task.** Do not `git push`; local `main` only.
- `cargo clippy --all-targets --locked -- -D warnings` must stay clean.

## File Structure

| file | responsibility |
| --- | --- |
| `tests/live/support/fake/mod.rs` | `FakeOrg`: server lifecycle, the catch-all handler, request routing, creds/config construction |
| `tests/live/support/fake/state.rs` | `OrgState`: the object graph, id allocation, url minting, the logical clock, pagination, back-references, the pending-delete machine |
| `tests/live/support/fake/kinds.rs` | the per-kind table: path segment, creatability, detail-GET support, list projections, required-field synthesis |
| `tests/live/support/fake/quirks.rs` | the learned-facts layer and its provenance registry + guard test |
| `tests/live/support/fake/validate.rs` | the strict rejections |
| `tests/live/support/mod.rs` | add `pub mod fake;` |
| `tests/live/scenarios/round_trip.rs` | extract the body; add the `fake_` wrapper |

## Verified facts this plan is built on

Read off the tree on 2026-09-07. Do not re-derive these; do check them if a
step misbehaves.

**Required fields per kind** — every other field on these models has a serde
default, so these are the only ones the fake MUST emit or a response fails to
deserialize:

| kind | required |
| --- | --- |
| `labels` | `name`, `organization` |
| `workspaces` | `name`, `organization` |
| `schemas` | `name`, `content` |
| `queues` | `name` |
| `inboxes` | `name`, `queues` |
| `hooks` | `name`, `type` (→ `hook_type`, `#[serde(rename = "type")]`) |
| `rules` | `name` |

**The 12 list endpoints a sync visits** (`CORE_LIST_ENDPOINTS`,
`tests/cli_sync.rs:199`): `hooks`, `workspaces`, `queues`, `inboxes`, `rules`,
`labels`, `engines`, `engine_fields`, `workflows`, `workflow_steps`,
`email_templates`, `saved_views`. Plus `GET /organizations/{id}`. **`/schemas`
is not listed** — rdc GETs each schema by id, which is why the list's omission
of `content` is safe.

**Seed creation order** is deterministic. `Manifest::topo_order` sorts the ready
set and `pop()`s the last, giving: `ws-secondary`, `ws-main`,
`schema-invoices-secondary`, `schema-invoices-main`, `queue-invoices-secondary`,
`queue-invoices-main`, `rule-totals`, `label-priority`, `inbox-invoices-main`,
`hook-validator`, `hook-post-validator`. With monotonic id allocation the
secondary workspace/schema/queue therefore get the LOWER ids — which is exactly
why `testdata/live/expected/round_trip.toml` records
`queue.workspace = "rdc://workspaces/<id>secondary"` and the bare `invoices`
slugs going to the secondary objects. **The golden is reproducible from creation
order.** If it does not match, that is a real divergence to investigate, not a
golden to re-capture.

**`rdc` treats `modified_at` as an opaque string** (`model::modified_at` returns
`Option<&str>`; `pull/common.rs` tests use `"t1"`/`"t2"`), so a logical clock is
safe.

**MDH needs no stub.** `api_base = <uri>/api/v1` derives the Data Storage base
as `<uri>/svc/data-storage/api` — same host and port (`src/config/mod.rs:29`) —
and a 404 there models an MDH-less org, which the pull driver tolerates
(`src/config/mod.rs:33`).

**The client-side rate limiter is off for loopback.** `RossumClient::new`
builds a limiter only `(!is_loopback_base(&base_url))` (`src/api/mod.rs`), so a
fake on `127.0.0.1` pays no pacing cost — which is why these tests can be fast
without anyone special-casing them.

**`wiremock` 0.6.5:** `matchers::any()` (`src/matchers.rs:126`);
`MockServerBuilder::disable_request_recording()` (`src/mock_server/builder.rs:85`);
closures implement `Respond` with `F: Send + Sync + Fn(&Request) ->
ResponseTemplate` (`src/respond.rs:147`) — **the handler is synchronous**, so
state is a `std::sync::Mutex` and nothing in it may be async.

---

### Task 1: `OrgState` — the store

Pure data, no HTTP. Everything here is synchronous.

**Files:**
- Create: `tests/live/support/fake/state.rs`
- Create: `tests/live/support/fake/kinds.rs`
- Create: `tests/live/support/fake/mod.rs` (module declarations only in this task)
- Modify: `tests/live/support/mod.rs` (add `pub mod fake;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `state::OrgState::new(api_base: String, org_id: u64) -> OrgState`
  - `OrgState::url(&self, kind: &str, id: u64) -> String`
  - `OrgState::org_url(&self) -> String`
  - `OrgState::organization(&self) -> Value`
  - `OrgState::patch_organization(&mut self, patch: &Value) -> Value`
  - `OrgState::create(&mut self, kind: &'static str, body: Value) -> Result<Value, ApiError>`
  - `OrgState::get(&self, kind: &'static str, id: u64) -> Option<Value>`
  - `OrgState::patch(&mut self, kind: &'static str, id: u64, patch: &Value) -> Result<Value, ApiError>`
  - `OrgState::delete(&mut self, kind: &'static str, id: u64) -> Result<Deletion, ApiError>`
  - `OrgState::list(&mut self, kind: &'static str, q: &ListQuery) -> Value`
  - `OrgState::tick_deletions(&mut self)`
  - `OrgState::ids(&self, kind: &str) -> Vec<u64>`
  - `state::ApiError { status: u16, body: Value }` with `bad_request`, `non_field`, `not_found`, `unauthorized`
  - `state::Deletion { Gone, Requested }`
  - `state::ListQuery { page: u64, page_size: u64 }`
  - `kinds::KindSpec { path, creatable, detail_get, list_omits, defaults }`
  - `kinds::spec(path: &str) -> Option<&'static KindSpec>`
  - `kinds::OrgCtx { org_url: String }`

- [ ] **Step 1: Write the failing tests**

Append to `tests/live/support/fake/state.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn st() -> OrgState {
        OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1)
    }

    #[test]
    fn create_mints_id_url_and_a_monotonic_timestamp() {
        let mut s = st();
        let a = s
            .create("labels", json!({ "name": "One", "color": "#ff0000" }))
            .expect("create");
        let b = s
            .create("labels", json!({ "name": "Two", "color": "#00ff00" }))
            .expect("create");
        assert_eq!(a["id"], json!(1));
        assert_eq!(b["id"], json!(2), "ids are monotonic in creation order");
        assert_eq!(a["url"], json!("http://127.0.0.1:9/api/v1/labels/1"));
        assert!(
            a["modified_at"].as_str().unwrap() < b["modified_at"].as_str().unwrap(),
            "the logical clock must advance: {a:?} then {b:?}"
        );
    }

    #[test]
    fn create_fills_the_fields_the_model_requires() {
        let mut s = st();
        // `organization` is required on a label and the seed bodies supply it,
        // but a body that omits it must still come back deserializable.
        let l = s.create("labels", json!({ "name": "One" })).expect("create");
        assert_eq!(l["organization"], json!("http://127.0.0.1:9/api/v1/organizations/1"));
        let sc = s.create("schemas", json!({ "name": "S" })).expect("create");
        assert_eq!(sc["content"], json!([]), "schema.content has no serde default");
        assert_eq!(sc["queues"], json!([]));
    }

    #[test]
    fn list_uses_the_rossum_envelope_and_pages() {
        let mut s = st();
        for i in 0..5 {
            s.create("labels", json!({ "name": format!("L{i}") })).unwrap();
        }
        let page1 = s.list("labels", &ListQuery { page: 1, page_size: 2 });
        assert_eq!(page1["pagination"]["total"], json!(5));
        assert_eq!(page1["pagination"]["total_pages"], json!(3));
        assert_eq!(page1["results"].as_array().unwrap().len(), 2);
        let page3 = s.list("labels", &ListQuery { page: 3, page_size: 2 });
        assert_eq!(page3["results"].as_array().unwrap().len(), 1);
        // Ordered by id, which is what `?ordering=id` asks for.
        assert_eq!(page1["results"][0]["id"], json!(1));
        assert_eq!(page1["results"][1]["id"], json!(2));
    }

    #[test]
    fn page_size_is_capped_at_a_hundred() {
        let mut s = st();
        s.create("labels", json!({ "name": "L" })).unwrap();
        let out = s.list("labels", &ListQuery { page: 1, page_size: 5000 });
        assert_eq!(out["pagination"]["total_pages"], json!(1));
    }

    #[test]
    fn an_empty_list_still_reports_one_page() {
        let mut s = st();
        let out = s.list("labels", &ListQuery { page: 1, page_size: 100 });
        assert_eq!(out["pagination"]["total"], json!(0));
        assert_eq!(
            out["pagination"]["total_pages"],
            json!(1),
            "reporting 0 would send rdc down its follow-`next` fallback (api/mod.rs:492)"
        );
        assert_eq!(out["results"], json!([]));
    }

    #[test]
    fn the_schemas_list_omits_content_but_a_detail_get_keeps_it() {
        let mut s = st();
        s.create("schemas", json!({ "name": "S", "content": [{ "id": "x" }] }))
            .unwrap();
        let listed = s.list("schemas", &ListQuery { page: 1, page_size: 100 });
        assert!(
            listed["results"][0].get("content").is_none(),
            "the real /schemas list omits content (pull/common.rs:66)"
        );
        let one = s.get("schemas", 1).expect("detail");
        assert_eq!(one["content"], json!([{ "id": "x" }]));
    }

    #[test]
    fn patch_merges_top_level_keys_and_advances_the_clock() {
        let mut s = st();
        let before = s
            .create("labels", json!({ "name": "One", "color": "#ff0000" }))
            .unwrap();
        let after = s
            .patch("labels", 1, &json!({ "color": "#00ff00" }))
            .expect("patch");
        assert_eq!(after["color"], json!("#00ff00"));
        assert_eq!(after["name"], json!("One"), "untouched keys survive");
        assert!(
            before["modified_at"].as_str().unwrap() < after["modified_at"].as_str().unwrap()
        );
    }

    #[test]
    fn patching_or_getting_an_unknown_id_is_a_404() {
        let mut s = st();
        assert_eq!(s.patch("labels", 99, &json!({})).unwrap_err().status, 404);
        assert!(s.get("labels", 99).is_none());
        assert_eq!(s.delete("labels", 99).unwrap_err().status, 404);
    }

    #[test]
    fn deleting_a_leaf_is_immediate() {
        let mut s = st();
        s.create("labels", json!({ "name": "One" })).unwrap();
        assert_eq!(s.delete("labels", 1).unwrap(), Deletion::Gone);
        assert!(s.get("labels", 1).is_none());
    }

    #[test]
    fn an_unmodelled_kind_is_a_404() {
        let mut s = st();
        assert_eq!(s.create("penguins", json!({})).unwrap_err().status, 404);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test live fake::state -- --nocapture`
Expected: FAIL — `state.rs` does not exist / `OrgState` not found.

- [ ] **Step 3: Write `kinds.rs`**

```rust
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
```

- [ ] **Step 4: Write `state.rs`**

```rust
//! The fake organization's object graph.
//!
//! Pure data — no HTTP, nothing async. `FakeOrg` owns one of these behind a
//! `std::sync::Mutex`, because wiremock's responder closure is synchronous
//! (`wiremock::respond::Respond`, `F: Fn(&Request) -> ResponseTemplate`).

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use super::kinds::{self, OrgCtx};

/// A rejection, shaped like the real API's.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub body: Value,
}

impl ApiError {
    pub fn bad_request(detail: impl Into<String>) -> ApiError {
        ApiError { status: 400, body: json!({ "detail": detail.into() }) }
    }

    /// The shape Rossum uses for cross-field refusals — the form
    /// `src/cli/push/mod.rs:88-96` quotes for the engine-field check.
    pub fn non_field(msg: impl Into<String>) -> ApiError {
        ApiError { status: 400, body: json!({ "non_field_errors": [msg.into()] }) }
    }

    pub fn not_found() -> ApiError {
        ApiError { status: 404, body: json!({ "detail": "Not found." }) }
    }

    pub fn unauthorized() -> ApiError {
        ApiError { status: 401, body: json!({ "detail": "Invalid token." }) }
    }
}

/// What a DELETE resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum Deletion {
    /// 204 — gone now.
    Gone,
    /// 202 `deletion_requested` — a queue, still listed for one more request.
    Requested,
}

/// `?page=&page_size=` as parsed off the query string.
pub struct ListQuery {
    pub page: u64,
    pub page_size: u64,
}

pub struct OrgState {
    api_base: String,
    org_id: u64,
    objects: BTreeMap<&'static str, BTreeMap<u64, Value>>,
    /// Monotonic across every kind, so creation order is recoverable from ids.
    /// `Manifest::topo_order` is deterministic, which is what makes
    /// `testdata/live/expected/round_trip.toml` reproducible here.
    next_id: u64,
    /// Logical clock in seconds past a fixed epoch. Never the wall clock: a
    /// fake test that reads the real time is a fake test that can flake.
    clock: i64,
    /// Queue id -> requests still to survive before it actually goes.
    pending_delete: BTreeMap<u64, u8>,
    org: Value,
}

impl OrgState {
    pub fn new(api_base: String, org_id: u64) -> OrgState {
        let api_base = api_base.trim_end_matches('/').to_string();
        let org = json!({
            "id": org_id,
            "url": format!("{api_base}/organizations/{org_id}"),
            "name": "Fake Org",
            "modified_at": "2026-01-01T00:00:00Z",
            "settings": {},
            "users": [],
            "workspaces": [],
        });
        OrgState {
            api_base,
            org_id,
            objects: BTreeMap::new(),
            next_id: 1,
            clock: 0,
            pending_delete: BTreeMap::new(),
            org,
        }
    }

    pub fn url(&self, kind: &str, id: u64) -> String {
        format!("{}/{}/{}", self.api_base, kind, id)
    }

    pub fn org_url(&self) -> String {
        format!("{}/organizations/{}", self.api_base, self.org_id)
    }

    pub fn org_id(&self) -> u64 {
        self.org_id
    }

    pub fn organization(&self) -> Value {
        self.org.clone()
    }

    pub fn patch_organization(&mut self, patch: &Value) -> Value {
        let stamp = self.now();
        if let (Some(dst), Some(src)) = (self.org.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
            dst.insert("modified_at".into(), json!(stamp));
        }
        self.org.clone()
    }

    /// A deterministic, strictly increasing RFC3339 instant. `rdc` treats
    /// `modified_at` as an opaque string (`model::modified_at`), so only the
    /// ordering matters — but a real instant keeps the fake honest.
    fn now(&mut self) -> String {
        self.clock += 1;
        let base = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("a static timestamp parses");
        (base + chrono::Duration::seconds(self.clock))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn ctx(&self) -> OrgCtx {
        OrgCtx { org_url: self.org_url() }
    }

    pub fn create(&mut self, kind: &'static str, mut body: Value) -> Result<Value, ApiError> {
        let spec = kinds::spec(kind).ok_or_else(ApiError::not_found)?;
        if !spec.creatable {
            return Err(ApiError::bad_request(format!("/{kind} is read-only")));
        }
        let id = self.next_id;
        let url = self.url(kind, id);
        let stamp = self.now();
        let ctx = self.ctx();
        let obj = body
            .as_object_mut()
            .ok_or_else(|| ApiError::bad_request("body must be a JSON object"))?;
        obj.insert("id".into(), json!(id));
        obj.insert("url".into(), json!(url));
        obj.insert("modified_at".into(), json!(stamp));
        (spec.defaults)(obj, &ctx);
        self.next_id += 1;
        self.objects.entry(kind).or_default().insert(id, body.clone());
        Ok(body)
    }

    pub fn get(&self, kind: &'static str, id: u64) -> Option<Value> {
        self.objects.get(kind)?.get(&id).cloned()
    }

    pub fn patch(
        &mut self,
        kind: &'static str,
        id: u64,
        patch: &Value,
    ) -> Result<Value, ApiError> {
        kinds::spec(kind).ok_or_else(ApiError::not_found)?;
        if self.get(kind, id).is_none() {
            return Err(ApiError::not_found());
        }
        let stamp = self.now();
        let slot = self
            .objects
            .get_mut(kind)
            .and_then(|m| m.get_mut(&id))
            .expect("presence checked above");
        if let (Some(dst), Some(src)) = (slot.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                // A Rossum PATCH is a shallow merge of the keys it carries.
                dst.insert(k.clone(), v.clone());
            }
            dst.insert("modified_at".into(), json!(stamp));
        }
        Ok(slot.clone())
    }

    pub fn delete(&mut self, kind: &'static str, id: u64) -> Result<Deletion, ApiError> {
        kinds::spec(kind).ok_or_else(ApiError::not_found)?;
        if self.get(kind, id).is_none() {
            return Err(ApiError::not_found());
        }
        if kind == "queues" {
            // `202 deletion_requested`: still listed for one more request.
            self.pending_delete.insert(id, 1);
            return Ok(Deletion::Requested);
        }
        self.objects.get_mut(kind).expect("kind present").remove(&id);
        Ok(Deletion::Gone)
    }

    pub fn list(&mut self, kind: &'static str, q: &ListQuery) -> Value {
        let spec = kinds::spec(kind);
        let all: Vec<Value> = self
            .objects
            .get(kind)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        let projected: Vec<Value> = all
            .into_iter()
            .map(|mut v| {
                if let (Some(spec), Some(o)) = (spec, v.as_object_mut()) {
                    for key in spec.list_omits {
                        o.remove(*key);
                    }
                }
                v
            })
            .collect();
        let page_size = q.page_size.clamp(1, 100) as usize;
        let total = projected.len();
        // Never 0: `api/mod.rs:492` reads a 0 as "this endpoint does not report
        // total_pages" and switches to following `next`, which the fake does
        // not serve.
        let total_pages = total.div_ceil(page_size).max(1);
        let start = (q.page.max(1) as usize - 1) * page_size;
        let results: Vec<Value> = projected.into_iter().skip(start).take(page_size).collect();
        json!({
            "pagination": {
                "total": total,
                "total_pages": total_pages,
                "next": Value::Null,
                "previous": Value::Null,
            },
            "results": results,
        })
    }

    /// Charge every pending queue delete one request. Called by the router
    /// after each response, so a 202 is followed by exactly one more sighting.
    pub fn tick_deletions(&mut self) {
        let mut done = Vec::new();
        for (id, left) in self.pending_delete.iter_mut() {
            match left.checked_sub(1) {
                Some(0) | None => done.push(*id),
                Some(n) => *left = n,
            }
        }
        for id in done {
            self.pending_delete.remove(&id);
            self.objects.get_mut("queues").map(|m| m.remove(&id));
        }
    }

    pub fn ids(&self, kind: &str) -> Vec<u64> {
        self.objects
            .get(kind)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    }
}
```

Note the borrow order in `create`: `url`, `now()` and `ctx()` are taken
BEFORE `body.as_object_mut()`, because they need `&mut self` / `&self`.

- [ ] **Step 5: Write the module declarations**

`tests/live/support/fake/mod.rs`:

```rust
//! An offline, stateful stand-in for a Rossum organization.
//!
//! See `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`.
//! The short version: the live scenario suite is already parameterized on
//! `(api_base, org_id, token)`, so a fake that speaks HTTP lets the same
//! scenario bodies run in a plain `cargo test` — which is the only way
//! "nothing should happen the second time" becomes an assertion rather than
//! something a human notices in a customer env.

pub mod kinds;
pub mod state;
```

Add to `tests/live/support/mod.rs`, keeping the file's existing ordering style:

```rust
pub mod fake;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test live fake:: -- --nocapture`
Expected: PASS — 10 tests in `fake::state::tests` and 2 in `fake::kinds::tests`.

- [ ] **Step 7: Commit**

```bash
git add tests/live/support/fake tests/live/support/mod.rs
git commit -m "test(fake): add the fake org's object store

Pure state: monotonic ids, minted urls, a logical clock, the Rossum list
envelope with page/page_size, and the per-kind table that fills in the
fields each model declares without a serde default. No HTTP yet.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `FakeOrg` — the transport

**Files:**
- Modify: `tests/live/support/fake/mod.rs`

**Interfaces:**
- Consumes: `state::{OrgState, ApiError, Deletion, ListQuery}`, `kinds::spec`.
- Produces:
  - `FakeOrg::start() -> FakeOrg` (async)
  - `FakeOrg::start_with_org(org_id: u64) -> FakeOrg` (async)
  - `FakeOrg::api_base(&self) -> String`
  - `FakeOrg::creds(&self) -> EnvCreds`
  - `FakeOrg::config(&self) -> LiveConfig`
  - `FakeOrg::paired_config(&self, target: &FakeOrg) -> LiveConfig`
  - `FakeOrg::state(&self) -> MutexGuard<'_, OrgState>`

- [ ] **Step 1: Write the failing tests**

Append to `tests/live/support/fake/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rdc::api::RossumClient;
    use serde_json::json;

    /// The typed client is the point: it is the same code path the seeder and
    /// `rdc` itself use, so a response the fake shapes wrongly fails here
    /// rather than somewhere deep in a pull.
    fn client(fake: &FakeOrg) -> RossumClient {
        let c = fake.creds();
        RossumClient::new(c.api_base, c.token).expect("client")
    }

    #[tokio::test]
    async fn a_label_round_trips_over_http() {
        let fake = FakeOrg::start().await;
        let c = client(&fake);
        let created = c
            .create_label(&json!({ "name": "One", "color": "#ff0000" }), None)
            .await
            .expect("create");
        assert_eq!(created.id, 1);
        assert_eq!(created.url, fake.state().url("labels", 1));

        let listed = c.list_labels(None).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "One");

        c.update_label(1, &json!({ "color": "#00ff00" }), None)
            .await
            .expect("patch");
        let listed = c.list_labels(None).await.expect("relist");
        assert_eq!(listed[0].extra.get("color"), Some(&json!("#00ff00")));

        c.delete_label(1, None).await.expect("delete");
        assert!(c.list_labels(None).await.expect("relist").is_empty());
    }

    #[tokio::test]
    async fn the_organization_endpoint_answers() {
        let fake = FakeOrg::start().await;
        let org = client(&fake).get_organization(1, None).await.expect("org");
        assert_eq!(org.id, 1);
    }

    #[tokio::test]
    async fn a_bad_token_is_rejected() {
        let fake = FakeOrg::start().await;
        let c = RossumClient::new(fake.api_base(), "wrong".to_string()).expect("client");
        let err = c.list_labels(None).await.expect_err("must be rejected");
        assert!(format!("{err:#}").contains("401"), "expected a 401: {err:#}");
    }

    /// Data Storage sits at the same host and port (`src/config/mod.rs:29`),
    /// and a 404 there is how an MDH-less org looks — which the pull driver
    /// tolerates (`src/config/mod.rs:33`).
    #[tokio::test]
    async fn data_storage_paths_are_404() {
        let fake = FakeOrg::start().await;
        let url = format!(
            "{}/svc/data-storage/api/v1/collections",
            fake.api_base().trim_end_matches("/api/v1")
        );
        let status = reqwest::get(&url).await.expect("request").status();
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn two_fakes_are_independent_orgs() {
        let a = FakeOrg::start().await;
        let b = FakeOrg::start_with_org(2).await;
        client(&a)
            .create_label(&json!({ "name": "OnlyInA" }), None)
            .await
            .expect("create");
        assert_eq!(client(&b).list_labels(None).await.expect("list").len(), 0);
        let cfg = a.paired_config(&b);
        assert_eq!(cfg.org_id, 1);
        assert_eq!(cfg.target.expect("target").org_id, 2);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test live fake::tests -- --nocapture`
Expected: FAIL — `FakeOrg` not found.

- [ ] **Step 3: Write the transport**

Prepend to `tests/live/support/fake/mod.rs` (after the existing module docs
and `pub mod` lines):

```rust
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::support::config::{EnvCreds, LiveConfig};
use state::{ApiError, Deletion, ListQuery, OrgState};

/// The token the fake accepts. Fixed, because nothing about it is secret and a
/// random one would make failures harder to read.
const TOKEN: &str = "fake-token";

pub struct FakeOrg {
    server: MockServer,
    inner: Arc<Mutex<OrgState>>,
    org_id: u64,
}

impl FakeOrg {
    pub async fn start() -> FakeOrg {
        Self::start_with_org(1).await
    }

    pub async fn start_with_org(org_id: u64) -> FakeOrg {
        // Recording off: a sync makes hundreds of calls and no test here reads
        // the history back (order assertions use `RDC_TRACE_HTTP` instead).
        let server = MockServer::builder()
            .disable_request_recording()
            .start()
            .await;
        let api_base = format!("{}/api/v1", server.uri());
        let inner = Arc::new(Mutex::new(OrgState::new(api_base, org_id)));
        let handler = inner.clone();
        Mock::given(any())
            .respond_with(move |req: &Request| {
                let mut st = handler.lock().unwrap_or_else(|p| p.into_inner());
                let out = route(&mut st, req);
                // Charge pending queue deletes for this request, so a 202 is
                // followed by exactly one more sighting.
                st.tick_deletions();
                out
            })
            .mount(&server)
            .await;
        FakeOrg { server, inner, org_id }
    }

    pub fn api_base(&self) -> String {
        format!("{}/api/v1", self.server.uri())
    }

    pub fn creds(&self) -> EnvCreds {
        EnvCreds {
            api_base: self.api_base(),
            org_id: self.org_id,
            token: TOKEN.to_string(),
        }
    }

    pub fn config(&self) -> LiveConfig {
        let c = self.creds();
        LiveConfig {
            api_base: c.api_base,
            org_id: c.org_id,
            token: c.token,
            target: None,
        }
    }

    /// Source + target, the shape a promotion needs — pointing both envs at
    /// one org quietly defeats every promotion assertion
    /// (`tests/live/support/config.rs:23`).
    pub fn paired_config(&self, target: &FakeOrg) -> LiveConfig {
        let mut cfg = self.config();
        cfg.target = Some(target.creds());
        cfg
    }

    /// Direct access for assertions and out-of-band seeding.
    pub fn state(&self) -> MutexGuard<'_, OrgState> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn json_response(status: u16, body: &Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body.clone())
}

fn err_response(e: ApiError) -> ResponseTemplate {
    json_response(e.status, &e.body)
}

fn authorized(req: &Request) -> bool {
    req.headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("token {TOKEN}"))
        .unwrap_or(false)
}

fn body_of(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap_or(Value::Null)
}

fn list_query(req: &Request) -> ListQuery {
    let num = |key: &str, default: u64| {
        req.url
            .query_pairs()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    ListQuery { page: num("page", 1), page_size: num("page_size", 20) }
}

/// Map a path segment onto the `&'static str` the store is keyed by, so a
/// typo in a URL cannot silently create a new bucket.
fn kind_key(segment: &str) -> Option<&'static str> {
    kinds::spec(segment).map(|k| k.path)
}

fn route(st: &mut OrgState, req: &Request) -> ResponseTemplate {
    if !authorized(req) {
        return err_response(ApiError::unauthorized());
    }
    let path = req.url.path().to_string();
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        // Everything outside the API prefix — Data Storage included.
        return err_response(ApiError::not_found());
    };
    let mut segs = rest.split('/').filter(|s| !s.is_empty());
    let Some(head) = segs.next() else {
        return err_response(ApiError::not_found());
    };
    let tail = segs.next().map(|s| s.to_string());
    let method = req.method.as_str().to_string();

    if head == "organizations" {
        return match method.as_str() {
            "GET" => json_response(200, &st.organization()),
            "PATCH" => json_response(200, &st.patch_organization(&body_of(req))),
            _ => err_response(ApiError::not_found()),
        };
    }

    let Some(kind) = kind_key(head) else {
        return err_response(ApiError::not_found());
    };
    let id = tail.as_deref().and_then(|s| s.parse::<u64>().ok());

    match (method.as_str(), id) {
        ("GET", None) => json_response(200, &st.list(kind, &list_query(req))),
        ("POST", None) => match st.create(kind, body_of(req)) {
            Ok(v) => json_response(201, &v),
            Err(e) => err_response(e),
        },
        ("GET", Some(id)) => {
            let detail = kinds::spec(kind).map(|k| k.detail_get).unwrap_or(false);
            if !detail {
                // Labels have no detail endpoint
                // (`tests/live/scenarios/round_trip.rs:120`).
                return err_response(ApiError::not_found());
            }
            match st.get(kind, id) {
                Some(v) => json_response(200, &v),
                None => err_response(ApiError::not_found()),
            }
        }
        ("PATCH", Some(id)) => match st.patch(kind, id, &body_of(req)) {
            Ok(v) => json_response(200, &v),
            Err(e) => err_response(e),
        },
        ("DELETE", Some(id)) => match st.delete(kind, id) {
            Ok(Deletion::Gone) => ResponseTemplate::new(204),
            Ok(Deletion::Requested) => json_response(
                202,
                &serde_json::json!({ "detail": "deletion_requested" }),
            ),
            Err(e) => err_response(e),
        },
        _ => err_response(ApiError::not_found()),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test live fake:: -- --nocapture`
Expected: PASS — the 5 new transport tests plus Task 1's.

If `reqwest::get` is unavailable in the test, use
`RossumClient`-independent access via `reqwest::Client::new().get(url).send()`;
`reqwest` is a regular dependency and available to integration tests.

- [ ] **Step 5: Commit**

```bash
git add tests/live/support/fake/mod.rs
git commit -m "test(fake): serve the object store over HTTP

One wiremock catch-all routes (method, path) onto the store, checks the
token, and answers 404 outside /api/v1 — which is also how Data Storage
looks to an MDH-less org, the shape rdc already tolerates. Pending queue
deletes are charged one request per response.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: The seven core kinds through the real typed client

Proves every response the seeder will receive deserializes into `crate::model::*`.

**Files:**
- Modify: `tests/live/support/fake/mod.rs` (tests only)

**Interfaces:**
- Consumes: Task 2's `FakeOrg`.
- Produces: nothing new — this task is a proof, and it is where a missing
  required field surfaces.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `tests/live/support/fake/mod.rs`:

```rust
    /// Every kind the round-trip manifest seeds, created through the same
    /// typed client `LiveClient::create` uses. A missing required field shows
    /// up here as a deserialization error naming the field.
    #[tokio::test]
    async fn every_core_kind_creates_and_deserializes() {
        let fake = FakeOrg::start().await;
        let c = client(&fake);
        let org = fake.state().org_url();

        let ws = c
            .create_workspace(&json!({ "name": "Main", "organization": org }), None)
            .await
            .expect("workspace");
        assert_eq!(ws.name, "Main");

        let schema = c
            .create_schema(
                &json!({ "name": "Invoices", "content": [{ "category": "section", "id": "header" }] }),
                None,
            )
            .await
            .expect("schema");
        assert_eq!(schema.content.len(), 1);

        let queue = c
            .create_queue(
                &json!({ "name": "Invoices", "workspace": ws.url, "schema": schema.url }),
                None,
            )
            .await
            .expect("queue");
        assert_eq!(queue.workspace.as_deref(), Some(ws.url.as_str()));

        let inbox = c
            .create_inbox(
                &json!({ "name": "Inbox", "email_prefix": "invoices", "queues": [queue.url] }),
                None,
            )
            .await
            .expect("inbox");
        assert_eq!(
            inbox.email, "invoices@fake.rossum.invalid",
            "email is server-assigned"
        );

        let hook = c
            .create_hook(
                &json!({
                    "name": "Validator",
                    "type": "function",
                    "events": ["annotation_content"],
                    "queues": [queue.url],
                    "config": { "runtime": "python3.12", "code": "pass\n" },
                }),
                None,
            )
            .await
            .expect("hook");
        assert_eq!(hook.hook_type, "function");

        let rule = c
            .create_rule(
                &json!({ "name": "Totals", "queues": [queue.url], "trigger_condition": "True\n" }),
                None,
            )
            .await
            .expect("rule");
        assert_eq!(rule.queues, vec![queue.url.clone()]);

        let label = c
            .create_label(&json!({ "name": "Priority", "organization": org, "color": "#ff0000" }), None)
            .await
            .expect("label");
        assert_eq!(label.name, "Priority");

        // A detail GET must work for every kind rdc fetches by id — schemas
        // above all, because the list omits `content`.
        assert_eq!(
            c.get_schema(schema.id, None).await.expect("get schema").content.len(),
            1
        );
    }
```

- [ ] **Step 2: Run it and read the failure**

Run: `cargo test --test live fake::tests::every_core_kind -- --nocapture`
Expected: PASS if Task 1's `defaults` table is complete. A FAIL here names the
exact missing field — add it to that kind's `defaults` fn in `kinds.rs`, not to
the test.

- [ ] **Step 3: Fix any gap the test names**

Only if step 2 failed: add the missing `ensure(...)` line to the offending
kind's `defaults` fn and re-run. Do not weaken the assertion.

- [ ] **Step 4: Commit**

```bash
git add tests/live/support/fake/mod.rs tests/live/support/fake/kinds.rs
git commit -m "test(fake): create every core kind through the typed client

The seeder creates objects through RossumClient's typed methods, so the
fake's responses must deserialize into crate::model::*. This is the test
that names a missing required field instead of letting it surface deep in
a pull.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Server-owned back-references

Creating a child changes its parents. A fake that skips this still *converges*
— it is self-consistent — while silently leaving `pull::queues::refresh_backrefs`
unexercised. Consistency is what convergence needs; fidelity is what
correctness needs.

**Files:**
- Modify: `tests/live/support/fake/state.rs`

**Interfaces:**
- Consumes: `OrgState`.
- Produces: `OrgState::relink(&mut self, kind: &'static str, id: u64)` —
  private; observable only through `create`/`patch`/`delete`.

- [ ] **Step 1: Write the failing tests**

Append to `state.rs`'s `tests` module:

```rust
    fn seeded_graph(s: &mut OrgState) -> (u64, u64, u64) {
        let ws = s.create("workspaces", json!({ "name": "Main" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "Invoices" })).unwrap();
        let q = s
            .create(
                "queues",
                json!({ "name": "Invoices", "workspace": ws["url"], "schema": sc["url"] }),
            )
            .unwrap();
        (
            ws["id"].as_u64().unwrap(),
            sc["id"].as_u64().unwrap(),
            q["id"].as_u64().unwrap(),
        )
    }

    #[test]
    fn creating_a_queue_grows_its_workspace_and_schema() {
        let mut s = st();
        let (ws, sc, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        assert_eq!(s.get("workspaces", ws).unwrap()["queues"], json!([q_url]));
        assert_eq!(
            s.get("schemas", sc).unwrap()["queues"],
            json!([q_url]),
            "schema.queues gains the queue on create (pull/queues.rs refresh_backrefs)"
        );
    }

    #[test]
    fn creating_an_inbox_sets_its_queues_inbox() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let inbox = s
            .create(
                "inboxes",
                json!({ "name": "In", "email_prefix": "p", "queues": [s.url("queues", q)] }),
            )
            .unwrap();
        assert_eq!(s.get("queues", q).unwrap()["inbox"], inbox["url"]);
    }

    #[test]
    fn creating_a_hook_or_rule_grows_its_queues_back_ref() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        let hook = s
            .create("hooks", json!({ "name": "H", "queues": [q_url.clone()] }))
            .unwrap();
        let rule = s
            .create("rules", json!({ "name": "R", "queues": [q_url.clone()] }))
            .unwrap();
        assert_eq!(s.get("queues", q).unwrap()["hooks"], json!([hook["url"]]));
        assert_eq!(s.get("queues", q).unwrap()["rules"], json!([rule["url"]]));
    }

    #[test]
    fn deleting_a_child_shrinks_the_back_ref() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        let hook = s
            .create("hooks", json!({ "name": "H", "queues": [q_url] }))
            .unwrap();
        s.delete("hooks", hook["id"].as_u64().unwrap()).unwrap();
        assert_eq!(s.get("queues", q).unwrap()["hooks"], json!([]));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test live fake::state -- --nocapture`
Expected: FAIL — back-refs stay `[]`.

- [ ] **Step 3: Implement the linking**

Add to `impl OrgState` in `state.rs`, and call `self.relink(kind, id)` at the
end of `create` and `patch`, and `self.unlink(kind, id)` at the start of
`delete` (and inside `tick_deletions` before removing a queue):

```rust
    /// Push `child_url` into `parent.<field>` if it is not already there.
    fn add_ref(&mut self, parent_kind: &'static str, parent_url: &str, field: &str, child_url: &str) {
        let Some(id) = parent_url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        let Some(parent) = self.objects.get_mut(parent_kind).and_then(|m| m.get_mut(&id)) else {
            return;
        };
        let Some(obj) = parent.as_object_mut() else { return };
        let arr = obj
            .entry(field.to_string())
            .or_insert_with(|| json!([]));
        if let Some(list) = arr.as_array_mut() {
            let v = json!(child_url);
            if !list.contains(&v) {
                list.push(v);
            }
        }
    }

    fn remove_ref(&mut self, parent_kind: &'static str, parent_url: &str, field: &str, child_url: &str) {
        let Some(id) = parent_url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        let Some(parent) = self.objects.get_mut(parent_kind).and_then(|m| m.get_mut(&id)) else {
            return;
        };
        if let Some(list) = parent.get_mut(field).and_then(|v| v.as_array_mut()) {
            list.retain(|v| v != &json!(child_url));
        }
    }

    fn set_field(&mut self, kind: &'static str, url: &str, field: &str, value: Value) {
        let Some(id) = url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        if let Some(obj) = self
            .objects
            .get_mut(kind)
            .and_then(|m| m.get_mut(&id))
            .and_then(|v| v.as_object_mut())
        {
            obj.insert(field.to_string(), value);
        }
    }

    /// Grow every back-reference this object's own refs imply. The real API
    /// maintains these server-side; `pull::queues::refresh_backrefs` exists
    /// because they change under rdc's feet.
    fn relink(&mut self, kind: &'static str, id: u64) {
        let Some(me) = self.get(kind, id) else { return };
        let my_url = self.url(kind, id);
        match kind {
            "queues" => {
                if let Some(ws) = me.get("workspace").and_then(|v| v.as_str()) {
                    self.add_ref("workspaces", ws, "queues", &my_url);
                }
                if let Some(sc) = me.get("schema").and_then(|v| v.as_str()) {
                    self.add_ref("schemas", sc, "queues", &my_url);
                }
            }
            "inboxes" => {
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let Some(q) = q.as_str() {
                        self.set_field("queues", q, "inbox", json!(my_url));
                    }
                }
            }
            "hooks" | "rules" => {
                let field = if kind == "hooks" { "hooks" } else { "rules" };
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let Some(q) = q.as_str() {
                        self.add_ref("queues", q, field, &my_url);
                    }
                }
            }
            _ => {}
        }
    }

    /// The inverse, so a delete leaves no dangling back-reference.
    fn unlink(&mut self, kind: &'static str, id: u64) {
        let Some(me) = self.get(kind, id) else { return };
        let my_url = self.url(kind, id);
        match kind {
            "queues" => {
                if let Some(ws) = me.get("workspace").and_then(|v| v.as_str()) {
                    self.remove_ref("workspaces", ws, "queues", &my_url);
                }
                if let Some(sc) = me.get("schema").and_then(|v| v.as_str()) {
                    self.remove_ref("schemas", sc, "queues", &my_url);
                }
            }
            "inboxes" => {
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let Some(q) = q.as_str() {
                        self.set_field("queues", q, "inbox", Value::Null);
                    }
                }
            }
            "hooks" | "rules" => {
                let field = if kind == "hooks" { "hooks" } else { "rules" };
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let Some(q) = q.as_str() {
                        self.remove_ref("queues", q, field, &my_url);
                    }
                }
            }
            _ => {}
        }
    }
```

`&vec![]` in a `.unwrap_or` needs a binding to satisfy the borrow checker —
use `let empty = Vec::new();` above each loop and `.unwrap_or(&empty)`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test live fake:: -- --nocapture`
Expected: PASS, including Task 3's typed-client test (back-refs must not break
deserialization).

- [ ] **Step 5: Commit**

```bash
git add tests/live/support/fake/state.rs
git commit -m "test(fake): maintain the back-refs the server owns

A queue create grows workspace.queues and schema.queues; an inbox sets
queue.inbox; a hook or rule grows queue.hooks/rules. Skipping these would
still converge — the fake would just be self-consistent — and would leave
pull::queues::refresh_backrefs unexercised, which is the path that had the
two-pass bug.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `Quirks`, the queue's default email templates, and the provenance guard

**Files:**
- Create: `tests/live/support/fake/quirks.rs`
- Modify: `tests/live/support/fake/mod.rs` (`pub mod quirks;`)
- Modify: `tests/live/support/fake/state.rs` (call the quirk on queue create)

**Interfaces:**
- Consumes: `OrgState`.
- Produces:
  - `quirks::Quirk { name: &'static str, proven_by: &'static str }`
  - `quirks::QUIRKS: &[Quirk]`
  - `quirks::materialize_queue_defaults(st: &mut OrgState, queue_url: &str)`

- [ ] **Step 1: Write the failing tests**

`tests/live/support/fake/quirks.rs`, tests module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::fake::state::{ListQuery, OrgState};
    use serde_json::json;

    #[test]
    fn a_new_queue_gets_the_servers_typed_defaults() {
        let mut s = OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1);
        let q = s.create("queues", json!({ "name": "Invoices" })).unwrap();
        let listed = s.list("email_templates", &ListQuery { page: 1, page_size: 100 });
        let names: Vec<&str> = listed["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "Annotation status change - confirmed",
                "Annotation status change - exported",
                "Annotation status change - received",
                "Default rejection template",
                "Email with no processable attachments",
            ]
        );
        for t in listed["results"].as_array().unwrap() {
            assert_eq!(t["queue"], q["url"], "each default belongs to the queue");
        }
    }

    #[test]
    fn the_unique_typed_defaults_carry_their_types() {
        let mut s = OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1);
        s.create("queues", json!({ "name": "Invoices" })).unwrap();
        let listed = s.list("email_templates", &ListQuery { page: 1, page_size: 100 });
        let types: Vec<&str> = listed["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                "custom",
                "custom",
                "custom",
                "rejection_default",
                "email_with_no_processable_attachments",
            ]
        );
    }

    /// A quirk nobody can prove against a real org is a quirk someone
    /// invented. Written in the same spirit as `tests/command_references.rs`:
    /// the check is mechanical so the citation cannot rot silently.
    #[test]
    fn every_quirk_names_a_live_scenario_that_proves_it() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live/scenarios");
        let mut sources = String::new();
        for entry in std::fs::read_dir(&root).expect("scenarios dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                sources.push_str(&std::fs::read_to_string(&path).expect("read scenario"));
            }
        }
        for q in QUIRKS {
            let (file, test) = q
                .proven_by
                .split_once("::")
                .unwrap_or_else(|| panic!("quirk '{}' has a malformed citation: {}", q.name, q.proven_by));
            assert!(
                root.join(file.rsplit('/').next().expect("file name")).exists(),
                "quirk '{}' cites a scenario file that does not exist: {file}",
                q.name
            );
            assert!(
                sources.contains(&format!("fn {test}(")),
                "quirk '{}' cites '{test}', which no scenario defines",
                q.name
            );
        }
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test live fake::quirks -- --nocapture`
Expected: FAIL — `quirks.rs` does not exist.

- [ ] **Step 3: Write `quirks.rs`**

```rust
//! The learned-facts layer: behaviors of the real Rossum API that `rdc` had to
//! discover in a live org, expressed once, executably.
//!
//! Every entry in [`QUIRKS`] carries the live scenario that proves it, and
//! `every_quirk_names_a_live_scenario_that_proves_it` enforces the citation.
//! The rule this encodes: the fake invents nothing. When a `fake_*` test and
//! its `live_*` twin disagree, exactly one of two things is true — the model
//! here is wrong, or `rdc` is wrong. Weakening the scenario is not a third
//! option.

use serde_json::{json, Value};

use super::state::OrgState;

pub struct Quirk {
    pub name: &'static str,
    /// `<scenario file>::<test fn>`.
    pub proven_by: &'static str,
}

pub const QUIRKS: &[Quirk] = &[
    Quirk {
        name: "queue_create_materializes_typed_email_template_defaults",
        proven_by: "email_templates.rs::live_email_templates_round_trip",
    },
    Quirk {
        name: "queue_delete_is_async_and_cascades",
        proven_by: "conflicts_deletes.rs::live_conflicts_deletes",
    },
    Quirk {
        name: "engine_delete_refused_while_a_queue_awaits_deletion",
        proven_by: "ordering.rs::live_push_create_ordering",
    },
    Quirk {
        name: "unresolvable_ref_is_an_invalid_hyperlink",
        proven_by: "cross_refs.rs::live_cross_refs",
    },
    Quirk {
        name: "over_length_field_is_refused_after_trailing_whitespace_trim",
        proven_by: "server_truth.rs::live_field_limits_match_the_server",
    },
    Quirk {
        name: "queue_carries_one_engine_slot_only",
        proven_by: "server_truth.rs::live_queue_engine_slot_counts_values_not_keys",
    },
];

/// The five typed defaults `POST /queues` materializes.
///
/// Names, types and subjects are the ones recorded in
/// `testdata/live/snapshot/**/email-templates/` — the five files with no
/// run-id prefix, i.e. the ones the harness never seeded. Three are `custom`;
/// only `rejection_default` and `email_with_no_processable_attachments` are
/// unique-typed, which is what makes the adopt-by-type path in
/// `push::email_templates` meaningful.
const QUEUE_DEFAULT_TEMPLATES: &[(&str, &str, &str)] = &[
    ("Annotation status change - confirmed", "custom", "Document confirmed"),
    ("Annotation status change - exported", "custom", "Document exported"),
    ("Annotation status change - received", "custom", "Document received"),
    ("Default rejection template", "rejection_default", "Document rejected"),
    (
        "Email with no processable attachments",
        "email_with_no_processable_attachments",
        "No processable documents",
    ),
];

/// Quirk `queue_create_materializes_typed_email_template_defaults`.
///
/// `POST /queues` on the real API creates these server-side; a blind POST of
/// one afterwards is refused (`src/cli/push/email_templates.rs:97`).
pub fn materialize_queue_defaults(st: &mut OrgState, queue_url: &str) {
    for (name, ty, subject) in QUEUE_DEFAULT_TEMPLATES {
        let body: Value = json!({
            "name": name,
            "type": ty,
            "subject": subject,
            "message": "<p>Fake default.</p>",
            "queue": queue_url,
            "automate": false,
        });
        // `create` here is the store's own path, so ids stay monotonic.
        let _ = st.create("email_templates", body);
    }
}
```

- [ ] **Step 4: Hook it into queue creation**

In `state.rs`'s `create`, after `self.relink(kind, id);`:

```rust
        if kind == "queues" {
            let url = self.url("queues", id);
            super::quirks::materialize_queue_defaults(self, &url);
        }
```

And add `pub mod quirks;` to `tests/live/support/fake/mod.rs`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --test live fake:: -- --nocapture`
Expected: PASS. If the provenance guard fails on a citation, fix the citation
to name a scenario test that actually exists — do not delete the quirk.

- [ ] **Step 6: Commit**

```bash
git add tests/live/support/fake
git commit -m "test(fake): materialize a queue's typed email-template defaults

Plus the quirk registry and the guard that refuses a quirk no live
scenario proves. The five names/types/subjects are the unprefixed files in
testdata/live/snapshot — the ones the harness never seeded, i.e. the
server's own.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Async queue delete, the cascade, and the engine guard

**Files:**
- Modify: `tests/live/support/fake/state.rs`
- Create: `tests/live/support/fake/validate.rs`
- Modify: `tests/live/support/fake/mod.rs` (`pub mod validate;`)

**Interfaces:**
- Consumes: `OrgState`, `ApiError`.
- Produces: `validate::on_delete(st: &OrgState, kind: &'static str, id: u64) -> Result<(), ApiError>`

- [ ] **Step 1: Write the failing tests**

Append to `state.rs`'s `tests` module:

```rust
    #[test]
    fn a_queue_delete_is_requested_then_takes_effect_one_request_later() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        assert_eq!(s.delete("queues", q).unwrap(), Deletion::Requested);
        // Still there, exactly as `202 deletion_requested` promises.
        assert!(s.get("queues", q).is_some());
        s.tick_deletions();
        assert!(s.get("queues", q).is_none(), "gone after one more request");
    }

    #[test]
    fn a_queue_delete_cascades_to_its_templates_and_inbox() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        s.create("inboxes", json!({ "name": "In", "email_prefix": "p", "queues": [q_url] }))
            .unwrap();
        assert_eq!(s.ids("email_templates").len(), 5);
        assert_eq!(s.ids("inboxes").len(), 1);
        s.delete("queues", q).unwrap();
        s.tick_deletions();
        assert!(s.ids("email_templates").is_empty(), "templates go with the queue");
        assert!(s.ids("inboxes").is_empty(), "so does the inbox");
    }

    #[test]
    fn a_cascaded_queue_delete_leaves_its_schema_for_the_caller() {
        // Teardown deletes schemas explicitly, with a retry, because the
        // schema outlives the queue's purge (`tests/live/support/teardown.rs:38`).
        let mut s = st();
        let (_, sc, q) = seeded_graph(&mut s);
        s.delete("queues", q).unwrap();
        s.tick_deletions();
        assert!(s.get("schemas", sc).is_some());
        assert_eq!(s.delete("schemas", sc).unwrap(), Deletion::Gone);
    }

    #[test]
    fn an_engine_cannot_be_deleted_while_a_queue_awaits_deletion() {
        let mut s = st();
        let engine = s.create("engines", json!({ "name": "E" })).unwrap();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "S" })).unwrap();
        let q = s
            .create(
                "queues",
                json!({
                    "name": "Q",
                    "workspace": ws["url"],
                    "schema": sc["url"],
                    "engine": engine["url"],
                }),
            )
            .unwrap();
        s.delete("queues", q["id"].as_u64().unwrap()).unwrap();
        let err = s
            .delete("engines", engine["id"].as_u64().unwrap())
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
        assert!(
            format!("{:?}", err.body).contains("engine_attached_to_queues_waiting_for_deletion"),
            "wrong body: {:?}",
            err.body
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test live fake::state -- --nocapture`
Expected: FAIL on the cascade and the engine guard.

- [ ] **Step 3: Write `validate.rs`**

```rust
//! The fake's rejections.
//!
//! Strict on purpose: a mid-run 400 that wedges a real env is one of the three
//! symptoms this whole exercise exists to make reproducible offline. A
//! permissive fake would model the state and miss the failure.

use serde_json::Value;

use super::state::{ApiError, OrgState};

/// Refuse a delete the real API refuses.
pub fn on_delete(st: &OrgState, kind: &'static str, id: u64) -> Result<(), ApiError> {
    if kind == "engines" {
        let engine_url = st.url("engines", id);
        if st.queues_awaiting_deletion().iter().any(|q| {
            q.get("engine").and_then(Value::as_str) == Some(engine_url.as_str())
        }) {
            // "after up to 24 hours" with no unbind escape hatch — see
            // `tests/live/support/teardown.rs:62`.
            return Err(ApiError::bad_request(
                "engine_attached_to_queues_waiting_for_deletion",
            ));
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Wire the cascade and the guard into `state.rs`**

Add to `impl OrgState`:

```rust
    /// Queues that have answered `202` but not yet vanished.
    pub fn queues_awaiting_deletion(&self) -> Vec<Value> {
        self.pending_delete
            .keys()
            .filter_map(|id| self.get("queues", *id))
            .collect()
    }

    /// Remove a queue and everything the server removes with it: its
    /// auto-created email templates and its inbox. The SCHEMA survives —
    /// teardown deletes it explicitly, and needs a retry precisely because it
    /// outlives the queue's purge.
    fn cascade_queue_delete(&mut self, queue_id: u64) {
        let queue_url = self.url("queues", queue_id);
        let doomed_templates: Vec<u64> = self
            .objects
            .get("email_templates")
            .map(|m| {
                m.iter()
                    .filter(|(_, t)| t.get("queue").and_then(Value::as_str) == Some(queue_url.as_str()))
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default();
        let doomed_inboxes: Vec<u64> = self
            .objects
            .get("inboxes")
            .map(|m| {
                m.iter()
                    .filter(|(_, i)| {
                        i.get("queues")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().any(|q| q.as_str() == Some(queue_url.as_str())))
                            .unwrap_or(false)
                    })
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default();
        for id in doomed_templates {
            self.objects.get_mut("email_templates").map(|m| m.remove(&id));
        }
        for id in doomed_inboxes {
            self.objects.get_mut("inboxes").map(|m| m.remove(&id));
        }
        self.unlink("queues", queue_id);
        self.objects.get_mut("queues").map(|m| m.remove(&queue_id));
    }
```

Change `tick_deletions` to call `self.cascade_queue_delete(id)` instead of the
bare `remove`, and change `delete` to consult the validator first:

```rust
    pub fn delete(&mut self, kind: &'static str, id: u64) -> Result<Deletion, ApiError> {
        kinds::spec(kind).ok_or_else(ApiError::not_found)?;
        if self.get(kind, id).is_none() {
            return Err(ApiError::not_found());
        }
        super::validate::on_delete(self, kind, id)?;
        if kind == "queues" {
            self.pending_delete.insert(id, 1);
            return Ok(Deletion::Requested);
        }
        self.unlink(kind, id);
        self.objects.get_mut(kind).expect("kind present").remove(&id);
        Ok(Deletion::Gone)
    }
```

Add `pub mod validate;` to `tests/live/support/fake/mod.rs`.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --test live fake:: -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add tests/live/support/fake
git commit -m "test(fake): model the async queue delete and its cascade

DELETE /queues answers 202 and the queue survives exactly one more
request, then goes with its auto-created templates and its inbox. The
schema stays, which is why teardown deletes it separately with a retry.
An engine bound to a queue awaiting deletion is refused, as the real API
refuses it for up to 24 hours.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: The remaining strict rejections

**Files:**
- Modify: `tests/live/support/fake/validate.rs`
- Modify: `tests/live/support/fake/state.rs` (call `validate::on_write`)

**Interfaces:**
- Consumes: `OrgState`, `ApiError`.
- Produces: `validate::on_write(st: &OrgState, kind: &'static str, body: &Value) -> Result<(), ApiError>`

- [ ] **Step 1: Write the failing tests**

Append to `state.rs`'s `tests` module:

```rust
    #[test]
    fn a_ref_that_matches_no_object_is_an_invalid_hyperlink() {
        let mut s = st();
        let err = s
            .create(
                "queues",
                json!({ "name": "Q", "schema": "http://127.0.0.1:9/api/v1/schemas/404" }),
            )
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
        assert!(
            format!("{:?}", err.body).contains("Invalid hyperlink - No URL match"),
            "wrong body: {:?}",
            err.body
        );
    }

    #[test]
    fn a_queue_needs_a_schema() {
        let mut s = st();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let err = s
            .create("queues", json!({ "name": "Q", "workspace": ws["url"] }))
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
    }

    #[test]
    fn a_queue_carries_one_engine_slot_at_most() {
        let mut s = st();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "S" })).unwrap();
        let e1 = s.create("engines", json!({ "name": "E" })).unwrap();
        let err = s
            .create(
                "queues",
                json!({
                    "name": "Q",
                    "workspace": ws["url"],
                    "schema": sc["url"],
                    "engine": e1["url"],
                    "generic_engine": e1["url"],
                }),
            )
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
    }

    #[test]
    fn an_over_length_field_is_refused_after_a_trailing_whitespace_trim() {
        let mut s = st();
        // A hook description is capped; the server trims trailing whitespace
        // BEFORE validating, so a value that fits once trimmed is accepted.
        let cap = super::validate::MAX_HOOK_DESCRIPTION;
        let fits = format!("{}{}", "x".repeat(cap), "   ");
        assert!(s
            .create("hooks", json!({ "name": "H", "description": fits }))
            .is_ok());
        let over = "x".repeat(cap + 1);
        let err = s
            .create("hooks", json!({ "name": "H2", "description": over }))
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
    }

    #[test]
    fn a_queue_is_refused_when_its_schema_outruns_the_bound_engine() {
        // The refusal that forced `push_classified` to create engines and
        // their fields BEFORE queues (`src/cli/push/mod.rs:88-96`). An engine
        // field's `name` is matched against a schema datapoint's `id` — see
        // the seeded pair in `testdata/live/snapshot/engines/**`.
        let mut s = st();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let engine = s.create("engines", json!({ "name": "E" })).unwrap();
        s.create(
            "engine_fields",
            json!({ "name": "invoice_id", "engine": engine["url"] }),
        )
        .unwrap();
        let sc = s
            .create(
                "schemas",
                json!({
                    "name": "S",
                    "content": [{
                        "category": "section",
                        "id": "header",
                        "children": [
                            { "category": "datapoint", "id": "invoice_id", "type": "string" },
                            { "category": "datapoint", "id": "amount_due", "type": "number" },
                        ],
                    }],
                }),
            )
            .unwrap();
        let err = s
            .create(
                "queues",
                json!({
                    "name": "Q",
                    "workspace": ws["url"],
                    "schema": sc["url"],
                    "engine": engine["url"],
                }),
            )
            .expect_err("amount_due has no engine field");
        assert_eq!(err.status, 400);
        assert!(
            format!("{:?}", err.body).contains("amount_due"),
            "the refusal must name the offending field: {:?}",
            err.body
        );

        // Add the missing field and the same create succeeds.
        s.create(
            "engine_fields",
            json!({ "name": "amount_due", "engine": engine["url"] }),
        )
        .unwrap();
        assert!(s
            .create(
                "queues",
                json!({
                    "name": "Q2",
                    "workspace": ws["url"],
                    "schema": sc["url"],
                    "engine": engine["url"],
                })
            )
            .is_ok());
    }

    #[test]
    fn a_blind_post_of_an_auto_created_typed_default_is_refused() {
        let mut s = st();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "S" })).unwrap();
        let q = s
            .create("queues", json!({ "name": "Q", "workspace": ws["url"], "schema": sc["url"] }))
            .unwrap();
        let err = s
            .create(
                "email_templates",
                json!({ "name": "Mine", "type": "rejection_default", "queue": q["url"] }),
            )
            .expect_err("the queue already has one");
        assert_eq!(err.status, 400);
        // `custom` is not unique-typed, so a second one is fine.
        assert!(s
            .create(
                "email_templates",
                json!({ "name": "Mine", "type": "custom", "queue": q["url"] })
            )
            .is_ok());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test live fake::state -- --nocapture`
Expected: FAIL — every create is currently accepted.

- [ ] **Step 3: Extend `validate.rs`**

```rust
/// A hook `description`'s cap. Kept here rather than imported from
/// `snapshot::limits` on purpose: the fake stands in for the SERVER, and
/// pinning the value independently is what lets
/// `live_field_limits_match_the_server` catch the two drifting apart.
pub const MAX_HOOK_DESCRIPTION: usize = 500;

/// Types a queue may hold exactly one of.
const UNIQUE_TEMPLATE_TYPES: &[&str] = &["rejection_default", "email_with_no_processable_attachments"];

/// Fields whose value is a url that must resolve to a live object.
const REF_FIELDS: &[(&str, &str)] = &[
    ("queues", "workspace"),
    ("queues", "schema"),
    ("queues", "engine"),
    ("queues", "generic_engine"),
    ("email_templates", "queue"),
];

pub fn on_write(st: &OrgState, kind: &'static str, body: &Value) -> Result<(), ApiError> {
    // 1. Every ref must resolve. This is the refusal rdc's whole
    //    deferred-relink path is built around (`src/snapshot/refs.rs:159`).
    for (k, field) in REF_FIELDS {
        if *k != kind {
            continue;
        }
        if let Some(url) = body.get(*field).and_then(Value::as_str)
            && !st.resolves(url)
        {
            return Err(ApiError::bad_request("Invalid hyperlink - No URL match"));
        }
    }
    for field in ["queues", "run_after"] {
        if let Some(list) = body.get(field).and_then(Value::as_array) {
            for v in list {
                if let Some(url) = v.as_str()
                    && !st.resolves(url)
                {
                    return Err(ApiError::bad_request("Invalid hyperlink - No URL match"));
                }
            }
        }
    }

    // 2. A queue is created with its schema, or not at all.
    if kind == "queues" {
        if body.get("schema").and_then(Value::as_str).is_none() {
            return Err(ApiError::bad_request("schema: This field is required."));
        }
        let slots = ["engine", "generic_engine"]
            .iter()
            .filter(|f| body.get(**f).map(|v| !v.is_null()).unwrap_or(false))
            .count();
        if slots > 1 {
            return Err(ApiError::non_field(
                "Only one of engine, generic_engine may be set.",
            ));
        }
    }

    // 3. Length caps, measured after the trailing-whitespace trim the server
    //    applies before validating.
    if let Some(d) = body.get("description").and_then(Value::as_str)
        && d.trim_end().chars().count() > MAX_HOOK_DESCRIPTION
    {
        return Err(ApiError::bad_request(format!(
            "description: Ensure this field has no more than {MAX_HOOK_DESCRIPTION} characters."
        )));
    }

    // 4. A queue holds one template of each unique type, and it already has
    //    the ones the server made (`src/cli/push/email_templates.rs:97`).
    if kind == "email_templates"
        && let Some(ty) = body.get("type").and_then(Value::as_str)
        && UNIQUE_TEMPLATE_TYPES.contains(&ty)
        && let Some(queue) = body.get("queue").and_then(Value::as_str)
        && st.has_template_of_type(queue, ty)
    {
        return Err(ApiError::bad_request(format!(
            "type: a '{ty}' template already exists on this queue."
        )));
    }

    // 5. `POST /queues` validates the queue's schema against the bound
    //    engine's field NAMES. This is the refusal that forced push to create
    //    engines and their fields BEFORE queues
    //    (`src/cli/push/mod.rs:88-96`), and it bites only when the engine
    //    already exists in the target.
    if kind == "queues"
        && let Some(engine_url) = body.get("engine").and_then(Value::as_str)
        && let Some(schema_url) = body.get("schema").and_then(Value::as_str)
    {
        let known = st.engine_field_names(engine_url);
        let mut extracted = Vec::new();
        if let Some(schema) = st.get_by_url(schema_url) {
            extracted_field_ids(schema.get("content").unwrap_or(&Value::Null), &mut extracted);
        }
        if let Some(missing) = extracted.iter().find(|f| !known.contains(*f)) {
            let engine_id = engine_url.rsplit('/').next().unwrap_or("?");
            return Err(ApiError::non_field(format!(
                "Engine (id: {engine_id}) restriction: extracted field \
                 '{missing}' is not present among names of engine fields"
            )));
        }
    }

    Ok(())
}

/// Every datapoint `id` a schema's content tree extracts.
///
/// Walks the same shape `snapshot::schema::extract_formulas` walks:
/// `children` is an ARRAY for sections and tuples but a single OBJECT for a
/// multivalue's element schema, so descending into both is what covers
/// line-item columns rather than only top-level datapoints.
fn extracted_field_ids(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Array(items) => {
            for item in items {
                extracted_field_ids(item, out);
            }
        }
        Value::Object(o) => {
            if o.get("category").and_then(Value::as_str) == Some("datapoint")
                && let Some(id) = o.get("id").and_then(Value::as_str)
            {
                out.push(id.to_string());
            }
            if let Some(children) = o.get("children") {
                extracted_field_ids(children, out);
            }
        }
        _ => {}
    }
}
```

Add these lookups to `impl OrgState`:

```rust
    /// Whether `url` names an object this org holds.
    pub fn resolves(&self, url: &str) -> bool {
        let mut parts = url.trim_end_matches('/').rsplit('/');
        let Some(id) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            return false;
        };
        let Some(kind) = parts.next() else { return false };
        if kind == "organizations" {
            return id == self.org_id;
        }
        kinds::spec(kind)
            .and_then(|k| self.objects.get(k.path))
            .map(|m| m.contains_key(&id))
            .unwrap_or(false)
    }

    /// Any object addressed by its url, whatever its kind.
    pub fn get_by_url(&self, url: &str) -> Option<Value> {
        let mut parts = url.trim_end_matches('/').rsplit('/');
        let id = parts.next()?.parse::<u64>().ok()?;
        let kind = kinds::spec(parts.next()?)?.path;
        self.get(kind, id)
    }

    /// The `name` of every engine field bound to `engine_url`. An engine
    /// field's name is what a schema datapoint's `id` is matched against.
    pub fn engine_field_names(&self, engine_url: &str) -> Vec<String> {
        self.objects
            .get("engine_fields")
            .map(|m| {
                m.values()
                    .filter(|f| f.get("engine").and_then(Value::as_str) == Some(engine_url))
                    .filter_map(|f| f.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn has_template_of_type(&self, queue_url: &str, ty: &str) -> bool {
        self.objects
            .get("email_templates")
            .map(|m| {
                m.values().any(|t| {
                    t.get("queue").and_then(Value::as_str) == Some(queue_url)
                        && t.get("type").and_then(Value::as_str) == Some(ty)
                })
            })
            .unwrap_or(false)
    }
```

- [ ] **Step 3b: Split `create` so the server's own writes bypass validation**

`materialize_queue_defaults` must NOT go through the check — the typed-default
rule would refuse the very templates it exists to compare against. Split the
store's create in `state.rs`:

```rust
    pub fn create(&mut self, kind: &'static str, body: Value) -> Result<Value, ApiError> {
        // Before an id is allocated: a refused create must not consume one,
        // or the ids stop matching creation order and the round_trip golden
        // stops being reproducible.
        super::validate::on_write(self, kind, &body)?;
        self.create_unchecked(kind, body)
    }

    /// The store half of [`Self::create`], with no validation.
    ///
    /// `quirks` materializes the server's OWN objects through this: they are
    /// created by the server, not POSTed by a client, so the checks a client
    /// POST answers to do not apply — and the unique-typed-template rule
    /// would refuse the very defaults it exists to compare against.
    pub(super) fn create_unchecked(
        &mut self,
        kind: &'static str,
        mut body: Value,
    ) -> Result<Value, ApiError> {
        // ...the body `create` had in Task 1, unchanged...
    }
```

Then change `quirks::materialize_queue_defaults` to call
`st.create_unchecked("email_templates", body)` instead of `st.create(...)`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test live fake:: -- --nocapture`
Expected: PASS. Task 3's typed-client test must still pass — it creates a queue
WITH a schema, so rule 2 does not bite it.

- [ ] **Step 5: Commit**

```bash
git add tests/live/support/fake
git commit -m "test(fake): reject what the server rejects

Unresolvable refs, a queue with no schema, both engine slots set, an
over-length field measured after the server's trailing-whitespace trim,
and a blind POST of a typed default the queue already has. Each mirrors a
refusal rdc learned the hard way; the caps are pinned here independently
of snapshot::limits so live_field_limits_match_the_server can catch the
two drifting apart.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Seed the manifest through the fake

**Files:**
- Modify: `tests/live/support/fake/mod.rs` (tests only)

**Interfaces:**
- Consumes: `FakeOrg`, `support::seeder::seed`, `support::client::LiveClient`,
  `support::staticdir::{load_manifest, static_dir}`, `support::run_id::RunId`.
- Produces: nothing new. This proves the real seeder works against the fake and
  pins the creation order the golden depends on.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `tests/live/support/fake/mod.rs`:

```rust
    /// The real seeder, the real manifest, the real typed client — against the
    /// fake. Also pins the creation ORDER, because
    /// `testdata/live/expected/round_trip.toml` records the secondary
    /// workspace/schema/queue winning the bare slugs, and that outcome follows
    /// from `Manifest::topo_order` plus monotonic ids.
    #[tokio::test]
    async fn the_manifest_seeds_against_the_fake_in_topo_order() {
        use crate::support::client::LiveClient;
        use crate::support::run_id::RunId;
        use crate::support::seeder::seed;
        use crate::support::staticdir::{load_manifest, static_dir};

        let fake = FakeOrg::start().await;
        let cfg = fake.config();
        let client = LiveClient::connect(&cfg).expect("connect");
        let run_id = RunId::new();
        let manifest = load_manifest().expect("manifest");
        let index = seed(&client, &run_id, &static_dir(), &manifest)
            .await
            .expect("seed");

        // Eleven objects, every one addressable by its manifest key.
        for key in [
            "label-priority",
            "ws-main",
            "ws-secondary",
            "schema-invoices-main",
            "schema-invoices-secondary",
            "queue-invoices-main",
            "queue-invoices-secondary",
            "inbox-invoices-main",
            "hook-validator",
            "hook-post-validator",
            "rule-totals",
        ] {
            assert!(index.id(key).is_some(), "manifest key not seeded: {key}");
        }

        // The order the golden depends on: secondary before main.
        assert!(
            index.id("ws-secondary").unwrap() < index.id("ws-main").unwrap(),
            "ws-secondary must take the lower id"
        );
        assert!(
            index.id("schema-invoices-secondary").unwrap()
                < index.id("schema-invoices-main").unwrap()
        );
        assert!(
            index.id("queue-invoices-secondary").unwrap()
                < index.id("queue-invoices-main").unwrap()
        );

        // Two queues means ten server-made email templates.
        assert_eq!(fake.state().ids("email_templates").len(), 10);

        // The hook's code sidecar was inlined by the seeder and stored.
        let hook_id = index.id("hook-validator").unwrap();
        let hook = fake.state().get("hooks", hook_id).expect("hook");
        assert!(
            hook["config"]["code"].as_str().unwrap_or("").contains("def "),
            "the seeder inlines bodies/hooks/validator.py into config.code"
        );
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --test live fake::tests::the_manifest_seeds -- --nocapture`
Expected: PASS. A failure here is informative in its own right:
- an `Invalid hyperlink` refusal means the seeder's `@kind/key` placeholder
  resolution produced a url the fake does not hold — check `resolve_placeholders`;
- a deserialization error names a field to add to `kinds.rs`;
- an ordering assertion failing means id allocation is not monotonic across
  kinds. Fix the fake, not the assertion.

- [ ] **Step 3: Commit**

```bash
git add tests/live/support/fake/mod.rs
git commit -m "test(fake): seed the real manifest against the fake

The real seeder and typed client build all eleven objects, and the test
pins the creation order the round_trip golden depends on: topo_order gives
the secondary workspace/schema/queue the lower ids, which is why they win
the bare slugs.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: The first offline convergence assertion

The milestone. A seeded fake, a real `rdc sync`, and `assert_converged`.

**Files:**
- Modify: `tests/live/support/fake/mod.rs` (tests only)

**Interfaces:**
- Consumes: everything above, plus `support::project::ProjectFixture` and
  `support::converge::assert_converged`.
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `tests/live/support/fake/mod.rs`:

```rust
    /// The whole point of the exercise: a real `rdc sync` against a stateful
    /// backend, asserted to have settled. Before this existed, convergence
    /// could only be checked against a live org.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pull_from_the_fake_converges() {
        use crate::support::client::LiveClient;
        use crate::support::converge::assert_converged;
        use crate::support::project::ProjectFixture;
        use crate::support::run_id::RunId;
        use crate::support::seeder::seed;
        use crate::support::staticdir::{load_manifest, static_dir};

        let fake = FakeOrg::start().await;
        let cfg = fake.config();
        let client = LiveClient::connect(&cfg).expect("connect");
        let run_id = RunId::new();
        let manifest = load_manifest().expect("manifest");
        seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

        let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init");
        let out = project.run_rdc(&["sync", "test", "--no-push"]);
        assert!(
            out.status.success(),
            "sync --no-push failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        assert_converged(&project, "test", &run_id.list_prefix(), "after a pull from the fake");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --test live fake::tests::a_pull_from_the_fake_converges -- --nocapture`
Expected: PASS.

This is the step most likely to surface something real. Triage:
- **A pull error** naming an endpoint: add it to `kinds::MODELLED`.
- **A pull error** naming a field: add it to that kind's `defaults`.
- **`assert_converged` reporting a plan or byte drift:** do NOT adjust the
  fake to make it quiet. Read the report — it names the plan lines and the
  exact bytes that moved. Two possibilities, and they need different answers:
  1. the fake returns something no pull would produce (a field the real API
     does not send, a url shaped wrongly) — fix the fake;
  2. the fake is faithful and `rdc` does not settle — **that is a real rdc
     defect found offline, which is the entire purpose.** Write it up in the
     task report, leave the test failing, and stop for review rather than
     working around it.

- [ ] **Step 3: Commit**

```bash
git add tests/live/support/fake/mod.rs
git commit -m "test(fake): assert convergence offline for the first time

Seed the manifest into the fake, run a real rdc sync against it, and
assert_converged. Until now that assertion needed a live org and a token
with a thirty-minute life; it now runs in cargo test.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Port `round_trip_core`, then verify

**Files:**
- Modify: `tests/live/scenarios/round_trip.rs`

**Interfaces:**
- Consumes: `FakeOrg::config`.
- Produces: `scenarios::round_trip::round_trip_core(cfg: &LiveConfig)`, plus the
  `fake_round_trip_core` and `live_round_trip_core` wrappers.

- [ ] **Step 1: Extract the body**

In `tests/live/scenarios/round_trip.rs`, rename the existing
`live_round_trip_core` to `round_trip_core`, change its signature to take
`cfg: &LiveConfig`, delete its `#[tokio::test]`/`#[ignore]` attributes and its
`LiveConfig::from_env()` block, and replace uses of `cfg` accordingly. **Change
nothing else in the body** — not an assertion, not a comment.

- [ ] **Step 2: Add both wrappers**

At the top of the file's test items:

```rust
/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_round_trip_core() {
    let fake = crate::support::fake::FakeOrg::start().await;
    round_trip_core(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_round_trip_core() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    round_trip_core(&cfg).await;
}
```

- [ ] **Step 3: Run the fake twin**

Run: `cargo test --test live fake_round_trip_core -- --nocapture`
Expected: PASS, including `load_or_compare` against
`testdata/live/expected/round_trip.toml`.

If the golden mismatches, apply Task 9's triage. **Do not run with
`RDC_LIVE_CAPTURE=1`** — re-capturing the golden from the fake would overwrite
a fact captured from a real org with whatever the fake happens to do, which is
the one move that turns this whole design into theatre.

- [ ] **Step 4: Confirm the live set is unchanged**

Run: `cargo test --test live -- --ignored --list`
Expected: exactly the 23 `live_*` scenarios, `fake_*` absent from the list.

Run: `cargo test --test live -- --list | grep -c fake_`
Expected: `1` (`fake_round_trip_core`).

- [ ] **Step 5: Full verification**

Run: `cargo test --locked`
Expected: the previous 1660 pass, plus this stage's new tests, 0 failures.

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: clean.

Do NOT run `cargo fmt` over the repo — this checkout is not fmt-clean under the
local rustfmt and a repo-wide format would bury the change.

- [ ] **Step 6: Commit**

```bash
git add tests/live/scenarios/round_trip.rs
git commit -m "test(live): run round_trip_core against the fake as well

The body moves into a function over &LiveConfig and gains two wrappers:
fake_round_trip_core, which runs in cargo test, and live_round_trip_core,
which keeps its #[ignore] and env gate so --ignored still selects exactly
the live set. The scenario itself is untouched.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 7: Report**

Write a short summary covering:
- whether the golden matched first try;
- every gap the fake had to grow (endpoints, fields) and what named it;
- **any rdc defect the fake surfaced** — the headline result if there is one;
- the new `cargo test` wall-clock time, against the 25 s baseline;
- what stage 2 should port first.

---

## Out of scope for Stage 1

- Porting the other 15 scenarios (stage 2), including the two-org promotion
  ones. `FakeOrg::paired_config` exists and is tested, but nothing uses it yet.
- The per-kind convergence matrix and the `proptest` fuzzer (stage 3).
- MDH / Data Storage (stage 4). The fake answers those paths 404, which is how
  an MDH-less org looks.
- `POST /hooks/create` (the store-install path). Returns 404 today; stage 2
  adds it when `live_deploy_flow` needs it.
- Collapsing the write-back law, and per-object failure isolation in push.
  Both are named in the spec's out-of-scope section and both want this
  instrument to exist first.
