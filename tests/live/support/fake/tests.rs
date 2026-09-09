//! Integration-shaped tests for the fake server: HTTP routing, auth, and the
//! end-to-end seed/pull/converge flows. Split out of `mod.rs` because that
//! file's job is the server and the router, not the tests that exercise
//! them.

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

/// A raw (non-typed-client), authenticated request against the fake —
/// for a raw-request test that needs the token header attached.
/// `a_missing_auth_header_is_rejected` and `data_storage_paths_are_404`
/// deliberately send no header at all and build their requests directly
/// instead of going through this helper.
async fn authed_request(method: reqwest::Method, url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(method, url)
        .header("Authorization", format!("token {TOKEN}"))
        .send()
        .await
        .expect("request")
}

/// `authed_request`'s body-carrying sibling: it sends immediately too, but
/// can attach a JSON payload first, which `authed_request` has no way to do
/// (it takes no body parameter and sends on the spot). Needed for any raw
/// (non-typed-client) PATCH/POST test that must control the exact wire body.
async fn authed_json(method: reqwest::Method, url: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .request(method, url)
        .header("Authorization", format!("token {TOKEN}"))
        .json(body)
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

/// The companion case: the tail here parses fine, but a segment survives
/// PAST it. `GET /hooks/<id>/secrets_keys` is the real endpoint this
/// protects (`get_hook_secrets_keys`, `src/api/mod.rs:223`), which `rdc`
/// calls on the deploy path — a stage-2 fake may eventually model it as a
/// list of key names, but until then answering it with the parent hook
/// object would be a confusing decode error where a caller expects an
/// honest 404.
#[tokio::test]
async fn a_segment_past_the_id_is_not_a_route() {
    let fake = FakeOrg::start().await;
    let c = client(&fake);
    let hook = c.create_hook(&json!({ "name": "H" }), None).await.expect("hook");
    let url = format!("{}/secrets_keys", fake.state().url("hooks", hook.id));
    let status = authed_request(reqwest::Method::GET, &url).await.status();
    assert_eq!(status, 404, "a segment past the id must not fall through to the parent object");
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

/// The fact the whole design exists for. `src/cli/push/organization.rs:161`
/// records what it cost: rdc wrote the PATCH response to disk, the response
/// was not GET-shaped, and every sync afterwards re-pulled the org to
/// correct itself — "one phantom '1 changed' cycle after every settings
/// push". A fake that answers both the same way would BLESS that bug.
#[tokio::test]
async fn the_organization_patch_response_is_not_get_shaped() {
    let fake = FakeOrg::start().await;
    let base = fake.api_base();
    let get_body: Value = authed_request(reqwest::Method::GET, &format!("{base}/organizations/1"))
        .await
        .json()
        .await
        .expect("json");
    assert!(
        get_body.get("rir_key").is_none(),
        "GET /organizations/{{id}} omits rir_key entirely"
    );

    let patched: Value = authed_json(
        reqwest::Method::PATCH,
        &format!("{base}/organizations/1"),
        &json!({ "settings": {
            "annotation_list_table": {},
            "some_width": { "width": 140 },
        }}),
    )
    .await
    .json()
    .await
    .expect("json");
    assert!(
        patched.get("rir_key").is_some(),
        "the PATCH response carries rir_key, which GET omits"
    );
    assert_eq!(
        patched["settings"]["annotation_list_table"],
        json!({ "columns": [] }),
        "the server normalizes an empty annotation_list_table to columns: []"
    );
    assert_eq!(
        patched["settings"]["some_width"]["width"],
        json!(140.0),
        "the server normalizes an integer width to a float"
    );
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
/// typed client `LiveClient::create` uses, with a REALISTIC body — the
/// shape a real client actually sends. Proves the fake's responses to
/// such a body deserialize into `crate::model::*`.
///
/// This does NOT prove `kinds.rs`'s `defaults()` are complete: every body
/// below already supplies each model-required field directly, and
/// `state.rs::create()` fills defaults with `Map::entry().or_insert()`,
/// which never overwrites a caller-supplied key — so a wrong or missing
/// `ensure(...)` here would go undetected.
/// `defaults_supply_every_field_the_models_require` below is the one
/// that exercises `defaults()` for real, with minimal bodies.
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

/// `every_core_kind_creates_and_deserializes` above supplies every
/// model-required field directly, so it cannot tell a present
/// `ensure(...)` in `kinds.rs` from a deleted one. This test creates each
/// kind with a MINIMAL body — only what a real client actually sends —
/// leaving every server-assigned or defaulted field for `defaults()` to
/// fill in. Drop the wrong `ensure(...)` line and it is THIS test, not
/// the one above, that fails to deserialize.
#[tokio::test]
async fn defaults_supply_every_field_the_models_require() {
    let fake = FakeOrg::start().await;
    let c = client(&fake);

    // `Workspace::organization` has no serde default; `org_owned` must
    // supply it.
    let ws = c.create_workspace(&json!({ "name": "W" }), None).await.expect("workspace");
    assert_eq!(ws.name, "W");

    // `Schema::content` has no serde default; `schema_defaults` must
    // supply it.
    let schema = c.create_schema(&json!({ "name": "S" }), None).await.expect("schema");
    assert!(schema.content.is_empty());

    // Queue requires only `name` — nothing in `queue_defaults` is
    // deserialization-critical the way the fields above are. Included
    // for completeness, not because it proves anything about
    // `defaults()`. A real `schema` is still supplied: a schema-less
    // `POST /queues` becomes a 400 from Task 7 on, and this test should
    // not need to change when that lands.
    let queue = c
        .create_queue(&json!({ "name": "Q", "schema": schema.url }), None)
        .await
        .expect("queue");
    assert_eq!(queue.schema.as_deref(), Some(schema.url.as_str()));

    // Rule requires only `name` too — same caveat as queues above.
    let rule = c.create_rule(&json!({ "name": "R" }), None).await.expect("rule");
    assert_eq!(rule.name, "R");

    // `Hook::hook_type` (wire name `type`) has no serde default;
    // `hook_defaults` must supply it.
    let hook = c.create_hook(&json!({ "name": "H" }), None).await.expect("hook");
    assert_eq!(hook.hook_type, "function");

    // `Label::organization` has no serde default; `org_owned` must
    // supply it.
    let label = c.create_label(&json!({ "name": "L" }), None).await.expect("label");
    assert_eq!(label.name, "L");

    // `Inbox::email` DOES have a serde default (`src/model/inbox.rs:18`,
    // `#[serde(default, skip_serializing_if = "String::is_empty")]`) —
    // dropping it from `inbox_defaults` would surface here as a wrong
    // `assert_eq!` on the address, not a deserialization error. Only
    // `Inbox::queues` has no serde default; `inbox_defaults` must supply
    // it — the body below omits `queues` entirely, unlike
    // `every_core_kind_creates_and_deserializes`'s.
    let inbox = c
        .create_inbox(&json!({ "name": "I", "email_prefix": "p" }), None)
        .await
        .expect("inbox");
    assert_eq!(inbox.email, "p@fake.rossum.invalid");
    assert!(inbox.queues.is_empty());
}

/// `state.rs`'s grace tests and `graph.rs`'s cascade tests call
/// `tick_deletions()` directly; what a real caller actually observes depends
/// on WHERE the router calls it — once per request, in `FakeOrg::start`'s
/// responder, after every response including the delete's own. Drive the
/// whole 202 arc over real HTTP to pin that observable sequence end to end,
/// including the nulled shape from `state.rs::delete()`'s comment.
#[tokio::test]
async fn a_deleted_queue_is_202_then_nulled_then_gone_over_http() {
    let fake = FakeOrg::start().await;
    let c = client(&fake);
    let schema = c.create_schema(&json!({ "name": "S" }), None).await.expect("schema");
    let queue = c
        .create_queue(&json!({ "name": "Q", "schema": schema.url }), None)
        .await
        .expect("queue");
    let queue_url = queue.url.clone();

    let del = authed_request(reqwest::Method::DELETE, &queue_url).await;
    assert_eq!(del.status(), 202);

    // One more sighting, with the nulled shape.
    let seen: Value = authed_request(reqwest::Method::GET, &queue_url)
        .await
        .json()
        .await
        .expect("still listed for one more request");
    assert_eq!(seen["workspace"], Value::Null);
    assert_eq!(seen["schema"], Value::Null);
    assert_eq!(seen["status"], json!("deletion_requested"));

    // Gone on the request after that.
    let status = authed_request(reqwest::Method::GET, &queue_url).await.status();
    assert_eq!(status, 404, "gone after one more request");
}

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

/// The whole point of the exercise: a real `rdc sync` against a stateful
/// backend, asserted to have settled. Before this existed, convergence
/// could only be checked against a live org.
///
/// Settling is necessary but not sufficient: `assert_converged`'s own
/// non-vacuousness guard requires only ONE captured file under this run's
/// prefix, so a silent partial pull — an entire kind dropped — would
/// still pass it. The block after `assert_converged` closes that gap by
/// checking the pulled lockfile actually carries a row for every kind the
/// manifest seeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_from_the_fake_converges() {
    use crate::support::assert_local::{load_lockfile, lockfile_keys};
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

    // Completeness floor, not a golden: every kind the manifest seeds
    // (`testdata/live/manifest.toml`) must show up as at least one
    // lockfile row for THIS run — `email_templates` excluded, because
    // those five-per-queue server defaults (`quirks.rs`) keep their
    // fixed, unprefixed names and so never carry the run's prefix.
    let prefix = run_id.list_prefix();
    let lockfile = load_lockfile(project.path(), "test").expect("load lockfile");
    for kind in ["labels", "workspaces", "queues", "schemas", "inboxes", "hooks", "rules"] {
        let present = lockfile_keys(&lockfile, kind).iter().any(|slug| slug.contains(&prefix));
        assert!(
            present,
            "pull produced no '{kind}' row for this run's prefix '{prefix}' — a \
             silent partial pull (one whole kind dropped) would otherwise still \
             pass `assert_converged`"
        );
    }
}

/// The falsifiability pin for [`crate::support::converge::assert_converged`]:
/// proof that it can actually FAIL, not just pass.
///
/// During the task that added `a_pull_from_the_fake_converges` above, the
/// discriminating power of `assert_converged` was demonstrated by hand —
/// PATCH a seeded label out of band, watch the assertion panic, then
/// delete the probe. That evidence lived only in a report. Without a
/// permanent version, a change that quietly defeated the run's prefix
/// filter (so it matched nothing) — or any other part of the
/// byte-identical check — would leave every fake-backed test green and
/// nobody would notice.
///
/// Shape: seed and sync exactly like the milestone test, then mutate one
/// seeded label straight through the fake's own store — never through
/// `rdc` — so the remote drifts behind rdc's back. `assert_converged`
/// panics on failure, so the check runs inside `catch_unwind`; a no-op
/// panic hook keeps the deliberate panic from spewing a backtrace into an
/// otherwise-passing suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assert_converged_actually_fails_when_the_remote_drifts() {
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
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        out.status.success(),
        "sync --no-push failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Out-of-band drift: straight through the fake's store, never
    // through `rdc`. This is exactly the shape a real convergence
    // regression takes — the remote moves and nothing in `rdc`'s own
    // request path ever saw it happen.
    let label_id = index.id("label-priority").expect("label-priority was seeded");
    fake.state()
        .patch("labels", label_id, &serde_json::json!({ "color": "#123456" }))
        .expect("out-of-band patch");

    // A caught panic still prints via the installed hook, and hooks are
    // PROCESS-GLOBAL — there is no per-thread hook API on stable Rust.
    // The test suite deliberately runs with the default thread count
    // (no `--test-threads=1`), so other tests are executing concurrently
    // on other OS threads for the entire window this hook is installed —
    // and that window is not an instant: `assert_converged` spawns two
    // real `rdc` subprocesses (a dry run, then a real sync) inside it. A
    // hook that unconditionally swallows every panic would, for that
    // whole window, silently eat the message and backtrace of any
    // unrelated test that genuinely panics on another thread — it would
    // still show FAILED, just with no diagnostic. So the hook below
    // checks WHICH thread is panicking: it suppresses only the one panic
    // this test is about to provoke on ITS OWN thread, and forwards
    // every other thread's panic to the real hook untouched. Against a
    // FOREIGN panic — any unrelated test that never swaps a hook itself —
    // this closes the race completely. It only NARROWS the race against
    // another copy of THIS SAME pattern running concurrently: `set_hook`
    // is a plain global overwrite with no compare-and-swap, so one such
    // test's restore can clobber another's still-live suppression, and
    // that other test's own expected panic then lands on a hook with no
    // thread check and prints a spurious backtrace into an otherwise
    // green run. Closing that sibling case needs a shared serialization
    // primitive — a `static Mutex<()>` held across the whole install ->
    // catch_unwind -> restore span — which thread-id filtering alone
    // cannot provide; add it once a second caller of this pattern
    // actually exists to serialize against.
    let this_thread = std::thread::current().id();
    let previous_hook = std::sync::Arc::new(std::panic::take_hook());
    {
        let previous_hook = previous_hook.clone();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() != this_thread {
                previous_hook(info);
            }
        }));
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_converged(
            &project,
            "test",
            &run_id.list_prefix(),
            "after an out-of-band drift",
        );
    }));
    // Restore unconditionally (forwarding to the real hook for every
    // thread) rather than reinstalling the plain original hook directly:
    // another thread could be mid-panic against the swap above, and
    // dropping straight back to the un-wrapped original here would be a
    // second, needless place this test reasons about hook identity.
    std::panic::set_hook(Box::new(move |info| previous_hook(info)));

    let payload = result
        .expect_err("assert_converged must fail once the remote has drifted behind rdc's back");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    // The label's local slug is the run's prefix plus its slugified name
    // (`src/slug.rs` lowercases), so this is exactly the substring a
    // convergence failure would name it by — in a plan line, a changed
    // file path, or a changed lockfile key.
    let needle = format!("{}priority", run_id.list_prefix());
    assert!(
        message.contains(&needle),
        "failure message should name the drifted label ('{needle}'): {message}"
    );
}
