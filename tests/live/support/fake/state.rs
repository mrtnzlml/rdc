//! The fake organization's object graph.
//!
//! Pure data — no HTTP, nothing async. `FakeOrg` owns one of these behind a
//! `std::sync::Mutex`, because wiremock's responder closure is synchronous
//! (`wiremock::respond::Respond`, `F: Fn(&Request) -> ResponseTemplate`).

use serde_json::{json, Value};
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
    /// Not yet produced by anything the router routes to (a later stage's
    /// quirks/validation work).
    #[allow(dead_code)]
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

    /// `FakeOrg` keeps its own copy of the org id (so `creds()` need not lock
    /// the store) rather than calling this — kept for a caller that only
    /// holds an `OrgState`.
    #[allow(dead_code)]
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
        self.relink(kind, id);
        if kind == "queues" {
            let url = self.url("queues", id);
            super::quirks::materialize_queue_defaults(self, &url);
        }
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
        // `relink` only ever adds. Without unlinking against the PRE-merge
        // refs first, a patch that re-parents an object (e.g. a hook's
        // `queues` from [q1] to [q2], or a queue's `workspace` from A to B)
        // would leave the stale parent still pointing at it — a shape the
        // real API cannot produce. This round trip preserves MEMBERSHIP —
        // `add_ref` dedupes and `remove_ref`'s `retain` on an already-absent
        // entry does nothing — but NOT order: unlink-then-relink moves a
        // still-present ref to the end of the parent's array (e.g. patching
        // one of two hooks on a queue can turn `queue.hooks` from [H1, H2]
        // into [H2, H1]). That is deliberately not a property this fake
        // gives you: `snapshot::noise::sort_url_arrays` (routed through
        // `canonicalize_for_hash`, so every content hash and drift check
        // sees it) sorts any array whose elements are all refs — its
        // `is_url` helper accepts both `https://…` and portable
        // `rdc://<kind>/<slug>` forms precisely so these back-ref arrays
        // stay order-insensitive — because the real Rossum API's array
        // order is itself non-deterministic per env/endpoint. A fake that
        // kept insertion order here would be less faithful, not more.
        self.unlink(kind, id);
        let stamp = self.now();
        let result = {
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
            slot.clone()
        };
        self.relink(kind, id);
        Ok(result)
    }

    pub fn delete(&mut self, kind: &'static str, id: u64) -> Result<Deletion, ApiError> {
        kinds::spec(kind).ok_or_else(ApiError::not_found)?;
        if self.get(kind, id).is_none() {
            return Err(ApiError::not_found());
        }
        super::validate::on_delete(self, kind, id)?;
        // Unlinking happens here, before the 202 branch below, for a queue
        // exactly as for anything else — not premature. The real API itself
        // nulls a `deletion_requested` queue's `workspace` right away (the
        // null-out below mirrors that), so `workspace.queues` /
        // `schema.queues` must already be clean by the time anyone can next
        // observe either parent. See `src/cli/sync/mod.rs:861-864`.
        self.unlink(kind, id);
        if kind == "queues" {
            // `202 deletion_requested`: still listed for one more request,
            // but not unchanged. The real `GET /queues` answers with
            // `workspace: null`, `schema: null` and `status:
            // "deletion_requested"` for a queue in this state — see the
            // comment and fixture at `src/cli/sync/mod.rs:861-864` and
            // `:1934-1944`, and the regression this shape exists to keep
            // reproducible offline, `tests/cli_sync.rs:4388-4400` ("Bug #1":
            // seeding a workspace-less queue into the classifier's working
            // lockfile made a referencing hook re-pull forever). This is
            // repo-documented and offline-regression-tested, NOT
            // live-asserted — no live scenario checks that `workspace` /
            // `schema` go null, only that `status` becomes
            // `deletion_requested` (`ordering.rs:348`). Only these three
            // fields are touched: nothing is known either way about
            // `hooks`/`rules`/anything else on a draining queue.
            if let Some(obj) = self
                .objects
                .get_mut(kind)
                .and_then(|m| m.get_mut(&id))
                .and_then(|v| v.as_object_mut())
            {
                obj.insert("workspace".into(), Value::Null);
                obj.insert("schema".into(), Value::Null);
                obj.insert("status".into(), json!("deletion_requested"));
            }
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

    /// Charge every pending queue delete one request, unconditionally. What a
    /// caller driving `OrgState` directly wants — `delete()` sets a fresh
    /// entry's grace with NO tick attached, so a single explicit call here
    /// afterward is "one more request" and removes it. The router wants
    /// something narrower; see `tick_pending_from`.
    pub fn tick_deletions(&mut self) {
        self.tick_matching(|_id| true);
    }

    /// Ids on the grace clock right now. The router snapshots this BEFORE
    /// routing a request and hands it to `tick_pending_from` AFTER, so a
    /// request's own `delete()` call — if this request is one — never
    /// charges the grace it just created.
    pub fn pending_delete_ids(&self) -> Vec<u64> {
        self.pending_delete.keys().copied().collect()
    }

    /// Charge one request's worth of grace, but only to ids present in
    /// `before` (a snapshot from `pending_delete_ids`, taken before this
    /// request was routed). `tick_deletions` above charges everything
    /// unconditionally, which is right for a caller ticking by hand — but
    /// the router calls a tick once per HTTP request, and a queue `DELETE`
    /// inserts its own fresh entry INSIDE that same request's `route()`
    /// call. Charging that entry too would consume `delete()`'s single unit
    /// of grace immediately: the `202` response would be followed by ZERO
    /// further sightings, not the one `Deletion::Requested`'s doc comment
    /// promises. Caught by
    /// `a_deleted_queue_is_202_then_nulled_then_gone_over_http`
    /// (`tests/live/support/fake/mod.rs`) — a state-level test alone can't
    /// see this, because it never goes through the router's tick placement.
    /// An id NOT in `before` (i.e., inserted by the request just routed) is
    /// left untouched; it gets its first tick on the NEXT request instead.
    pub fn tick_pending_from(&mut self, before: &[u64]) {
        let keep: std::collections::BTreeSet<u64> = before.iter().copied().collect();
        self.tick_matching(|id| keep.contains(&id));
    }

    fn tick_matching(&mut self, keep: impl Fn(u64) -> bool) {
        let mut done = Vec::new();
        for (id, left) in self.pending_delete.iter_mut() {
            if !keep(*id) {
                continue;
            }
            match left.checked_sub(1) {
                Some(0) | None => done.push(*id),
                Some(n) => *left = n,
            }
        }
        for id in done {
            self.pending_delete.remove(&id);
            self.cascade_queue_delete(id);
        }
    }

    /// Queues that have answered `202` but not yet vanished.
    pub fn queues_awaiting_deletion(&self) -> Vec<Value> {
        self.pending_delete.keys().filter_map(|id| self.get("queues", *id)).collect()
    }

    /// Remove a queue and everything the server removes with it: its
    /// auto-created email templates and its inbox. The SCHEMA survives —
    /// teardown deletes it explicitly, and needs a retry precisely because it
    /// outlives the queue's purge (`tests/live/support/teardown.rs:38`).
    ///
    /// No `unlink("queues", queue_id)` call here: `delete()` already severed
    /// `workspace.queues` / `schema.queues` at request time, before this
    /// queue was nulled out and put on the grace clock — see the comment
    /// there.
    fn cascade_queue_delete(&mut self, queue_id: u64) {
        let queue_url = self.url("queues", queue_id);
        let doomed_templates: Vec<u64> = self
            .objects
            .get("email_templates")
            .map(|m| {
                m.iter()
                    .filter(|(_, t)| {
                        t.get("queue").and_then(Value::as_str) == Some(queue_url.as_str())
                    })
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
            self.objects.get_mut("email_templates").and_then(|m| m.remove(&id));
        }
        for id in doomed_inboxes {
            self.objects.get_mut("inboxes").and_then(|m| m.remove(&id));
        }
        self.objects.get_mut("queues").and_then(|m| m.remove(&queue_id));
    }

    pub fn ids(&self, kind: &str) -> Vec<u64> {
        self.objects
            .get(kind)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Push `child_url` into `parent.<field>` if it is not already there.
    fn add_ref(
        &mut self,
        parent_kind: &'static str,
        parent_url: &str,
        field: &str,
        child_url: &str,
    ) {
        let Some(id) = parent_url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        let Some(parent) = self.objects.get_mut(parent_kind).and_then(|m| m.get_mut(&id)) else {
            return;
        };
        let Some(obj) = parent.as_object_mut() else { return };
        let arr = obj.entry(field.to_string()).or_insert_with(|| json!([]));
        if let Some(list) = arr.as_array_mut() {
            let v = json!(child_url);
            if !list.contains(&v) {
                list.push(v);
            }
        }
    }

    /// The inverse of `add_ref`.
    fn remove_ref(
        &mut self,
        parent_kind: &'static str,
        parent_url: &str,
        field: &str,
        child_url: &str,
    ) {
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

    /// Set `<kind>/<id from url>.<field>` to `value`.
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

    /// The inverse of `set_field` — vacates the key entirely rather than
    /// setting it to `null`. Some Rossum fields (e.g. `queue.inbox`, see
    /// `src/model/queue.rs`) are `Option<T>` with
    /// `skip_serializing_if = "Option::is_none"` because the real API
    /// rejects `null` on PATCH ("This field may not be null."); the fake
    /// must vacate the key the same way the real server does, or a re-fetch
    /// would hand back a shape the real API never emits.
    fn remove_field(&mut self, kind: &'static str, url: &str, field: &str) {
        let Some(id) = url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        if let Some(obj) = self
            .objects
            .get_mut(kind)
            .and_then(|m| m.get_mut(&id))
            .and_then(|v| v.as_object_mut())
        {
            obj.remove(field);
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
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.set_field("queues", q, "inbox", json!(my_url));
                    }
                }
            }
            "hooks" | "rules" => {
                let field = if kind == "hooks" { "hooks" } else { "rules" };
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
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
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.remove_field("queues", q, "inbox");
                    }
                }
            }
            "hooks" | "rules" => {
                let field = if kind == "hooks" { "hooks" } else { "rules" };
                let empty = Vec::new();
                for q in me.get("queues").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    if let Some(q) = q.as_str() {
                        self.remove_ref("queues", q, field, &my_url);
                    }
                }
            }
            _ => {}
        }
    }
}

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

    #[test]
    fn patching_a_hooks_queues_moves_the_back_ref() {
        let mut s = st();
        let (_, _, q1) = seeded_graph(&mut s);
        let q1_obj = s.get("queues", q1).unwrap();
        let q2 = s
            .create(
                "queues",
                json!({ "name": "Q2", "workspace": q1_obj["workspace"], "schema": q1_obj["schema"] }),
            )
            .unwrap();
        let q2_id = q2["id"].as_u64().unwrap();
        let q1_url = s.url("queues", q1);
        let q2_url = s.url("queues", q2_id);
        let hook = s
            .create("hooks", json!({ "name": "H", "queues": [q1_url] }))
            .unwrap();
        let hook_id = hook["id"].as_u64().unwrap();
        s.patch("hooks", hook_id, &json!({ "queues": [q2_url] })).unwrap();
        assert_eq!(
            s.get("queues", q1).unwrap()["hooks"],
            json!([]),
            "the old parent must lose the back-ref, not just the new one gain it"
        );
        assert_eq!(s.get("queues", q2_id).unwrap()["hooks"], json!([s.url("hooks", hook_id)]));
    }

    #[test]
    fn patching_a_queues_workspace_moves_the_back_ref() {
        let mut s = st();
        let a = s.create("workspaces", json!({ "name": "A" })).unwrap();
        let b = s.create("workspaces", json!({ "name": "B" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "S" })).unwrap();
        let a_id = a["id"].as_u64().unwrap();
        let b_id = b["id"].as_u64().unwrap();
        let q = s
            .create("queues", json!({ "name": "Q", "workspace": a["url"], "schema": sc["url"] }))
            .unwrap();
        let q_id = q["id"].as_u64().unwrap();
        s.patch("queues", q_id, &json!({ "workspace": b["url"] })).unwrap();
        assert_eq!(
            s.get("workspaces", a_id).unwrap()["queues"],
            json!([]),
            "the queue must not still be listed under its old workspace"
        );
        assert_eq!(
            s.get("workspaces", b_id).unwrap()["queues"],
            json!([s.url("queues", q_id)])
        );
    }

    #[test]
    fn deleting_an_inbox_removes_its_queues_inbox_key() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let inbox = s
            .create(
                "inboxes",
                json!({ "name": "In", "email_prefix": "p", "queues": [s.url("queues", q)] }),
            )
            .unwrap();
        s.delete("inboxes", inbox["id"].as_u64().unwrap()).unwrap();
        let queue = s.get("queues", q).unwrap();
        assert!(
            !queue.as_object().unwrap().contains_key("inbox"),
            "the key must be vacated (real API rejects inbox: null), not set to null: {queue:?}"
        );
    }

    #[test]
    fn deleting_a_queue_shrinks_its_workspace_and_schema() {
        let mut s = st();
        let (ws, sc, q) = seeded_graph(&mut s);
        assert_eq!(s.delete("queues", q).unwrap(), Deletion::Requested);
        s.tick_deletions();
        assert_eq!(s.get("workspaces", ws).unwrap()["queues"], json!([]));
        assert_eq!(s.get("schemas", sc).unwrap()["queues"], json!([]));
    }

    /// `patch`'s unlink-then-relink round trip preserves MEMBERSHIP, not
    /// order — see the comment on the `self.unlink(kind, id)` call in
    /// `patch`. This pins the property that actually matters: patching one
    /// child does not evict its siblings from the shared parent back-ref.
    /// Compared as a sorted set on purpose, so this does not accidentally
    /// re-assert an order guarantee the fake deliberately does not make.
    #[test]
    fn patching_one_child_keeps_every_sibling_back_ref() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        let q_url = s.url("queues", q);
        let h1 = s
            .create("hooks", json!({ "name": "H1", "queues": [q_url.clone()] }))
            .unwrap();
        let h2 = s.create("hooks", json!({ "name": "H2", "queues": [q_url] })).unwrap();
        let h1_id = h1["id"].as_u64().unwrap();
        let h1_url = s.url("hooks", h1_id);
        let h2_url = s.url("hooks", h2["id"].as_u64().unwrap());
        // A drift-repair PATCH that never touches `queues` at all (the shape
        // `src/cli/push/hooks.rs:544` sends whenever ANY field of a hook has
        // drifted, not only its `queues`).
        s.patch("hooks", h1_id, &json!({ "name": "H1 renamed" })).unwrap();
        let mut got: Vec<String> = s.get("queues", q).unwrap()["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        got.sort();
        let mut want = vec![h1_url, h2_url];
        want.sort();
        assert_eq!(
            got, want,
            "both siblings must survive a patch to just one of them, order aside"
        );
    }

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

    /// The real `GET /queues` does not just leave a `deletion_requested`
    /// queue unchanged for its one extra sighting — it nulls `workspace` and
    /// `schema` and flips `status`. Repo-documented, not live-asserted: see
    /// the comment on `delete()`.
    #[test]
    fn a_queue_awaiting_deletion_has_workspace_and_schema_nulled() {
        let mut s = st();
        let (_, _, q) = seeded_graph(&mut s);
        s.delete("queues", q).unwrap();
        let seen = s.get("queues", q).expect("still listed for one more request");
        assert_eq!(seen["workspace"], Value::Null);
        assert_eq!(seen["schema"], Value::Null);
        assert_eq!(seen["status"], json!("deletion_requested"));
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
}
