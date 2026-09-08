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
    let path = req.url.path().to_string();
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        // Everything outside the API prefix — Data Storage included — is a
        // flat 404, unauthenticated or not: there is no route to be
        // unauthorized against. Checked before `authorized()` so an
        // anonymous probe of a nonexistent path still reads as "not here"
        // rather than "not allowed".
        return err_response(ApiError::not_found());
    };
    if !authorized(req) {
        return err_response(ApiError::unauthorized());
    }
    let mut segs = rest.split('/').filter(|s| !s.is_empty());
    let Some(head) = segs.next() else {
        return err_response(ApiError::not_found());
    };
    let tail = segs.next().map(|s| s.to_string());
    let method = req.method.as_str().to_string();

    if head == "organizations" {
        // The real endpoint is `GET`/`PATCH /organizations/{id}` and nothing
        // else — no bare collection, no sub-path. Same rule as the kind
        // branch below: a tail that doesn't parse as a plain id, or a
        // segment beyond it, is not a route and must not fall through to
        // returning the org body anyway.
        let is_bare_id = tail.as_deref().is_some_and(|t| t.parse::<u64>().is_ok());
        if !is_bare_id || segs.next().is_some() {
            return err_response(ApiError::not_found());
        }
        return match method.as_str() {
            "GET" => json_response(200, &st.organization()),
            "PATCH" => json_response(200, &st.patch_organization(&body_of(req))),
            _ => err_response(ApiError::not_found()),
        };
    }

    let Some(kind) = kind_key(head) else {
        return err_response(ApiError::not_found());
    };

    // Match on whether a tail segment is PRESENT, not on whether it parses
    // as an id: a present-but-non-numeric tail (e.g. the store-hook install
    // path `/hooks/create`) must 404, not fall through to the no-tail arm
    // and get misread as a plain `POST /hooks`.
    match tail {
        None => match method.as_str() {
            "GET" => json_response(200, &st.list(kind, &list_query(req))),
            "POST" => match st.create(kind, body_of(req)) {
                Ok(v) => json_response(201, &v),
                Err(e) => err_response(e),
            },
            _ => err_response(ApiError::not_found()),
        },
        Some(seg) => {
            let Ok(id) = seg.parse::<u64>() else {
                return err_response(ApiError::not_found());
            };
            match method.as_str() {
                "GET" => {
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
                "PATCH" => match st.patch(kind, id, &body_of(req)) {
                    Ok(v) => json_response(200, &v),
                    Err(e) => err_response(e),
                },
                "DELETE" => match st.delete(kind, id) {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdc::api::{anyhow_has_status, RossumClient};
    use serde_json::json;

    /// The typed client is the point: it is the same code path the seeder and
    /// `rdc` itself use, so a response the fake shapes wrongly fails here
    /// rather than somewhere deep in a pull.
    fn client(fake: &FakeOrg) -> RossumClient {
        let c = fake.creds();
        RossumClient::new(c.api_base, c.token).expect("client")
    }

    /// A raw (non-typed-client) request against the fake, with its token
    /// attached — every test below that steps outside `RossumClient` to
    /// probe routing behavior directly builds one of these.
    async fn authed_request(method: reqwest::Method, url: &str) -> reqwest::Response {
        reqwest::Client::new()
            .request(method, url)
            .header("Authorization", format!("token {TOKEN}"))
            .send()
            .await
            .expect("request")
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

        // `update_label` is the real push path (`src/cli/push/labels.rs:245`):
        // it PATCHes a full `Label`, not a JSON fragment — labels have no
        // raw-value PATCH method the way hooks/inboxes/engine fields do.
        let mut patched = created.clone();
        patched.extra.insert("color".to_string(), json!("#00ff00"));
        c.update_label(1, &patched, None).await.expect("patch");
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
        assert!(anyhow_has_status(&err, 401), "expected a 401: {err:#}");
    }

    /// `a_bad_token_is_rejected` covers a *wrong* token; `data_storage_paths_are_404`
    /// sends no header at all but targets a path outside the `/api/v1` prefix,
    /// so it never reaches the auth check. Neither exercises a missing header
    /// against a real, in-prefix path — this does.
    #[tokio::test]
    async fn a_missing_auth_header_is_rejected() {
        let fake = FakeOrg::start().await;
        let url = fake.state().org_url();
        let status = reqwest::Client::new().get(&url).send().await.expect("request").status();
        assert_eq!(status, 401);
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
        let status = reqwest::Client::new().get(&url).send().await.expect("request").status();
        assert_eq!(status, 404);
    }

    /// `kinds.rs` sets `detail_get: false` for labels and `route()` enforces
    /// it, but nothing above called `GET /labels/{id}` directly —
    /// `RossumClient` has no label-detail method. Reach the raw URL the same
    /// way `data_storage_paths_are_404` does, and confirm the label is still
    /// reachable through the endpoints that DO exist for it.
    #[tokio::test]
    async fn a_label_has_no_detail_endpoint() {
        let fake = FakeOrg::start().await;
        let c = client(&fake);
        let created = c.create_label(&json!({ "name": "One" }), None).await.expect("create");

        let url = fake.state().url("labels", created.id);
        let status = authed_request(reqwest::Method::GET, &url).await.status();
        assert_eq!(status, 404, "labels have no detail-GET endpoint");

        let listed = c.list_labels(None).await.expect("still listed");
        assert_eq!(listed.len(), 1);

        let mut patched = created.clone();
        patched.extra.insert("color".to_string(), json!("#00ff00"));
        c.update_label(created.id, &patched, None).await.expect("still patchable");
    }

    /// The router matches on whether a tail segment is PRESENT, not on
    /// whether it parses as an id. `POST /hooks/create` is a real Rossum
    /// endpoint (the store-hook install path, `create_hook_via_install` in
    /// `src/api/mod.rs`) with a non-numeric tail; it must 404 here rather
    /// than fall through to the `POST /hooks` collection arm and silently
    /// create a hook from an install payload — that endpoint is stage-2
    /// territory and nothing in this stage calls it.
    #[tokio::test]
    async fn a_non_numeric_sub_path_is_not_a_create() {
        let fake = FakeOrg::start().await;
        let url = format!("{}/hooks/create", fake.api_base());
        let status = authed_request(reqwest::Method::POST, &url).await.status();
        assert_eq!(status, 404);
        assert!(fake.state().ids("hooks").is_empty(), "no hook must have been created");
    }

    /// `route()` reads `head == "organizations"` and used to answer the org
    /// body (or accept a PATCH) for ANY tail, ignoring it entirely — so a
    /// path like `/organizations/{id}/queues` fell through to the same
    /// branch as the real `/organizations/{id}` endpoint. The real API has
    /// no such sub-path; it must 404 like any other unmodelled route.
    #[tokio::test]
    async fn organizations_sub_paths_are_not_a_route() {
        let fake = FakeOrg::start().await;
        let url = format!("{}/queues", fake.state().org_url());
        let status = authed_request(reqwest::Method::GET, &url).await.status();
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
}
