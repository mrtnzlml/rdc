//! In-memory twin of `tests/embed_sync_two_way.rs`: nothing touches the real
//! disk. A locally-edited object is PUSHED by `sync_logged`'s default options,
//! where `no_push: true` (tests/embed_sync.rs) leaves it alone. This is the
//! behaviour change the desktop app is adopting; pinned here so it cannot
//! regress into silence.

use rdc::cli::sync::embed::{
    sync_logged_in, watch_logged_in, CycleTrigger, EmbedSyncOptions, Prompt, PromptRoute,
};
use rdc::cli::sync::watch::CancelToken;
use rdc::vfs::{MemStore, Store};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn embed_sync_logged_in_pushes_a_local_edit_in_memory() {
    let server = MockServer::start().await;

    // Minimal Rossum surface, copied from tests/embed_sync.rs: organization
    // GET + empty listings for every kind EXCEPT "workspaces" and "queues",
    // which this test overrides below. A queue is only reachable through
    // the classifier once its `workspace` URL resolves to a slug (see the
    // queue-derivation pass in `cli::sync::mod.rs`), so the workspace
    // listing must return a real workspace rather than staying empty.
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "test-org", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    for kind in [
        "schemas", "inboxes", "hooks", "rules", "labels", "engines", "engine_fields",
        "workflows", "workflow_steps", "email_templates", "saved_views",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }

    let ws_url = format!("{}/workspaces/1", server.uri());
    let queue_url = format!("{}/queues/1", server.uri());

    Mock::given(method("GET"))
        .and(path("/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "id": 1,
                "url": ws_url,
                "name": "main",
                "organization": format!("{}/organizations/1", server.uri()),
                "queues": [queue_url],
            }],
            "pagination": {"next": null, "total_pages": 1, "total": 1}
        })))
        .mount(&server)
        .await;

    // The same queue body is served on every GET /queues call, across both
    // syncs below: the pull that first learns about it and the push-side
    // drift check the second sync performs before PATCHing.
    Mock::given(method("GET"))
        .and(path("/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "id": 1,
                "url": queue_url,
                "name": "Invoices",
                "workspace": ws_url,
            }],
            "pagination": {"next": null, "total_pages": 1, "total": 1}
        })))
        .mount(&server)
        .await;

    // The PATCH the second sync's LocalEdit push must issue exactly once.
    // `MockServer` verifies `.expect(1)` on drop, so no explicit call-count
    // assertion is needed below.
    Mock::given(method("PATCH"))
        .and(path("/queues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "url": queue_url,
            "name": "Invoices (locally edited)",
            "workspace": ws_url,
        })))
        .expect(1)
        .mount(&server)
        .await;

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
                    r#"[envs.test]
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

    // First sync: the queue is unknown (no local file, no lockfile entry),
    // so this pulls it fresh — writing `envs/test/workspaces/main/queues/
    // invoices/queue.json` and a matching lockfile entry. This makes the
    // object KNOWN rather than new for the second sync below, using rdc's
    // own pull logic rather than a hand-built fixture (the object's slug,
    // on-disk shape, and lockfile hash are exactly what the second sync
    // expects, because the same code produced them).
    sync_logged_in(
        store.clone() as Arc<dyn Store>,
        &root,
        "test",
        "tok",
        EmbedSyncOptions::default(),
        Box::new(std::io::sink()),
    )
    .await
    .expect("first sync_logged should pull the queue and workspace");

    let queue_key = "envs/test/workspaces/main/queues/invoices/queue.json";
    let raw = store
        .files()
        .get(queue_key)
        .cloned()
        .unwrap_or_else(|| panic!("first sync should have written {queue_key}"));

    // Edit the local file so it diverges from the mocked remote body — the
    // remote continues to serve the original "Invoices" name unchanged.
    let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    v["name"] = serde_json::Value::String("Invoices (locally edited)".to_string());
    // Edit through the store itself, as a host would; the tree has no disk.
    rdc::vfs::scope(store.clone() as Arc<dyn Store>, async {
        rdc::vfs::write(
            root.join(queue_key),
            format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
        )
        .unwrap();
    })
    .await;

    // Second sync: the queue is now a LocalEdit (local diverges from base,
    // remote still matches base) — `sync_logged`'s default options push it.
    sync_logged_in(
        store.clone() as Arc<dyn Store>,
        &root,
        "test",
        "tok",
        EmbedSyncOptions::default(),
        Box::new(std::io::sink()),
    )
    .await
    .expect("second sync_logged_in should push the local edit");

    assert!(!root.exists(), "the sync touched the real disk");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "the sync wrote under the tempdir");
}

#[derive(Clone, Default)]
struct SharedLog(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for SharedLog {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct NoAnswer;
impl PromptRoute for NoAnswer {
    fn ask<'a>(&'a self, _: &'a Prompt) -> rdc::rt::BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
}

/// A host trigger after a store edit makes the watch push it, and no file
/// event source (`notify`) or disk is involved.
#[tokio::test]
async fn watch_in_memory_never_touches_disk() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "test-org", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;
    for kind in [
        "schemas", "inboxes", "hooks", "rules", "labels", "engines", "engine_fields",
        "workflows", "workflow_steps", "email_templates", "saved_views",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }
    let ws_url = format!("{}/workspaces/1", server.uri());
    let queue_url = format!("{}/queues/1", server.uri());
    Mock::given(method("GET"))
        .and(path("/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "id": 1, "url": ws_url, "name": "main",
                "organization": format!("{}/organizations/1", server.uri()),
                "queues": [queue_url],
            }],
            "pagination": {"next": null, "total_pages": 1, "total": 1}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"id": 1, "url": queue_url, "name": "Invoices", "workspace": ws_url}],
            "pagination": {"next": null, "total_pages": 1, "total": 1}
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/queues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "url": queue_url, "name": "Invoices (locally edited)", "workspace": ws_url,
        })))
        .expect(1)
        .mount(&server)
        .await;

    // A real tempdir holds the virtual root, so a stray native write would
    // succeed and be caught below.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = Arc::new(
        MemStore::from_files(
            root.clone(),
            [(
                "rdc.toml".to_string(),
                format!("[envs.test]\napi_base = \"{}\"\norg_id = 1\n", server.uri())
                    .into_bytes(),
            )],
        )
        .unwrap(),
    );

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let cancel = CancelToken::new();
    let log = SharedLog::default();
    let api_base = server.uri();
    let watch = watch_logged_in(
        store.clone() as Arc<dyn Store>,
        &root,
        "test",
        &api_base,
        "tok".to_string(),
        None,
        Box::new(log.clone()),
        Arc::new(NoAnswer),
        cancel.clone(),
        None,
        Some(rx),
    );

    let driver = async {
        let queue_key = "envs/test/workspaces/main/queues/invoices/queue.json";
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        // The watch logs "start" once its initial reconcile has pulled the
        // queue into the store and the event loop is about to run.
        while !String::from_utf8_lossy(&log.0.lock().unwrap()).contains("start envs/test") {
            assert!(std::time::Instant::now() < deadline, "initial pull never finished");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let raw = store.files().get(queue_key).cloned().expect("initial pull wrote the queue");

        let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        v["name"] = serde_json::Value::String("Invoices (locally edited)".to_string());
        rdc::vfs::scope(store.clone() as Arc<dyn Store>, async {
            rdc::vfs::write(
                root.join(queue_key),
                format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
            )
            .unwrap();
        })
        .await;
        tx.send(CycleTrigger::FileEvent).await.unwrap();

        loop {
            let patched = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.method.as_str() == "PATCH" && r.url.path() == "/queues/1");
            if patched {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the edit was never pushed");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        cancel.cancel();
    };

    let (res, ()) = tokio::join!(watch, driver);
    res.expect("watch stops cleanly");
    assert!(!root.exists(), "the watch touched the real disk");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "the watch wrote under the tempdir");
}

/// A 401 in a watch is answered with `force_relogin` from the credentials in
/// the in-memory `secrets/` file; the fresh token lands back in that file and
/// nothing reaches the real disk.
#[tokio::test]
async fn watch_in_memory_relogs_in_after_a_401() {
    let server = MockServer::start().await;
    // The initial reconcile (which does not refresh a token) must pass, so the
    // stale token is only revoked once the driver flips this flag.
    let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let revoked_in_mock = revoked.clone();
    let org_url = format!("{}/organizations/1", server.uri());
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(move |req: &wiremock::Request| {
            let stale = req.headers.get("authorization").map(|v| v.to_str().unwrap()) == Some("token stale");
            if stale && revoked_in_mock.load(std::sync::atomic::Ordering::SeqCst) {
                ResponseTemplate::new(401).set_body_json(serde_json::json!({"detail": "Invalid token."}))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": 1, "name": "test-org", "url": org_url
                }))
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh"
        })))
        .expect(1)
        .mount(&server)
        .await;
    for kind in [
        "workspaces", "queues", "schemas", "inboxes", "hooks", "rules", "labels", "engines",
        "engine_fields", "workflows", "workflow_steps", "email_templates", "saved_views",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let store = Arc::new(
        MemStore::from_files(
            root.clone(),
            [
                (
                    "rdc.toml".to_string(),
                    format!("[envs.test]\napi_base = \"{}\"\norg_id = 1\n", server.uri())
                        .into_bytes(),
                ),
                // The shape `secrets::save_password_credentials` writes.
                (
                    "secrets/test.secrets.json".to_string(),
                    br#"{"username": "alice", "password": "pw"}"#.to_vec(),
                ),
            ],
        )
        .unwrap(),
    );

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let cancel = CancelToken::new();
    let log = SharedLog::default();
    let api_base = server.uri();
    let watch = watch_logged_in(
        store.clone() as Arc<dyn Store>,
        &root,
        "test",
        &api_base,
        "stale".to_string(),
        None,
        Box::new(log.clone()),
        Arc::new(NoAnswer),
        cancel.clone(),
        None,
        Some(rx),
    );

    let driver = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !String::from_utf8_lossy(&log.0.lock().unwrap()).contains("start envs/test") {
            assert!(
                std::time::Instant::now() < deadline,
                "the watch never recovered from the 401: {}",
                String::from_utf8_lossy(&log.0.lock().unwrap())
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        revoked.store(true, std::sync::atomic::Ordering::SeqCst);
        tx.send(CycleTrigger::Poll).await.unwrap();
        while !String::from_utf8_lossy(&log.0.lock().unwrap()).contains("token expired") {
            assert!(std::time::Instant::now() < deadline, "the 401 never reached the refresher");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Give the retried cycle time to finish before stopping the watch.
        while !server.received_requests().await.unwrap().iter().any(|r| {
            r.url.path() == "/organizations/1"
                && r.headers.get("authorization").map(|v| v.to_str().unwrap()) == Some("token fresh")
        }) {
            assert!(std::time::Instant::now() < deadline, "the fresh token was never used");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        cancel.cancel();
    };

    let (res, ()) = tokio::join!(watch, driver);
    res.expect("watch stops cleanly");

    let secrets: serde_json::Value = serde_json::from_slice(
        store.files().get("secrets/test.secrets.json").expect("secrets file kept"),
    )
    .unwrap();
    assert_eq!(secrets["api_token"], "fresh", "the refreshed token is written to the store");
    assert_eq!(secrets["username"], "alice", "the credentials are kept");
    assert!(!root.exists(), "the watch touched the real disk");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "the watch wrote under the tempdir");
}
