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
        .stderr(predicate::str::contains("saved token to"))
        .stderr(predicate::str::contains("validated against org"));

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

#[test]
fn auth_username_and_token_are_mutually_exclusive() {
    use assert_cmd::Command;
    use predicates::str::contains;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        r#"
name = "fixture"
[envs.dev]
api_base = "https://example.rossum.app/api/v1"
org_id = 1
"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["auth", "dev", "--username", "alice", "--token", "T"])
        .assert()
        .failure()
        .stderr(contains("--username").and(contains("--token")));
}

#[tokio::test]
async fn auth_username_logs_in_and_writes_token_with_expires_at() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "minted-by-login",
            "domain": "example",
        })))
        .mount(&server)
        .await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "name": "Example Org",
            "url": format!("{}/v1/organizations/1", server.uri()),
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        format!(
            r#"
name = "fixture"
[envs.dev]
api_base = "{}/v1"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();

    use assert_cmd::Command;
    let output = tokio::task::spawn_blocking({
        let dir_path = dir.path().to_path_buf();
        move || {
            Command::cargo_bin("rdc")
                .unwrap()
                .current_dir(&dir_path)
                .args(["auth", "dev", "--username", "alice"])
                .write_stdin("hunter2\n")
                .assert()
                .success()
        }
    })
    .await
    .unwrap();
    let _ = output;

    let raw = std::fs::read_to_string(dir.path().join("secrets/dev.secrets.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["api_token"], "minted-by-login");
    assert!(parsed["expires_at"].is_number(), "expires_at must be present: {parsed}");
}
