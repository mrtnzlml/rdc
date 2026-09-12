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
    /// Produced by `validate::on_write`'s queue-engine-slot and
    /// schema-vs-engine-fields rules.
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
    pub(super) objects: BTreeMap<&'static str, BTreeMap<u64, Value>>,
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

    /// Merges `patch` into stored state and returns the result. What comes
    /// back from THIS function is not, by itself, what a client observes on
    /// the wire: `mod.rs::kind_response` runs it through
    /// `quirks::shape_response` afterward, which inserts `rir_key` — the one
    /// difference that belongs at the response layer. See
    /// `quirks::insert_organization_rir_key`'s doc comment for why.
    ///
    /// `settings` normalization, by contrast, happens HERE, as the merge —
    /// not at the response seam. Quirk
    /// `organization_patch_response_is_not_get_shaped` (`quirks.rs`) is
    /// `Provenance::Modelled`, and the real fact it models for `settings`
    /// (`width: 140` comes back `140.0`; an empty `annotation_list_table`
    /// comes back `{ "columns": [] }`) is that the real server normalizes
    /// `settings` when it is WRITTEN — so the normalized value is what's
    /// STORED, and a `GET` taken after this PATCH returns it too, same as a
    /// real org. Normalizing only the response returned by THIS call (an
    /// earlier version of this fake did exactly that) would leave stored
    /// state holding the raw, unnormalized value, so a later `GET` would
    /// hand back something no real org ever would — the review that caught
    /// this called it out directly.
    ///
    /// This is `patch_organization`'s call into the write-path seam,
    /// `quirks::normalize_write`, made AFTER the raw shallow merge above —
    /// it runs against the fully-merged `self.org`, and its `"organizations"`
    /// rule only touches the `settings` field it finds there
    /// (`quirks::normalize_write`'s doc comment), so a patch that never
    /// mentions `settings` at all leaves it untouched (already-normalized
    /// input is a no-op: re-normalizing an idempotent shape changes
    /// nothing). This used to be a hand-wired `if k == "settings"` check on
    /// the incoming patch, right here — moved so this fact and the inbox
    /// one below (`create_unchecked`, `patch`) share one seam instead of
    /// each getting its own bespoke wiring.
    ///
    /// Still NOT modelled: the real `users`-reorder difference — this fake's
    /// organization always carries `users: []` (`OrgState::new` below), so
    /// reversing an empty list is a no-op; see
    /// `quirks::insert_organization_rir_key`'s doc comment for why that is a
    /// deliberate omission, not a gap.
    pub fn patch_organization(&mut self, patch: &Value) -> Value {
        let stamp = self.now();
        if let (Some(dst), Some(src)) = (self.org.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
            dst.insert("modified_at".into(), json!(stamp));
        }
        super::quirks::normalize_write("organizations", &mut self.org);
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
    ///
    /// `quirks::normalize_write` runs AFTER `defaults` and AFTER `id`/`url`
    /// are already assigned — never before, and never in [`Self::create`]
    /// above, which only validates. Validation must see the RAW body (a
    /// derived field appearing early could hide a refusal the real API
    /// would still issue against the client's actual input), and a refused
    /// create must not have touched `next_id` at all, which is why this
    /// whole function only runs once [`Self::create`] has already accepted
    /// the body. Once here, though, deriving before or after `defaults`
    /// makes no observable difference for today's one write-time rule
    /// (`inboxes`: `defaults` only fills in `queues`), so it runs right
    /// after for readability — everything that shapes the body happens
    /// together, before the persist step below.
    pub(super) fn create_unchecked(
        &mut self,
        kind: &'static str,
        mut body: Value,
    ) -> Result<Value, ApiError> {
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
        if spec.has_modified_at {
            obj.insert("modified_at".into(), json!(stamp));
        }
        (spec.defaults)(obj, &ctx);
        super::quirks::normalize_write(kind, &mut body);
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
        let spec = kinds::spec(kind).ok_or_else(ApiError::not_found)?;
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
                    // A Rossum PATCH is a shallow merge of the keys it
                    // carries. Chosen, unverified: nothing here protects
                    // read-only server-owned keys like `id`/`url` from a
                    // body that happens to carry them — see quirk
                    // `id_and_url_survive_a_client_sent_patch` (`quirks.rs`).
                    dst.insert(k.clone(), v.clone());
                }
                if spec.has_modified_at {
                    dst.insert("modified_at".into(), json!(stamp));
                }
            }
            // The write-path seam, symmetric with `create_unchecked`'s call
            // and `shape_response`'s response-side one — run AFTER the
            // merge above, against the fully-merged object, so a rule like
            // the inbox one (re-derive `email` from `email_prefix`) sees
            // whatever the PATCH just changed, not the pre-merge body.
            super::quirks::normalize_write(kind, slot);
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
            // `schema` go null. The nearest live mention is
            // `push_create_ordering`'s closing "the queue must be deleted or
            // draining" assertion (`ordering.rs:357-365`), and it is a
            // DISJUNCTION: not listed at all, OR listed with `status ==
            // "deletion_requested"`. A synchronous delete satisfies the
            // first branch, so a green run does not even establish that the
            // delete is async, let alone what a draining body looks like —
            // see quirk `queue_delete_is_async_and_cascades` (`quirks.rs`),
            // whose citation was corrected for exactly this. Only these
            // three fields are touched: nothing is known either way about
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
    /// (`tests/live/support/fake/tests.rs`) — a state-level test alone can't
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

    /// Every queue currently bound to `engine_url` — active or draining
    /// alike. `validate::on_delete` does NOT intersect this with
    /// [`Self::queues_awaiting_deletion`] — the two checks run
    /// sequentially, and whichever fires first wins outright: see
    /// `on_delete`'s two sequential `if … return Err(...)` checks
    /// (`validate.rs:56-67`). It calls `queues_awaiting_deletion` FIRST: if
    /// any draining queue is bound to this engine, it returns
    /// `engine_attached_to_queues_waiting_for_deletion` right there, and
    /// this method is never even called. Only when that first check finds
    /// nothing does it call this method, refusing with
    /// `engine_attached_to_active_queues` if the result is non-empty. So an
    /// engine bound to both a draining queue and a live one is refused with
    /// the DRAINING message — this method's own result is not consulted in
    /// that case at all (`tests/live/support/teardown.rs:59-61`).
    pub fn queues_bound_to_engine(&self, engine_url: &str) -> Vec<Value> {
        self.objects
            .get("queues")
            .map(|m| {
                m.values()
                    .filter(|q| q.get("engine").and_then(Value::as_str) == Some(engine_url))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn ids(&self, kind: &str) -> Vec<u64> {
        self.objects
            .get(kind)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Whether `url` names an object this org holds of EXACTLY `expected_kind`.
    ///
    /// A well-formed url of a DIFFERENT kind (e.g. a queue's `schema` field
    /// carrying a `workspace` url) is refused, matching the real API's
    /// ref-type checking — checking only that an object of the url's OWN
    /// kind exists, without checking that kind against what the field
    /// expects, is exactly the hole this method exists to close.
    pub fn resolves_kind(&self, url: &str, expected_kind: &str) -> bool {
        let mut parts = url.trim_end_matches('/').rsplit('/');
        let Some(id) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            return false;
        };
        let Some(kind) = parts.next() else { return false };
        if kind != expected_kind {
            return false;
        }
        if kind == "organizations" {
            return id == self.org_id;
        }
        kinds::spec(kind)
            .and_then(|k| self.objects.get(k.path))
            .map(|m| m.contains_key(&id))
            .unwrap_or(false)
    }

    /// Any object addressed by its url, but only when `url`'s own kind
    /// segment is `expected_kind` — `None` otherwise, even if an object of
    /// ITS kind exists at that id. So a caller cannot silently walk a
    /// wrong-kind object even if an earlier ref check were ever bypassed.
    pub fn get_by_url_kind(&self, url: &str, expected_kind: &str) -> Option<Value> {
        let mut parts = url.trim_end_matches('/').rsplit('/');
        let id = parts.next()?.parse::<u64>().ok()?;
        let kind = parts.next()?;
        if kind != expected_kind {
            return None;
        }
        self.get(kinds::spec(kind)?.path, id)
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
        // 101 objects, not 1: with a single object, `1.div_ceil(100).max(1)`
        // and `1.div_ceil(5000).max(1)` are both `1` — deleting the
        // `clamp(1, 100)` in `list()` entirely would leave this test green.
        // 101 objects makes the cap and no-cap answers diverge (2 pages vs.
        // 1), so this actually exercises the clamp.
        let mut s = st();
        for i in 0..101 {
            s.create("labels", json!({ "name": format!("L{i}") })).unwrap();
        }
        let out = s.list("labels", &ListQuery { page: 1, page_size: 5000 });
        assert_eq!(
            out["pagination"]["total_pages"],
            json!(2),
            "101 results at a page_size capped to 100 must be 2 pages"
        );
        assert_eq!(
            out["results"].as_array().unwrap().len(),
            100,
            "the capped page must hold exactly 100 results, not all 101"
        );
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

    /// The sibling refusal: an engine bound to a queue that is still fully
    /// live — never asked to delete at all — is refused with a DIFFERENT
    /// code than the draining case above
    /// (`tests/live/support/teardown.rs:59-61`). This is the branch
    /// `ordering.rs`'s cascade actually hits: `push::deletes` orders engines
    /// before queues, so the bound queue is always still active when its
    /// engine's delete is attempted.
    #[test]
    fn an_engine_cannot_be_deleted_while_bound_to_an_active_queue() {
        let mut s = st();
        let engine = s.create("engines", json!({ "name": "E" })).unwrap();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let sc = s.create("schemas", json!({ "name": "S" })).unwrap();
        s.create(
            "queues",
            json!({
                "name": "Q",
                "workspace": ws["url"],
                "schema": sc["url"],
                "engine": engine["url"],
            }),
        )
        .unwrap();
        // No `delete("queues", ...)` here — the queue is untouched.
        let err = s
            .delete("engines", engine["id"].as_u64().unwrap())
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
        assert!(
            format!("{:?}", err.body).contains("engine_attached_to_active_queues"),
            "wrong body: {:?}",
            err.body
        );
    }

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
        // A hook description is capped at 2000; the server trims trailing
        // whitespace BEFORE validating, so a value that fits once trimmed is
        // accepted. `field_caps` is pinned independently of
        // `crate::snapshot::limits::field_limits` — see the comment on it in
        // `validate.rs` — so this reads the cap from `validate`, not `limits`.
        let cap = crate::support::fake::validate::field_caps("hooks")
            .iter()
            .find(|(field, _)| *field == "description")
            .map(|(_, cap)| *cap)
            .expect("hooks/description has a cap");
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

    #[test]
    fn a_ref_of_the_wrong_kind_is_refused_even_though_it_resolves() {
        // A well-formed, EXISTING url of the wrong resource kind must be
        // refused, not just a url that resolves to nothing — the real API
        // checks ref TYPE, not just presence. This also protects rule 5's
        // schema lookup: `get_by_url_kind` there refuses to walk a non-schema
        // object, so the engine-field-name check cannot pass vacuously
        // against a mismatched ref that slipped past this one.
        let mut s = st();
        let ws = s.create("workspaces", json!({ "name": "W" })).unwrap();
        let err = s
            .create(
                "queues",
                json!({ "name": "Q", "workspace": ws["url"], "schema": ws["url"] }),
            )
            .expect_err("a workspace url is not a valid schema ref");
        assert_eq!(err.status, 400);
        assert!(
            format!("{:?}", err.body).contains("Invalid hyperlink - No URL match"),
            "wrong body: {:?}",
            err.body
        );
    }

    #[test]
    fn a_label_color_at_the_cap_is_accepted_and_one_over_is_refused() {
        // The cap that matters most: the seed fixture's label sits exactly
        // at it. `an_over_length_field_is_refused_after_a_trailing_whitespace_trim`
        // above already proves inclusivity for `hooks`/`description`; this
        // proves it as a property of `field_caps` in general, not one entry.
        let mut s = st();
        assert!(s
            .create("labels", json!({ "name": "L1", "color": "#ff0000" }))
            .is_ok());
        let err = s
            .create("labels", json!({ "name": "L2", "color": "#ff00000" }))
            .expect_err("must be refused");
        assert_eq!(err.status, 400);
    }
}
