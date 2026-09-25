use crate::support::assert_local::{load_lockfile, queue_file_path};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. No golden, so nothing to refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_queue_namesake() {
    let fake = crate::support::fake::FakeOrg::start().await;
    queue_namesake(&fake.config()).await;
}

/// The live twin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_queue_namesake() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    queue_namesake(&cfg).await;
}

/// A queue deleted on the env and replaced, in the same sync, by a new queue
/// of the same name is pulled in that one sync.
///
/// The classifier named the newcomer while the deleted queue still held the
/// slug, so it got the next free one. The remote-delete phase then released
/// the slug, and the pull driver, re-deriving, gave the newcomer the freed
/// slug instead — one the pull subset did not contain, so the queue was
/// skipped. Nothing referencing it could portabilize, and it took three syncs
/// to settle: the second pulled the queue, the third fixed its own `url`
/// (the deleted queue's emptied directory shadowed the new one).
///
/// A hook that points at the newcomer and conflicts locally is adopted from
/// the env through the conflict resolver, which must write the newcomer's
/// `rdc://` ref under the slug the pull records — not its URL.
async fn queue_namesake(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up.
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");

    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "initial pull failed: {}", combined(&pull));
    let prefix = run_id.list_prefix();

    // Replace the second "Invoices" queue with a namesake in the same
    // workspace, and point a hook at the newcomer.
    let old_id = index.id("queue-invoices-secondary").expect("seeded queue id");
    let old_slug = load_lockfile(project.path(), "test")
        .expect("lockfile")
        .slug_for_id("queues", old_id)
        .expect("seeded queue")
        .to_string();
    let old_dir = queue_file_path(project.path(), "test", &old_slug, "queue.json")
        .expect("the seeded queue's queue.json")
        .parent()
        .unwrap()
        .to_path_buf();
    client.delete("queue", old_id).await.expect("delete the queue");
    let (_, schema_url) = client
        .create(
            "schema",
            &serde_json::json!({ "name": run_id.prefix("Invoices"), "content": [] }),
        )
        .await
        .expect("create a schema");
    let (new_id, new_url) = client
        .create(
            "queue",
            &serde_json::json!({
                "name": run_id.prefix("Invoices"),
                "workspace": index.url("workspace", "ws-secondary").unwrap(),
                "schema": schema_url,
            }),
        )
        .await
        .expect("create the namesake queue");
    let main_url = index.url("queue", "queue-invoices-main").unwrap().to_string();
    let hook_id = index.id("hook-validator").expect("seeded hook id");
    client
        .patch_fields("hook", hook_id, serde_json::json!({ "queues": [main_url, new_url] }))
        .await
        .expect("point the hook at the namesake");
    // Edit the hook's `queues` locally too — the same field, so no auto-merge
    // — and the env's side is adopted through the conflict resolver. That
    // writes the namesake's ref under the slug the projection predicts, which
    // must be the slug the pull then records.
    let hook_slug = load_lockfile(project.path(), "test")
        .expect("lockfile")
        .slug_for_id("hooks", hook_id)
        .expect("seeded hook")
        .to_string();
    let hook_rel = format!("envs/test/hooks/{hook_slug}.json");
    let mut local_hook = project.read_json(&hook_rel);
    local_hook["queues"] = serde_json::json!([]);
    project.write_json(&hook_rel, &local_hook);

    let out = project.run_rdc(&["sync", "test", "--no-push", "--conflict", "use-remote"]);
    assert!(out.status.success(), "sync failed: {}", combined(&out));

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let slug = lf
        .slug_for_id("queues", new_id)
        .unwrap_or_else(|| panic!("the namesake queue must be pulled in one sync:\n{}", combined(&out)))
        .to_string();
    let queue_json = queue_file_path(project.path(), "test", &slug, "queue.json")
        .expect("the namesake's queue.json");
    let queue: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&queue_json).unwrap()).unwrap();
    assert_eq!(queue["url"], format!("rdc://queues/{slug}"), "{}", queue_json.display());
    let hook = project.read_json(&hook_rel);
    assert!(
        hook["queues"].as_array().unwrap().contains(&format!("rdc://queues/{slug}").into()),
        "the hook's ref to the namesake must be portable: {}",
        hook["queues"]
    );
    assert!(
        !old_dir.exists(),
        "the deleted queue's dir must go, not linger empty: {}",
        old_dir.display()
    );
    assert_converged(&project, "test", &prefix, "a queue replaced by a namesake");

    drop(teardown);
}
