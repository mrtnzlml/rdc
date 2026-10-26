//! A watch must survive a local object that names an object with no file yet.
//!
//! An editor (or an agent) writing a snapshot by hand writes one file at a
//! time, so a watch cycle can fire between a file that names
//! `rdc://<kind>/<slug>` and the file that defines it. Such a cycle holds the
//! naming object back instead of failing, and a later cycle pushes it once
//! the object it names exists.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdc::cli::sync::embed::{watch_logged, Prompt, PromptRoute};
use rdc::cli::sync::watch::CancelToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Answers no prompt: a test that reaches one has gone wrong.
struct NoPrompts;
impl PromptRoute for NoPrompts {
    fn ask<'a>(&'a self, prompt: &'a Prompt) -> rdc::rt::BoxFuture<'a, Option<String>> {
        panic!("unexpected prompt: {prompt:?}");
    }
}

#[derive(Clone, Default)]
struct SharedLog(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl SharedLog {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn page(results: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "pagination": { "total": results.len(), "total_pages": 1, "next": null, "previous": null },
        "results": results
    })
}

fn write_json(p: &Path, v: serde_json::Value) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut b = serde_json::to_vec_pretty(&v).unwrap();
    b.push(b'\n');
    std::fs::write(p, b).unwrap();
}

/// An env that starts empty and lets a workspace, a schema, a queue, a hook and
/// a label be created, each listed only after its POST. A create without a
/// `name` is refused with 400, and every engine create with 403 (engines not
/// writable on the plan).
struct Env {
    server: MockServer,
    cwd: tempfile::TempDir,
}

impl Env {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/organizations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 1, "name": "test-org", "url": format!("{base}/organizations/1")
            })))
            .mount(&server)
            .await;
        for kind in [
            "inboxes", "rules", "engines", "engine_fields", "workflows",
            "workflow_steps", "email_templates", "saved_views",
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{kind}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![])))
                .mount(&server)
                .await;
        }

        let ws_url = format!("{base}/workspaces/10");
        let schema_url = format!("{base}/schemas/20");
        let queue_url = format!("{base}/queues/30");
        let ws = serde_json::json!({
            "id": 10, "url": ws_url, "name": "Main",
            "organization": format!("{base}/organizations/1"), "queues": [],
            "modified_at": "2026-05-01T08:00:00Z"
        });
        let schema = serde_json::json!({
            "id": 20, "url": schema_url, "name": "Invoices", "queues": [queue_url],
            "content": [], "modified_at": "2026-05-01T08:00:00Z"
        });
        let queue = serde_json::json!({
            "id": 30, "url": queue_url, "name": "Invoices", "workspace": ws_url,
            "schema": schema_url, "modified_at": "2026-05-01T08:00:00Z"
        });
        let hook = serde_json::json!({
            "id": 40, "url": format!("{base}/hooks/40"), "name": "Notify", "type": "webhook",
            "queues": [queue_url], "events": ["annotation_content"],
            "config": { "url": "https://example.com/notify" },
            "modified_at": "2026-05-01T08:00:00Z"
        });
        let label = serde_json::json!({
            "id": 50, "url": format!("{base}/labels/50"), "name": "Urgent",
            "organization": format!("{base}/organizations/1")
        });
        for (p, obj) in [
            ("/workspaces", ws),
            ("/schemas", schema.clone()),
            ("/queues", queue),
            ("/hooks", hook),
            ("/labels", label),
        ] {
            let created = Arc::new(AtomicBool::new(false));
            let (flag, listed) = (created.clone(), obj.clone());
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(move |_: &Request| {
                    let results =
                        if flag.load(Ordering::SeqCst) { vec![listed.clone()] } else { vec![] };
                    ResponseTemplate::new(200).set_body_json(page(results))
                })
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(p))
                .respond_with(move |req: &Request| {
                    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                    if body.get("name").is_none() {
                        return ResponseTemplate::new(400)
                            .set_body_json(serde_json::json!({ "name": ["This field is required."] }));
                    }
                    created.store(true, Ordering::SeqCst);
                    ResponseTemplate::new(201).set_body_json(&obj)
                })
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/engines"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(serde_json::json!({ "detail": "forbidden" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/schemas/20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&schema))
            .mount(&server)
            .await;

        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(
            cwd.path().join("rdc.toml"),
            format!("[envs.test]\napi_base = \"{base}\"\norg_id = 1\n"),
        )
        .unwrap();
        Env { server, cwd }
    }

    fn envs(&self) -> PathBuf {
        self.cwd.path().join("envs/test")
    }

    async fn posts(&self, p: &str) -> Vec<serde_json::Value> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method == http::Method::POST && r.url.path() == p)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// Run a watch while `steps` drives it, then stop it. Fails if the watch
    /// stops on its own.
    async fn watch<F: std::future::Future<Output = ()>>(
        &self,
        steps: impl FnOnce(SharedLog, Arc<Mutex<Option<String>>>) -> F,
    ) {
        let log = SharedLog::default();
        let cancel = CancelToken::new();
        let stopped = Arc::new(Mutex::new(None));
        let base = self.server.uri();
        // The watch future is not `Send`, so it runs on this task beside the
        // driver rather than on a spawned one.
        let watch = async {
            let r = watch_logged(
                self.cwd.path(),
                "test",
                &base,
                "tok".into(),
                Some(Duration::from_secs(1)),
                Box::new(log.clone()),
                Arc::new(NoPrompts),
                cancel.clone(),
                None,
            )
            .await;
            *stopped.lock().unwrap() = Some(format!("{r:?}"));
            r
        };
        let driver = async {
            steps(log.clone(), stopped.clone()).await;
            cancel.cancel();
        };
        let (res, ()) =
            tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(watch, driver) })
                .await
                .expect("the watch stops on cancel");
        res.expect("the watch ends cleanly");
    }
}

/// Poll `cond` every 100 ms for up to 20 s, failing early if the watch stops.
async fn wait_for<F, Fut>(what: &str, log: &SharedLog, stopped: &Mutex<Option<String>>, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if let Some(r) = stopped.lock().unwrap().as_ref() {
            panic!("the watch stopped waiting for {what}: {r}; log:\n{}", log.text());
        }
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}; log:\n{}", log.text());
}

fn write_workspace(env: &Env) {
    write_json(&env.envs().join("workspaces/main/workspace.json"), serde_json::json!({ "name": "Main" }));
}

fn write_queue(env: &Env) {
    write_json(
        &env.envs().join("workspaces/main/queues/invoices/queue.json"),
        serde_json::json!({
            "name": "Invoices",
            "workspace": "rdc://workspaces/main",
            "schema": "rdc://schemas/invoices"
        }),
    );
}

fn write_schema(env: &Env) {
    write_json(
        &env.envs().join("workspaces/main/queues/invoices/schema.json"),
        serde_json::json!({ "name": "Invoices", "content": [] }),
    );
}

/// A required ref: `POST /queues` cannot go out without its `schema`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_holds_back_a_queue_whose_schema_file_is_not_written_yet() {
    let env = Env::start().await;
    write_workspace(&env);
    write_queue(&env);

    let env = &env;
    env.watch(|log, stopped| async move {
        wait_for("the workspace create", &log, &stopped, || async {
            env.posts("/workspaces").await.len() == 1
        })
        .await;
        assert!(env.posts("/queues").await.is_empty(), "the queue must be held back");
        assert!(
            log.text().contains("rdc://schemas/invoices"),
            "the log must name the missing ref; log:\n{}",
            log.text()
        );

        write_schema(env);
        wait_for("the queue create", &log, &stopped, || async {
            env.posts("/queues").await.len() == 1
        })
        .await;
        assert_eq!(env.posts("/schemas").await.len(), 1);
        assert_eq!(env.posts("/workspaces").await.len(), 1, "the workspace is created once");
    })
    .await;
}

/// A deferrable ref: a hook can be created without its `queues`, so pushing it
/// early would create it unlinked and then fail the relink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_holds_back_a_hook_whose_queue_file_is_not_written_yet() {
    let env = Env::start().await;
    write_workspace(&env);
    write_json(
        &env.envs().join("hooks/notify.json"),
        serde_json::json!({
            "name": "Notify",
            "type": "webhook",
            "queues": ["rdc://queues/invoices"],
            "events": ["annotation_content"],
            "config": { "url": "https://example.com/notify" }
        }),
    );

    let env = &env;
    env.watch(|log, stopped| async move {
        wait_for("the workspace create", &log, &stopped, || async {
            env.posts("/workspaces").await.len() == 1
        })
        .await;
        assert!(env.posts("/hooks").await.is_empty(), "the hook must be held back");
        assert!(
            log.text().contains("rdc://queues/invoices"),
            "the log must name the missing ref; log:\n{}",
            log.text()
        );

        // The schema first, then the queue: the hook waits through both.
        write_schema(env);
        write_queue(env);
        wait_for("the hook create", &log, &stopped, || async {
            env.posts("/hooks").await.len() == 1
        })
        .await;
        let hook = &env.posts("/hooks").await[0];
        assert_eq!(
            hook["queues"],
            serde_json::json!([format!("{}/queues/30", env.server.uri())]),
            "the hook is created linked to its queue"
        );
    })
    .await;
}

fn write_queue_with_engine(env: &Env) {
    write_json(
        &env.envs().join("engines/extractor/engine.json"),
        serde_json::json!({ "name": "Extractor", "type": "extractor" }),
    );
    write_json(
        &env.envs().join("workspaces/main/queues/invoices/queue.json"),
        serde_json::json!({
            "name": "Invoices",
            "workspace": "rdc://workspaces/main",
            "schema": "rdc://schemas/invoices",
            "engine": "rdc://engines/extractor"
        }),
    );
}

/// The engine has a file, but the server refuses to create it. The queue that
/// names it must not be created without it: that create would also rewrite
/// queue.json from the server's answer and drop the `engine` line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_holds_back_a_queue_whose_engine_create_is_refused() {
    let env = Env::start().await;
    write_workspace(&env);
    write_schema(&env);
    write_queue_with_engine(&env);
    let queue_json = env.envs().join("workspaces/main/queues/invoices/queue.json");

    let env = &env;
    env.watch(|log, stopped| async move {
        wait_for("the schema create", &log, &stopped, || async {
            env.posts("/schemas").await.len() == 1
        })
        .await;
        wait_for("the hold warning", &log, &stopped, || async {
            log.text().contains("not pushed yet")
        })
        .await;
        assert!(env.posts("/queues").await.is_empty(), "the queue must be held back");
        assert!(log.text().contains("rdc://engines/extractor"), "log:\n{}", log.text());
        let on_disk = std::fs::read_to_string(&queue_json).unwrap();
        assert!(on_disk.contains("rdc://engines/extractor"), "queue.json lost its engine:\n{on_disk}");
    })
    .await;
}

/// The same refusal in a one-shot sync: the queue is not created, its file
/// keeps the ref, and the run fails naming it.
#[tokio::test]
async fn sync_refuses_a_queue_whose_engine_create_is_refused() {
    let env = Env::start().await;
    write_workspace(&env);
    write_schema(&env);
    write_queue_with_engine(&env);

    let err = rdc::cli::sync::embed::sync_logged(
        env.cwd.path(),
        "test",
        "tok",
        rdc::cli::sync::embed::EmbedSyncOptions::default(),
        Box::new(std::io::sink()),
    )
    .await
    .expect_err("the run must fail on the held queue");

    let msg = format!("{err:#}");
    assert!(msg.contains("queues/invoices") && msg.contains("rdc://engines/extractor"), "{msg}");
    assert!(env.posts("/queues").await.is_empty(), "the queue must not be created");
    let on_disk =
        std::fs::read_to_string(env.envs().join("workspaces/main/queues/invoices/queue.json")).unwrap();
    assert!(on_disk.contains("rdc://engines/extractor"), "queue.json lost its engine:\n{on_disk}");
}

/// A file the server rejects (400) does not stop the watch; fixing the file
/// does push it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_survives_a_rejected_create_and_pushes_the_fixed_file() {
    let env = Env::start().await;
    let label = env.envs().join("labels/urgent.json");
    write_json(&label, serde_json::json!({ "color": "red" }));

    let env = &env;
    env.watch(|log, stopped| async move {
        wait_for("the rejected create", &log, &stopped, || async {
            log.text().contains("cycle failed (fix the file and save it)")
        })
        .await;
        assert!(log.text().contains("This field is required"), "log:\n{}", log.text());

        write_json(&label, serde_json::json!({ "name": "Urgent", "color": "red" }));
        wait_for("the fixed create", &log, &stopped, || async {
            env.posts("/labels").await.iter().any(|b| b["name"] == "Urgent")
        })
        .await;
    })
    .await;
}

/// A file that is not valid JSON (a save half-way through an edit) does not
/// stop the watch either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_survives_a_file_that_is_not_valid_json() {
    let env = Env::start().await;
    let label = env.envs().join("labels/urgent.json");
    std::fs::create_dir_all(label.parent().unwrap()).unwrap();
    std::fs::write(&label, b"{\"name\": \"Urgent\",").unwrap();

    let env = &env;
    env.watch(|log, stopped| async move {
        wait_for("the refused cycle", &log, &stopped, || async {
            log.text().contains("cycle failed")
        })
        .await;

        write_json(&label, serde_json::json!({ "name": "Urgent" }));
        wait_for("the fixed create", &log, &stopped, || async {
            env.posts("/labels").await.len() == 1
        })
        .await;
    })
    .await;
}
