use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Snapshot body for a Master Data Hub store extension, mirrored from
/// `tests/cli_push.rs`. `api_base` is the full API base URL (e.g.
/// `http://127.0.0.1:PORT/api/v1`).
fn mdh_snapshot_body(api_base: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "Master Data Hub",
        "type": "webhook",
        "events": ["annotation_content.initialize", "annotation_content.started", "annotation_content.updated"],
        "queues": [],
        "active": true,
        "run_after": [],
        "metadata": {},
        "config": { "private": true, "timeout_s": 60, "payload_logging_enabled": false },
        "settings": { "configurations": [{"name": "customised"}] },
        "sideload": ["schemas"],
        "settings_schema": null,
        "secrets_schema": { "type": "object", "additionalProperties": {"type": "string"} },
        "description": "Enhance the extracted data with details from your master records.",
        "guide": "<div>...</div>",
        "read_more_url": "https://docs.rossum.ai/mdh",
        "extension_image_url": "https://example.com/mdh.png",
        "token_lifetime_s": 7200,
        "token_owner": format!("{api_base}/users/938493"),
        "extension_source": "rossum_store",
        "hook_template": format!("{api_base}/hook_templates/39")
    })
}

/// Same shape as `mdh_snapshot_body` but representing what the server returns
/// immediately after `POST /hooks/create` — template defaults (un-customised
/// `settings`), plus id/url assigned.
#[allow(dead_code)]
fn mdh_installed_body(api_base: &str, id: u64) -> serde_json::Value {
    let mut body = mdh_snapshot_body(api_base);
    body["id"] = serde_json::Value::from(id);
    body["url"] = serde_json::Value::from(format!("{api_base}/hooks/{id}"));
    body["settings"] = serde_json::json!({"configurations": []}); // template default, NOT customised
    body
}

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

    // Set overlay BEFORE the second prod pull so the lockfile records the
    // stripped hash. Same caveat documented in the README for "overlay
    // added after first pull".
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

    // `rdc deploy` now owns the full cross-env workflow — it auto-builds
    // the mapping, prints a plan, and runs the update sub-step in one
    // call. Only validator-invoices needs a PATCH (overlay renames it);
    // sftp-import is byte-identical between test and prod after
    // env-specific stripping, so it's skipped as idempotent.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert().success()
        .stdout(predicate::str::contains("Plan: test -> prod"))
        .stdout(predicate::str::contains("Applied 1 hooks"))
        .stdout(predicate::str::contains("(1 PATCHes)"));

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

    // `rdc deploy` does map + plan + apply in one call.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert().success()
        .stdout(predicate::str::contains("1 queues"))
        .stdout(predicate::str::contains("1 schemas"));

    let captured = queue_patch_body.lock().unwrap().clone()
        .expect("queue PATCH body captured");
    assert_eq!(captured["default_score_threshold"], serde_json::json!(0.99));
    assert!(*schema_patch_seen.lock().unwrap(), "schema PATCH was made");
}

// ============================================================
// `rdc deploy` — one-shot cross-env deploy
// ============================================================

/// Bootstrap a fresh prod env from a populated test env via a single
/// `rdc deploy test prod --yes` invocation. Verifies that:
/// 1. POSTs land on every kind in dependency order (workspace, schema,
///    queue, hook).
/// 2. The hook's POST body has its `queues` array URL-rewritten from
///    src queue URL to the just-created tgt queue URL (the README's
///    "links between resources must be replicated" contract).
/// 3. Re-running `rdc deploy` on the now-synced state produces 0 POSTs
///    and 0 PATCHes (idempotency).
#[tokio::test]
async fn deploy_bootstraps_empty_target_with_url_rewriting() {
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    // --- TEST env: 1 workspace + 1 queue + 1 schema + 1 hook ---
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&test_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 500,
                "url": "https://test.rossum.app/api/v1/workspaces/500",
                "name": "Invoices AP",
                "organization": "https://test.rossum.app/api/v1/organizations/1",
                "queues": ["https://test.rossum.app/api/v1/queues/600"]
            }]
        })))
        .mount(&test_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 600,
                "url": "https://test.rossum.app/api/v1/queues/600",
                "name": "Cost Invoices",
                "workspace": "https://test.rossum.app/api/v1/workspaces/500",
                "schema": "https://test.rossum.app/api/v1/schemas/700"
            }]
        })))
        .mount(&test_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/700"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 700,
            "url": "https://test.rossum.app/api/v1/schemas/700",
            "name": "Cost Invoices schema",
            "queues": ["https://test.rossum.app/api/v1/queues/600"],
            "content": [],
        })))
        .mount(&test_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 800,
                "url": "https://test.rossum.app/api/v1/hooks/800",
                "name": "Validator",
                "type": "function",
                "queues": ["https://test.rossum.app/api/v1/queues/600"],
                "events": ["annotation_content.initialize"],
                "config": { "runtime": "python3.12", "code": "def run(p):\n    return {}\n" }
            }]
        })))
        .mount(&test_server).await;
    for ep in [
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server).await;
    }

    // --- PROD env: empty (every list returns 0 results) ---
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/hooks",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server).await;
    }

    // --- PROD env POST mocks: each returns a body shaped like the response,
    //     with server-assigned ids. The hook POST body is captured so we
    //     can assert its `queues` URL got rewritten.
    let post_count = Arc::new(Mutex::new(0u32));
    let pc = post_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/workspaces"))
        .respond_with(move |req: &wiremock::Request| {
            *pc.lock().unwrap() += 1;
            let mut body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["id"] = serde_json::json!(5500);
            body["url"] = serde_json::json!("https://prod.rossum.app/api/v1/workspaces/5500");
            ResponseTemplate::new(201).set_body_json(body)
        }).mount(&prod_server).await;
    let pc = post_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/schemas"))
        .respond_with(move |req: &wiremock::Request| {
            *pc.lock().unwrap() += 1;
            let mut body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["id"] = serde_json::json!(7700);
            body["url"] = serde_json::json!("https://prod.rossum.app/api/v1/schemas/7700");
            ResponseTemplate::new(201).set_body_json(body)
        }).mount(&prod_server).await;
    let pc = post_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/queues"))
        .respond_with(move |req: &wiremock::Request| {
            *pc.lock().unwrap() += 1;
            let mut body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["id"] = serde_json::json!(6600);
            body["url"] = serde_json::json!("https://prod.rossum.app/api/v1/queues/6600");
            ResponseTemplate::new(201).set_body_json(body)
        }).mount(&prod_server).await;
    let hook_post_body: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured = hook_post_body.clone();
    let pc = post_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/hooks"))
        .respond_with(move |req: &wiremock::Request| {
            *pc.lock().unwrap() += 1;
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *captured.lock().unwrap() = Some(body.clone());
            let mut resp = body;
            resp["id"] = serde_json::json!(8800);
            resp["url"] = serde_json::json!("https://prod.rossum.app/api/v1/hooks/8800");
            ResponseTemplate::new(201).set_body_json(resp)
        }).mount(&prod_server).await;

    // After queue POST, deploy calls list_email_templates to capture auto-
    // created peers. The empty-list mock above already covers it.

    // Apply (the update sub-step) hits per-object GETs during drift checks.
    // Mocks must reflect the post-deploy server state so apply sees the
    // just-created objects as "in sync" rather than missing.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/8800"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 8800,
                "url": "https://prod.rossum.app/api/v1/hooks/8800",
                "name": "Validator",
                "type": "function",
                "queues": ["https://prod.rossum.app/api/v1/queues/6600"],
                "events": ["annotation_content.initialize"],
                "config": { "runtime": "python3.12", "code": "def run(p):\n    return {}\n" }
            }))
        }).mount(&prod_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/7700"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 7700,
                "url": "https://prod.rossum.app/api/v1/schemas/7700",
                "name": "Cost Invoices schema",
                "queues": ["https://prod.rossum.app/api/v1/queues/6600"],
                "content": [],
            }))
        }).mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init", "--env", &format!("test={}/api/v1:1", test_server.uri()),
            "--env", &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert().success();
    std::fs::write(project.path().join("secrets/test.secrets.json"), r#"{"api_token":"TEST"}"#).unwrap();
    std::fs::write(project.path().join("secrets/prod.secrets.json"), r#"{"api_token":"PROD"}"#).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "test"]).assert().success();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "prod"]).assert().success();

    // === The one-command deploy. ===
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert().success()
        .stdout(predicate::str::contains("Plan: test -> prod"))
        .stdout(predicate::str::contains("workspaces"))
        .stdout(predicate::str::contains("schemas"))
        .stdout(predicate::str::contains("queues"))
        .stdout(predicate::str::contains("hooks"));

    // 4 POSTs: 1 workspace + 1 schema + 1 queue + 1 hook
    assert_eq!(*post_count.lock().unwrap(), 4, "expected exactly 4 POSTs");

    // The hook's POST body must reference the PROD queue URL, not the test one.
    let hook_body = hook_post_body.lock().unwrap().clone().expect("hook POST body captured");
    let queues = hook_body["queues"].as_array().expect("queues array");
    assert_eq!(queues.len(), 1);
    assert_eq!(
        queues[0].as_str().unwrap(),
        "https://prod.rossum.app/api/v1/queues/6600",
        "hook.queues must be rewritten from test URL to PROD queue URL"
    );
}

/// When `rdc deploy test prod --yes` (non-TTY: `--yes` suppresses the
/// interactive prompt) is run and the tgt overlay has no `token_owner` for a
/// store extension, the pre-pass must abort before issuing any mutating
/// requests (POST/PATCH/DELETE) to the tgt server.
#[tokio::test]
async fn deploy_refuses_non_tty_without_token_owner_overlay() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;
    let src_api = format!("{}/api/v1", test_server.uri());
    let tgt_api = format!("{}/api/v1", prod_server.uri());

    // ── Src (test) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&test_server).await;
    let mdh_src = {
        let mut b = mdh_snapshot_body(&src_api);
        b["id"] = serde_json::Value::from(999u64);
        b["url"] = serde_json::Value::String(format!("{src_api}/hooks/999"));
        b
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [mdh_src.clone()]
        })))
        .mount(&test_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server).await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{src_api}/hook_templates/39"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&test_server).await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/hooks",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server).await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{tgt_api}/hook_templates/41"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&prod_server).await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env", &format!("test={src_api}:1"),
            "--env", &format!("prod={tgt_api}:1"),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "test"])
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert().success();

    // Deliberately omit the tgt overlay — no store_extension_token_owner set.

    // Deploy with --yes (non-interactive) must fail.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("token_owner"))
        .stderr(predicate::str::contains("master-data-hub"))
        .stderr(predicate::str::contains("envs/prod/overlay.toml"));

    // No mutating requests (POST/PATCH/DELETE) to Rossum API paths must have
    // reached the tgt server. Data-storage paths use POST for reads by
    // convention and are excluded from this check.
    for req in prod_server.received_requests().await.unwrap_or_default() {
        let path = req.url.path();
        if path.contains("/svc/data-storage/") {
            continue;
        }
        assert!(
            !matches!(
                req.method,
                http::Method::POST | http::Method::PATCH | http::Method::DELETE
            ),
            "unexpected mutating request: {} {}",
            req.method,
            path
        );
    }
}

/// When the tgt cluster does not have the required hook template at all
/// (empty `GET /api/v1/hook_templates`), `build_template_url_map` must
/// fail fast with a user-visible error before any mutating request reaches
/// the tgt server — even when the tgt overlay already supplies
/// `store_extension_token_owner`.
#[tokio::test]
async fn deploy_errors_when_template_missing_on_tgt() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;
    let src_api = format!("{}/api/v1", test_server.uri());
    let tgt_api = format!("{}/api/v1", prod_server.uri());

    // ── Src (test) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&test_server).await;
    let mdh_src = {
        let mut b = mdh_snapshot_body(&src_api);
        b["id"] = serde_json::Value::from(999u64);
        b["url"] = serde_json::Value::String(format!("{src_api}/hooks/999"));
        b
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [mdh_src.clone()]
        })))
        .mount(&test_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server).await;
    }
    // src /hook_templates — returns the MDH template (id 39)
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{src_api}/hook_templates/39"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&test_server).await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/hooks",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server).await;
    }
    // tgt /hook_templates — EMPTY: the template is not available on prod
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": []
        })))
        .mount(&prod_server).await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env", &format!("test={src_api}:1"),
            "--env", &format!("prod={tgt_api}:1"),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "test"])
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert().success();

    // Tgt overlay HAS store_extension_token_owner — so the failure is
    // purely template-related, not token-related.
    let prod_overlay_path = project.path().join("envs/prod/overlay.toml");
    std::fs::create_dir_all(prod_overlay_path.parent().unwrap()).unwrap();
    std::fs::write(
        &prod_overlay_path,
        format!(
            "version = 1\n\n[defaults]\nstore_extension_token_owner = \"{tgt_api}/users/521884\"\n"
        ),
    ).unwrap();

    // Deploy with --yes must fail because the template is absent on prod.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Master Data Hub"))
        .stderr(predicate::str::contains("not available on prod"));

    // No mutating requests (POST/PATCH/DELETE) to Rossum API paths must have
    // reached the tgt server before the early failure.
    for req in prod_server.received_requests().await.unwrap_or_default() {
        let path = req.url.path();
        if path.contains("/svc/data-storage/") {
            continue;
        }
        assert!(
            !matches!(
                req.method,
                http::Method::POST | http::Method::PATCH | http::Method::DELETE
            ),
            "unexpected mutating request: {} {}",
            req.method,
            path
        );
    }
}

/// Deploy pre-pass resolves cross-cluster template URLs and reads token_owner
/// from the tgt overlay (non-interactive path). After `rdc deploy test prod
/// --yes`, the `.rdc/map/test-to-prod.toml` must contain a `[hook_templates]`
/// section with the src→tgt template URL pair, and the tgt lockfile must
/// record the new hook id (proving both the pre-pass and the two-call install
/// + PATCH path ran).
#[tokio::test]
async fn deploy_resolves_templates_and_prompts_for_token_owner() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;
    let src_api = format!("{}/api/v1", test_server.uri());
    let tgt_api = format!("{}/api/v1", prod_server.uri());

    // ── Src (test) pull mocks ────────────────────────────────────────────────
    // org endpoint needed by rdc pull
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&test_server).await;
    // hooks list: returns the MDH store extension
    let mdh_src = {
        let mut b = mdh_snapshot_body(&src_api);
        b["id"] = serde_json::Value::from(999u64);
        b["url"] = serde_json::Value::String(format!("{src_api}/hooks/999"));
        b
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [mdh_src.clone()]
        })))
        .mount(&test_server).await;
    // all other src list endpoints — empty
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server).await;
    }
    // src /hook_templates — pre-pass lists this to build the src→tgt pair
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{src_api}/hook_templates/39"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&test_server).await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server).await;
    // prod starts with no hooks
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/hooks",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server).await;
    }
    // tgt /hook_templates — pre-pass lists this; id 41 (different from src's 39)
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{tgt_api}/hook_templates/41"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&prod_server).await;

    // ── Tgt deploy mocks ─────────────────────────────────────────────────────
    // The installed body returned by POST /hooks/create (template defaults,
    // un-customised settings). The reconcile PATCH then applies customisations.
    let installed_on_tgt = mdh_installed_body(&tgt_api, 700);
    // The customised body returned by PATCH /hooks/700 and used for the
    // apply drift check.
    let customised_on_tgt = {
        let mut b = mdh_snapshot_body(&tgt_api);
        b["id"] = serde_json::Value::from(700u64);
        b["url"] = serde_json::Value::String(format!("{tgt_api}/hooks/700"));
        b["hook_template"] = serde_json::Value::String(format!("{tgt_api}/hook_templates/41"));
        b["token_owner"] = serde_json::Value::String(format!("{tgt_api}/users/521884"));
        b
    };
    // Orphan check: tgt has no hooks yet.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
        .mount(&prod_server).await;
    // Two-call install: POST /hooks/create.
    Mock::given(method("POST"))
        .and(path("/api/v1/hooks/create"))
        .respond_with(ResponseTemplate::new(201).set_body_json(installed_on_tgt.clone()))
        .mount(&prod_server).await;
    // Reconcile PATCH /hooks/700.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/700"))
        .respond_with(ResponseTemplate::new(200).set_body_json(customised_on_tgt.clone()))
        .mount(&prod_server).await;
    // apply drift check: GET /api/v1/hooks/700.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/700"))
        .respond_with(ResponseTemplate::new(200).set_body_json(customised_on_tgt.clone()))
        .mount(&prod_server).await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env", &format!("test={src_api}:1"),
            "--env", &format!("prod={tgt_api}:1"),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    ).unwrap();

    // Pull both envs to establish lockfiles and snapshots.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "test"])
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert().success();

    // Pre-populate the tgt overlay with the system user URL so the pre-pass
    // resolves token_owner without hanging on stdin (non-interactive path).
    let prod_overlay_path = project.path().join("envs/prod/overlay.toml");
    std::fs::create_dir_all(prod_overlay_path.parent().unwrap()).unwrap();
    std::fs::write(
        &prod_overlay_path,
        format!(
            "version = 1\n\n[defaults]\nstore_extension_token_owner = \"{tgt_api}/users/521884\"\n"
        ),
    ).unwrap();

    // Deploy — pre-pass resolves templates, reads token_owner from overlay.
    let out = Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .output().unwrap();

    assert!(
        out.status.success(),
        "deploy should succeed.\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    // The map cache must contain the template URL pair built by the pre-pass.
    let map_path = project.path().join(".rdc/map/test-to-prod.toml");
    let raw = std::fs::read_to_string(&map_path).expect("map file should exist");
    assert!(
        raw.contains("[hook_templates]"),
        "[hook_templates] section missing from map file:\n{raw}"
    );
    assert!(
        raw.contains("hook_templates/41"),
        "tgt template id 41 missing from map file:\n{raw}"
    );

    // The tgt lockfile must record the new hook id (proving the two-call
    // install + PATCH path actually ran, not just the pre-pass template
    // resolution).
    let lf_path = project.path().join(".rdc/state/prod.lock.json");
    let lf_raw = std::fs::read_to_string(&lf_path).expect("tgt lockfile should exist");
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).expect("lockfile must be valid JSON");
    let hook_id = lf["objects"]["hooks"]["master-data-hub"]["id"]
        .as_u64()
        .expect("hooks.master-data-hub.id must be an integer in tgt lockfile");
    assert!(hook_id > 0, "hook id in tgt lockfile must be positive, got {hook_id}");
}

/// `rdc deploy --only hooks/nonexistent` must fail fast with a clear error
/// mentioning the offending selector and "matched 0 objects".
#[tokio::test]
async fn deploy_only_with_unknown_selector_errors() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;
    mount_full_pull(&test_server, empty_list()).await;
    mount_full_pull(&prod_server, empty_list()).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .args(["init", "--env", &format!("test={}/api/v1:1", test_server.uri()),
               "--env", &format!("prod={}/api/v1:1", prod_server.uri())])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "test"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "prod"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .args(["deploy", "test", "prod", "--yes", "--only", "hooks/nonexistent"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .failure()
        .stderr(predicate::str::contains("hooks/nonexistent"))
        .stderr(predicate::str::contains("matched 0 objects"));
}

/// `rdc deploy --only hooks/*` dry-run must list hooks in the plan but must
/// NOT list rules, even when both kinds have new objects relative to tgt.
#[tokio::test]
async fn deploy_only_filters_plan() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    let hooks_list = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 100,
            "url": format!("{}/api/v1/hooks/100", test_server.uri()),
            "name": "validator",
            "type": "function",
            "queues": [],
            "events": ["annotation_status"],
            "config": { "runtime": "python3.12", "code": "def x(payload):\n    return {}\n" }
        }]
    });
    let rules_list = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 200,
            "url": format!("{}/api/v1/rules/200", test_server.uri()),
            "name": "check-totals",
            "queues": [],
            "code": "def r():\n    return None\n"
        }]
    });
    mount_full_pull(&test_server, hooks_list).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rules_list))
        .mount(&test_server).await;
    mount_full_pull(&prod_server, empty_list()).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .args(["init",
               "--env", &format!("test={}/api/v1:1", test_server.uri()),
               "--env", &format!("prod={}/api/v1:1", prod_server.uri())])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "test"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "prod"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert().success();

    let assert = Command::cargo_bin("rdc").unwrap()
        .args(["deploy", "test", "prod", "--yes", "--dry-run", "--only", "hooks/*"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert().success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(out.contains("hooks"), "expected hooks in plan; got:\n{out}");
    assert!(!out.contains("rules"), "rules must not appear under --only hooks/*; got:\n{out}");
}

/// `rdc deploy --only hooks/x` must POST /hooks but never POST /rules, even
/// when both kinds have new objects in src that are absent from tgt.
#[tokio::test]
async fn deploy_only_creates_filtered_kind_only() {
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    let rule_post_calls = Arc::new(Mutex::new(0u32));

    let hooks_list = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 100,
            "url": format!("{}/api/v1/hooks/100", test_server.uri()),
            "name": "x", "type": "function", "queues": [],
            "events": ["annotation_status"],
            "config": { "runtime": "python3.12", "code": "def x(p):\n    return {}\n" }
        }]
    });
    let rules_list = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 200,
            "url": format!("{}/api/v1/rules/200", test_server.uri()),
            "name": "y", "queues": [], "code": "def y():\n    return None\n"
        }]
    });
    mount_full_pull(&test_server, hooks_list).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rules_list))
        .mount(&test_server).await;
    mount_full_pull(&prod_server, empty_list()).await;

    let counter = rule_post_calls.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/rules"))
        .respond_with(move |_req: &wiremock::Request| {
            *counter.lock().unwrap() += 1;
            ResponseTemplate::new(201).set_body_json(serde_json::json!({}))
        })
        .mount(&prod_server).await;
    let hook_body = serde_json::json!({
        "id": 999,
        "url": format!("{}/api/v1/hooks/999", prod_server.uri()),
        "name": "x", "type": "function", "queues": [], "events": ["annotation_status"],
        "config": { "runtime": "python3.12", "code": "def x(p):\n    return {}\n" }
    });
    Mock::given(method("POST"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(201).set_body_json(hook_body.clone()))
        .mount(&prod_server).await;
    // Apply's drift check does GET /hooks/999 — return the same body so apply
    // sees the hook as already in sync and issues no PATCH.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_body))
        .mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .args(["init",
               "--env", &format!("test={}/api/v1:1", test_server.uri()),
               "--env", &format!("prod={}/api/v1:1", prod_server.uri())])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "test"]).current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "prod"]).current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T").assert().success();

    Command::cargo_bin("rdc").unwrap()
        .args(["deploy", "test", "prod", "--yes", "--only", "hooks/x"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();

    assert_eq!(*rule_post_calls.lock().unwrap(), 0,
        "POST /rules must not be called when --only hooks/x");
}

/// `--only hooks/x-v2` must PATCH the matching hook but never PATCH
/// /rules/<id>, even when both objects diverge between src and tgt.
#[tokio::test]
async fn deploy_only_update_sweep_skips_unmatched_kinds() {
    // Both envs already have hook X and rule Y (existing objects).
    // src and tgt diverge on each. `--only hooks/x-v2` must PATCH the
    // hook but NEVER PATCH /rules/<id>.
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    let rule_patch_calls = Arc::new(Mutex::new(0u32));

    let hook_test = serde_json::json!({
        "id": 100, "url": format!("{}/api/v1/hooks/100", test_server.uri()),
        "name": "x-v2", "type": "function", "queues": [], "events": ["annotation_status"],
        "config": { "runtime": "python3.12", "code": "def x(p):\n    return {'v': 2}\n" }
    });
    let hook_prod = serde_json::json!({
        "id": 900, "url": format!("{}/api/v1/hooks/900", prod_server.uri()),
        "name": "x-v2", "type": "function", "queues": [], "events": ["annotation_status"],
        "config": { "runtime": "python3.12", "code": "def x(p):\n    return {'v': 1}\n" }
    });
    let rule_test = serde_json::json!({
        "id": 200, "url": format!("{}/api/v1/rules/200", test_server.uri()),
        "name": "y", "queues": [], "code": "def r():\n    return 2\n"
    });
    let rule_prod = serde_json::json!({
        "id": 950, "url": format!("{}/api/v1/rules/950", prod_server.uri()),
        "name": "y", "queues": [], "code": "def r():\n    return 1\n"
    });

    mount_full_pull(
        &test_server,
        serde_json::json!({"pagination": {"next": null}, "results": [hook_test.clone()]}),
    ).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [rule_test.clone()]
        })))
        .mount(&test_server).await;
    mount_full_pull(
        &prod_server,
        serde_json::json!({"pagination": {"next": null}, "results": [hook_prod.clone()]}),
    ).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [rule_prod.clone()]
        })))
        .mount(&prod_server).await;
    // GET single hook for apply drift check.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_prod.clone()))
        .mount(&prod_server).await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_prod.clone()))
        .mount(&prod_server).await;
    let counter = rule_patch_calls.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/rules/950"))
        .respond_with(move |_req: &wiremock::Request| {
            *counter.lock().unwrap() += 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
        })
        .mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .args(["init",
               "--env", &format!("test={}/api/v1:1", test_server.uri()),
               "--env", &format!("prod={}/api/v1:1", prod_server.uri())])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "test"]).current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "prod"]).current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T").assert().success();

    Command::cargo_bin("rdc").unwrap()
        .args(["deploy", "test", "prod", "--yes", "--only", "hooks/x-v2"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();

    assert_eq!(*rule_patch_calls.lock().unwrap(), 0,
        "PATCH /rules/950 must not be called when --only hooks/x-v2");
}

/// `rdc deploy test prod --dry-run` with a store extension in src must print
/// the store-extension sub-line in the plan summary, naming the slug and the
/// template on the target cluster. No writes should reach the tgt server.
#[tokio::test]
async fn deploy_plan_lists_store_extensions_in_dry_run() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;
    let src_api = format!("{}/api/v1", test_server.uri());
    let tgt_api = format!("{}/api/v1", prod_server.uri());

    // ── Src (test) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&test_server).await;
    let mdh_src = {
        let mut b = mdh_snapshot_body(&src_api);
        b["id"] = serde_json::Value::from(999u64);
        b["url"] = serde_json::Value::String(format!("{src_api}/hooks/999"));
        b
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [mdh_src.clone()]
        })))
        .mount(&test_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server).await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{src_api}/hook_templates/39"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&test_server).await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/hooks",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server).await;
    }
    // tgt /hook_templates — id 41 (different from src's 39)
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{tgt_api}/hook_templates/41"),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&prod_server).await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env", &format!("test={src_api}:1"),
            "--env", &format!("prod={tgt_api}:1"),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "test"])
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert().success();

    // Pre-populate the tgt overlay with the system user URL so the pre-pass
    // resolves token_owner without hanging on stdin.
    let prod_overlay_path = project.path().join("envs/prod/overlay.toml");
    std::fs::create_dir_all(prod_overlay_path.parent().unwrap()).unwrap();
    std::fs::write(
        &prod_overlay_path,
        format!(
            "version = 1\n\n[defaults]\nstore_extension_token_owner = \"{tgt_api}/users/521884\"\n"
        ),
    ).unwrap();

    // --dry-run: plan is printed but no writes happen.
    let out = Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--dry-run"])
        .output().unwrap();

    assert!(
        out.status.success(),
        "dry-run deploy should succeed.\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);

    // The plan summary must contain the store-extension sub-line.
    assert!(
        stdout.contains("store extension"),
        "stdout should mention 'store extension':\n{stdout}"
    );
    assert!(
        stdout.contains("master-data-hub"),
        "stdout should name the slug 'master-data-hub':\n{stdout}"
    );

    // No mutating requests (POST/PATCH/DELETE) should have reached tgt.
    for req in prod_server.received_requests().await.unwrap_or_default() {
        let path = req.url.path();
        if path.contains("/svc/data-storage/") {
            continue;
        }
        assert!(
            !matches!(
                req.method,
                http::Method::POST | http::Method::PATCH | http::Method::DELETE
            ),
            "unexpected mutating request in dry-run: {} {}",
            req.method,
            path
        );
    }
}

#[tokio::test]
async fn deploy_only_mirror_only_deletes_in_scope() {
    // tgt has hook A and rule B; src has neither. `--mirror --only hooks/*`
    // should DELETE /hooks/<id> but NOT DELETE /rules/<id>.
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    let rule_delete_calls = Arc::new(Mutex::new(0u32));
    let hook_delete_calls = Arc::new(Mutex::new(0u32));

    mount_full_pull(&test_server, empty_list()).await;

    let prod_hooks = serde_json::json!({
        "pagination": {"next": null}, "results": [{
            "id": 900, "url": format!("{}/api/v1/hooks/900", prod_server.uri()),
            "name": "a", "type": "function", "queues": [], "events": ["annotation_status"],
            "config": { "runtime": "python3.12", "code": "def a(p): pass\n" }
        }]
    });
    let prod_rules = serde_json::json!({
        "pagination": {"next": null}, "results": [{
            "id": 950, "url": format!("{}/api/v1/rules/950", prod_server.uri()),
            "name": "b", "queues": [], "code": "def b(): pass\n"
        }]
    });

    // Mount prod pull manually so we can return prod_rules instead of empty.
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_hooks))
        .mount(&prod_server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_rules))
        .mount(&prod_server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server).await;
    }

    let hc = hook_delete_calls.clone();
    Mock::given(method("DELETE"))
        .and(path("/api/v1/hooks/900"))
        .respond_with(move |_: &wiremock::Request| {
            *hc.lock().unwrap() += 1;
            ResponseTemplate::new(204)
        })
        .mount(&prod_server).await;
    let rc = rule_delete_calls.clone();
    Mock::given(method("DELETE"))
        .and(path("/api/v1/rules/950"))
        .respond_with(move |_: &wiremock::Request| {
            *rc.lock().unwrap() += 1;
            ResponseTemplate::new(204)
        })
        .mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .args(["init",
               "--env", &format!("test={}/api/v1:1", test_server.uri()),
               "--env", &format!("prod={}/api/v1:1", prod_server.uri())])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "test"]).current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").assert().success();
    Command::cargo_bin("rdc").unwrap()
        .args(["pull", "prod"]).current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T").assert().success();

    Command::cargo_bin("rdc").unwrap()
        .args(["deploy", "test", "prod", "--yes", "--mirror", "--only", "hooks/*"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T").env("RDC_TOKEN_PROD", "T")
        .assert().success();

    assert_eq!(*hook_delete_calls.lock().unwrap(), 1, "hook A must be deleted");
    assert_eq!(*rule_delete_calls.lock().unwrap(), 0, "rule B must survive --only hooks/*");
}
