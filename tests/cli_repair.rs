use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn empty_list() -> serde_json::Value {
    serde_json::json!({ "pagination": { "next": null }, "results": [] })
}

async fn mount_minimal_pull(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/hooks", "/api/v1/rules", "/api/v1/labels",
        "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server).await;
    }
}

#[tokio::test]
async fn repair_requires_rebuild_lock_flag() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", "dev=https://x/api/v1:1"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["repair", "dev"])
        .assert().failure()
        .stderr(predicate::str::contains("--rebuild-lock"));
}

#[tokio::test]
async fn repair_backs_up_lockfile_and_repulls() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    let lockfile_path = project.path().join(".rdc/state/dev.lock.json");
    assert!(lockfile_path.exists(), "lockfile created by initial sync");

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["repair", "dev", "--rebuild-lock"])
        .assert().success()
        .stderr(predicate::str::contains("done env 'dev' rebuilt"))
        .stderr(predicate::str::contains("backed up lockfile to"));

    assert!(lockfile_path.exists(), "lockfile re-created by sync");

    // Backup file should exist.
    let state_dir = project.path().join(".rdc/state");
    let backups: Vec<_> = std::fs::read_dir(&state_dir).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
        .collect();
    assert_eq!(backups.len(), 1, "exactly one backup file");
}

#[tokio::test]
async fn repair_works_when_lockfile_is_missing() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    // No lockfile yet.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["repair", "dev", "--rebuild-lock"])
        .assert().success()
        .stderr(predicate::str::contains("done env 'dev' rebuilt"))
        .stderr(predicate::str::contains("no existing lockfile"));

    assert!(project.path().join(".rdc/state/dev.lock.json").exists());
}

#[tokio::test]
async fn fix_store_anomaly_lists_anomalous_hooks_then_exits_in_check_mode() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    // Override /hooks with two hooks: one anomalous, one clean.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [
                {
                    "id": 42, "url": format!("{}/api/v1/hooks/42", server.uri()),
                    "name": "Broken Store Hook", "type": "webhook",
                    "queues": [], "events": [], "config": {},
                    "extension_source": "rossum_store", "hook_template": null
                },
                {
                    "id": 43, "url": format!("{}/api/v1/hooks/43", server.uri()),
                    "name": "Healthy Hook", "type": "function",
                    "queues": [], "events": [], "config": {},
                    "extension_source": "custom", "hook_template": null
                }
            ]
        })))
        .with_priority(1)
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["repair", "dev", "--fix-store-anomaly", "--check"])
        .assert().success()
        .stderr(predicate::str::contains("broken-store-hook"))
        .stderr(predicate::str::contains("id 42"))
        .stderr(predicate::str::contains("1 anomalous hook"));
}

#[tokio::test]
async fn fix_store_anomaly_cure_b_patches_extension_source_to_custom() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 42, "url": format!("{}/api/v1/hooks/42", server.uri()),
                "name": "Broken", "type": "webhook",
                "queues": [], "events": [], "config": {},
                "extension_source": "rossum_store", "hook_template": null
            }]
        })))
        .with_priority(1)
        .mount(&server).await;

    // The PATCH the cure will issue. Capture and verify the body.
    let patched = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
    let patched_clone = patched.clone();
    let server_uri = server.uri();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/42"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap();
            *patched_clone.lock().unwrap() = body.clone();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42, "url": format!("{}/api/v1/hooks/42", server_uri),
                "name": "Broken", "type": "webhook",
                "queues": [], "events": [], "config": {},
                "extension_source": "custom", "hook_template": null,
                "modified_at": "2026-05-22T12:00:00.000000Z"
            }))
        })
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Non-interactive (`--yes`): default cure is convert-to-custom.
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["--yes", "repair", "dev", "--fix-store-anomaly"])
        .assert().success()
        .stderr(predicate::str::contains("hooks/broken (id 42) \u{2192} converted to custom"));

    let body = patched.lock().unwrap().clone();
    assert_eq!(body, serde_json::json!({"extension_source": "custom"}));

    // Local snapshot reflects the change.
    let local: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join("envs/dev/hooks/broken.json")).unwrap()
    ).unwrap();
    assert_eq!(local["extension_source"], "custom");
}
