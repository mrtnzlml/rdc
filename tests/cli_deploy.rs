use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
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
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hooks_payload))
        .mount(server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server)
            .await;
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
        .mount(&prod_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/402"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_hook_402))
        .mount(&prod_server)
        .await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/401"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *captured_clone.lock().unwrap() = Some(body.clone());
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/402"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"PROD_TOKEN"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

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
    )
    .unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // `rdc deploy` now owns the full cross-env workflow — it auto-builds
    // the mapping, prints a plan, and runs the update sub-step in one
    // call. Only validator-invoices needs a PATCH (overlay renames it);
    // sftp-import is byte-identical between test and prod after
    // env-specific stripping, so it's skipped as idempotent.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .success()
        // Preview pass emits the per-object diff with a `- tgt before / + tgt
        // after` legend so the reader sees the tgt state delta directly,
        // instead of an apples-to-oranges src-vs-tgt comparison.
        .stdout(predicate::str::contains("tgt before"))
        .stdout(predicate::str::contains("tgt after"))
        .stdout(predicate::str::contains("1 hooks"))
        .stdout(predicate::str::contains("(1 PATCHes)"));

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("PATCH body for hook 401");
    assert_eq!(
        body["name"],
        serde_json::Value::String("Validator (PROD)".into())
    );

    // Inline write-back: after `rdc deploy`, the local tgt snapshot must
    // reflect the PATCH response (no separate `rdc sync` needed).
    let written = std::fs::read_to_string(
        project
            .path()
            .join("envs/prod/hooks/validator-invoices.json"),
    )
    .expect("local prod hook file must exist after deploy");
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["name"],
        serde_json::Value::String("Validator (PROD)".into()),
        "local prod hook file should reflect the PATCH response",
    );
}

/// Mount mocks sufficient to pull a single workspace + queue + schema, with
/// every other kind empty.
async fn mount_minimal_for_deploy(server: &MockServer, schema: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server)
        .await;
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
        .mount(server)
        .await;
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
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(schema))
        .mount(server)
        .await;
    for ep in [
        "/api/v1/hooks",
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server)
            .await;
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
        .mount(&prod_server)
        .await;

    let schema_patch_seen: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let schema_patch_seen_clone = schema_patch_seen.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(move |req: &wiremock::Request| {
            *schema_patch_seen_clone.lock().unwrap() = true;
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"PROD"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // Edit test queue + schema formula to differ from prod (so apply has
    // a real change to push; otherwise apply's idempotency would
    // correctly skip the no-diff cases).
    let queue_path = project
        .path()
        .join("envs/test/workspaces/invoices-ap/queues/cost-invoices/queue.json");
    let raw = std::fs::read_to_string(&queue_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["default_score_threshold"] = serde_json::json!(0.99);
    std::fs::write(
        &queue_path,
        format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    )
    .unwrap();

    // Edit test schema's first formula so it differs from prod's schema.
    let formula_dir = project
        .path()
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
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 queues"))
        .stdout(predicate::str::contains("1 schemas"));

    let captured = queue_patch_body
        .lock()
        .unwrap()
        .clone()
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
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/700"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 700,
            "url": "https://test.rossum.app/api/v1/schemas/700",
            "name": "Cost Invoices schema",
            "queues": ["https://test.rossum.app/api/v1/queues/600"],
            "content": [],
        })))
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
    for ep in [
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
    }

    // --- PROD env: empty (every list returns 0 results) ---
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server)
            .await;
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
        })
        .mount(&prod_server)
        .await;
    let pc = post_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/schemas"))
        .respond_with(move |req: &wiremock::Request| {
            *pc.lock().unwrap() += 1;
            let mut body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["id"] = serde_json::json!(7700);
            body["url"] = serde_json::json!("https://prod.rossum.app/api/v1/schemas/7700");
            ResponseTemplate::new(201).set_body_json(body)
        })
        .mount(&prod_server)
        .await;
    let pc = post_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/queues"))
        .respond_with(move |req: &wiremock::Request| {
            *pc.lock().unwrap() += 1;
            let mut body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["id"] = serde_json::json!(6600);
            body["url"] = serde_json::json!("https://prod.rossum.app/api/v1/queues/6600");
            ResponseTemplate::new(201).set_body_json(body)
        })
        .mount(&prod_server)
        .await;
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
        })
        .mount(&prod_server)
        .await;

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
        })
        .mount(&prod_server)
        .await;
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
        })
        .mount(&prod_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"PROD"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // === The one-command deploy. ===
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .success()
        // Preview emits create-body new-file diffs labeled by kind/slug.
        .stdout(predicate::str::contains(
            "--- create bodies (would-be POST) ---",
        ))
        .stdout(predicate::str::contains("workspaces"))
        .stdout(predicate::str::contains("schemas"))
        .stdout(predicate::str::contains("queues"))
        .stdout(predicate::str::contains("hooks"));

    // 4 POSTs: 1 workspace + 1 schema + 1 queue + 1 hook
    assert_eq!(*post_count.lock().unwrap(), 4, "expected exactly 4 POSTs");

    // The hook's POST body must reference the PROD queue URL, not the test one.
    let hook_body = hook_post_body
        .lock()
        .unwrap()
        .clone()
        .expect("hook POST body captured");
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
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
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
        .mount(&test_server)
        .await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server)
            .await;
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
        .mount(&prod_server)
        .await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={src_api}:1"),
            "--env",
            &format!("prod={tgt_api}:1"),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // Deliberately omit the tgt overlay — no store_extension_token_owner set.

    // Deploy with --yes (non-interactive) must fail.
    Command::cargo_bin("rdc")
        .unwrap()
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
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
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
        .mount(&test_server)
        .await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server)
            .await;
    }
    // tgt /hook_templates — EMPTY: the template is not available on prod
    Mock::given(method("GET"))
        .and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null},
            "results": []
        })))
        .mount(&prod_server)
        .await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={src_api}:1"),
            "--env",
            &format!("prod={tgt_api}:1"),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // Tgt overlay HAS store_extension_token_owner — so the failure is
    // purely template-related, not token-related.
    let prod_overlay_path = project.path().join("envs/prod/overlay.toml");
    std::fs::create_dir_all(prod_overlay_path.parent().unwrap()).unwrap();
    std::fs::write(
        &prod_overlay_path,
        format!(
            "version = 1\n\n[defaults]\nstore_extension_token_owner = \"{tgt_api}/users/521884\"\n"
        ),
    )
    .unwrap();

    // Deploy with --yes must fail because the template is absent on prod.
    Command::cargo_bin("rdc")
        .unwrap()
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
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
    // all other src list endpoints — empty
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
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
        .mount(&test_server)
        .await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server)
        .await;
    // prod starts with no hooks
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server)
            .await;
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
        .mount(&prod_server)
        .await;

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
        .mount(&prod_server)
        .await;
    // Two-call install: POST /hooks/create.
    Mock::given(method("POST"))
        .and(path("/api/v1/hooks/create"))
        .respond_with(ResponseTemplate::new(201).set_body_json(installed_on_tgt.clone()))
        .mount(&prod_server)
        .await;
    // Reconcile PATCH /hooks/700.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/700"))
        .respond_with(ResponseTemplate::new(200).set_body_json(customised_on_tgt.clone()))
        .mount(&prod_server)
        .await;
    // apply drift check: GET /api/v1/hooks/700.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/700"))
        .respond_with(ResponseTemplate::new(200).set_body_json(customised_on_tgt.clone()))
        .mount(&prod_server)
        .await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={src_api}:1"),
            "--env",
            &format!("prod={tgt_api}:1"),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    )
    .unwrap();

    // Pull both envs to establish lockfiles and snapshots.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // Pre-populate the tgt overlay with the system user URL so the pre-pass
    // resolves token_owner without hanging on stdin (non-interactive path).
    let prod_overlay_path = project.path().join("envs/prod/overlay.toml");
    std::fs::create_dir_all(prod_overlay_path.parent().unwrap()).unwrap();
    std::fs::write(
        &prod_overlay_path,
        format!(
            "version = 1\n\n[defaults]\nstore_extension_token_owner = \"{tgt_api}/users/521884\"\n"
        ),
    )
    .unwrap();

    // Deploy — pre-pass resolves templates, reads token_owner from overlay.
    let out = Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .output()
        .unwrap();

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
    assert!(
        hook_id > 0,
        "hook id in tgt lockfile must be positive, got {hook_id}"
    );
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
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "deploy",
            "test",
            "prod",
            "--yes",
            "--only",
            "hooks/nonexistent",
        ])
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
        .mount(&test_server)
        .await;
    mount_full_pull(&prod_server, empty_list()).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    let assert = Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "deploy",
            "test",
            "prod",
            "--yes",
            "--dry-run",
            "--only",
            "hooks/*",
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // Preview should emit a create body for the in-scope hook…
    assert!(
        out.contains("hooks/validator.json"),
        "expected hooks/validator.json in preview; got:\n{out}"
    );
    // …and never reference the rule's slug, since `--only hooks/*` excludes
    // it. The apply summary line legitimately contains the bare word
    // "rules" (as "0 rules"), so match the slug instead.
    assert!(
        !out.contains("check-totals"),
        "rule slug must not appear under --only hooks/*; got:\n{out}"
    );
    assert!(
        !out.contains("rules/"),
        "rules/<slug> path must not appear under --only hooks/*; got:\n{out}"
    );
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
        .mount(&test_server)
        .await;
    mount_full_pull(&prod_server, empty_list()).await;

    let counter = rule_post_calls.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/rules"))
        .respond_with(move |_req: &wiremock::Request| {
            *counter.lock().unwrap() += 1;
            ResponseTemplate::new(201).set_body_json(serde_json::json!({}))
        })
        .mount(&prod_server)
        .await;
    let hook_body = serde_json::json!({
        "id": 999,
        "url": format!("{}/api/v1/hooks/999", prod_server.uri()),
        "name": "x", "type": "function", "queues": [], "events": ["annotation_status"],
        "config": { "runtime": "python3.12", "code": "def x(p):\n    return {}\n" }
    });
    Mock::given(method("POST"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(201).set_body_json(hook_body.clone()))
        .mount(&prod_server)
        .await;
    // Apply's drift check does GET /hooks/999 — return the same body so apply
    // sees the hook as already in sync and issues no PATCH.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_body))
        .mount(&prod_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .args(["deploy", "test", "prod", "--yes", "--only", "hooks/x"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    assert_eq!(
        *rule_post_calls.lock().unwrap(),
        0,
        "POST /rules must not be called when --only hooks/x"
    );
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
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [rule_test.clone()]
        })))
        .mount(&test_server)
        .await;
    mount_full_pull(
        &prod_server,
        serde_json::json!({"pagination": {"next": null}, "results": [hook_prod.clone()]}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [rule_prod.clone()]
        })))
        .mount(&prod_server)
        .await;
    // GET single hook for apply drift check.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_prod.clone()))
        .mount(&prod_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_prod.clone()))
        .mount(&prod_server)
        .await;
    let counter = rule_patch_calls.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/rules/950"))
        .respond_with(move |_req: &wiremock::Request| {
            *counter.lock().unwrap() += 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
        })
        .mount(&prod_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .args(["deploy", "test", "prod", "--yes", "--only", "hooks/x-v2"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    assert_eq!(
        *rule_patch_calls.lock().unwrap(),
        0,
        "PATCH /rules/950 must not be called when --only hooks/x-v2"
    );
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
        .mount(&test_server)
        .await;
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
        .mount(&test_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
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
        .mount(&test_server)
        .await;

    // ── Tgt (prod) pull mocks ────────────────────────────────────────────────
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&prod_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server)
            .await;
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
        .mount(&prod_server)
        .await;

    // ── Project bootstrap ────────────────────────────────────────────────────
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={src_api}:1"),
            "--env",
            &format!("prod={tgt_api}:1"),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TKN_TEST"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"TKN_PROD"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // Pre-populate the tgt overlay with the system user URL so the pre-pass
    // resolves token_owner without hanging on stdin.
    let prod_overlay_path = project.path().join("envs/prod/overlay.toml");
    std::fs::create_dir_all(prod_overlay_path.parent().unwrap()).unwrap();
    std::fs::write(
        &prod_overlay_path,
        format!(
            "version = 1\n\n[defaults]\nstore_extension_token_owner = \"{tgt_api}/users/521884\"\n"
        ),
    )
    .unwrap();

    // --dry-run: plan is printed but no writes happen.
    let out = Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "dry-run deploy should succeed.\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);

    // The preview must surface the store-extension by name + by its
    // `extension_source: "rossum_store"` marker in the POST body. (The
    // old plan summary spelled out "store extension" in a sub-line; now
    // the same evidence lives directly in the rendered diff bodies.)
    assert!(
        stdout.contains("master-data-hub"),
        "stdout should name the slug 'master-data-hub':\n{stdout}"
    );
    assert!(
        stdout.contains("rossum_store"),
        "stdout should expose extension_source=rossum_store in the preview body:\n{stdout}"
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
        .mount(&prod_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_hooks))
        .mount(&prod_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prod_rules))
        .mount(&prod_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&prod_server)
            .await;
    }

    let hc = hook_delete_calls.clone();
    Mock::given(method("DELETE"))
        .and(path("/api/v1/hooks/900"))
        .respond_with(move |_: &wiremock::Request| {
            *hc.lock().unwrap() += 1;
            ResponseTemplate::new(204)
        })
        .mount(&prod_server)
        .await;
    let rc = rule_delete_calls.clone();
    Mock::given(method("DELETE"))
        .and(path("/api/v1/rules/950"))
        .respond_with(move |_: &wiremock::Request| {
            *rc.lock().unwrap() += 1;
            ResponseTemplate::new(204)
        })
        .mount(&prod_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "deploy", "test", "prod", "--yes", "--mirror", "--only", "hooks/*",
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    assert_eq!(
        *hook_delete_calls.lock().unwrap(),
        1,
        "hook A must be deleted"
    );
    assert_eq!(
        *rule_delete_calls.lock().unwrap(),
        0,
        "rule B must survive --only hooks/*"
    );
}

/// `rdc deploy --only hooks/h --yes` must refuse when the selected hook
/// references a queue that is not yet on tgt and not in --only.
/// The error message must name the missing dep (queues/q) and suggest
/// adding it via --only.
///
/// Mounts the test server manually (not via mount_full_pull) so we can
/// return a real workspace+queue pair without fighting wiremock's
/// first-registered-wins rule. The queue must have a non-null workspace
/// so it gets recorded in the src lockfile (orphan queues with workspace=null
/// are skipped at pull time and thus invisible to the dep-check).
#[tokio::test]
async fn deploy_only_missing_dep_ci_refuses_with_suggestion() {
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    let ws_url = format!("{}/api/v1/workspaces/500", test_server.uri());
    let q_url = format!("{}/api/v1/queues/600", test_server.uri());

    let workspace_body = serde_json::json!({
        "id": 500, "url": ws_url,
        "name": "ws",
        "organization": format!("{}/api/v1/organizations/1", test_server.uri()),
        "queues": [q_url]
    });
    let queue_body = serde_json::json!({
        "id": 600, "url": q_url,
        "name": "q", "workspace": ws_url, "schema": null
    });
    let hook_body = serde_json::json!({
        "id": 100, "url": format!("{}/api/v1/hooks/100", test_server.uri()),
        "name": "h", "type": "function",
        "queues": [q_url],
        "events": ["annotation_content"],
        "config": { "runtime": "python3.12", "code": "def h(p): pass\n" }
    });

    // Mount test server manually so both workspace and queue are returned.
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&test_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [hook_body]
        })))
        .mount(&test_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [workspace_body]
        })))
        .mount(&test_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": {"next": null}, "results": [queue_body]
        })))
        .mount(&test_server)
        .await;
    for ep in [
        "/api/v1/inboxes",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
    }

    mount_full_pull(&prod_server, empty_list()).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .args(["deploy", "test", "prod", "--yes", "--only", "hooks/h"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .failure()
        .stderr(predicate::str::contains("queues/q"))
        .stderr(predicate::str::contains("--only"));
}

/// `rdc deploy --only hooks/h --dry-run --yes` must produce zero write API
/// calls to the target environment. The dry-run flag must suppress all
/// POST and PATCH requests even when there is a real diff to apply.
///
/// The POST/PATCH catch-all mocks are mounted only after the pull commands
/// complete, so the data-storage MDH probe (POST .../collections/list) that
/// runs during pull sees an unmatched request → 404 → quiet MDH skip, and
/// the counters only capture writes that the deploy phase would emit.
#[tokio::test]
async fn deploy_only_dry_run_makes_no_api_calls() {
    use std::sync::{Arc, Mutex};
    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    let post_or_patch = Arc::new(Mutex::new(0u32));

    let hook_body = serde_json::json!({
        "id": 100, "url": format!("{}/api/v1/hooks/100", test_server.uri()),
        "name": "h", "type": "function", "queues": [], "events": ["annotation_status"],
        "config": { "runtime": "python3.12", "code": "def h(p): pass\n" }
    });
    mount_full_pull(
        &test_server,
        serde_json::json!({"pagination": {"next": null}, "results": [hook_body]}),
    )
    .await;
    mount_full_pull(&prod_server, empty_list()).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", test_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "test", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .args(["sync", "prod", "--no-push"])
        .current_dir(project.path())
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    // Mount write-interceptors only after pulls so the MDH probe
    // (POST .../collections/list) during pull returns 404 instead of
    // hitting these catch-alls.  The regex restricts to /api/v1/* so
    // any stray data-storage POSTs during deploy are also excluded.
    let c1 = post_or_patch.clone();
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/v1/"))
        .respond_with(move |_: &wiremock::Request| {
            *c1.lock().unwrap() += 1;
            ResponseTemplate::new(201).set_body_json(serde_json::json!({}))
        })
        .mount(&prod_server)
        .await;
    let c2 = post_or_patch.clone();
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/api/v1/"))
        .respond_with(move |_: &wiremock::Request| {
            *c2.lock().unwrap() += 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
        })
        .mount(&prod_server)
        .await;

    Command::cargo_bin("rdc")
        .unwrap()
        .args([
            "deploy",
            "test",
            "prod",
            "--yes",
            "--dry-run",
            "--only",
            "hooks/h",
        ])
        .current_dir(project.path())
        .env("RDC_TOKEN_TEST", "T")
        .env("RDC_TOKEN_PROD", "T")
        .assert()
        .success();

    assert_eq!(
        *post_or_patch.lock().unwrap(),
        0,
        "dry-run must make no write API calls"
    );
}

/// Deploy must wait for the target env's lock before entering its write
/// phase. A separate thread holds the prod lock for ~600 ms; we then
/// spawn `rdc deploy test prod` and assert it spent at least 400 ms
/// blocked on lock acquisition before returning. The blocker uses a
/// channel to signal "lock acquired" so the main thread doesn't race
/// the blocker's startup latency.
#[tokio::test]
async fn deploy_waits_for_tgt_env_lock() {
    use rdc::cli::sync::lock::EnvLock;
    use std::time::Duration;

    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().to_path_buf();

    // Hold the prod lock for 600 ms in a background thread. The blocker
    // sends a notification over `ready_tx` immediately after acquiring
    // the lock; the main thread blocks on `ready_rx.recv()` so deploy
    // only starts after the lock is provably held — no thread-startup
    // race.
    let lock_dir = project_dir.join(".rdc/state");
    std::fs::create_dir_all(&lock_dir).unwrap();
    let lock_path = lock_dir.join("prod.lock");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let blocker = std::thread::spawn({
        let lock_path = lock_path.clone();
        move || {
            let l = EnvLock::acquire(&lock_path, Duration::from_secs(2)).unwrap();
            // Signal: the prod lock is now held.
            ready_tx.send(()).expect("ready_rx dropped before send");
            std::thread::sleep(Duration::from_millis(600));
            drop(l);
        }
    });

    // Wait deterministically until the blocker holds the prod lock.
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("blocker thread failed to acquire the prod lock within 5s");

    // Minimum project layout — empty snapshots, both envs point at an
    // unreachable URL. We don't need a real deploy to succeed; we only
    // need execution to reach the lock-acquisition point. With empty
    // snapshots the store-extension prepass exits early without API
    // calls, and the (empty) plan skips the confirm.
    std::fs::write(
        project_dir.join("rdc.toml"),
        r#"[envs.test]
api_base = "http://127.0.0.1:1/api/v1"
org_id = 1
[envs.prod]
api_base = "http://127.0.0.1:1/api/v1"
org_id = 1
"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("secrets")).unwrap();
    std::fs::write(
        project_dir.join("secrets/test.secrets.json"),
        r#"{"api_token":"t"}"#,
    )
    .unwrap();
    std::fs::write(
        project_dir.join("secrets/prod.secrets.json"),
        r#"{"api_token":"t"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("envs/test")).unwrap();
    std::fs::create_dir_all(project_dir.join("envs/prod")).unwrap();

    let start = std::time::Instant::now();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rdc"))
        .args(["--yes", "deploy", "test", "prod"])
        .current_dir(&project_dir)
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    blocker.join().unwrap();

    // Deploy may succeed or fail (the API URL is unreachable). What
    // matters is the elapsed time — it must have waited for the lock.
    // 400ms (a little less than 500) accommodates thread-start jitter.
    assert!(
        elapsed >= Duration::from_millis(400),
        "deploy returned too quickly ({elapsed:?}); should have waited for the lock. \
         stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Regression: `rdc deploy` from one env to another used to POST the src
/// `organization` URL on workspace create, which the API rejected with
/// `400 {"organization":["Invalid hyperlink - Object does not exist."]}`
/// because the src org URL doesn't resolve in the tgt env. The fix rewrites
/// `organization` URLs via the tgt lockfile's `organization/self` entry
/// (organization isn't a deployable kind, so no mapping entry exists for it).
///
/// This test uses distinct src/tgt org IDs so the rewrite has work to do.
#[tokio::test]
async fn deploy_rewrites_organization_url_on_workspace_create() {
    use std::sync::{Arc, Mutex};

    let dev_server = MockServer::start().await;
    let test_server = MockServer::start().await;

    // --- DEV env: org_id 111, one workspace ---
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/111"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 111,
            "url": format!("{}/api/v1/organizations/111", dev_server.uri()),
            "name": "Dev Org",
            "modified_at": "2026-05-22T08:00:00Z",
            "settings": {},
            "users": [],
        })))
        .mount(&dev_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 500,
                "url": format!("{}/api/v1/workspaces/500", dev_server.uri()),
                "name": "Main Workspace",
                "organization": format!("{}/api/v1/organizations/111", dev_server.uri()),
                "queues": []
            }]
        })))
        .mount(&dev_server)
        .await;
    for ep in [
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&dev_server)
            .await;
    }

    // --- TEST env: org_id 222, empty, distinct org URL ---
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/222"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 222,
            "url": format!("{}/api/v1/organizations/222", test_server.uri()),
            "name": "Test Org",
            "modified_at": "2026-05-22T08:00:00Z",
            "settings": {},
            "users": [],
        })))
        .mount(&test_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&test_server)
            .await;
    }

    // --- TEST env workspace POST: capture body, assert org URL is rewritten ---
    let captured_ws_body: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let cap = captured_ws_body.clone();
    let test_uri = test_server.uri();
    Mock::given(method("POST"))
        .and(path("/api/v1/workspaces"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *cap.lock().unwrap() = Some(body.clone());
            let mut resp = body;
            resp["id"] = serde_json::json!(900);
            resp["url"] = serde_json::json!(format!("{test_uri}/api/v1/workspaces/900"));
            ResponseTemplate::new(201).set_body_json(resp)
        })
        .mount(&test_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("dev={}/api/v1:111", dev_server.uri()),
            "--env",
            &format!("test={}/api/v1:222", test_server.uri()),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"DEV"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "dev", "test", "--yes"])
        .assert()
        .success();

    let body = captured_ws_body
        .lock()
        .unwrap()
        .clone()
        .expect("workspace POST body captured");
    assert_eq!(
        body["organization"].as_str().unwrap(),
        format!("{}/api/v1/organizations/222", test_server.uri()),
        "workspace POST body must carry the TEST org URL, not the DEV one"
    );
    assert_eq!(body["name"].as_str().unwrap(), "Main Workspace");
}

/// First deploy with hook secrets unset on the target: the precheck must
/// abort the deploy AND write `secrets/<tgt>.hook-secrets.json` populated
/// with every required key (empty placeholders), so the next deploy is a
/// fill-in-the-blanks loop instead of a JSON-shape scavenger hunt.
#[tokio::test]
async fn deploy_pre_populates_hook_secrets_file_on_missing_keys() {
    let src_server = MockServer::start().await;
    let tgt_server = MockServer::start().await;

    let hook_id = 4242u64;
    let src_uri = src_server.uri();
    let src_hooks = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": hook_id,
            "url": format!("{src_uri}/api/v1/hooks/{hook_id}"),
            "name": "MDH lookup",
            "type": "webhook",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "url": "https://mdh.example.com/lookup" }
        }]
    });
    mount_full_pull(&src_server, src_hooks).await;
    mount_full_pull(&tgt_server, empty_list()).await;

    // The new precheck step issues GET /hooks/<id>/secrets_keys on src.
    // Return two keys so the rendered template covers the multi-key case.
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/hooks/{hook_id}/secrets_keys")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!(["api_key", "signing_secret"])),
        )
        .mount(&src_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("test={}/api/v1:1", src_server.uri()),
            "--env",
            &format!("prod={}/api/v1:1", tgt_server.uri()),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"T"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"P"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "test", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "prod", "--no-push"])
        .assert()
        .success();

    // Deploy must abort: no `secrets/prod.hook-secrets.json` yet.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("deploy refused"))
        .stderr(predicate::str::contains("hooks/mdh-lookup"))
        .stderr(predicate::str::contains("api_key"))
        .stderr(predicate::str::contains("signing_secret"))
        .stderr(predicate::str::contains("pre-populated"))
        .stderr(predicate::str::contains("prod.hook-secrets.json"));

    // The template file must exist on disk with both required keys
    // pre-populated with the UNFILLED sentinel — re-running deploy
    // without edits must NOT count the placeholders as user input.
    let secrets_path = project.path().join("secrets/prod.hook-secrets.json");
    assert!(
        secrets_path.exists(),
        "precheck must pre-populate {}",
        secrets_path.display()
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&secrets_path).unwrap()).unwrap();
    assert_eq!(
        v["hooks"]["mdh-lookup"]["api_key"],
        rdc::secrets::UNFILLED_SENTINEL
    );
    assert_eq!(
        v["hooks"]["mdh-lookup"]["signing_secret"],
        rdc::secrets::UNFILLED_SENTINEL
    );

    // Bug regression: a re-run with the file unchanged must still
    // refuse, listing the same keys as missing. The previous
    // empty-string placeholder was indistinguishable from a user-
    // provided "" and let the second run pass without any edit.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("deploy refused"))
        .stderr(predicate::str::contains("api_key"))
        .stderr(predicate::str::contains("signing_secret"));

    // Existing values must be preserved on a re-run: pre-fill one key,
    // re-deploy, and assert the value isn't wiped while the still-missing
    // key remains the sentinel.
    std::fs::write(
        &secrets_path,
        r#"{ "hooks": { "mdh-lookup": { "api_key": "kept-by-user" } } }"#,
    )
    .unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "test", "prod", "--yes"])
        .assert()
        .failure();
    let v2: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&secrets_path).unwrap()).unwrap();
    assert_eq!(
        v2["hooks"]["mdh-lookup"]["api_key"], "kept-by-user",
        "the user's typed-in value must survive a re-deploy"
    );
    assert_eq!(
        v2["hooks"]["mdh-lookup"]["signing_secret"],
        rdc::secrets::UNFILLED_SENTINEL
    );
}

/// Deploy-CREATE records the codec baseline for engines: `engine.json` on the
/// tgt snapshot must have `agenda_id` replaced with the redaction sentinel
/// (NOT the raw value returned by the API), must have no `modified_at`, and
/// the lockfile `content_hash` must equal `combined_hash(disk_json, &[])`.
///
/// Regression: before this fix the create path used raw `to_vec_pretty` which
/// wrote the live `agenda_id` to disk and recorded its hash, causing every
/// subsequent pull/sync to compute a codec-baseline hash that differed →
/// phantom drift on engines after a deploy-create.
#[tokio::test]
async fn deploy_create_engine_records_codec_baseline() {
    let src_server = MockServer::start().await;
    let tgt_server = MockServer::start().await;
    let src_uri = src_server.uri();
    let tgt_uri = tgt_server.uri();

    // --- src env: org 1, one engine with a live agenda_id ---
    let src_engine = serde_json::json!({
        "id": 501,
        "url": format!("{src_uri}/api/v1/engines/501"),
        "name": "Invoice Engine",
        "type": "extractor",
        "agenda_id": "tnt_src_live_agenda",
        "modified_at": "2026-05-01T12:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "url": format!("{src_uri}/api/v1/organizations/1"),
            "name": "Src Org", "modified_at": "2026-01-01T00:00:00Z",
            "settings": {}, "users": []
        })))
        .mount(&src_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [src_engine.clone()]
        })))
        .mount(&src_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&src_server)
            .await;
    }

    // --- tgt env: org 2, empty ---
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 2, "url": format!("{tgt_uri}/api/v1/organizations/2"),
            "name": "Tgt Org", "modified_at": "2026-01-01T00:00:00Z",
            "settings": {}, "users": []
        })))
        .mount(&tgt_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&tgt_server)
            .await;
    }

    // The tgt server returns an engine with a *different* tgt-env agenda_id.
    let tgt_uri_c = tgt_uri.clone();
    Mock::given(method("POST"))
        .and(path("/api/v1/engines"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 701,
                "url": format!("{tgt_uri_c}/api/v1/engines/701"),
                "name": "Invoice Engine",
                "type": "extractor",
                "agenda_id": "tnt_tgt_live_agenda",
                "modified_at": "2026-05-02T08:00:00Z"
            }))
        })
        .mount(&tgt_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("src={src_uri}/api/v1:1"),
            "--env",
            &format!("tgt={tgt_uri}/api/v1:2"),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/src.secrets.json"),
        r#"{"api_token":"SRC"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/tgt.secrets.json"),
        r#"{"api_token":"TGT"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "src", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "tgt", "--no-push"])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "src", "tgt", "--yes"])
        .assert()
        .success();

    // --- Assertions ---
    // 1. The tgt on-disk engine.json must have agenda_id = sentinel, no modified_at.
    let engine_json_path = project
        .path()
        .join("envs/tgt/engines/invoice-engine/engine.json");
    assert!(
        engine_json_path.exists(),
        "engine.json must be written to tgt snapshot"
    );
    let disk_str = std::fs::read_to_string(&engine_json_path).unwrap();
    let disk: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
    assert_eq!(
        disk["agenda_id"].as_str().unwrap_or(""),
        rdc::snapshot::create::REDACTED_VALUE_SENTINEL,
        "deploy-create must write agenda_id = codec sentinel, not the raw live value"
    );
    assert!(
        disk.get("modified_at").is_none(),
        "deploy-create must strip modified_at from the tgt snapshot (codec strip)"
    );

    // 2. The tgt lockfile content_hash must equal combined_hash(disk_json, &[]).
    let lf_path = project.path().join(".rdc/state/tgt.lock.json");
    let lf: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lf_path).unwrap()).unwrap();
    let recorded_hash = lf["objects"]["engines"]["invoice-engine"]["content_hash"]
        .as_str()
        .expect("content_hash must be present in tgt lockfile for engine");

    let disk_bytes = std::fs::read(&engine_json_path).unwrap();
    // The deploy create-path records the baseline BEFORE the engine is in the tgt
    // lockfile, so its self-url is not yet normalized (URL form). Match that with
    // an empty lockfile; a subsequent `sync tgt` rebaselines to rdc:// form.
    let expected_hash =
        rdc::snapshot::codec::combined_hash(&disk_bytes, &[], &rdc::state::Lockfile::default());
    assert_eq!(
        recorded_hash, expected_hash,
        "lockfile content_hash must equal combined_hash(disk_bytes, &[]) — \
         codec baseline must be consistent between create-path and pull/sync"
    );
}

/// A cross-env deploy that PATCHes an existing target engine must NOT echo
/// `agenda_id` in the PATCH body: it's a read-only, per-env identifier, and
/// the source value on disk is the redaction sentinel. Sending it back is at
/// best ignored and at worst overwrites the target engine's identifier with
/// the sentinel (or 400s). `strip_patch_extra` must remove it before the PATCH.
#[tokio::test]
async fn deploy_patch_engine_body_omits_agenda_id() {
    let src_server = MockServer::start().await;
    let tgt_server = MockServer::start().await;
    let src_uri = src_server.uri();
    let tgt_uri = tgt_server.uri();

    // src engine: a `description` differs from the tgt's so the deploy is a
    // real PATCH (not an idempotent skip — agenda_id is stripped from the
    // drift comparison, so the two must differ on a NON-stripped field).
    let src_engine = serde_json::json!({
        "id": 501,
        "url": format!("{src_uri}/api/v1/engines/501"),
        "name": "Invoice Engine",
        "type": "extractor",
        "description": "src-side description",
        "agenda_id": "tnt_src_live_agenda",
        "modified_at": "2026-05-01T12:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "url": format!("{src_uri}/api/v1/organizations/1"),
            "name": "Src Org", "modified_at": "2026-01-01T00:00:00Z",
            "settings": {}, "users": []
        })))
        .mount(&src_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [src_engine.clone()]
        })))
        .mount(&src_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&src_server)
            .await;
    }

    // tgt env: org 2 already has a matching engine (id 701) with its OWN
    // agenda_id and a different description.
    let tgt_engine = serde_json::json!({
        "id": 701,
        "url": format!("{tgt_uri}/api/v1/engines/701"),
        "name": "Invoice Engine",
        "type": "extractor",
        "description": "tgt-side description",
        "agenda_id": "tnt_tgt_live_agenda",
        "modified_at": "2026-05-02T08:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 2, "url": format!("{tgt_uri}/api/v1/organizations/2"),
            "name": "Tgt Org", "modified_at": "2026-01-01T00:00:00Z",
            "settings": {}, "users": []
        })))
        .mount(&tgt_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [tgt_engine.clone()]
        })))
        .mount(&tgt_server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&tgt_server)
            .await;
    }

    // The PATCH the deploy issues against the existing tgt engine.
    let tgt_uri_c = tgt_uri.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/engines/701"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 701,
                "url": format!("{tgt_uri_c}/api/v1/engines/701"),
                "name": "Invoice Engine",
                "type": "extractor",
                "description": "src-side description",
                "agenda_id": "tnt_tgt_live_agenda",
                "modified_at": "2026-05-03T08:00:00Z"
            }))
        })
        .expect(1)
        .mount(&tgt_server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("src={src_uri}/api/v1:1"),
            "--env",
            &format!("tgt={tgt_uri}/api/v1:2"),
        ])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/src.secrets.json"),
        r#"{"api_token":"SRC"}"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("secrets/tgt.secrets.json"),
        r#"{"api_token":"TGT"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "src", "--no-push"])
        .assert()
        .success();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "tgt", "--no-push"])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["deploy", "src", "tgt", "--yes"])
        .assert()
        .success();

    // The PATCH body must NOT carry agenda_id.
    let patch_bodies: Vec<serde_json::Value> = tgt_server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/engines/701")
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .collect();
    assert_eq!(
        patch_bodies.len(),
        1,
        "expected exactly one engine PATCH during deploy; got {}",
        patch_bodies.len()
    );
    assert!(
        patch_bodies[0].get("agenda_id").is_none(),
        "deploy engine PATCH body must NOT contain `agenda_id`; got:\n{}",
        serde_json::to_string_pretty(&patch_bodies[0]).unwrap()
    );
}
