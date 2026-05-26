//! End-to-end test of the embedding entry point against a wiremock'd
//! Rossum. Exercises a no-push pull into a tempdir.

use rdc::cli::sync::embed::sync_no_push;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn embed_sync_no_push_pulls_into_tempdir() {
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
        "email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }

    let tmp = tempdir().unwrap();
    let cwd = tmp.path();

    // Seed rdc.toml that points at our wiremock.
    std::fs::write(
        cwd.join("rdc.toml"),
        format!(
            r#"[envs.main]
api_base = "{}"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();

    sync_no_push(cwd, "main", "fake-token")
        .await
        .expect("sync_no_push succeeds");

    assert!(cwd.join("envs/main/_index.md").exists());
    assert!(cwd.join("envs/main/organization.json").exists());
}
