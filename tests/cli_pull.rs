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
