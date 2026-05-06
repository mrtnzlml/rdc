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
        .stdout(predicate::str::contains("1 inboxes"))
        .stdout(predicate::str::contains("2 hooks"));

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
