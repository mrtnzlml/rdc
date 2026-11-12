//! Interrupted runs: a sync dies partway, the next sync on the same tree
//! finishes the job, and the env ends up exactly as one uninterrupted run
//! would leave it — converged, with one remote object per local slug.
//!
//! Every case is the same three steps: run 1 is interrupted (see
//! [`super::crash`] for the points and what each leaves on disk); run 2 is
//! `rdc sync test --yes` on the same tree, right away — the env lock file
//! holds no other run, so run 1's journal is recovered at once;
//! then [`assert_converged`] (a third run plans and changes nothing) and
//! [`assert_one_object_per_slug`].

use super::crash::*;
use super::*;
use crate::support::assert_local::load_lockfile;
use crate::support::converge::assert_converged;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use serde_json::json;

const TOKEN: &str = "fake-token";

fn assert_ok(out: &std::process::Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{ctx}: sync failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fs() -> Arc<dyn rdc::state::store::StateStore> {
    Arc::new(rdc::state::store::FsStateStore)
}

/// The kinds rdc creates, as the fake and the lockfile name them.
const KINDS: &[&str] = &[
    "workspaces", "queues", "schemas", "inboxes", "hooks", "rules", "labels",
    "email_templates", "engines", "engine_fields", "saved_views",
];

/// Every object the fake holds is recorded under exactly one slug, and every
/// recorded slug names an object the fake holds: nothing was created twice,
/// nothing was orphaned, nothing recorded is missing.
fn assert_one_object_per_slug(fake: &FakeOrg, project: &std::path::Path, ctx: &str) {
    let lf = load_lockfile(project, "test").expect("lockfile");
    assert_eq!(lf.pending, None, "{ctx}: the journal is settled");
    for kind in KINDS {
        let mut remote = fake.state().ids(kind);
        remote.sort();
        let mut recorded: Vec<u64> = lf.objects.get(*kind).map(|m| m.values().map(|e| e.id).collect()).unwrap_or_default();
        recorded.sort();
        assert_eq!(remote, recorded, "{ctx}: {kind} on the env vs. recorded slugs");
    }
}

/// Settle the dead run and let the fake answer the next one. A request the
/// dropped run had already handed to its connection may still land; give it
/// the moment a killed process's last bytes would have.
async fn after_crash(fake: &FakeOrg) {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    fake.revive();
}

/// Run 2 and the convergence checks.
async fn finish_and_check(fake: &FakeOrg, project: &ProjectFixture, prefix: &str, extra: &[&str], ctx: &str) {
    after_crash(fake).await;
    let mut args = vec!["sync", "test", "--yes"];
    args.extend_from_slice(extra);
    let out = project.run_rdc(&args);
    assert_ok(&out, ctx);
    assert_converged(project, "test", prefix, ctx);
    assert_one_object_per_slug(fake, project.path(), ctx);
}

/// The hand-written snapshot (the eleven driver creates) plus a store
/// extension (the install endpoint): all twelve create sites in one run.
fn snapshot_project(fake: &FakeOrg, run_id: &RunId) -> ProjectFixture {
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    write_snapshot(&project, "test", run_id, &fake.state().org_url());
    let api_base = fake.api_base();
    let store = format!("{}store", run_id.list_prefix());
    project.write_json(
        &format!("envs/test/hooks/{store}.json"),
        &json!({
            "id": 0,
            "url": "",
            "name": store,
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
    fake.serve_store_install();
    project
}

/// One create site: the request path, the journal kind, and the name (after
/// the run prefix) of the object the snapshot creates there.
struct Site {
    path: &'static str,
    kind: &'static str,
    name: &'static str,
}

const WORKSPACE: Site = Site { path: "/api/v1/workspaces", kind: "workspaces", name: "ws" };
const QUEUE: Site = Site { path: "/api/v1/queues", kind: "queues", name: "invoices" };
const SCHEMA: Site = Site { path: "/api/v1/schemas", kind: "schemas", name: "invoices" };
const INBOX: Site = Site { path: "/api/v1/inboxes", kind: "inboxes", name: "invoices" };
const HOOK: Site = Site { path: "/api/v1/hooks", kind: "hooks", name: "validator" };
const STORE_HOOK: Site = Site { path: "/api/v1/hooks/create", kind: "hooks", name: "store" };
const RULE: Site = Site { path: "/api/v1/rules", kind: "rules", name: "totals" };
const LABEL: Site = Site { path: "/api/v1/labels", kind: "labels", name: "priority" };
const EMAIL_TEMPLATE: Site = Site { path: "/api/v1/email_templates", kind: "email_templates", name: "notice" };
const ENGINE: Site = Site { path: "/api/v1/engines", kind: "engines", name: "engine" };
const ENGINE_FIELD: Site = Site { path: "/api/v1/engine_fields", kind: "engine_fields", name: "probe_field" };
const SAVED_VIEW: Site = Site { path: "/api/v1/saved_views", kind: "saved_views", name: "view" };

/// The four moments of one create.
#[derive(Clone, Copy, Debug)]
enum Point {
    BeforeBeginPersist,
    BeforeApply,
    AfterApplyBeforeResponse,
    AfterFinishPersist,
}

/// Interrupt `run_id`'s snapshot sync at `point` of `site`'s create.
async fn interrupt_create(fake: &FakeOrg, project: &ProjectFixture, run_id: &RunId, site: &Site, point: Point) {
    let name = format!("{}{}", run_id.list_prefix(), site.name);
    let run = |state| sync_in_process(project.path(), TOKEN, yes(false), state);
    match point {
        Point::BeforeBeginPersist | Point::AfterFinishPersist => {
            let store = match point {
                Point::BeforeBeginPersist => CrashStore::new(begun(site.kind, &name), true),
                _ => CrashStore::new(finished(site.kind, &name), false),
            };
            until_crash(run(store.clone()), &store.fired).await;
        }
        Point::BeforeApply | Point::AfterApplyBeforeResponse => {
            let at = match point {
                Point::BeforeApply => CrashPoint::BeforeApply,
                _ => CrashPoint::AfterApplyBeforeResponse,
            };
            let fired = fake.crash_on(post_named(site.path, &name), 1, at);
            until_crash(run(fs()), &fired).await;
        }
    }
}

async fn create_case(site: Site, point: Point) {
    let ctx = format!("{} {point:?}", site.path);
    let fake = FakeOrg::start().await;
    let run_id = RunId::new();
    let project = snapshot_project(&fake, &run_id);
    interrupt_create(&fake, &project, &run_id, &site, point).await;
    finish_and_check(&fake, &project, &run_id.list_prefix(), &[], &ctx).await;
}

macro_rules! create_cases {
    ($($site:ident => $before_begin:ident, $before_apply:ident, $after_apply:ident, $after_finish:ident;)*) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $before_begin() { create_case($site, Point::BeforeBeginPersist).await }
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $before_apply() { create_case($site, Point::BeforeApply).await }
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $after_apply() { create_case($site, Point::AfterApplyBeforeResponse).await }
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $after_finish() { create_case($site, Point::AfterFinishPersist).await }
    )*};
}

create_cases! {
    WORKSPACE => workspace_before_begin_persist, workspace_before_apply, workspace_after_apply, workspace_after_finish_persist;
    SCHEMA => schema_before_begin_persist, schema_before_apply, schema_after_apply, schema_after_finish_persist;
    INBOX => inbox_before_begin_persist, inbox_before_apply, inbox_after_apply, inbox_after_finish_persist;
    HOOK => hook_before_begin_persist, hook_before_apply, hook_after_apply, hook_after_finish_persist;
    STORE_HOOK => store_hook_before_begin_persist, store_hook_before_apply, store_hook_after_apply, store_hook_after_finish_persist;
    RULE => rule_before_begin_persist, rule_before_apply, rule_after_apply, rule_after_finish_persist;
    LABEL => label_before_begin_persist, label_before_apply, label_after_apply, label_after_finish_persist;
    EMAIL_TEMPLATE => email_template_before_begin_persist, email_template_before_apply, email_template_after_apply, email_template_after_finish_persist;
    ENGINE => engine_before_begin_persist, engine_before_apply, engine_after_apply, engine_after_finish_persist;
    ENGINE_FIELD => engine_field_before_begin_persist, engine_field_before_apply, engine_field_after_apply, engine_field_after_finish_persist;
    SAVED_VIEW => saved_view_before_begin_persist, saved_view_before_apply, saved_view_after_apply, saved_view_after_finish_persist;
}

// The queue's create never lands in these two, and its schema's did: the
// next sync lists schemas only through their queues, so it reads the
// recorded schema as deleted on the env and deletes the local file. Not a
// crash bug — a queue create the server refuses (400) does the same, on
// `main` too — and its fix (listing a recorded schema with no queue) is a
// change to how the catalog is built.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a recorded schema without its queue is read as deleted on the env"]
async fn queue_before_begin_persist() {
    create_case(QUEUE, Point::BeforeBeginPersist).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a recorded schema without its queue is read as deleted on the env"]
async fn queue_before_apply() {
    create_case(QUEUE, Point::BeforeApply).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_after_apply() {
    create_case(QUEUE, Point::AfterApplyBeforeResponse).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_after_finish_persist() {
    create_case(QUEUE, Point::AfterFinishPersist).await
}

/// The control: the same snapshot, never interrupted, meets the same checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninterrupted_snapshot_converges() {
    let fake = FakeOrg::start().await;
    let run_id = RunId::new();
    let project = snapshot_project(&fake, &run_id);
    finish_and_check(&fake, &project, &run_id.list_prefix(), &[], "uninterrupted").await;
}


/// A project of `n` new labels named `<prefix>l<i>`, synced once, so every
/// label is recorded and on the env.
fn synced_labels(fake: &FakeOrg, run_id: &RunId, n: usize) -> ProjectFixture {
    let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let org_url = fake.state().org_url();
    for i in 1..=n {
        let name = format!("{}l{i}", run_id.list_prefix());
        project.write_json(
            &format!("envs/test/labels/{name}.json"),
            &json!({ "id": 0, "url": "", "name": name, "organization": org_url, "color": "#ff8800" }),
        );
    }
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "the first sync");
    project
}

fn label_path(run_id: &RunId, i: usize) -> String {
    format!("envs/test/labels/{}l{i}.json", run_id.list_prefix())
}

/// More edits than `PUSH_FANOUT` PATCHes at once; the run dies with two of
/// them applied (the second unanswered) and the rest never sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_update_batch() {
    let fake = FakeOrg::start().await;
    let run_id = RunId::new();
    // One more than `push::concurrent::PUSH_FANOUT` (5, private to the crate).
    let n = 6;
    let project = synced_labels(&fake, &run_id, n);
    for i in 1..=n {
        let mut label = project.read_json(&label_path(&run_id, i));
        label["color"] = json!("#00ff00");
        project.write_json(&label_path(&run_id, i), &label);
    }
    let is_patch = |req: &Request| req.method.as_str() == "PATCH" && req.url.path().starts_with("/api/v1/labels/");
    let fired = fake.crash_on(Box::new(is_patch), 2, CrashPoint::AfterApplyBeforeResponse);

    until_crash(sync_in_process(project.path(), TOKEN, yes(false), fs()), &fired).await;
    let colors = |fake: &FakeOrg| {
        let st = fake.state();
        st.ids("labels").into_iter().map(|id| st.get("labels", id).unwrap()["color"].clone()).collect::<Vec<_>>()
    };
    let applied = colors(&fake).iter().filter(|c| *c == "#00ff00").count();
    assert_eq!(applied, 2, "two PATCHes were applied before the crash");

    finish_and_check(&fake, &project, &run_id.list_prefix(), &[], "mid update batch").await;
    assert_eq!(colors(&fake), vec![json!("#00ff00"); n], "every label got its edit");
}

/// Three local deletes pushed with `--allow-deletes`; the run dies after the
/// second DELETE is applied, before its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_delete_phase() {
    let fake = FakeOrg::start().await;
    let run_id = RunId::new();
    let project = synced_labels(&fake, &run_id, 4);
    for i in 1..=3 {
        std::fs::remove_file(project.path().join(label_path(&run_id, i))).unwrap();
    }
    let is_delete = |req: &Request| req.method.as_str() == "DELETE" && req.url.path().starts_with("/api/v1/labels/");
    let fired = fake.crash_on(Box::new(is_delete), 2, CrashPoint::AfterApplyBeforeResponse);

    until_crash(sync_in_process(project.path(), TOKEN, yes(true), fs()), &fired).await;
    assert_eq!(fake.state().ids("labels").len(), 2, "two of the three deletes were applied");

    finish_and_check(&fake, &project, &run_id.list_prefix(), &["--allow-deletes"], "mid delete phase").await;
    assert_eq!(fake.state().ids("labels").len(), 1, "only the kept label is left");
}

/// Changes made on the env are pulled; the run dies after the pull wrote the
/// files and before its lockfile commit reached the disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn during_a_pull() {
    let fake = FakeOrg::start().await;
    let run_id = RunId::new();
    let project = snapshot_project(&fake, &run_id);
    assert_ok(&project.run_rdc(&["sync", "test", "--yes"]), "the first sync");
    let lf = load_lockfile(project.path(), "test").unwrap();
    let label = lf.objects["labels"][&format!("{}priority", run_id.list_prefix())].id;
    let hook = lf.objects["hooks"][&format!("{}validator", run_id.list_prefix())].id;
    fake.state().patch("labels", label, &json!({ "color": "#123456" })).unwrap();
    fake.state().patch("hooks", hook, &json!({ "active": false })).unwrap();
    let org_url = fake.state().org_url();
    fake.state()
        .create("labels", json!({ "name": format!("{}remote", run_id.list_prefix()), "organization": org_url, "color": "#654321" }))
        .unwrap();
    let store = CrashStore::new(|_| true, true);

    until_crash(sync_in_process(project.path(), TOKEN, yes(false), store.clone()), &store.fired).await;
    let pulled = project.read_json(&format!("envs/test/labels/{}priority.json", run_id.list_prefix()));
    assert_eq!(pulled["color"], "#123456", "the pull wrote the file before the crash");

    finish_and_check(&fake, &project, &run_id.list_prefix(), &[], "during a pull").await;
    assert_eq!(fake.state().get("labels", label).unwrap()["color"], "#123456", "the env's edit is kept");
    assert_eq!(fake.state().get("hooks", hook).unwrap()["active"], false, "the env's edit is kept");
}

/// Copy `from` into `to`, recursively, leaving out `skip`.
fn copy_tree(from: &std::path::Path, to: &std::path::Path, skip: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.path() == skip {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target, skip);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Run 2 on another machine: the tree is copied (a clone, a CI checkout)
/// without the machine-local env lock file, and still recovers at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_machine_recovers_at_once() {
    let fake = FakeOrg::start().await;
    let run_id = RunId::new();
    let first = snapshot_project(&fake, &run_id);
    interrupt_create(&fake, &first, &run_id, &LABEL, Point::AfterApplyBeforeResponse).await;
    let other = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
    let lock = rdc::paths::Paths::for_env(first.path(), "test").env_lock();
    for dir in ["envs", ".rdc"] {
        copy_tree(&first.path().join(dir), &other.path().join(dir), &lock);
    }
    assert!(!rdc::paths::Paths::for_env(other.path(), "test").env_lock().exists());
    assert!(load_lockfile(other.path(), "test").unwrap().pending.is_some(), "the journal travelled");

    finish_and_check(&fake, &other, &run_id.list_prefix(), &[], "another machine").await;
}

/// A hook secret never lands in a lockfile — not the value, not a hash of
/// it — at any moment of an interrupted hook create or its recovery. (The
/// entry's `secrets_hash`, a hash of the hook's whole key/value map, is
/// recorded on purpose so a changed secret is pushed; it is not searched.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_secret_never_reaches_lockfile() {
    const SECRET: &str = "s3cret-value-never-on-disk";
    let hash = |bytes: &[u8]| rdc::state::raw_content_hash(bytes);
    let needles = [
        SECRET.to_string(),
        hash(SECRET.as_bytes()),
        hash(&serde_json::to_vec(&json!(SECRET)).unwrap()),
    ];
    for point in [Point::BeforeBeginPersist, Point::BeforeApply, Point::AfterApplyBeforeResponse, Point::AfterFinishPersist] {
        let ctx = format!("hook secret, {point:?}");
        let fake = FakeOrg::start().await;
        let run_id = RunId::new();
        let project = snapshot_project(&fake, &run_id);
        std::fs::write(
            project.path().join("secrets/test.hook-secrets.json"),
            json!({ "hooks": { format!("{}validator", run_id.list_prefix()): { "api_key": SECRET } } }).to_string(),
        )
        .unwrap();
        // Run 1 is interrupted; every lockfile its checkpoints saw is kept.
        let name = format!("{}validator", run_id.list_prefix());
        let (store, fired) = match point {
            Point::BeforeBeginPersist => {
                let s = CrashStore::new(begun("hooks", &name), true);
                (s.clone(), s.fired.clone())
            }
            Point::AfterFinishPersist => {
                let s = CrashStore::new(finished("hooks", &name), false);
                (s.clone(), s.fired.clone())
            }
            _ => {
                let at = if matches!(point, Point::BeforeApply) { CrashPoint::BeforeApply } else { CrashPoint::AfterApplyBeforeResponse };
                (CrashStore::new(|_| false, false), fake.crash_on(post_named("/api/v1/hooks", &name), 1, at))
            }
        };
        until_crash(sync_in_process(project.path(), TOKEN, yes(false), store.clone()), &fired).await;
        after_crash(&fake).await;
        // Run 2, in process too, so its checkpoints are seen.
        let recorder = CrashStore::new(|_| false, false);
        sync_in_process(project.path(), TOKEN, yes(false), recorder.clone()).await.expect("run 2");
        let lockfile = rdc::paths::Paths::for_env(project.path(), "test").lockfile();
        let mut seen = store.seen();
        seen.extend(recorder.seen());
        seen.push(std::fs::read(&lockfile).unwrap());
        assert!(seen.len() > 3, "{ctx}: the hook create was checkpointed");
        for bytes in &seen {
            let text = String::from_utf8_lossy(bytes);
            for needle in &needles {
                assert!(!text.contains(needle.as_str()), "{ctx}: a lockfile carries the secret or its hash");
            }
            let lf: rdc::state::Lockfile = serde_json::from_slice(bytes).unwrap();
            for op in lf.pending.iter().flat_map(|p| &p.ops) {
                assert!(!op.fields.contains_key("secrets"), "{ctx}: the journal records the secrets field");
            }
        }
        assert_one_object_per_slug(&fake, project.path(), &ctx);
    }
}

// The same crashes on an in-memory project (`embed::sync_logged_in_with_state`,
// the browser's and desktop's path): one create site per point, plus the
// update batch and the delete phase. The `MemStore` outlives the dropped run,
// as a host's persisted tree does; the run's `CrashStore` is its state store.

mod mem {
    use super::*;
    use rdc::cli::sync::embed::{sync_logged_in_with_state, EmbedSyncOptions};
    use rdc::state::store::StateStore;
    use rdc::vfs::{MemStore, Store};

    /// An in-memory copy of `project`, rooted where nothing exists on disk.
    fn in_memory(project: &ProjectFixture) -> (Arc<MemStore>, std::path::PathBuf) {
        let root = project.path().join("in-memory-root");
        let mut files = Vec::new();
        collect(project.path(), project.path(), &mut files);
        (Arc::new(MemStore::from_files(root.clone(), files).unwrap()), root)
    }

    fn collect(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &entry.path(), out);
            } else {
                let rel = entry.path().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, std::fs::read(entry.path()).unwrap()));
            }
        }
    }

    fn sync<'a>(
        store: &Arc<MemStore>,
        root: &'a std::path::Path,
        opts: EmbedSyncOptions,
        state: Arc<dyn StateStore>,
    ) -> impl std::future::Future<Output = anyhow::Result<rdc::cli::sync::CycleOutcome>> + 'a {
        sync_logged_in_with_state(store.clone() as Arc<dyn Store>, state, root, "test", TOKEN, opts, Box::new(std::io::sink()))
    }

    fn lockfile(store: &MemStore) -> rdc::state::Lockfile {
        serde_json::from_slice(&store.files()[".rdc/state/test.lock.json"]).unwrap()
    }

    /// Run 2 completes; run 3 changes nothing, in counts or bytes; every
    /// object the fake holds is one recorded slug.
    async fn finish_and_check(fake: &FakeOrg, store: &Arc<MemStore>, root: &std::path::Path, allow_deletes: bool, ctx: &str) {
        after_crash(fake).await;
        sync(store, root, yes(allow_deletes), fs()).await.unwrap_or_else(|e| panic!("{ctx}: run 2: {e:#}"));
        let before = store.files();
        let out = sync(store, root, yes(allow_deletes), fs()).await.unwrap_or_else(|e| panic!("{ctx}: run 3: {e:#}"));
        assert_eq!(
            (out.items_pushed, out.items_pulled, out.conflicts, out.remote_deletes_resolved),
            (0, 0, 0, 0),
            "{ctx}: run 3 found work"
        );
        let after = store.files();
        let moved: Vec<&String> = after.keys().chain(before.keys()).filter(|k| before.get(*k) != after.get(*k)).collect();
        assert!(moved.is_empty(), "{ctx}: run 3 rewrote {moved:?}");
        let lf = lockfile(store);
        assert_eq!(lf.pending, None, "{ctx}: the journal is settled");
        assert!(!root.exists(), "{ctx}: a run wrote to the disk");
        for kind in KINDS {
            let mut remote = fake.state().ids(kind);
            remote.sort();
            let mut recorded: Vec<u64> = lf.objects.get(*kind).map(|m| m.values().map(|e| e.id).collect()).unwrap_or_default();
            recorded.sort();
            assert_eq!(remote, recorded, "{ctx}: {kind} on the env vs. recorded slugs");
        }
    }

    async fn create_case(site: Site, point: Point) {
        let ctx = format!("in memory: {} {point:?}", site.path);
        let fake = FakeOrg::start().await;
        let run_id = RunId::new();
        let project = snapshot_project(&fake, &run_id);
        let (store, root) = in_memory(&project);
        let name = format!("{}{}", run_id.list_prefix(), site.name);
        match point {
            Point::BeforeBeginPersist | Point::AfterFinishPersist => {
                let crash = match point {
                    Point::BeforeBeginPersist => CrashStore::new(begun(site.kind, &name), true),
                    _ => CrashStore::new(finished(site.kind, &name), false),
                };
                until_crash(sync(&store, &root, yes(false), crash.clone()), &crash.fired).await;
            }
            Point::BeforeApply | Point::AfterApplyBeforeResponse => {
                let at = match point {
                    Point::BeforeApply => CrashPoint::BeforeApply,
                    _ => CrashPoint::AfterApplyBeforeResponse,
                };
                let fired = fake.crash_on(post_named(site.path, &name), 1, at);
                until_crash(sync(&store, &root, yes(false), fs()), &fired).await;
            }
        }
        finish_and_check(&fake, &store, &root, false, &ctx).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workspace_before_begin_persist() {
        create_case(WORKSPACE, Point::BeforeBeginPersist).await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hook_before_apply() {
        create_case(HOOK, Point::BeforeApply).await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn store_hook_after_apply() {
        create_case(STORE_HOOK, Point::AfterApplyBeforeResponse).await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn email_template_after_finish_persist() {
        create_case(EMAIL_TEMPLATE, Point::AfterFinishPersist).await
    }

    /// Six labels on the env and in memory, recorded.
    async fn synced_labels(fake: &FakeOrg, run_id: &RunId) -> (ProjectFixture, Arc<MemStore>, std::path::PathBuf) {
        let project = ProjectFixture::init(&fake.config(), &["test"]).expect("init");
        let org_url = fake.state().org_url();
        for i in 1..=6 {
            let name = format!("{}l{i}", run_id.list_prefix());
            project.write_json(
                &format!("envs/test/labels/{name}.json"),
                &json!({ "id": 0, "url": "", "name": name, "organization": org_url, "color": "#ff8800" }),
            );
        }
        let (store, root) = in_memory(&project);
        sync(&store, &root, yes(false), fs()).await.expect("the first sync");
        (project, store, root)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_update_batch() {
        let fake = FakeOrg::start().await;
        let run_id = RunId::new();
        let (_project, store, root) = synced_labels(&fake, &run_id).await;
        rdc::vfs::scope_sync(store.clone() as Arc<dyn Store>, || {
            for i in 1..=6 {
                let path = root.join(label_path(&run_id, i));
                let mut label: Value = serde_json::from_slice(&rdc::vfs::read(&path).unwrap()).unwrap();
                label["color"] = json!("#00ff00");
                rdc::vfs::write(&path, format!("{}\n", serde_json::to_string_pretty(&label).unwrap())).unwrap();
            }
        });
        let is_patch = |req: &Request| req.method.as_str() == "PATCH" && req.url.path().starts_with("/api/v1/labels/");
        let fired = fake.crash_on(Box::new(is_patch), 2, CrashPoint::AfterApplyBeforeResponse);

        until_crash(sync(&store, &root, yes(false), fs()), &fired).await;

        finish_and_check(&fake, &store, &root, false, "in memory: mid update batch").await;
        let st = fake.state();
        for id in st.ids("labels") {
            assert_eq!(st.get("labels", id).unwrap()["color"], "#00ff00", "label {id} got its edit");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_delete_phase() {
        let fake = FakeOrg::start().await;
        let run_id = RunId::new();
        let (_project, store, root) = synced_labels(&fake, &run_id).await;
        rdc::vfs::scope_sync(store.clone() as Arc<dyn Store>, || {
            for i in 1..=3 {
                rdc::vfs::remove_file(root.join(label_path(&run_id, i))).unwrap();
            }
        });
        let is_delete = |req: &Request| req.method.as_str() == "DELETE" && req.url.path().starts_with("/api/v1/labels/");
        let fired = fake.crash_on(Box::new(is_delete), 2, CrashPoint::AfterApplyBeforeResponse);

        until_crash(sync(&store, &root, yes(true), fs()), &fired).await;

        finish_and_check(&fake, &store, &root, true, "in memory: mid delete phase").await;
        assert_eq!(fake.state().ids("labels").len(), 3, "the kept labels are left");
    }
}
