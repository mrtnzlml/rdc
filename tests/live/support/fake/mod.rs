//! An offline, stateful stand-in for a Rossum organization.
//!
//! See `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`.
//! The short version: the live scenario suite is already parameterized on
//! `(api_base, org_id, token)`, so a fake that speaks HTTP lets the same
//! scenario bodies run in a plain `cargo test` — which is the only way
//! "nothing should happen the second time" becomes an assertion rather than
//! something a human notices in a customer env.

pub mod data_storage;
pub mod graph;
pub mod kinds;
pub mod quirks;
pub mod state;
pub mod validate;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::support::config::{EnvCreds, LiveConfig};
use state::{ApiError, Deletion, ListQuery, OrgState};

/// The token the fake accepts. Fixed, because nothing about it is secret and a
/// random one would make failures harder to read.
const TOKEN: &str = "fake-token";

/// A per-test override, run before `route()` on every request in the order
/// added. The first one to answer `Some` replaces the router's answer; one
/// that answers `None` only observes. Opt-in: a fake with none behaves
/// exactly as the router says.
pub type Intercept = Box<dyn FnMut(&mut OrgState, &Request) -> Option<ResponseTemplate> + Send>;

/// Which requests a staged fault counts.
pub type RequestMatch = Box<dyn Fn(&Request) -> bool + Send>;

/// Where a staged crash ([`FakeOrg::crash_on`]) stops the request it fires on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// The request reached the server, which did nothing with it.
    BeforeApply,
    /// The server applied the request; its answer never left.
    AfterApplyBeforeResponse,
}

/// Counts the requests `matches` accepts and says when the `nth` (1-based)
/// arrives — once.
struct Nth {
    matches: RequestMatch,
    nth: usize,
    seen: usize,
}

impl Nth {
    fn hit(&mut self, req: &Request) -> bool {
        if !(self.matches)(req) {
            return false;
        }
        self.seen += 1;
        self.seen == self.nth
    }
}

/// A staged crash, armed until it fires.
struct Crash {
    at: Nth,
    point: CrashPoint,
    fired: Arc<tokio::sync::Notify>,
}

pub struct FakeOrg {
    server: MockServer,
    inner: Arc<Mutex<OrgState>>,
    intercepts: Arc<Mutex<Vec<Intercept>>>,
    crash: Arc<Mutex<Option<Crash>>>,
    /// Set when a staged crash fires: the client is gone, so every later
    /// request is held unanswered and unapplied until [`FakeOrg::revive`].
    dead: Arc<AtomicBool>,
    org_id: u64,
}

impl FakeOrg {
    pub async fn start() -> FakeOrg {
        Self::start_with_org(1).await
    }

    pub async fn start_with_org(org_id: u64) -> FakeOrg {
        Self::start_with(org_id, None).await
    }

    /// A fake listening on loopback `port`, for a test that compares bytes
    /// across two fakes: the api base (port included) is embedded in what
    /// rdc writes, so the second fake must reuse the first one's port.
    #[allow(dead_code)]
    pub async fn start_on_port(port: u16) -> FakeOrg {
        // The first fake's server shuts down in the background after its drop.
        let mut tries = 0;
        let listener = loop {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => break l,
                Err(_) if tries < 100 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("binding port {port} again: {e}"),
            }
        };
        Self::start_with(1, Some(listener)).await
    }

    async fn start_with(org_id: u64, listener: Option<std::net::TcpListener>) -> FakeOrg {
        // Recording off: a sync makes hundreds of calls and no test here reads
        // the history back (order assertions use `RDC_TRACE_HTTP` instead).
        let mut builder = MockServer::builder().disable_request_recording();
        if let Some(listener) = listener {
            builder = builder.listener(listener);
        }
        let server = builder.start().await;
        let api_base = format!("{}/api/v1", server.uri());
        let inner = Arc::new(Mutex::new(OrgState::new(api_base, org_id)));
        let intercepts: Arc<Mutex<Vec<Intercept>>> = Arc::default();
        let crash: Arc<Mutex<Option<Crash>>> = Arc::default();
        let dead = Arc::new(AtomicBool::new(false));
        let handler = inner.clone();
        let overrides = intercepts.clone();
        let staged = crash.clone();
        let gone = dead.clone();
        Mock::given(any())
            .respond_with(move |req: &Request| {
                let mut st = handler.lock().unwrap_or_else(|p| p.into_inner());
                let mut overrides = overrides.lock().unwrap_or_else(|p| p.into_inner());
                // Checked under the state lock, which the crash below also
                // holds while it sets `gone`: a request that was waiting on
                // the lock when the crash fired sees it and is not applied.
                if gone.load(Ordering::SeqCst) {
                    return never();
                }
                let fired = {
                    let mut staged = staged.lock().unwrap_or_else(|p| p.into_inner());
                    let hit = staged.as_mut().is_some_and(|c| c.at.hit(req));
                    if hit { staged.take() } else { None }
                };
                let Some(crash) = fired else {
                    return serve(&mut st, &mut overrides, req);
                };
                if crash.point == CrashPoint::AfterApplyBeforeResponse {
                    let _lost = serve(&mut st, &mut overrides, req);
                }
                gone.store(true, Ordering::SeqCst);
                crash.fired.notify_one();
                never()
            })
            .mount(&server)
            .await;
        FakeOrg { server, inner, intercepts, crash, dead, org_id }
    }

    /// Serve Data Storage (`/svc/data-storage/api/v1/...`) from an empty
    /// in-memory store ([`data_storage`]). Off by default: every Data Storage
    /// path is a 404 until a test calls this.
    pub fn enable_data_storage(&self) {
        self.state().data_storage.get_or_insert_with(Default::default);
    }

    /// Add an [`Intercept`]. See its doc for the order and the opt-in.
    pub fn intercept(
        &self,
        f: impl FnMut(&mut OrgState, &Request) -> Option<ResponseTemplate> + Send + 'static,
    ) {
        self.intercepts.lock().unwrap_or_else(|p| p.into_inner()).push(Box::new(f));
    }

    /// Answer `status` to the `nth` (1-based) `POST /<kind>`, after creating
    /// the object when `apply` is set — a response lost on the way back —
    /// and without touching the store otherwise. Every other request goes
    /// to the router.
    pub fn fail_nth_create(&self, kind: &'static str, nth: usize, status: u16, apply: bool) {
        let mut at = Nth { matches: request_to("POST", &format!("/api/v1/{kind}")), nth, seen: 0 };
        self.intercept(move |st, req| {
            if !at.hit(req) {
                return None;
            }
            if apply {
                st.create(kind, body_of(req)).expect("the injected create applies");
            }
            Some(json_response(status, &serde_json::json!({ "detail": "injected by the test" })))
        });
    }

    /// Serve the store-extension install, `POST /hooks/create`, which the
    /// router does not model: it creates the hook as a store extension.
    pub fn serve_store_install(&self) {
        self.intercept(|st, req| {
            if req.method.as_str() != "POST" || req.url.path() != "/api/v1/hooks/create" {
                return None;
            }
            let mut body = body_of(req);
            body["extension_source"] = serde_json::json!("rossum_store");
            body["type"] = serde_json::json!("webhook");
            Some(match st.create("hooks", body) {
                Ok(v) => json_response(201, &v),
                Err(e) => err_response(e),
            })
        });
    }

    /// Crash the client at `point` of the `n`th (1-based) API write — any
    /// non-`GET` under `/api/v1/` — as [`FakeOrg::crash_on`] does.
    pub fn crash_after(&self, n: usize, point: CrashPoint) -> Arc<tokio::sync::Notify> {
        self.crash_on(
            Box::new(|req| req.method.as_str() != "GET" && req.url.path().starts_with("/api/v1/")),
            n,
            point,
        )
    }

    /// Crash the client at `point` of the `n`th (1-based) request `matches`
    /// accepts: the fake applies it or not, signals the returned `Notify`,
    /// and never answers it — nor any later request, as for a client that is
    /// gone, until [`FakeOrg::revive`]. The test drops the sync's future on
    /// the signal: a closed tab or a killed process. Replaces any crash
    /// staged before and not fired yet.
    pub fn crash_on(&self, matches: RequestMatch, n: usize, point: CrashPoint) -> Arc<tokio::sync::Notify> {
        let fired = Arc::new(tokio::sync::Notify::new());
        *self.crash.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(Crash { at: Nth { matches, nth: n, seen: 0 }, point, fired: fired.clone() });
        fired
    }

    /// Serve requests again after a crash fired: the next run is a new client.
    pub fn revive(&self) {
        self.dead.store(false, Ordering::SeqCst);
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

/// Matches `method` requests to exactly `path`.
pub fn request_to(method: &'static str, path: &str) -> RequestMatch {
    let path = path.to_string();
    Box::new(move |req| req.method.as_str() == method && req.url.path() == path)
}

/// An answer that never arrives: held for a day, long past any test.
fn never() -> ResponseTemplate {
    ResponseTemplate::new(503).set_delay(std::time::Duration::from_secs(24 * 3600))
}

/// The fake's answer to `req`: the first intercept that answers, else the
/// router.
fn serve(st: &mut OrgState, overrides: &mut [Intercept], req: &Request) -> ResponseTemplate {
    if let Some(out) = overrides.iter_mut().find_map(|f| f(st, req)) {
        return out;
    }
    // Before the tick below: a Data Storage request is not a sighting of a
    // queue the core API is still deleting.
    if let Some(out) = data_storage::serve(st.data_storage.as_mut(), req) {
        return out;
    }
    // Snapshot BEFORE routing: if THIS request is itself a queue
    // `DELETE`, `route()` below inserts its fresh grace entry
    // during this very call — and that entry must not be
    // charged by the tick that follows, or a 202 would be
    // followed by zero more sightings instead of one. See
    // `OrgState::tick_pending_from`.
    let already_pending = st.pending_delete_ids();
    let out = route(st, req);
    // Charge pending queue deletes for this request, so a 202 is
    // followed by exactly one more sighting.
    st.tick_pending_from(&already_pending);
    out
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
/// meaningless.
///
/// `shape_response` now carries TWO transforms, not one, and they are keyed
/// differently on purpose: `insert_organization_rir_key` fires for the single
/// `(kind, method)` pair `("organizations", "PATCH")`, while
/// `quirks::impose_field_order` applies to every body regardless of kind or
/// method, because field order on a real API is the serializer's job rather
/// than any one endpoint's. See `quirks::shape_response`'s doc comment for
/// both, and the `field_order_is_a_property_of_the_kind_not_of_the_request`
/// row of `quirks::QUIRKS` for what imposing that order costs the fake.
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
                // Real sub-resource endpoints exist here for at least one
                // kind — `GET /hooks/<id>/secrets_keys` returns a list of
                // secret key names — and stage 2 may model one. But
                // answering it with the parent object, the way this used to
                // fall through and do, is worse than a 404: a caller
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
#[cfg(test)]
mod create_journal;
#[cfg(test)]
mod recover;
#[cfg(test)]
mod crash;
#[cfg(test)]
mod crash_matrix;
#[cfg(test)]
mod state_store;
#[cfg(test)]
mod browser_migrate;
