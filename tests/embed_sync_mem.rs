//! `tests/embed_sync.rs`, run on an in-memory project: the pull lands in a
//! `MemStore` and nothing touches the real disk.

use rdc::cli::sync::embed::{sync_logged_in, sync_logged_in_with_state, EmbedSyncOptions};
use rdc::paths::Paths;
use rdc::state::store::{FsStateStore, StateStore};
use rdc::vfs::{MemStore, Store};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn embed_sync_no_push_pulls_into_memory() {
    let server = MockServer::start().await;

    // Minimal Rossum surface: organization GET + empty listings.
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "test-org", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    for kind in [
        "workspaces", "queues", "schemas", "inboxes", "hooks", "rules",
        "labels", "engines", "engine_fields", "workflows", "workflow_steps",
        "email_templates", "saved_views",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }

    // A real tempdir holds the virtual root, so a stray native write would
    // succeed and be caught below.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = Arc::new(
        MemStore::from_files(
            root.clone(),
            [(
                "rdc.toml".to_string(),
                format!(
                    r#"[envs.main]
api_base = "{}"
org_id = 1
"#,
                    server.uri()
                )
                .into_bytes(),
            )],
        )
        .unwrap(),
    );

    let opts = EmbedSyncOptions { interactive: false, no_push: true, ..Default::default() };
    sync_logged_in(
        store.clone() as Arc<dyn Store>,
        &root,
        "main",
        "fake-token",
        opts,
        Box::new(std::io::sink()),
    )
    .await
    .expect("no-push sync succeeds");

    let files = store.files();
    assert!(files.contains_key("envs/main/_index.md"));
    assert!(files.contains_key("envs/main/organization.json"));
    assert!(!root.exists(), "the sync touched the real disk");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "the sync wrote under the tempdir");
}

/// Counts the calls a sync makes on its state store.
#[derive(Default)]
struct CountingState {
    hydrates: AtomicUsize,
    checkpoints: AtomicUsize,
}

impl StateStore for CountingState {
    fn hydrate<'a>(&'a self, _: &'a Paths) -> rdc::rt::BoxFuture<'a, anyhow::Result<()>> {
        self.hydrates.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn checkpoint<'a>(&'a self, _: &'a Paths) -> rdc::rt::BoxFuture<'a, anyhow::Result<()>> {
        self.checkpoints.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// Organization GET plus an empty listing for every kind.
async fn mount_empty_org(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "test-org", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(server)
        .await;
    for kind in [
        "workspaces", "queues", "schemas", "inboxes", "hooks", "rules",
        "labels", "engines", "engine_fields", "workflows", "workflow_steps",
        "email_templates", "saved_views",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(server)
            .await;
    }
}

/// An in-memory project with env `main` pointing at `server`, plus `extra`.
fn project(server: &MockServer, root: &std::path::Path, extra: &[(&str, String)]) -> Arc<MemStore> {
    let mut files = vec![(
        "rdc.toml".to_string(),
        format!("[envs.main]\napi_base = \"{}\"\norg_id = 1\n", server.uri()).into_bytes(),
    )];
    files.extend(extra.iter().map(|(p, c)| (p.to_string(), c.clone().into_bytes())));
    Arc::new(MemStore::from_files(root.to_path_buf(), files).unwrap())
}

#[tokio::test]
async fn checkpoint_called_after_every_lockfile_save() {
    let server = MockServer::start().await;
    mount_empty_org(&server).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = project(&server, &root, &[]);
    let state = Arc::new(CountingState::default());

    let opts = EmbedSyncOptions { interactive: false, no_push: true, ..Default::default() };
    sync_logged_in_with_state(
        store.clone() as Arc<dyn Store>,
        state.clone(),
        &root,
        "main",
        "fake-token",
        opts,
        Box::new(std::io::sink()),
    )
    .await
    .expect("no-push sync succeeds");

    assert_eq!(state.hydrates.load(Ordering::SeqCst), 1);
    // Nothing is journaled yet, so the run's one save is the final commit;
    // the tree is also checkpointed before the first write and after
    // `_index.md`, for a store that keeps the env tree.
    assert_eq!(state.checkpoints.load(Ordering::SeqCst), 3);
    assert!(store.files().contains_key(".rdc/state/main.lock.json"));
}

#[tokio::test]
async fn checkpoint_runs_when_a_push_fails_midway() {
    let server = MockServer::start().await;
    mount_empty_org(&server).await;
    // Creates run in slug order: `a` succeeds, `b` is refused.
    Mock::given(method("POST"))
        .and(path("/labels"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 11,
            "url": format!("{}/labels/11", server.uri()),
            "name": "a",
            "organization": format!("{}/organizations/1", server.uri()),
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/labels"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"detail": "no"})))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = project(
        &server,
        &root,
        &[
            ("envs/main/labels/a.json", "{\n  \"name\": \"a\"\n}\n".to_string()),
            ("envs/main/labels/b.json", "{\n  \"name\": \"b\"\n}\n".to_string()),
        ],
    );
    let state = Arc::new(CountingState::default());

    let opts = EmbedSyncOptions { interactive: false, ..Default::default() };
    let err = sync_logged_in_with_state(
        store.clone() as Arc<dyn Store>,
        state.clone(),
        &root,
        "main",
        "fake-token",
        opts,
        Box::new(std::io::sink()),
    )
    .await
    .expect_err("the second create fails the run");
    assert!(format!("{err:#}").contains("creating 'b'"), "{err:#}");

    assert_eq!(state.hydrates.load(Ordering::SeqCst), 1);
    // The start of the run, begin a, finish a, begin b, abandon b (refused),
    // then the error-path commit.
    assert_eq!(state.checkpoints.load(Ordering::SeqCst), 6, "the error path commits");
    let lock: serde_json::Value =
        serde_json::from_slice(&store.files()[".rdc/state/main.lock.json"]).unwrap();
    assert_eq!(lock["objects"]["labels"]["a"]["id"], 11, "partial progress saved: {lock}");
    assert!(lock["objects"]["labels"].get("b").is_none());
    assert!(lock.get("pending").is_none(), "no op is left: {lock}");
}

#[tokio::test]
async fn checkpoint_called_three_times_for_one_create() {
    let server = MockServer::start().await;
    mount_empty_org(&server).await;
    Mock::given(method("POST"))
        .and(path("/labels"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 11,
            "url": format!("{}/labels/11", server.uri()),
            "name": "a",
            "organization": format!("{}/organizations/1", server.uri()),
        })))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store =
        project(&server, &root, &[("envs/main/labels/a.json", "{\n  \"name\": \"a\"\n}\n".to_string())]);
    let state = Arc::new(CountingState::default());

    sync_logged_in_with_state(
        store.clone() as Arc<dyn Store>,
        state.clone(),
        &root,
        "main",
        "fake-token",
        // No pull: the mock's listings stay empty, so a read-back would see
        // the new label as deleted remotely.
        EmbedSyncOptions { interactive: false, no_pull: true, ..Default::default() },
        Box::new(std::io::sink()),
    )
    .await
    .expect("the sync creates the label");

    // begin, finish, commit — at least; the commit is the last.
    assert!(state.checkpoints.load(Ordering::SeqCst) >= 3, "{}", state.checkpoints.load(Ordering::SeqCst));
    let lock: serde_json::Value =
        serde_json::from_slice(&store.files()[".rdc/state/main.lock.json"]).unwrap();
    assert_eq!(lock["objects"]["labels"]["a"]["id"], 11);
    assert!(lock.get("pending").is_none(), "the commit settles the op: {lock}");
}

/// A journal left by a run that died is recovered by the next sync at once,
/// even non-interactively (`--yes`): its create never landed (nothing on the
/// env matches), so the op is dropped and no journal is left.
#[tokio::test]
async fn a_crashed_runs_journal_is_recovered_at_once() {
    let server = MockServer::start().await;
    mount_empty_org(&server).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let lock = serde_json::json!({
        "version": 4,
        "objects": {},
        "pending": {
            "run": "0123456789abcdef0123456789abcdef",
            "ops": [{
                "seq": 1, "kind": "labels", "slug": "a", "fields": {},
                "started_at_unix": 0, "id": null
            }]
        }
    });
    let store = project(&server, &root, &[(".rdc/state/main.lock.json", lock.to_string())]);

    sync_logged_in_with_state(
        store.clone() as Arc<dyn Store>,
        Arc::new(FsStateStore),
        &root,
        "main",
        "fake-token",
        EmbedSyncOptions { interactive: false, ..Default::default() },
        Box::new(std::io::sink()),
    )
    .await
    .expect("the journal is recovered");
    let searched = server.received_requests().await.unwrap().iter().any(|r| r.method.as_str() == "GET" && r.url.path() == "/labels");
    assert!(searched, "recovery searched the env for the lost create");
    let after: serde_json::Value =
        serde_json::from_slice(&store.files()[".rdc/state/main.lock.json"]).unwrap();
    assert!(after.get("pending").is_none(), "the journal is settled: {after}");
}

/// Fails every checkpoint once `_index.md` exists: only the save after it.
struct FailsAfterIndex;

impl StateStore for FailsAfterIndex {
    fn hydrate<'a>(&'a self, _: &'a Paths) -> rdc::rt::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn checkpoint<'a>(&'a self, paths: &'a Paths) -> rdc::rt::BoxFuture<'a, anyhow::Result<()>> {
        let indexed = rdc::vfs::exists(paths.env_root().join("_index.md"));
        Box::pin(async move { if indexed { anyhow::bail!("refused by the test") } else { Ok(()) } })
    }
}

#[derive(Clone, Default)]
struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The save after `_index.md` only makes a derived file durable: the commit
/// before it already saved the state and the tree. Its failure is a warning,
/// not a failed sync.
#[tokio::test]
async fn a_failed_save_after_the_index_only_warns() {
    let server = MockServer::start().await;
    mount_empty_org(&server).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = project(&server, &root, &[]);
    let log = Buf::default();
    let opts = EmbedSyncOptions { interactive: false, no_push: true, ..Default::default() };
    sync_logged_in_with_state(store.clone() as Arc<dyn Store>, Arc::new(FailsAfterIndex), &root, "main", "fake-token", opts, Box::new(log.clone()))
        .await
        .expect("the sync is done");
    let out = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("_index.md") && out.contains("refused by the test"), "{out}");
}
