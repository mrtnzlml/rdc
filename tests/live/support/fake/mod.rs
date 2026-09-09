//! An offline, stateful stand-in for a Rossum organization.
//!
//! See `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`.
//! The short version: the live scenario suite is already parameterized on
//! `(api_base, org_id, token)`, so a fake that speaks HTTP lets the same
//! scenario bodies run in a plain `cargo test` — which is the only way
//! "nothing should happen the second time" becomes an assertion rather than
//! something a human notices in a customer env.

pub mod graph;
pub mod kinds;
pub mod quirks;
pub mod state;
pub mod validate;

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
                // Snapshot BEFORE routing: if THIS request is itself a queue
                // `DELETE`, `route()` below inserts its fresh grace entry
                // during this very call — and that entry must not be
                // charged by the tick that follows, or a 202 would be
                // followed by zero more sightings instead of one. See
                // `OrgState::tick_pending_from`.
                let already_pending = st.pending_delete_ids();
                let out = route(&mut st, req);
                // Charge pending queue deletes for this request, so a 202 is
                // followed by exactly one more sighting.
                st.tick_pending_from(&already_pending);
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

/// The seam: every response body `route()` builds for a real `(kind,
/// method)` pair — organizations included, even though it sits outside the
/// `kinds` registry — passes through `quirks::shape_response` here before it
/// goes over the wire, so a per-endpoint asymmetry between the real API's
/// GET and PATCH shapes has exactly one place to attach. Error bodies do
/// NOT come through here: they go straight through `json_response`, because
/// they are never GET/PATCH-shaped kind bodies, and shaping one would be
/// meaningless — see `quirks::shape_response`'s doc comment for what the one
/// rule it carries actually does.
fn kind_response(kind: &str, method: &str, status: u16, body: &Value) -> ResponseTemplate {
    let mut shaped = body.clone();
    quirks::shape_response(kind, method, &mut shaped);
    json_response(status, &shaped)
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
    // Whether a segment survives PAST the tail — checked once, here, because
    // both the `organizations` branch and the per-kind branch below must
    // apply exactly the same rule: a tail that doesn't parse as a plain id,
    // OR any further segment beyond it, is not a route. It used to be
    // checked only in the `organizations` branch, while the kind branch's
    // `Some(seg)` arm parsed `seg` as an id and never consulted `segs`
    // again — so `GET /hooks/5/secrets_keys` was served as `GET /hooks/5`,
    // 200 with the hook body instead of an honest 404.
    let extra_segment = segs.next().is_some();
    let method = req.method.as_str().to_string();

    if head == "organizations" {
        // The real endpoint is `GET`/`PATCH /organizations/{id}` and nothing
        // else — no bare collection, no sub-path. Same rule as the kind
        // branch below: a tail that doesn't parse as a plain id, or a
        // segment beyond it, is not a route and must not fall through to
        // returning the org body anyway.
        let is_bare_id = tail.as_deref().is_some_and(|t| t.parse::<u64>().is_ok());
        if !is_bare_id || extra_segment {
            return err_response(ApiError::not_found());
        }
        return match method.as_str() {
            "GET" => kind_response(head, &method, 200, &st.organization()),
            "PATCH" => kind_response(head, &method, 200, &st.patch_organization(&body_of(req))),
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
            "GET" => kind_response(kind, &method, 200, &st.list(kind, &list_query(req))),
            "POST" => match st.create(kind, body_of(req)) {
                Ok(v) => kind_response(kind, &method, 201, &v),
                Err(e) => err_response(e),
            },
            _ => err_response(ApiError::not_found()),
        },
        Some(seg) => {
            let Ok(id) = seg.parse::<u64>() else {
                return err_response(ApiError::not_found());
            };
            if extra_segment {
                // A real sub-resource endpoint exists here for at least one
                // kind — `GET /hooks/<id>/secrets_keys`
                // (`get_hook_secrets_keys`, `src/api/mod.rs:223`), which
                // `rdc` calls on the deploy path — and stage 2 may model it.
                // But answering it with the parent object, the way this used
                // to fall through and do, is worse than a 404: a caller
                // expecting a list of key names would get a hook object
                // instead of an honest failure.
                return err_response(ApiError::not_found());
            }
            match method.as_str() {
                "GET" => {
                    let detail = kinds::spec(kind).map(|k| k.detail_get).unwrap_or(false);
                    if !detail {
                        // Labels have no detail endpoint
                        // (`tests/live/scenarios/round_trip.rs:120`).
                        return err_response(ApiError::not_found());
                    }
                    match st.get(kind, id) {
                        Some(v) => kind_response(kind, &method, 200, &v),
                        None => err_response(ApiError::not_found()),
                    }
                }
                "PATCH" => match st.patch(kind, id, &body_of(req)) {
                    Ok(v) => kind_response(kind, &method, 200, &v),
                    Err(e) => err_response(e),
                },
                "DELETE" => match st.delete(kind, id) {
                    Ok(Deletion::Gone) => ResponseTemplate::new(204),
                    Ok(Deletion::Requested) => kind_response(
                        kind,
                        &method,
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
mod tests;
