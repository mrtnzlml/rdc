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
    mount_full_pull(&prod_server, prod_hooks.clone()).await;

    // Apply's drift check does GET /hooks/{id} per object — mock both.
    let prod_hook_401 = prod_hooks["results"][0].clone();
    let prod_hook_402 = prod_hooks["results"][1].clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/401"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_hook_401))
        .mount(&prod_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/402"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_hook_402))
        .mount(&prod_server).await;

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

    // Realistic workflow: set overlay BEFORE pull so the lockfile records
    // the stripped hash. Pulling here re-baselines prod's lockfile with the
    // overlay-stripped form (this is the same caveat that's documented in
    // the README for "overlay added after first pull").
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
        .args(["pull", "prod"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["apply", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Applied 2"));

    let body = captured.lock().unwrap().clone().expect("PATCH body for hook 401");
    assert_eq!(body["name"], serde_json::Value::String("Validator (PROD)".into()));
}

/// Mount mocks sufficient to pull a single workspace + queue + schema, with
/// every other kind empty.
async fn mount_minimal_for_deploy(server: &MockServer, schema: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 700852,
                "url": "https://mock.rossum.app/api/v1/workspaces/700852",
                "name": "Invoices AP",
                "organization": "https://mock.rossum.app/api/v1/organizations/1",
                "queues": ["https://mock.rossum.app/api/v1/queues/100"]
            }]
        })))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 100,
                "url": "https://mock.rossum.app/api/v1/queues/100",
                "name": "Cost Invoices",
                "workspace": "https://mock.rossum.app/api/v1/workspaces/700852",
                "schema": "https://mock.rossum.app/api/v1/schemas/200"
            }]
        })))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(schema))
        .mount(server).await;
    for ep in [
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

/// Deploy a queue (settings.default_score_threshold) AND a schema from
/// test → prod. Verifies that mapping picks up both kinds and apply
/// PATCHes both endpoints.
#[tokio::test]
async fn deploy_queue_and_schema() {
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    // Both envs have the same queue (id 100 on test, 100 on prod for simplicity)
    // and the same schema (id 200 on both). Deploy should map by slug.
    mount_minimal_for_deploy(&test_server, fixture("schema_1.json")).await;
    mount_minimal_for_deploy(&prod_server, fixture("schema_1.json")).await;

    let queue_patch_body: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let queue_patch_body_clone = queue_patch_body.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/queues/100"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *queue_patch_body_clone.lock().unwrap() = Some(body.clone());
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server).await;

    let schema_patch_seen: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let schema_patch_seen_clone = schema_patch_seen.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(move |req: &wiremock::Request| {
            *schema_patch_seen_clone.lock().unwrap() = true;
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init", "--env", &format!("test={}/api/v1:1", test_server.uri()),
            "--env", &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"PROD"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "test"])
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert().success();

    // Edit test queue + schema formula to differ from prod (so apply has
    // a real change to push; otherwise apply's idempotency would
    // correctly skip the no-diff cases).
    let queue_path = project.path()
        .join("envs/test/workspaces/invoices-ap/queues/cost-invoices/queue.json");
    let raw = std::fs::read_to_string(&queue_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["default_score_threshold"] = serde_json::json!(0.99);
    std::fs::write(&queue_path, format!("{}\n", serde_json::to_string_pretty(&v).unwrap())).unwrap();

    // Edit test schema's first formula so it differs from prod's schema.
    let formula_dir = project.path()
        .join("envs/test/workspaces/invoices-ap/queues/cost-invoices/formulas");
    if formula_dir.exists() {
        for entry in std::fs::read_dir(&formula_dir).unwrap().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("py") {
                std::fs::write(&p, "amount_due * 1.21\n").unwrap();
                break;
            }
        }
    }

    // Map.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["map", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("1 new queues"))
        .stdout(predicate::str::contains("1 new schemas"));

    // Plan: should mention queues + schemas mapped.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["plan", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("queues/cost-invoices"))
        .stdout(predicate::str::contains("schemas/cost-invoices"));

    // Apply.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["apply", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("1 queues"))
        .stdout(predicate::str::contains("1 schemas"));

    let captured = queue_patch_body.lock().unwrap().clone()
        .expect("queue PATCH body captured");
    assert_eq!(captured["default_score_threshold"], serde_json::json!(0.99));
    assert!(*schema_patch_seen.lock().unwrap(), "schema PATCH was made");
}
