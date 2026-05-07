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

async fn mount_full_pull(server: &MockServer, hooks_payload: serde_json::Value) {
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
async fn map_plan_apply_full_flow() {
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    mount_full_pull(&test_server, fixture("hooks_list.json")).await;

    let prod_hooks = serde_json::json!({
        "pagination": { "total": 2, "next": null, "previous": null },
        "results": [
            {
                "id": 401,
                "url": "https://prod.rossum.app/api/v1/hooks/401",
                "name": "Validator: invoices",
                "type": "function",
                "queues": [],
                "events": ["annotation_content"],
                "config": { "runtime": "python3.12", "code": "def x(payload):\n    return {}\n" }
            },
            {
                "id": 402,
                "url": "https://prod.rossum.app/api/v1/hooks/402",
                "name": "SFTP import",
                "type": "function",
                "queues": [],
                "events": ["annotation_status"],
                "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
            }
        ]
    });
    mount_full_pull(&prod_server, prod_hooks).await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/401"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *captured_clone.lock().unwrap() = Some(body.clone());
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server).await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/402"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "x",
            "--env", &format!("test={}/api/v1:1", test_server.uri()),
            "--env", &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"PROD_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "test"]).assert().success();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "prod"]).assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["map", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Auto-matched 2"));

    let mapping_file = project.path().join(".rdc/map/test→prod.toml");
    assert!(mapping_file.exists());

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["plan", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Plan: test → prod"))
        .stdout(predicate::str::contains("validator-invoices"))
        .stdout(predicate::str::contains("(id 401)"));

    std::fs::write(
        project.path().join("envs/prod/overlay.toml"),
        r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["apply", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Applied 2"));

    let body = captured.lock().unwrap().clone().expect("PATCH body for hook 401");
    assert_eq!(body["name"], serde_json::Value::String("Validator (PROD)".into()));
}
