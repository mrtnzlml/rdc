//! rdc's create journal, driven end to end against the fake: every create
//! is recorded in the lockfile's `pending` before its POST and gets its id
//! after, and a gateway error on a create is never resent blind.
//!
//! The fake answers in the test's own process while `rdc` runs as a child, so
//! an observing [`Intercept`] can read the lockfile off disk at the moment a
//! request arrives — the state a crash right then would leave behind.

use super::*;
use crate::support::assert_local::load_lockfile;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use rdc::state::journal::CreateIntent;
use serde_json::json;

/// What the lockfile on disk said when one request reached the fake.
struct Seen {
    method: String,
    path: String,
    ops: Vec<CreateIntent>,
}

/// Record [`Seen`] for every request, without answering any.
fn observe(fake: &FakeOrg, project: &std::path::Path) -> Arc<Mutex<Vec<Seen>>> {
    let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let sink = log.clone();
    let project = project.to_path_buf();
    fake.intercept(move |_, req| {
        let ops = load_lockfile(&project, "test")
            .ok()
            .and_then(|lf| lf.pending)
            .map(|p| p.ops)
            .unwrap_or_default();
        sink.lock().unwrap().push(Seen {
            method: req.method.as_str().to_string(),
            path: req.url.path().to_string(),
            ops,
        });
        None
    });
    log
}

fn assert_ok(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "sync failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A project whose only local object is one new label, `priority`.
fn one_label_project(fake: &FakeOrg) -> ProjectFixture {
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let org_url = fake.state().org_url();
    project.write_json(
        "envs/test/labels/priority.json",
        &json!({ "id": 0, "url": "", "name": "priority", "organization": org_url, "color": "#ff8800" }),
    );
    project
}

/// Every create call site — the eleven driver POSTs of the hand-written
/// snapshot plus the store-extension install — persists an op naming its
/// kind and slug, with no id, before its request leaves; the op carries the
/// recorded id before the run commits; and the commit clears them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_create_kind_is_journaled() {
    let fake = FakeOrg::start().await;
    let api_base = fake.api_base();
    let run_id = RunId::new();
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    write_snapshot(&project, "test", &run_id, &fake.state().org_url());
    let store_slug = format!("{}store", run_id.list_prefix());
    project.write_json(
        &format!("envs/test/hooks/{store_slug}.json"),
        &json!({
            "id": 0,
            "url": "",
            "name": store_slug,
            "type": "webhook",
            "events": ["annotation_content"],
            "queues": [format!("rdc://queues/{}invoices", run_id.list_prefix())],
            "active": true,
            "config": {},
            "extension_source": "rossum_store",
            "hook_template": format!("{api_base}/hook_templates/39"),
            "token_owner": format!("{api_base}/users/1"),
        }),
    );

    let seen = observe(&fake, project.path());
    fake.serve_store_install();

    assert_ok(&project.run_rdc(&["sync", "test"]));

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    assert_eq!(lf.pending, None, "the commit settles every op");

    let seen = seen.lock().unwrap();
    let mut covered = std::collections::BTreeSet::new();
    for (i, s) in seen.iter().enumerate() {
        if s.method != "POST" {
            continue;
        }
        let Some(rest) = s.path.strip_prefix("/api/v1/") else { continue };
        let kind = rest.split('/').next().unwrap();
        let op = s.ops.last().unwrap_or_else(|| panic!("POST {} with no op on disk", s.path));
        assert_eq!(op.kind, kind, "POST {}: the newest op is for another kind", s.path);
        assert_eq!(op.id, None, "POST {}: the op has an id before the response", s.path);
        let id = lf
            .objects
            .get(kind)
            .and_then(|m| m.get(&op.slug))
            .unwrap_or_else(|| panic!("{kind}/{} not recorded", op.slug))
            .id;
        assert!(
            seen[i + 1..].iter().any(|later| later
                .ops
                .iter()
                .any(|o| o.kind == kind && o.slug == op.slug && o.id == Some(id))),
            "{kind}/{}: no later state carries the op's id {id}",
            op.slug
        );
        covered.insert(rest.to_string());
    }
    let expected = [
        "email_templates", "engine_fields", "engines", "hooks", "hooks/create", "inboxes",
        "labels", "queues", "rules", "saved_views", "schemas", "workspaces",
    ];
    assert_eq!(covered.iter().map(String::as_str).collect::<Vec<_>>(), expected);
}

/// A create the server refused never happened, so its op is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_create_leaves_no_pending() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    let seen = observe(&fake, project.path());
    fake.fail_nth_create("labels", 1, 400, false);

    let out = project.run_rdc(&["sync", "test"]);
    assert!(!out.status.success(), "the refused create fails the run");
    // Data Storage reads are POSTs too; only the create counts.
    let is_create = |s: &Seen| s.method == "POST" && s.path == "/api/v1/labels";
    let post = seen.lock().unwrap().iter().position(is_create).expect("a POST");
    assert_eq!(seen.lock().unwrap()[post].ops.len(), 1, "the create was journaled before its POST");

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    assert_eq!(lf.pending, None);
    assert!(fake.state().ids("labels").is_empty());
}

/// The POST landed but its answer was a 502: the client finds the object it
/// created instead of creating a second one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_error_after_apply_does_not_duplicate() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    fake.fail_nth_create("labels", 1, 502, true);

    assert_ok(&project.run_rdc(&["sync", "test"]));

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    assert_eq!(lf.objects["labels"]["priority"].id, ids[0]);
    assert_eq!(lf.pending, None);
}

/// The POST never landed: nothing matches, so the client sends it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_error_before_apply_resends() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    let seen = observe(&fake, project.path());
    fake.fail_nth_create("labels", 1, 502, false);

    assert_ok(&project.run_rdc(&["sync", "test"]));

    // Between the failed POST and the resend, the matcher listed the kind.
    let seen = seen.lock().unwrap();
    let posts: Vec<usize> =
        (0..seen.len()).filter(|&i| seen[i].method == "POST" && seen[i].path == "/api/v1/labels").collect();
    assert_eq!(posts.len(), 2, "one failed POST, one resend");
    assert!(
        seen[posts[0]..posts[1]].iter().any(|s| s.method == "GET" && s.path == "/api/v1/labels"),
        "the resend was not preceded by a search for the object"
    );

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    assert_eq!(lf.objects["labels"]["priority"].id, ids[0]);
    assert_eq!(lf.pending, None);
}

/// Two unclaimed objects match the lost create: the client cannot tell which
/// is its own, so it stops, names both, and leaves the op for recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_error_with_several_matches_stops() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    fake.intercept(|st, req| {
        if req.method.as_str() != "POST" || req.url.path() != "/api/v1/labels" {
            return None;
        }
        st.create("labels", body_of(req)).expect("first copy");
        st.create("labels", body_of(req)).expect("second copy");
        Some(json_response(503, &json!({ "detail": "injected by the test" })))
    });

    let out = project.run_rdc(&["sync", "test"]);
    assert!(!out.status.success(), "several matches stop the run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for id in fake.state().ids("labels") {
        assert!(stderr.contains(&id.to_string()), "candidate {id} not named:\n{stderr}");
    }
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let ops = lf.pending.expect("the op stays for recovery").ops;
    assert_eq!((ops[0].kind.as_str(), ops[0].slug.as_str(), ops[0].id), ("labels", "priority", None));
}

/// A same-name label already on the env, which this run lists and pulls,
/// existed before the local create, so a 502 must not adopt it even though
/// every field agrees. (A cycle pushes before it pulls: what protects it is
/// the pre-push listing, not the lockfile.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_error_does_not_adopt_an_object_pulled_this_run() {
    let fake = FakeOrg::start().await;
    let org_url = fake.state().org_url();
    let label = json!({ "name": "priority", "organization": org_url, "color": "#ff8800" });
    let remote = fake.state().create("labels", label.clone()).expect("seed").get("id").and_then(Value::as_u64).unwrap();
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let mut local = label;
    local["id"] = json!(0);
    local["url"] = json!("");
    project.write_json("envs/test/labels/priority-local.json", &local);
    fake.fail_nth_create("labels", 1, 502, false);

    assert_ok(&project.run_rdc(&["sync", "test"]));

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 2, "the local label was created: {ids:?}");
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let labels = &lf.objects["labels"];
    assert_eq!(labels["priority"].id, remote, "the pulled label keeps its id");
    assert_ne!(labels["priority-local"].id, remote, "the local label got its own object");
    assert_eq!(lf.pending, None);
}

/// A same-name label with another color, appearing after the run listed the
/// env (so nothing claims it), is not the object the lost create made: the
/// client resends instead of adopting it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_error_does_not_adopt_a_same_name_object_with_other_fields() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    let mut first = true;
    fake.intercept(move |st, req| {
        if !first || req.method.as_str() != "POST" || req.url.path() != "/api/v1/labels" {
            return None;
        }
        first = false;
        let mut other = body_of(req);
        other["color"] = json!("#000000");
        st.create("labels", other).expect("someone else's label");
        Some(json_response(502, &json!({ "detail": "injected by the test" })))
    });

    assert_ok(&project.run_rdc(&["sync", "test"]));

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 2, "the create was resent: {ids:?}");
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let mine = lf.objects["labels"]["priority"].id;
    let color = |id: u64| fake.state().get("labels", id).unwrap()["color"].clone();
    assert_eq!(color(mine), json!("#ff8800"), "the recorded label is the one rdc sent");
    assert_eq!(lf.pending, None);
}
