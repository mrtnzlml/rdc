use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn pull_writes_organization_workspaces_and_hook_files() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "test-pull",
            "--env",
            &format!("dev={}/api/v1:1", server.uri()),
        ])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pulled 1 organization"))
        .stdout(predicate::str::contains("2 workspaces"))
        .stdout(predicate::str::contains("2 hooks"));

    let env_root = project.path().join("envs/dev");
    assert!(env_root.join("organization.json").exists());
    let org_raw = std::fs::read_to_string(env_root.join("organization.json")).unwrap();
    assert!(org_raw.contains("Acme Test Org"));

    let hooks_dir = env_root.join("hooks");
    assert!(hooks_dir.join("validator-invoices.json").exists());
    assert!(hooks_dir.join("sftp-import.json").exists());

    let ws_root = env_root.join("workspaces");
    assert!(ws_root.join("invoices-ap/workspace.json").exists());
    assert!(ws_root.join("purchase-orders/workspace.json").exists());

    let ws_raw = std::fs::read_to_string(ws_root.join("invoices-ap/workspace.json")).unwrap();
    assert!(ws_raw.contains("Invoices AP"));

    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("organization"));
    assert!(lf.contains("validator-invoices"));
    assert!(lf.contains("workspaces"));
    assert!(lf.contains("invoices-ap"));
}

#[tokio::test]
async fn pull_with_missing_token_fails_with_helpful_error() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "x",
            "--env", "dev=https://nope.invalid/api/v1:1",
        ])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RDC_TOKEN_DEV"));
}

#[tokio::test]
async fn pull_with_unknown_env_fails() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "x",
            "--env", "dev=https://nope.invalid/api/v1:1",
        ])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("env 'prod' is not defined"));
}
