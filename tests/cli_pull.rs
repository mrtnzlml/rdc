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
    // content_hash is populated for every entry (used by the three-way
    // merge as the base hash on subsequent pulls/pushes).
    assert!(lf.contains("\"content_hash\""), "lockfile should record content_hash for entries");
    // Hashes are 64-char hex (SHA-256). Spot-check by counting at least one full hash.
    let hash_re = regex::Regex::new(r#""content_hash":\s*"[0-9a-f]{64}""#).unwrap();
    assert!(hash_re.is_match(&lf), "expected at least one 64-char hex content_hash in lockfile");

    // _index.md generated.
    let index_path = env_root.join("_index.md");
    assert!(index_path.exists(), "_index.md should be generated");
    let index_body = std::fs::read_to_string(&index_path).unwrap();
    assert!(index_body.starts_with("<!-- generated by rdc; do not edit -->"));
    assert!(index_body.contains("# dev"));
    assert!(index_body.contains("- workspaces: 2"));
    assert!(index_body.contains("- queues: 3"));
    assert!(index_body.contains("- hooks: 2"));
    assert!(index_body.contains("`validator-invoices`"));

    // Org-level kinds.
    assert!(env_root.join("rules/e-invoice-validation.json").exists());
    assert!(env_root.join("labels/priority-high.json").exists());
    assert!(env_root.join("labels/needs-review.json").exists());
    // Engines own a directory (engine.json + fields/). Engine fields
    // nest under their parent engine to mirror API 1:N ownership.
    assert!(env_root.join("engines/invoice-engine/engine.json").exists());
    assert!(env_root.join("engines/invoice-engine/fields/invoice-id.json").exists());
    assert!(env_root.join("engines/invoice-engine/fields/total-amount.json").exists());

    // Lockfile records the kinds.
    assert!(lf.contains("\"rules\""));
    assert!(lf.contains("\"labels\""));
    assert!(lf.contains("\"engines\""));
    assert!(lf.contains("\"engine_fields\""));

    // Workflow kinds (same nested pattern as engines).
    assert!(env_root.join("workflows/ap-approval-flow/workflow.json").exists());
    assert!(env_root.join("workflows/ap-approval-flow/steps/manager-approval.json").exists());
    assert!(env_root.join("workflows/ap-approval-flow/steps/finance-approval.json").exists());
    // Email templates nest under their queue (the live API associates
    // each template with exactly one queue).
    assert!(env_root
        .join("workspaces/invoices-ap/queues/cost-invoices/email-templates/rejection-notice.json")
        .exists());

    assert!(lf.contains("\"workflows\""));
    assert!(lf.contains("\"workflow_steps\""));
    assert!(lf.contains("\"email_templates\""));
}

#[tokio::test]
async fn pull_mdh_when_endpoints_present() {
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

    use wiremock::matchers::body_partial_json;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/collections/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_collections.json")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/indexes/list"))
        .and(body_partial_json(serde_json::json!({"collectionName": "vendors"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_indexes_vendors.json")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/search_indexes/list"))
        .and(body_partial_json(serde_json::json!({"collectionName": "vendors"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_search_indexes_vendors.json")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/indexes/list"))
        .and(body_partial_json(serde_json::json!({"collectionName": "purchase_orders"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_indexes_purchase_orders.json")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/search_indexes/list"))
        .and(body_partial_json(serde_json::json!({"collectionName": "purchase_orders"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("mdh_search_indexes_purchase_orders.json")))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            &format!("dev={}/api/v1:1", server.uri()),
        ])
        .assert()
        .success();

    // No data_storage_base in rdc.toml — the URL is derived from
    // api_base via EnvConfig::data_storage_base(). Both pull helpers
    // (Rossum API and Data Storage API) hit the same mock server's URL
    // space.

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
async fn pull_skips_mdh_when_endpoint_returns_404() {
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
    // NO data storage endpoints mocked — wiremock returns 404 for unknown
    // paths and the MDH driver tolerates that, so pull still succeeds
    // and `mdh/` is never created. Mirrors the real-world case of a
    // Rossum cluster that doesn't have MDH enabled.

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
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
    assert!(!stdout.contains("dataset"), "MDH should be silently skipped on 404: {stdout}");
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
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
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
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
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
        .args(["init", "--env", &format!("dev={}/api/v1:1", server1.uri())])
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
async fn re_pull_emits_remote_file_on_queue_conflict() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    let modified_queues = serde_json::json!({
        "pagination": { "total": 3, "next": null, "previous": null },
        "results": [
            {
                "id": 100,
                "url": "https://mock.rossum.app/api/v1/queues/100",
                "name": "Cost Invoices (REMOTE EDIT)",
                "workspace": "https://mock.rossum.app/api/v1/workspaces/700852",
                "schema": "https://mock.rossum.app/api/v1/schemas/200",
                "inbox": "https://mock.rossum.app/api/v1/inboxes/300",
                "modified_at": "2026-04-10T09:00:00Z"
            },
            {
                "id": 101,
                "url": "https://mock.rossum.app/api/v1/queues/101",
                "name": "Credit Notes",
                "workspace": "https://mock.rossum.app/api/v1/workspaces/700852",
                "schema": "https://mock.rossum.app/api/v1/schemas/201",
                "modified_at": "2026-04-10T09:30:00Z"
            },
            {
                "id": 102,
                "url": "https://mock.rossum.app/api/v1/queues/102",
                "name": "Purchase Orders",
                "workspace": "https://mock.rossum.app/api/v1/workspaces/743213",
                "schema": "https://mock.rossum.app/api/v1/schemas/202",
                "modified_at": "2026-04-10T10:00:00Z"
            }
        ]
    });

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });

    for srv in [&server1, &server2] {
        Mock::given(method("GET"))
            .and(path("/api/v1/organizations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/201"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/202"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/inboxes/300"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
            .mount(srv).await;
        for ep in [
            "/api/v1/hooks", "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
            "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
        ] {
            Mock::given(method("GET"))
                .and(path(ep))
                .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
                .mount(srv).await;
        }
    }

    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server1).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(modified_queues))
        .mount(&server2).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server1.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "dev"]).assert().success();

    let queue_path = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/queue.json");
    let original = std::fs::read_to_string(&queue_path).unwrap();
    let local_edit = original.replace("Cost Invoices", "Cost Invoices (LOCAL EDIT)");
    std::fs::write(&queue_path, &local_edit).unwrap();

    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let new_cfg = cfg.replace(&format!("{}/api/v1", server1.uri()), &format!("{}/api/v1", server2.uri()));
    std::fs::write(&cfg_path, new_cfg).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("1 conflict"));

    let after_local = std::fs::read_to_string(&queue_path).unwrap();
    assert_eq!(after_local, local_edit);

    let remote_path = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/queue.json.remote");
    assert!(remote_path.exists());
    let remote_content = std::fs::read_to_string(&remote_path).unwrap();
    assert!(remote_content.contains("REMOTE EDIT"));
}

#[tokio::test]
async fn re_pull_preserves_local_formula_edit_when_remote_unchanged() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/202"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
        .mount(&server).await;
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server).await;
    }

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "dev"]).assert().success();

    let formula_path = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/formulas/amount_total.py");
    let original = std::fs::read_to_string(&formula_path).unwrap();
    let edited = format!("{original} + 0  # local tweak");
    std::fs::write(&formula_path, &edited).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("conflict").not());

    let after = std::fs::read_to_string(&formula_path).unwrap();
    assert_eq!(after, edited, "local formula edit must be preserved");
}

#[tokio::test]
async fn re_pull_emits_remote_files_on_formula_conflict() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    let modified_schema = serde_json::json!({
        "id": 200,
        "url": "https://mock.rossum.app/api/v1/schemas/200",
        "name": "Cost Invoices Schema",
        "queues": ["https://mock.rossum.app/api/v1/queues/100"],
        "content": [
            {
                "category": "section",
                "id": "header",
                "label": "Header",
                "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" },
                    {
                        "category": "datapoint",
                        "id": "amount_total",
                        "type": "number",
                        "formula": "amount_due + amount_tax + REMOTE_FORMULA_EDIT"
                    }
                ]
            }
        ],
        "modified_at": "2026-04-10T09:00:00Z"
    });

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });

    for srv in [&server1, &server2] {
        Mock::given(method("GET"))
            .and(path("/api/v1/organizations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/queues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/201"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/202"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/inboxes/300"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
            .mount(srv).await;
        for ep in [
            "/api/v1/hooks", "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
            "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
        ] {
            Mock::given(method("GET"))
                .and(path(ep))
                .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
                .mount(srv).await;
        }
    }

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server1).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(modified_schema))
        .mount(&server2).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server1.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "dev"]).assert().success();

    let formula_path = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/formulas/amount_total.py");
    let local_edit = "LOCAL_FORMULA_EDIT".to_string();
    std::fs::write(&formula_path, &local_edit).unwrap();

    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let new_cfg = cfg.replace(&format!("{}/api/v1", server1.uri()), &format!("{}/api/v1", server2.uri()));
    std::fs::write(&cfg_path, new_cfg).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("1 conflict"));

    let after = std::fs::read_to_string(&formula_path).unwrap();
    assert_eq!(after, local_edit);

    let queue_dir = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices");
    assert!(queue_dir.join("schema.json.remote").exists());
    assert!(queue_dir.join("formulas.remote/amount_total.py").exists());
    let remote_formula = std::fs::read_to_string(queue_dir.join("formulas.remote/amount_total.py")).unwrap();
    assert!(remote_formula.contains("REMOTE_FORMULA_EDIT"));
}

#[tokio::test]
async fn pull_with_missing_token_fails_with_helpful_error() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
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

/// Pull strips overlay-managed paths from the snapshot (spec §9.3).
/// The user configures `overlay.toml` with `name = "Validator (PROD)"`
/// for a hook; after pulling, the on-disk JSON should NOT contain the
/// `name` field.
#[tokio::test]
async fn pull_strips_overlay_paths_from_snapshot() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server).await;
    let empty_list = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list.clone()))
            .mount(&server).await;
    }

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    // Write overlay BEFORE first pull so it's applied during the pull.
    let overlay_path = project.path().join("envs/dev/overlay.toml");
    std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
    std::fs::write(&overlay_path, r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"
"#).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    let json_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let raw = std::fs::read_to_string(&json_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(v.get("name").is_none(), "name was overlay-managed and should be stripped: {raw}");
    assert!(
        v.get("config").and_then(|c| c.get("runtime")).is_none(),
        "config.runtime was overlay-managed and should be stripped: {raw}"
    );
    // Other fields untouched.
    assert_eq!(v["id"], serde_json::json!(1));
    assert_eq!(v["url"], serde_json::json!("https://mock.rossum.app/api/v1/hooks/1"));
}

#[tokio::test]
async fn pull_no_conflict_when_only_modified_at_differs() {
    // Proves the noise-field suppression stack (Tasks 1-5): a re-pull where
    // only `modified_at` changed must not emit a conflict and must leave the
    // on-disk file byte-identical to the first pull (NoChange path).

    let server = MockServer::start().await;

    // Organization endpoint required for bootstrap.
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // All non-label endpoints return empty results for both pulls.
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    // First pull: label with initial modified_at.
    let initial = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 1,
            "url": format!("{}/api/v1/labels/1", server.uri()),
            "name": "audit-hold",
            "organization": format!("{}/api/v1/organizations/1", server.uri()),
            "modified_at": "2026-01-01T00:00:00Z"
        }]
    });
    let _g = Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&initial))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
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

    // Confirm first pull wrote the label file.
    let label_path = project.path().join("envs/dev/labels/audit-hold.json");
    assert!(label_path.exists(), "label file should exist after first pull");
    let first_pull_content = std::fs::read_to_string(&label_path).unwrap();
    assert!(
        first_pull_content.contains("2026-01-01T00:00:00Z"),
        "first pull content should have initial modified_at"
    );

    // Drop scoped mock — now the second pull sees a bumped modified_at.
    drop(_g);

    let bumped = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 1,
            "url": format!("{}/api/v1/labels/1", server.uri()),
            "name": "audit-hold",
            "organization": format!("{}/api/v1/organizations/1", server.uri()),
            "modified_at": "2026-12-31T23:59:59Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&bumped))
        .mount(&server)
        .await;

    let out = Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "second pull should succeed. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stderr.contains("conflict") && !stdout.contains("conflict"),
        "expected no conflict on modified_at-only re-pull. stdout={stdout}\nstderr={stderr}"
    );

    // Disk-stability invariant: the local file retains first-pull bytes
    // (NoChange path skips the write).
    let on_disk = std::fs::read_to_string(&label_path).unwrap();
    assert!(
        on_disk.contains("2026-01-01T00:00:00Z"),
        "on-disk file should retain first-pull modified_at (NoChange path). found: {on_disk}"
    );
}

/// Round-trip — after overlay strip on pull, push re-applies the
/// overlay so the PATCH body has the env-specific value. Verifies that
/// `read_hook_value` + apply-overlay-then-deserialize handles a file
/// missing the typed `name` field.
#[tokio::test]
async fn push_succeeds_after_overlay_strip_on_pull() {
    use std::sync::{Arc, Mutex};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server).await;
    let empty_list = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list.clone()))
            .mount(&server).await;
    }

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
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    let overlay_path = project.path().join("envs/dev/overlay.toml");
    std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
    std::fs::write(&overlay_path, r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"#).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    // Edit the .py file to trigger a push (combined hash changes).
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# overlay-strip round-trip\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success();

    let body = captured.lock().unwrap().clone()
        .expect("PATCH should have been called once we triggered a real edit");
    assert_eq!(
        body["name"],
        serde_json::Value::String("Validator (PROD)".into()),
        "overlay re-applies name on push: {body}",
    );
}

#[tokio::test]
async fn pull_with_orphan_queue_surfaces_count_in_done_line() {
    let server = MockServer::start().await;

    // Organization endpoint required for bootstrap.
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // All non-queue endpoints return empty results.
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/workspaces",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    // One queue with workspace: null (orphan — no parent workspace known).
    let queues_resp = serde_json::json!({
        "pagination": { "next": null },
        "results": [{
            "id": 7,
            "url": format!("{}/api/v1/queues/7", server.uri()),
            "name": "orphan-q",
            "workspace": null,
            "schema": null
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&queues_resp))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let out = Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "pull should succeed. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With the single-bar design, orphan counts accumulate globally and
    // appear in the final pull summary line (stdout) rather than per-phase
    // stderr lines.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("orphans skipped"),
        "expected orphans-skipped count in pull summary. stdout was: {stdout}"
    );
}
