//! The fake organization's object graph.
//!
//! Pure data — no HTTP, nothing async. `FakeOrg` owns one of these behind a
//! `std::sync::Mutex`, because wiremock's responder closure is synchronous
//! (`wiremock::respond::Respond`, `F: Fn(&Request) -> ResponseTemplate`).

use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::kinds::{self, OrgCtx};

/// A rejection, shaped like the real API's.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub body: Value,
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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
