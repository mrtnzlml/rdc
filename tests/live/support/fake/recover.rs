//! Recovery of a crashed run's create journal, end to end against the fake.
//!
//! A crash is staged the honest way: a real `rdc sync` whose label create
//! answers 500 (or a 503 with two matches), which fails the run with the op
//! still in `pending`, exactly as a run that died mid-create leaves it. The
//! test then (for an op whose response did arrive) sets its id and lets the
//! next sync recover.

use super::*;
use crate::support::assert_local::load_lockfile;
use crate::support::project::ProjectFixture;
use rdc::state::journal::Pending;
use serde_json::json;

fn assert_ok(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "sync failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

const LABEL: &str = "envs/test/labels/priority.json";

/// A project whose only local object is one new label, `priority`.
fn one_label_project(fake: &FakeOrg) -> ProjectFixture {
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let org_url = fake.state().org_url();
    project.write_json(
        LABEL,
        &json!({ "id": 0, "url": "", "name": "priority", "organization": org_url, "color": "#ff8800" }),
    );
    project
}

/// Fail `project`'s next sync on its first label create, answering 500 after
/// applying it when `applied`: the op stays pending with no id.
fn crash_on_label_create(fake: &FakeOrg, project: &ProjectFixture, applied: bool) {
    fake.fail_nth_create("labels", 1, 500, applied);
    let out = project.run_rdc(&["sync", "test", "--yes"]);
    assert!(!out.status.success(), "the staged crash fails the run");
    let ops = load_lockfile(project.path(), "test").unwrap().pending.expect("an op is left").ops;
    assert_eq!((ops[0].kind.as_str(), ops[0].slug.as_str(), ops[0].id), ("labels", "priority", None));
}

/// Rewrite `project`'s pending journal with `f`.
fn edit_pending(project: &std::path::Path, f: impl FnOnce(&mut Pending)) {
    let paths = rdc::paths::Paths::for_env(project, "test");
    let mut lf = rdc::state::Lockfile::load(&paths.lockfile()).unwrap();
    f(lf.pending.as_mut().expect("a pending journal"));
    lf.save(&paths.lockfile()).unwrap();
}

/// Count the requests that reach the fake, without answering any.
fn count_requests(fake: &FakeOrg) -> Arc<Mutex<Vec<String>>> {
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = log.clone();
    fake.intercept(move |_, req| {
        sink.lock().unwrap().push(format!("{} {}", req.method, req.url.path()));
        None
    });
    log
}

fn label_posts(seen: &Arc<Mutex<Vec<String>>>) -> usize {
    seen.lock().unwrap().iter().filter(|r| *r == "POST /api/v1/labels").count()
}

/// The three things a create leaves behind for `slug`: the env file, its
/// base-cache copy, and the lockfile entry without `modified_at`/`modified_by`
/// (a recovered create whose local file was not yet canonical gets one more
/// PATCH, which moves them), as bytes.
fn create_artifacts(project: &std::path::Path) -> (Vec<u8>, Vec<u8>, String) {
    let paths = rdc::paths::Paths::for_env(project, "test");
    let env_file = project.join(LABEL);
    let cache = rdc::state::base_cache::cache_mirror(&paths, &env_file).expect("a cache path");
    let lf = load_lockfile(project, "test").unwrap();
    assert_eq!(lf.pending, None, "the run settles every op");
    let mut entry = lf.objects["labels"]["priority"].clone();
    entry.modified_at = None;
    entry.modified_by = None;
    (
        std::fs::read(&env_file).expect("env file"),
        std::fs::read(&cache).expect("base cache file"),
        serde_json::to_string(&entry).unwrap(),
    )
}

/// The API writes of a run, from [`count_requests`]: Data Storage reads are
/// POSTs too, so only `/api/v1/` non-GETs count.
fn api_writes(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.contains(" /api/v1/") && !r.starts_with("GET "))
        .cloned()
        .collect()
}

/// Copy `from` into `to`, recursively.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A crashed run's journal is recovered by the very next sync, with no wait:
/// the sync runs to completion, leaving no journal behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_runs_journal_is_recovered_at_once() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    crash_on_label_create(&fake, &project, true);
    let before = load_lockfile(project.path(), "test").unwrap().pending.unwrap().run;

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.pending, None, "the journal was recovered and settled (was run {before})");
    assert_eq!(fake.state().ids("labels").len(), 1);
}

/// The create's response arrived (the op has its id) but the run died before
/// recording it: recovery records it exactly as the uninterrupted run would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn op_with_id_completes_the_create() {
    // The uninterrupted run, on a fake of its own: the same deterministic ids
    // and clock, so its bytes are the reference. The second fake reuses the
    // port, because the organization url (port included) is in the bytes.
    let reference_fake = FakeOrg::start().await;
    let port = reference_fake.server.address().port();
    let reference = one_label_project(&reference_fake);
    assert_ok(&reference.run_rdc(&["sync", "test", "--yes"]));
    drop(reference_fake);

    let fake = FakeOrg::start_on_port(port).await;
    let project = one_label_project(&fake);
    crash_on_label_create(&fake, &project, true);
    let id = fake.state().ids("labels")[0];
    edit_pending(project.path(), |p| {
        p.ops[0].id = Some(id);
    });
    let seen = count_requests(&fake);

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    assert_eq!(label_posts(&seen), 0, "the object exists; nothing is created again");
    assert_eq!(fake.state().ids("labels"), vec![id]);
    assert_eq!(create_artifacts(project.path()), create_artifacts(reference.path()));

    // Converged: the next sync has nothing to send.
    let again = count_requests(&fake);
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));
    assert_eq!(api_writes(&again), Vec::<String>::new());
    assert_eq!(fake.state().ids("labels"), vec![id]);
}

/// The local file was edited after the crash: recovery leaves it alone, and
/// the sync sends the edit to the recovered object instead of losing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_keeps_a_post_crash_local_edit() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    crash_on_label_create(&fake, &project, true);
    let mut local = project.read_json(LABEL);
    local["color"] = json!("#00ff00");
    project.write_json(LABEL, &local);

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    assert_eq!(fake.state().get("labels", ids[0]).unwrap()["color"], json!("#00ff00"));
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.objects["labels"]["priority"].id, ids[0]);
    assert_eq!(lf.pending, None);
    assert_eq!(project.read_json(LABEL)["color"], json!("#00ff00"));
}

/// The run died between a store extension's install and the PATCH that
/// gives it the local config: the next sync PATCHes it, on the same hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_completes_an_interrupted_store_extension_install() {
    let fake = FakeOrg::start().await;
    let api_base = fake.api_base();
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let hook = "envs/test/hooks/store.json";
    project.write_json(
        hook,
        &json!({
            "id": 0,
            "url": "",
            "name": "store",
            "type": "webhook",
            "events": ["annotation_content"],
            "queues": [],
            "active": true,
            "config": {},
            "settings": { "mode": "local" },
            "extension_source": "rossum_store",
            "hook_template": format!("{api_base}/hook_templates/39"),
            "token_owner": format!("{api_base}/users/1"),
        }),
    );
    // The install endpoint (absent from the fake's router), and the crash: the
    // first PATCH of the installed hook fails.
    let installed: Arc<Mutex<Option<u64>>> = Arc::default();
    let seen_install = installed.clone();
    fake.intercept(move |st, req| {
        if req.method.as_str() != "POST" || req.url.path() != "/api/v1/hooks/create" {
            return None;
        }
        let mut body = body_of(req);
        body["extension_source"] = json!("rossum_store");
        body["type"] = json!("webhook");
        let created = st.create("hooks", body).expect("install");
        *seen_install.lock().unwrap() = created["id"].as_u64();
        Some(json_response(201, &created))
    });
    let failed = Arc::new(Mutex::new(false));
    let crash = installed.clone();
    fake.intercept(move |_, req| {
        let id = (*crash.lock().unwrap())?;
        if req.method.as_str() != "PATCH" || req.url.path() != format!("/api/v1/hooks/{id}") {
            return None;
        }
        let mut failed = failed.lock().unwrap();
        if *failed {
            return None;
        }
        *failed = true;
        Some(json_response(500, &json!({ "detail": "injected by the test" })))
    });
    assert!(!project.run_rdc(&["sync", "test", "--yes"]).status.success());
    let id = installed.lock().unwrap().expect("installed");
    let ops = load_lockfile(project.path(), "test").unwrap().pending.expect("the op is left").ops;
    assert_eq!((ops[0].kind.as_str(), ops[0].id), ("hooks", Some(id)));
    assert_ne!(fake.state().get("hooks", id).unwrap()["settings"], json!({ "mode": "local" }));

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    assert_eq!(fake.state().ids("hooks"), vec![id], "the installed hook is the only one");
    assert_eq!(fake.state().get("hooks", id).unwrap()["settings"], json!({ "mode": "local" }));
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.objects["hooks"]["store"].id, id);
    assert_eq!(lf.pending, None);
}

/// The op has an id, but the object is gone (GET answers 404): the op is
/// dropped and the local file is created as any new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn op_with_id_whose_object_is_gone_is_dropped() {
    let fake = FakeOrg::start().await;
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let org_url = fake.state().org_url();
    project.write_json(
        "envs/test/workspaces/ops/workspace.json",
        &json!({ "id": 0, "url": "", "name": "ops", "organization": org_url, "queues": [] }),
    );
    fake.fail_nth_create("workspaces", 1, 500, false);
    assert!(!project.run_rdc(&["sync", "test", "--yes"]).status.success());
    edit_pending(project.path(), |p| {
        p.ops[0].id = Some(4242);
    });
    let seen = count_requests(&fake);

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|r| r == "GET /api/v1/workspaces/4242"), "the object was looked up: {seen:?}");
    assert_eq!(seen.iter().filter(|r| *r == "POST /api/v1/workspaces").count(), 1);
    let ids = fake.state().ids("workspaces");
    assert_eq!(ids.len(), 1);
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.objects["workspaces"]["ops"].id, ids[0]);
    assert_eq!(lf.pending, None);
}

/// The run died while the POST was in flight, and the server applied it: the
/// one object that matches is adopted, not created a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn op_without_id_and_one_match_is_adopted() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    crash_on_label_create(&fake, &project, true);
    let seen = count_requests(&fake);

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    assert_eq!(label_posts(&seen), 0);
    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.objects["labels"]["priority"].id, ids[0]);
    assert_eq!(lf.pending, None);
}

/// The POST never landed: nothing matches, the op is dropped, and the label
/// is created by the normal path — exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn op_without_id_and_no_match_is_dropped_and_reposted_once() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    crash_on_label_create(&fake, &project, false);
    let seen = count_requests(&fake);

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    assert_eq!(label_posts(&seen), 1);
    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.objects["labels"]["priority"].id, ids[0]);
    assert_eq!(lf.pending, None);
}

/// Crash with two objects matching the lost create (a 503 the guard cannot
/// resolve).
fn crash_with_two_matches(fake: &FakeOrg) -> ProjectFixture {
    let project = one_label_project(fake);
    fake.intercept(|st, req| {
        if req.method.as_str() != "POST" || req.url.path() != "/api/v1/labels" {
            return None;
        }
        st.create("labels", body_of(req)).expect("first copy");
        st.create("labels", body_of(req)).expect("second copy");
        Some(json_response(503, &json!({ "detail": "injected by the test" })))
    });
    assert!(!project.run_rdc(&["sync", "test", "--yes"]).status.success());
    project
}

/// Several candidates and no way to choose: the sync stops, names every one,
/// and leaves the journal as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_matches_stop_under_yes() {
    let fake = FakeOrg::start().await;
    let project = crash_with_two_matches(&fake);
    let before = load_lockfile(project.path(), "test").unwrap();

    let out = project.run_rdc(&["sync", "test", "--yes"]);

    assert!(!out.status.success(), "several matches stop the run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 2);
    let named = format!("ids {}, {}", ids[0], ids[1]);
    assert!(stderr.contains(&named), "candidates not named as `{named}`:\n{stderr}");
    assert_eq!(load_lockfile(project.path(), "test").unwrap(), before, "nothing was persisted");
}

/// Answers every prompt with `answer` and records what it was asked.
struct Answer {
    answer: &'static str,
    seen: Mutex<Vec<rdc::cli::sync::embed::Prompt>>,
}

impl rdc::cli::sync::embed::PromptRoute for Answer {
    fn ask<'a>(
        &'a self,
        prompt: &'a rdc::cli::sync::embed::Prompt,
    ) -> rdc::rt::BoxFuture<'a, Option<String>> {
        self.seen.lock().unwrap().push(prompt.clone());
        Box::pin(async move { Some(self.answer.to_string()) })
    }
}

/// Interactively, the user picks the object: one choice per candidate plus
/// abort, and the chosen id is the one recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_matches_prompt_interactively() {
    let fake = FakeOrg::start().await;
    let project = crash_with_two_matches(&fake);
    let ids = fake.state().ids("labels");

    let paths = rdc::paths::Paths::for_env(project.path(), "test");
    let mut lf = rdc::state::Lockfile::load(&paths.lockfile()).unwrap();
    lf.api_base = fake.api_base();
    let client = rdc::api::RossumClient::new(fake.api_base(), "fake-token".into()).unwrap();
    let log = rdc::log::Log::new(rdc::cli::resolve::ColorMode::Plain);
    let route = Arc::new(Answer { answer: "2", seen: Mutex::default() });
    let report = rdc::cli::sync::embed::with_route(
        route.clone(),
        rdc::cli::sync::recover::recover(&paths, &mut lf, &client, true, false, &log),
    )
    .await
    .expect("recovery");

    let asked = route.seen.lock().unwrap()[0].clone();
    let keys: Vec<char> = asked.keys.iter().map(|k| k.key).collect();
    assert_eq!(keys, vec!['1', '2', 'a']);
    assert_eq!(asked.kind, rdc::cli::sync::embed::PromptKind::RecoverCreate);
    assert_eq!(
        asked.question,
        format!(
            "labels/priority: an unfinished run's create may have made any of 2 labels (ids {}, {}). \
             Which one did it create?",
            ids[0], ids[1]
        ),
        "the whole question"
    );
    assert_eq!(report.completed, vec![("labels".to_string(), "priority".to_string(), ids[1])]);
    assert_eq!(lf.objects["labels"]["priority"].id, ids[1]);
    assert!(project.path().join(LABEL).exists());
}

/// A clone that never had the local file recovers the create all the same:
/// the file is written from the server's object, under the journal's slug.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovers_on_a_machine_without_the_local_file() {
    let fake = FakeOrg::start().await;
    let first = one_label_project(&fake);
    crash_on_label_create(&fake, &first, true);
    let other = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let lockfile = rdc::paths::Paths::for_env(first.path(), "test").lockfile();
    let copy = rdc::paths::Paths::for_env(other.path(), "test").lockfile();
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::copy(&lockfile, &copy).unwrap();
    assert!(!other.path().join(LABEL).exists());

    assert_ok(&other.run_rdc(&["sync", "test", "--yes"]));

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    let lf = load_lockfile(other.path(), "test").unwrap();
    assert_eq!(lf.objects["labels"]["priority"].id, ids[0]);
    assert_eq!(lf.pending, None);
    let written: Value = serde_json::from_slice(&std::fs::read(other.path().join(LABEL)).unwrap()).unwrap();
    assert_eq!(written["name"], json!("priority"));
}

/// A clone without the queue directory: the queue, its schema and inbox are
/// written where the pull layout puts them, parents first, whatever order
/// the journal has them in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovers_queue_nested_objects_on_a_machine_without_their_directory() {
    let fake = FakeOrg::start().await;
    let run_id = crate::support::run_id::RunId::new();
    let first = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    crate::support::snapshot::write_snapshot(&first, "test", &run_id, &fake.state().org_url());
    assert_ok(&first.run_rdc(&["sync", "test", "--yes"]));

    let other = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    copy_tree(&first.path().join("envs/test"), &other.path().join("envs/test"));
    let paths = rdc::paths::Paths::for_env(other.path(), "test");
    let mut lf = rdc::state::Lockfile::load(&rdc::paths::Paths::for_env(first.path(), "test").lockfile()).unwrap();
    let q = format!("{}invoices", run_id.list_prefix());
    let ws = format!("{}ws", run_id.list_prefix());
    let queue_dir = paths.queue_dir(&ws, &q);
    std::fs::remove_dir_all(&queue_dir).unwrap();
    // As the crashed run left it: the three creates in create order, none recorded.
    let mut ops = Vec::new();
    for (seq, kind) in [(1, "schemas"), (2, "queues"), (3, "inboxes")] {
        let id = lf.objects.get_mut(kind).unwrap().remove(&q).expect("recorded").id;
        ops.push(rdc::state::journal::CreateIntent {
            seq,
            kind: kind.into(),
            slug: q.clone(),
            fields: Default::default(),
            started_at_unix: 0,
            id: Some(id),
        });
    }
    let prefix = format!("{ws}/{q}/");
    lf.objects.get_mut("email_templates").unwrap().retain(|k, _| !k.starts_with(&prefix));
    let ids: Vec<u64> = ops.iter().map(|o| o.id.unwrap()).collect();
    lf.pending = Some(Pending { run: "0".repeat(32), ops });
    std::fs::create_dir_all(paths.lockfile().parent().unwrap()).unwrap();
    lf.save(&paths.lockfile()).unwrap();
    let seen = count_requests(&fake);

    assert_ok(&other.run_rdc(&["sync", "test", "--yes"]));

    let creates: Vec<String> = api_writes(&seen).into_iter().filter(|r| r.starts_with("POST ")).collect();
    assert_eq!(creates, Vec::<String>::new(), "nothing is created again");
    for file in ["schema.json", "queue.json", "inbox.json"] {
        assert!(queue_dir.join(file).exists(), "{file} not written");
    }
    let lf = load_lockfile(other.path(), "test").unwrap();
    assert_eq!(lf.pending, None);
    for (kind, id) in ["schemas", "queues", "inboxes"].into_iter().zip(ids) {
        assert_eq!(lf.objects[kind][&q].id, id, "{kind}");
    }
}

/// A `[k]` restore re-creates a label and commits its entry only at the end of
/// the run; a crash during that POST leaves the old entry (id of the deleted
/// object) on disk next to the op. The recovered object replaces the stale
/// entry instead of being orphaned and created again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_replaces_a_stale_entry_left_by_an_interrupted_restore() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));
    let old = fake.state().ids("labels")[0];
    fake.state().delete("labels", old).expect("the remote delete");
    let mut body = project.read_json(LABEL);
    body.as_object_mut().unwrap().retain(|k, _| k != "id" && k != "url");
    let new = fake.state().create("labels", body).expect("the restore's create")["id"].as_u64().unwrap();
    let paths = rdc::paths::Paths::for_env(project.path(), "test");
    let mut lf = rdc::state::Lockfile::load(&paths.lockfile()).unwrap();
    assert_eq!(lf.objects["labels"]["priority"].id, old, "the stale entry is on disk");
    lf.pending = Some(Pending {
        run: "0".repeat(32),
        ops: vec![rdc::state::journal::CreateIntent {
            seq: 1,
            kind: "labels".into(),
            slug: "priority".into(),
            fields: Default::default(),
            started_at_unix: 0,
            id: Some(new),
        }],
    });
    lf.save(&paths.lockfile()).unwrap();
    let seen = count_requests(&fake);

    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));

    assert_eq!(label_posts(&seen), 0, "the restored label is not created again");
    assert_eq!(fake.state().ids("labels"), vec![new]);
    let lf = load_lockfile(project.path(), "test").unwrap();
    assert_eq!(lf.objects["labels"]["priority"].id, new);
    assert_eq!(lf.pending, None);
    let again = count_requests(&fake);
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));
    assert_eq!(api_writes(&again), Vec::<String>::new());
}

/// Adopting a hook clears the base-cache sidecar of the other runtime's
/// extension, as the driver's create does, so the cache matches the entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_removes_a_stale_cache_sidecar() {
    let fake = FakeOrg::start().await;
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    project.write_json(
        "envs/test/hooks/check.json",
        &json!({
            "id": 0, "url": "", "name": "check", "type": "function",
            "events": ["annotation_content"], "queues": [], "active": true,
            "config": { "runtime": "python3.12" }
        }),
    );
    std::fs::write(project.path().join("envs/test/hooks/check.py"), "def rossum_hook_request_handler(p):\n    return {}\n").unwrap();
    fake.fail_nth_create("hooks", 1, 500, true);
    assert!(!project.run_rdc(&["sync", "test", "--yes"]).status.success());

    let paths = rdc::paths::Paths::for_env(project.path(), "test");
    let mirror = |file: &str| {
        rdc::state::base_cache::cache_mirror(&paths, &project.path().join("envs/test/hooks").join(file)).unwrap()
    };
    std::fs::create_dir_all(mirror("check.js").parent().unwrap()).unwrap();
    std::fs::write(mirror("check.js"), "stale").unwrap();

    let mut lf = rdc::state::Lockfile::load(&paths.lockfile()).unwrap();
    lf.api_base = fake.api_base();
    let client = rdc::api::RossumClient::new(fake.api_base(), "fake-token".into()).unwrap();
    let log = rdc::log::Log::new(rdc::cli::resolve::ColorMode::Plain);
    rdc::cli::sync::recover::recover(&paths, &mut lf, &client, false, false, &log).await.expect("recovery");

    assert_eq!(lf.objects["hooks"]["check"].id, fake.state().ids("hooks")[0]);
    assert!(mirror("check.py").exists(), "the code sidecar is cached");
    assert!(!mirror("check.js").exists(), "the stale sidecar is forgotten");
}

/// Recovery is deterministic: two clones of the same crashed state record the
/// same object for the op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_machines_recover_to_the_same_id() {
    let fake = FakeOrg::start().await;
    let first = one_label_project(&fake);
    crash_on_label_create(&fake, &first, true);
    let second = one_label_project(&fake);
    let copy = rdc::paths::Paths::for_env(second.path(), "test").lockfile();
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::copy(rdc::paths::Paths::for_env(first.path(), "test").lockfile(), &copy).unwrap();

    assert_ok(&first.run_rdc(&["sync", "test", "--yes"]));
    assert_ok(&second.run_rdc(&["sync", "test", "--yes"]));

    let ids = fake.state().ids("labels");
    assert_eq!(ids.len(), 1, "exactly one label: {ids:?}");
    let id = |p: &std::path::Path| load_lockfile(p, "test").unwrap().objects["labels"]["priority"].id;
    assert_eq!(id(first.path()), ids[0]);
    assert_eq!(id(second.path()), ids[0]);
}

/// `--dry-run` recovers nothing: it only reads, and says what it would do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_recovery_writes_nothing() {
    let fake = FakeOrg::start().await;
    let project = one_label_project(&fake);
    crash_on_label_create(&fake, &project, true);
    let before = load_lockfile(project.path(), "test").unwrap();
    let local = std::fs::read(project.path().join(LABEL)).unwrap();
    let seen = count_requests(&fake);

    let out = project.run_rdc(&["sync", "test", "--dry-run"]);

    assert_ok(&out);
    let id = fake.state().ids("labels")[0];
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&format!("labels/priority id={id}")), "{stderr}");
    // Data Storage reads are POSTs too; the API is only read.
    let writes: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.contains(" /api/v1/") && !r.starts_with("GET "))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "only reads: {writes:?}");
    assert_eq!(load_lockfile(project.path(), "test").unwrap(), before);
    assert_eq!(std::fs::read(project.path().join(LABEL)).unwrap(), local);
}

/// The hand-written snapshot, its sync killed when the label's create
/// reaches the env (`site`'s POST, before it applies): every create before it
/// is in the journal with its id.
async fn crash_snapshot_at(fake: &FakeOrg, site: &str, name: &str) -> (ProjectFixture, String) {
    use super::crash::*;
    let run_id = crate::support::run_id::RunId::new();
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    crate::support::snapshot::write_snapshot(&project, "test", &run_id, &fake.state().org_url());
    let name = format!("{}{name}", run_id.list_prefix());
    let fired = fake.crash_on(post_named(site, &name), 1, CrashPoint::BeforeApply);
    let state: Arc<dyn rdc::state::store::StateStore> = Arc::new(rdc::state::store::FsStateStore);
    until_crash(sync_in_process(project.path(), "fake-token", yes(false), state), &fired).await;
    fake.revive();
    (project, run_id.list_prefix())
}

/// Recovered objects that name each other (a workspace its queue) are
/// recorded with `rdc://` refs, as the next listing reads them; with raw urls
/// every such base reads as changed on the env and the sync conflicts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovered_objects_name_each_other_by_ref() {
    let fake = FakeOrg::start().await;
    let (project, prefix) = crash_snapshot_at(&fake, "/api/v1/labels", "priority").await;

    let out = project.run_rdc(&["sync", "test", "--yes"]);

    assert_ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("conflict"), "{stderr}");
    assert!(!project.path().join(".rdc/conflicts").exists(), "no conflict shadow");
    let again = count_requests(&fake);
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]));
    assert_eq!(api_writes(&again), Vec::<String>::new(), "converged: {prefix}");
}

/// The run died after a queue's create, before the email-template driver
/// adopted the templates the server provisioned for it: recovery adopts them
/// for the local files, instead of the sync conflicting with each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_adopts_the_templates_a_queue_provisioned() {
    let fake = FakeOrg::start().await;
    let (project, prefix) = crash_snapshot_at(&fake, "/api/v1/inboxes", "invoices").await;

    let out = project.run_rdc(&["sync", "test", "--yes"]);

    assert_ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("conflict"), "{stderr}");
    let lf = load_lockfile(project.path(), "test").unwrap();
    let mut recorded: Vec<u64> = lf.objects["email_templates"].values().map(|e| e.id).collect();
    recorded.sort();
    assert_eq!(recorded, fake.state().ids("email_templates"), "every template is recorded once");
    assert!(lf.objects["email_templates"].keys().all(|k| k.starts_with(&format!("{prefix}ws/{prefix}invoices/"))));
}

/// A run whose create of `hook` (a body with `config`) failed unapplied
/// leaves its op pending with no id. Then someone makes, by hand, a hook
/// with the same name, queues and events but `config` `theirs`. Recovery
/// must not adopt it: the next push would overwrite its config.
async fn recovery_leaves_a_namesake_hook_alone(ours: serde_json::Value, theirs: serde_json::Value, sidecar: Option<&str>) {
    let fake = FakeOrg::start().await;
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let mut hook = json!({
        "id": 0, "url": "", "name": "check", "events": ["annotation_content"], "queues": [], "active": true,
    });
    hook.as_object_mut().unwrap().extend(ours.as_object().unwrap().clone());
    project.write_json("envs/test/hooks/check.json", &hook);
    if let Some(code) = sidecar {
        std::fs::write(project.path().join("envs/test/hooks/check.py"), code).unwrap();
    }
    fake.fail_nth_create("hooks", 1, 500, false);
    assert!(!project.run_rdc(&["sync", "test", "--yes"]).status.success());
    assert!(fake.state().ids("hooks").is_empty(), "the failed create left nothing");

    let mut by_hand = hook.clone();
    let obj = by_hand.as_object_mut().unwrap();
    obj.remove("id");
    obj.remove("url");
    obj.extend(theirs.as_object().unwrap().clone());
    let made = fake.state().create("hooks", by_hand).expect("the hand-made hook");
    let made_id = made["id"].as_u64().unwrap();

    let paths = rdc::paths::Paths::for_env(project.path(), "test");
    let mut lf = rdc::state::Lockfile::load(&paths.lockfile()).unwrap();
    lf.api_base = fake.api_base();
    let client = rdc::api::RossumClient::new(fake.api_base(), "fake-token".into()).unwrap();
    let log = rdc::log::Log::new(rdc::cli::resolve::ColorMode::Plain);
    rdc::cli::sync::recover::recover(&paths, &mut lf, &client, false, false, &log).await.expect("recovery");

    let adopted = lf.objects.get("hooks").and_then(|m| m.get("check")).map(|e| e.id);
    assert_ne!(adopted, Some(made_id), "the hand-made hook was adopted");
    assert_eq!(fake.state().get("hooks", made_id).unwrap()["config"], made["config"], "its config is untouched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_does_not_adopt_a_same_name_hook_with_other_code() {
    recovery_leaves_a_namesake_hook_alone(
        json!({ "type": "function", "config": { "runtime": "python3.12" } }),
        json!({ "config": { "runtime": "python3.12", "code": "def rossum_hook_request_handler(p):\n    return {'by': 'hand'}\n" } }),
        Some("def rossum_hook_request_handler(p):\n    return {}\n"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_does_not_adopt_a_same_name_webhook_with_another_url() {
    recovery_leaves_a_namesake_hook_alone(
        json!({ "type": "webhook", "config": { "url": "https://ours.invalid/hook" } }),
        json!({ "config": { "url": "https://theirs.invalid/hook" } }),
        None,
    )
    .await;
}
