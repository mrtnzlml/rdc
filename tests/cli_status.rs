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
async fn status_reports_auth_ok_and_no_edits_after_pull() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["status", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("Env 'dev'"))
        .stdout(predicate::str::contains("token:    present"))
        .stdout(predicate::str::contains("auth:     ok"))
        .stdout(predicate::str::contains("lockfile: v2"))
        .stdout(predicate::str::contains("edits:    none"));
}

#[tokio::test]
async fn status_reports_missing_token() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    // intentionally no secrets file written.

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        // Override any externally-set RDC_TOKEN_DEV to keep this test
        // hermetic.
        .env_remove("RDC_TOKEN_DEV")
        .args(["status", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("token:    missing"));
}

#[tokio::test]
async fn status_reports_missing_lockfile() {
    let server = MockServer::start().await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    // Mount only the org endpoint (auth check) — no pull yet.
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["status", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("auth:     ok"))
        .stdout(predicate::str::contains("lockfile: missing"));
}

#[tokio::test]
async fn status_detects_local_edits() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    // Override hooks list with one hook so we have something to edit.
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server2).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server2).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels",
        "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&server2).await;
    }

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server2.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    // Edit a hook's .py file.
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# status edit\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["status", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("hooks/validator-invoices"));
}
