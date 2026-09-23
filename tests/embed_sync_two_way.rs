//! A locally-edited object is PUSHED by `sync_logged`'s default options,
//! where `no_push: true` (tests/embed_sync.rs) leaves it alone. This is the
//! behaviour change the desktop app is adopting; pinned here so it cannot
//! regress into silence.

use rdc::cli::sync::embed::{sync_logged, EmbedSyncOptions};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn embed_sync_logged_pushes_a_local_edit() {
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

    let tmp = tempdir().unwrap();
    let cwd = tmp.path();

    std::fs::write(
        cwd.join("rdc.toml"),
        format!(
            r#"[envs.test]
api_base = "{}"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();

    // First sync: the queue is unknown (no local file, no lockfile entry),
    // so this pulls it fresh — writing `envs/test/workspaces/main/queues/
    // invoices/queue.json` and a matching lockfile entry. This makes the
    // object KNOWN rather than new for the second sync below, using rdc's
    // own pull logic rather than a hand-built fixture (the object's slug,
    // on-disk shape, and lockfile hash are exactly what the second sync
    // expects, because the same code produced them).
    sync_logged(
        cwd,
        "test",
        "tok",
        EmbedSyncOptions::default(),
        Box::new(std::io::sink()),
    )
    .await
    .expect("first sync_logged should pull the queue and workspace");

    let queue_path = cwd.join("envs/test/workspaces/main/queues/invoices/queue.json");
    assert!(
        queue_path.exists(),
        "first sync should have written the queue snapshot at {}",
        queue_path.display()
    );

    // Edit the local file so it diverges from the mocked remote body — the
    // remote continues to serve the original "Invoices" name unchanged.
    let raw = std::fs::read_to_string(&queue_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["name"] = serde_json::Value::String("Invoices (locally edited)".to_string());
    std::fs::write(
        &queue_path,
        format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    )
    .unwrap();

    // Second sync: the queue is now a LocalEdit (local diverges from base,
    // remote still matches base) — `sync_logged`'s default options push it.
    sync_logged(
        cwd,
        "test",
        "tok",
        EmbedSyncOptions::default(),
        Box::new(std::io::sink()),
    )
    .await
    .expect("second sync_logged should push the local edit");
}

/// Pins `EmbedSyncOptions::default()` against a future "simplification":
/// each field's default value is load-bearing on its own, not just as a
/// bundle. See the doc comment on the struct in `src/cli/sync/embed.rs`
/// for why.
#[test]
fn default_embed_options_prompt_rather_than_decide() {
    let o = EmbedSyncOptions::default();
    assert!(
        o.interactive,
        "false would bail! on a pending delete and kill a watch"
    );
    assert!(!o.allow_deletes, "true would skip the delete gate entirely");
    assert!(
        o.conflict.is_none(),
        "Some(_) would resolve divergence without asking"
    );
    assert!(!o.no_push && !o.no_pull, "the app's sync is two-way");
}
