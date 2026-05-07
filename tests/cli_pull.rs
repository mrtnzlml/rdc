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
async fn pull_writes_full_workspace_tree() {
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

    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/202"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rules_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("labels_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engines_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/engine_fields"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engine_fields_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflows_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/workflow_steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflow_steps_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("email_templates_list.json")))
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
        .stdout(predicate::str::contains("3 queues"))
        .stdout(predicate::str::contains("3 schemas"))
        .stdout(predicate::str::contains("1 inbox"))
        .stdout(predicate::str::contains("2 hooks"))
        .stdout(predicate::str::contains("1 rule"))
        .stdout(predicate::str::contains("2 labels"))
        .stdout(predicate::str::contains("1 engine"))
        .stdout(predicate::str::contains("2 engine fields"))
        .stdout(predicate::str::contains("1 workflow"))
        .stdout(predicate::str::contains("2 workflow steps"))
        .stdout(predicate::str::contains("1 email template"));

    let env_root = project.path().join("envs/dev");

    // Organization
    assert!(env_root.join("organization.json").exists());

    // Workspaces
    let ws_root = env_root.join("workspaces");
    assert!(ws_root.join("invoices-ap/workspace.json").exists());
    assert!(ws_root.join("purchase-orders/workspace.json").exists());

    // Queues nested under workspaces
    let cost = ws_root.join("invoices-ap/queues/cost-invoices");
    assert!(cost.join("queue.json").exists());
    assert!(cost.join("schema.json").exists());
    assert!(cost.join("inbox.json").exists());
    assert!(cost.join("formulas/amount_total.py").exists());

    let credit = ws_root.join("invoices-ap/queues/credit-notes");
    assert!(credit.join("queue.json").exists());
    assert!(credit.join("schema.json").exists());
    // No inbox for this queue
    assert!(!credit.join("inbox.json").exists());
    // No formulas for this queue
    assert!(!credit.join("formulas").exists());

    let po = ws_root.join("purchase-orders/queues/purchase-orders");
    assert!(po.join("queue.json").exists());
    assert!(po.join("schema.json").exists());

    // Formula content
    let f = std::fs::read_to_string(cost.join("formulas/amount_total.py")).unwrap();
    assert_eq!(f, "amount_due + amount_tax");

    // Schema JSON does NOT contain the formula string
    let schema_raw = std::fs::read_to_string(cost.join("schema.json")).unwrap();
    assert!(!schema_raw.contains("amount_due + amount_tax"));

    // Hooks still pulled
    let hooks_dir = env_root.join("hooks");
    assert!(hooks_dir.join("validator-invoices.json").exists());

    // Lockfile records all kinds with content_hash populated.
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("\"organization\""));
    assert!(lf.contains("\"workspaces\""));
    assert!(lf.contains("\"queues\""));
    assert!(lf.contains("\"schemas\""));
    assert!(lf.contains("\"inboxes\""));
    assert!(lf.contains("\"hooks\""));
    assert!(lf.contains("invoices-ap"));
    assert!(lf.contains("cost-invoices"));
    // content_hash is populated (M2 reviewer's recommendation)
    assert!(lf.contains("\"content_hash\""), "lockfile should record content_hash for entries");
    // Hashes are 64-char hex (SHA-256). Spot-check by counting at least one full hash.
    let hash_re = regex::Regex::new(r#""content_hash":\s*"[0-9a-f]{64}""#).unwrap();
    assert!(hash_re.is_match(&lf), "expected at least one 64-char hex content_hash in lockfile");

    // M4 kinds present
    assert!(env_root.join("rules/e-invoice-validation.json").exists());
    assert!(env_root.join("labels/priority-high.json").exists());
    assert!(env_root.join("labels/needs-review.json").exists());
    assert!(env_root.join("engines/invoice-engine.json").exists());
    assert!(env_root.join("engine-fields/invoice-id.json").exists());
    assert!(env_root.join("engine-fields/total-amount.json").exists());

    // Lockfile records new kinds
    assert!(lf.contains("\"rules\""));
    assert!(lf.contains("\"labels\""));
    assert!(lf.contains("\"engines\""));
    assert!(lf.contains("\"engine_fields\""));

    // M5 kinds present
    assert!(env_root.join("workflows/ap-approval-flow.json").exists());
    assert!(env_root.join("workflow-steps/manager-approval.json").exists());
    assert!(env_root.join("workflow-steps/finance-approval.json").exists());
    assert!(env_root.join("email-templates/rejection-notice.json").exists());

    assert!(lf.contains("\"workflows\""));
    assert!(lf.contains("\"workflow_steps\""));
    assert!(lf.contains("\"email_templates\""));
}

#[tokio::test]
async fn pull_with_workspace_filter_skips_non_matching() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/202"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rules_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("labels_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engines_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engine_fields"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engine_fields_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflows_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workflow_steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflow_steps_list.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("email_templates_list.json")))
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

    // Hand-edit rdc.toml to add workspace_filter that only matches "Invoices AP".
    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let cfg = cfg.replace("[envs.dev]", "[envs.dev]\nworkspace_filter = \"^Invoices AP$\"");
    std::fs::write(&cfg_path, cfg).unwrap();

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
        // Only one workspace pulled (Invoices AP); two queues belong to it.
        .stdout(predicate::str::contains("1 workspace"))
        .stdout(predicate::str::contains("2 queues"));

    let env_root = project.path().join("envs/dev");
    let ws_root = env_root.join("workspaces");
    assert!(ws_root.join("invoices-ap").is_dir());
    assert!(!ws_root.join("purchase-orders").exists(), "filtered workspace should not be pulled");

    // Parse the lockfile JSON and assert exact entry counts/keys (more robust than
    // the old `lf.contains(...)` substring checks, per M4 reviewer recommendation).
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf_value: serde_json::Value = serde_json::from_str(&lf).unwrap();

    let ws_obj = lf_value["objects"]["workspaces"].as_object().unwrap();
    assert_eq!(ws_obj.len(), 1, "expected 1 workspace, got {}: {:?}", ws_obj.len(), ws_obj.keys().collect::<Vec<_>>());
    assert!(ws_obj.contains_key("invoices-ap"));

    let q_obj = lf_value["objects"]["queues"].as_object().unwrap();
    assert_eq!(q_obj.len(), 2, "expected 2 queues, got {}: {:?}", q_obj.len(), q_obj.keys().collect::<Vec<_>>());
    assert!(q_obj.contains_key("cost-invoices"));
    assert!(q_obj.contains_key("credit-notes"));
    assert!(!q_obj.contains_key("purchase-orders"));
}

#[tokio::test]
async fn pull_mdh_when_data_storage_base_is_set() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let empty_list = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list.clone()))
            .mount(&server)
            .await;
    }

    Mock::given(method("GET"))
        .and(path("/data/v1/collections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_collections.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections/vendors/indexes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_indexes_vendors.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections/vendors/search-indexes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_search_indexes_vendors.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections/purchase_orders/indexes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_indexes_purchase_orders.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/v1/collections/purchase_orders/search-indexes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_search_indexes_purchase_orders.json")))
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

    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let cfg = cfg.replace(
        "[envs.dev]",
        &format!("[envs.dev]\ndata_storage_base = \"{}/data/v1\"", server.uri()),
    );
    std::fs::write(&cfg_path, cfg).unwrap();

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
        .stdout(predicate::str::contains("2 datasets"));

    let env_root = project.path().join("envs/dev");
    let mdh = env_root.join("mdh");
    assert!(mdh.join("vendors/collection.json").exists());
    assert!(mdh.join("vendors/indexes.json").exists());
    assert!(mdh.join("purchase-orders/collection.json").exists());
    assert!(mdh.join("purchase-orders/indexes.json").exists());

    let ix_raw = std::fs::read_to_string(mdh.join("vendors/indexes.json")).unwrap();
    let ix_value: serde_json::Value = serde_json::from_str(&ix_raw).unwrap();
    assert_eq!(ix_value["regular"].as_array().unwrap().len(), 2);
    assert_eq!(ix_value["search"].as_array().unwrap().len(), 1);

    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("\"mdh_collections\""));
    assert!(lf.contains("\"mdh_indexes\""));
}

#[tokio::test]
async fn pull_skips_mdh_when_data_storage_base_is_absent() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let empty_list = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list.clone()))
            .mount(&server)
            .await;
    }
    // NO data storage endpoints mocked — if MDH driver runs, the test will fail.

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

    let assert_result = Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert_result.get_output().stdout).to_string();
    assert!(!stdout.contains("dataset"), "MDH should be skipped when data_storage_base is not set: {stdout}");
    assert!(!project.path().join("envs/dev/mdh").exists(), "no mdh/ dir should be created");
}

#[tokio::test]
async fn re_pull_with_no_changes_is_idempotent() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
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
        .success();

    let lf_path = project.path().join(".rdc/state/dev.lock.json");
    let first_lf = std::fs::read_to_string(&lf_path).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("conflict").not());

    let second_lf = std::fs::read_to_string(&lf_path).unwrap();
    assert_eq!(first_lf, second_lf, "lockfile should be byte-identical after no-op re-pull");
}

#[tokio::test]
async fn re_pull_preserves_local_edits_when_remote_unchanged() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
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
        .success();

    let hook_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let original = std::fs::read_to_string(&hook_path).unwrap();
    let edited = original.replace("Validator: invoices", "Validator: invoices (LOCAL EDIT)");
    std::fs::write(&hook_path, &edited).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("conflict").not());

    let after = std::fs::read_to_string(&hook_path).unwrap();
    assert_eq!(after, edited, "local edit must be preserved on re-pull when remote unchanged");
}

#[tokio::test]
async fn re_pull_emits_remote_file_on_real_conflict() {
    // Two MockServers: server1 returns the original payload (used for the
    // first pull), server2 returns a modified payload (used for the second
    // pull). Between pulls we rewrite rdc.toml to point at server2.
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    let modified_hooks = serde_json::json!({
        "pagination": { "total": 2, "next": null, "previous": null },
        "results": [
            {
                "id": 1,
                "url": "https://mock.rossum.app/api/v1/hooks/1",
                "name": "Validator: invoices (REMOTE EDIT)",
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

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });

    // Wire both servers identically except for /hooks.
    for srv in [&server1, &server2] {
        Mock::given(method("GET"))
            .and(path("/api/v1/organizations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
            .mount(srv)
            .await;
        for ep in [
            "/api/v1/workspaces", "/api/v1/queues",
            "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
            "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
        ] {
            Mock::given(method("GET"))
                .and(path(ep))
                .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
                .mount(srv)
                .await;
        }
    }

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server1)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(modified_hooks))
        .mount(&server2)
        .await;

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server1.uri())])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    // First pull against server1.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success();

    let hook_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let original = std::fs::read_to_string(&hook_path).unwrap();
    let local_edit = original.replace("Validator: invoices", "Validator: invoices (LOCAL EDIT)");
    std::fs::write(&hook_path, &local_edit).unwrap();

    // Repoint to server2 (which returns the modified hooks).
    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let new_cfg = cfg.replace(&format!("{}/api/v1", server1.uri()), &format!("{}/api/v1", server2.uri()));
    assert_ne!(cfg, new_cfg, "rdc.toml should change after repoint");
    std::fs::write(&cfg_path, new_cfg).unwrap();

    let assert = Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let err = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    let lockfile = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let actual = std::fs::read_to_string(&hook_path).unwrap();
    assert!(
        out.contains("1 conflict"),
        "expected '1 conflict' in stdout. stdout={out}\nstderr={err}\nhook_local={actual}\nlockfile={lockfile}"
    );

    let after_local = std::fs::read_to_string(&hook_path).unwrap();
    assert_eq!(after_local, local_edit, "local must be preserved on conflict");

    let remote_path = project.path().join("envs/dev/hooks/validator-invoices.json.remote");
    assert!(remote_path.exists(), "<slug>.json.remote should be written on conflict");
    let remote_content = std::fs::read_to_string(&remote_path).unwrap();
    assert!(remote_content.contains("REMOTE EDIT"), "remote file should contain remote content");
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
