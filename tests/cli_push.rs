use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn empty_list() -> serde_json::Value {
    serde_json::json!({ "pagination": { "next": null }, "results": [] })
}

async fn mount_get_only_hooks_org(server: &MockServer, hooks_payload: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hooks_payload))
        .mount(server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server).await;
    }
}

#[tokio::test]
async fn push_succeeds_when_local_edited_and_remote_unchanged() {
    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hook_1.json")))
        .mount(&server).await;

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

    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    let edited = format!("{original}# local edit\n");
    std::fs::write(&py_path, &edited).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("Pushed 1 hook"));
}

#[tokio::test]
async fn push_skips_when_remote_has_drifted() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    mount_get_only_hooks_org(&server1, fixture("hooks_list.json")).await;

    let drifted_hooks = serde_json::json!({
        "pagination": { "total": 2, "next": null, "previous": null },
        "results": [
            {
                "id": 1,
                "url": "https://mock.rossum.app/api/v1/hooks/1",
                "name": "Validator: invoices (REMOTE DRIFT)",
                "type": "function",
                "queues": ["https://mock.rossum.app/api/v1/queues/100"],
                "events": ["annotation_content"],
                "config": { "runtime": "python3.12", "code": "def x(payload):\n    return {}\n" }
            },
            {
                "id": 2,
                "url": "https://mock.rossum.app/api/v1/hooks/2",
                "name": "SFTP import",
                "type": "function",
                "queues": [],
                "events": ["annotation_status"],
                "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
            }
        ]
    });
    mount_get_only_hooks_org(&server2, drifted_hooks).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server1.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# local edit\n")).unwrap();

    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let new_cfg = cfg.replace(&format!("{}/api/v1", server1.uri()), &format!("{}/api/v1", server2.uri()));
    std::fs::write(&cfg_path, new_cfg).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("0 hooks"))
        .stdout(predicate::str::contains("1 skipped"));
}

#[tokio::test]
async fn push_applies_overlay_values_to_outbound_patch() {
    use std::sync::{Arc, Mutex};

    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *captured_clone.lock().unwrap() = Some(body.clone());
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server).await;

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

    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# local edit\n")).unwrap();

    let overlay_path = project.path().join("envs/dev/overlay.toml");
    std::fs::write(&overlay_path, r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (DEV-OVERLAY)"
"config.runtime" = "python3.12-overlay"
"#).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("Pushed 1 hook"));

    let body = captured.lock().unwrap().clone().expect("PATCH body should be captured");
    assert_eq!(body["name"], serde_json::Value::String("Validator (DEV-OVERLAY)".into()));
    assert_eq!(body["config"]["runtime"], serde_json::Value::String("python3.12-overlay".into()));
}

#[tokio::test]
async fn push_with_no_local_edits_is_noop() {
    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

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
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("0 hooks"));
}

/// Regression: after a successful push, the local file should be rewritten
/// with the canonical (server-authoritative) form. Without this, files
/// edited by tooling that escapes Unicode differently (e.g. Python with
/// ensure_ascii=True) leave the disk bytes diverged from the lockfile hash,
/// and the user sees their file mutate on the next pull.
#[tokio::test]
async fn push_rewrites_local_file_with_canonical_form() {
    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

    // Server response: description has a real em-dash character (UTF-8).
    let canonical_response = serde_json::json!({
        "id": 1,
        "url": "https://mock.rossum.app/api/v1/hooks/1",
        "name": "Validator: invoices",
        "type": "function",
        "description": "post-push canonical \u{2014} value",
        "queues": ["https://mock.rossum.app/api/v1/queues/100"],
        "events": ["annotation_content"],
        "config": { "runtime": "python3.12", "code": "def validate(payload):\n    return {}\n" }
    });
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(canonical_response))
        .mount(&server).await;

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

    // Mangle the local JSON: inject an ASCII-escaped em-dash literal (`—`).
    // `serde_json::to_vec_pretty` writes `—` as a raw 3-byte UTF-8 sequence;
    // `python -c "json.dump(..., ensure_ascii=True)"` writes the same character
    // as a 6-byte ASCII escape. Both decode to the same string at parse time.
    let json_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let raw = std::fs::read_to_string(&json_path).unwrap();
    let mangled = raw.replace(
        "\"name\": \"Validator: invoices\",",
        "\"name\": \"Validator: invoices\",\n  \"description\": \"local \\u2014 mangle\",",
    );
    std::fs::write(&json_path, &mangled).unwrap();
    let pre_push = std::fs::read(&json_path).unwrap();
    assert!(pre_push.windows(6).any(|w| w == b"\\u2014"),
            "test setup: file should contain literal \\u2014 escape");

    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let py = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{py}# trigger push\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("Pushed 1 hook"));

    let post_push = std::fs::read(&json_path).unwrap();
    assert!(!post_push.windows(6).any(|w| w == b"\\u2014"),
            "after push, literal \\u2014 escape should be gone — file should match canonical server response");
    assert!(post_push.windows(3).any(|w| w == "—".as_bytes()),
            "after push, the em-dash should be present as raw UTF-8 (3 bytes)");
}
