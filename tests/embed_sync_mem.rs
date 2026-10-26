//! `tests/embed_sync.rs`, run on an in-memory project: the pull lands in a
//! `MemStore` and nothing touches the real disk.

use rdc::cli::sync::embed::{sync_logged_in, EmbedSyncOptions};
use rdc::vfs::{MemStore, Store};
use std::path::{Path, PathBuf};
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

    let root = PathBuf::from("/rdc-mem/project");
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
    assert!(!Path::new("/rdc-mem").exists(), "the sync touched the real disk");
}
