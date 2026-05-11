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
async fn auth_writes_validated_token_to_secrets_file() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .and(header("Authorization", "token GOOD_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["auth", "dev", "--token", "GOOD_TOKEN"])
        .assert().success()
        .stdout(predicate::str::contains("Token written"))
        .stdout(predicate::str::contains("validated against org"));

    let secrets_path = project.path().join("secrets/dev.secrets.json");
    let body = std::fs::read_to_string(&secrets_path).unwrap();
    assert!(body.contains("GOOD_TOKEN"));

    // 0600 on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secrets_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secrets file should be mode 0600");
    }
}

#[tokio::test]
async fn auth_rejects_bad_token_and_does_not_write() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "detail": "Invalid token.",
            "code": "authentication_failed",
        })))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["auth", "dev", "--token", "BAD_TOKEN"])
        .assert().failure()
        .stderr(predicate::str::contains("Invalid token"));

    // No secrets file should have been written.
    let secrets_path = project.path().join("secrets/dev.secrets.json");
    assert!(!secrets_path.exists(), "no secrets file written on validation failure");
}

#[tokio::test]
async fn auth_unknown_env_errors_clearly() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", "dev=https://x/api/v1:1"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["auth", "nonexistent", "--token", "T"])
        .assert().failure()
        .stderr(predicate::str::contains("env 'nonexistent' is not defined"));
}
