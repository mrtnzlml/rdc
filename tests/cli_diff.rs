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
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hook_1.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 2,
            "url": "https://mock.rossum.app/api/v1/hooks/2",
            "name": "SFTP import",
            "type": "function",
            "queues": [],
            "events": ["annotation_status"],
            "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
        })))
        .mount(server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels",
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
async fn diff_local_remote_no_changes() {
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
        .args(["pull", "dev"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));
}

#[tokio::test]
async fn diff_local_remote_shows_edit_in_unified_format() {
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
        .args(["pull", "dev"])
        .assert().success();

    // Edit hook code locally.
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# DIFF MARKER LINE\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("--- hooks/validator-invoices.py (local)"))
        .stdout(predicate::str::contains("+++ hooks/validator-invoices.py (remote)"))
        .stdout(predicate::str::contains("DIFF MARKER LINE"));
}

#[tokio::test]
async fn diff_snapshot_vs_snapshot_no_api_calls() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    mount_minimal_pull(&server_a).await;
    mount_minimal_pull(&server_b).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init", "--env", &format!("a={}/api/v1:1", server_a.uri()),
            "--env", &format!("b={}/api/v1:1", server_b.uri()),
        ])
        .assert().success();
    std::fs::write(project.path().join("secrets/a.secrets.json"), r#"{"api_token":"X"}"#).unwrap();
    std::fs::write(project.path().join("secrets/b.secrets.json"), r#"{"api_token":"X"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path()).args(["pull", "a"]).assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path()).args(["pull", "b"]).assert().success();

    // Identical snapshots → no diffs.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "a", "b"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs (snapshots 'a' and 'b' are identical)"));

    // Diverge: edit a's hook .py.
    let py_path = project.path().join("envs/a/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# A-only edit\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "a", "b"])
        .assert().success()
        .stdout(predicate::str::contains("hooks/validator-invoices.py (in a)"))
        .stdout(predicate::str::contains("hooks/validator-invoices.py (in b)"))
        .stdout(predicate::str::contains("A-only edit"));
}
