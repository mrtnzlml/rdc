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

async fn mount_minimal_pull(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hook_1.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 2,
            "url": "https://mock.rossum.app/api/v1/hooks/2",
            "name": "SFTP import",
            "type": "function",
            "queues": [],
            "events": ["annotation_status"],
            "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
        })))
        .mount(server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels",
        "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server).await;
    }
}

#[tokio::test]
async fn diff_local_remote_no_changes() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));
}

#[tokio::test]
async fn diff_local_remote_shows_edit_in_unified_format() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    // Edit hook code locally.
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# DIFF MARKER LINE\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("--- hooks/validator-invoices.py (local)"))
        .stdout(predicate::str::contains("+++ hooks/validator-invoices.py (remote)"))
        .stdout(predicate::str::contains("DIFF MARKER LINE"));
}

#[tokio::test]
async fn diff_snapshot_vs_snapshot_no_api_calls() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    mount_minimal_pull(&server_a).await;
    mount_minimal_pull(&server_b).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init", "--env", &format!("a={}/api/v1:1", server_a.uri()),
            "--env", &format!("b={}/api/v1:1", server_b.uri()),
        ])
        .assert().success();
    std::fs::write(project.path().join("secrets/a.secrets.json"), r#"{"api_token":"X"}"#).unwrap();
    std::fs::write(project.path().join("secrets/b.secrets.json"), r#"{"api_token":"X"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path()).args(["sync", "a", "--no-push"]).assert().success();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path()).args(["sync", "b", "--no-push"]).assert().success();

    // Identical snapshots → no diffs.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "a", "b"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));

    // Diverge: edit a's hook .py.
    let py_path = project.path().join("envs/a/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# A-only edit\n")).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "a", "b"])
        .assert().success()
        .stdout(predicate::str::contains("hooks/validator-invoices.py (in a)"))
        .stdout(predicate::str::contains("hooks/validator-invoices.py (in b)"))
        .stdout(predicate::str::contains("A-only edit"));
}

// ─── Mapping-aware cross-env canonicalisation ──────────────────────────
//
// Helpers below set up a minimal project directly on disk (no API mocks)
// to assert the mapping-aware diff invariants in isolation.

/// Write a minimal `rdc.toml` declaring two envs `test` and `prod` —
/// the URLs are placeholders; the diff path never hits the network.
fn write_two_env_project(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("envs/test/hooks")).unwrap();
    std::fs::create_dir_all(root.join("envs/prod/hooks")).unwrap();
    std::fs::create_dir_all(root.join(".rdc/state")).unwrap();
    std::fs::write(
        root.join("rdc.toml"),
        r#"[envs.test]
api_base = "https://test.rossum.app/api/v1"
org_id = 1

[envs.prod]
api_base = "https://prod.rossum.app/api/v1"
org_id = 2
"#,
    ).unwrap();
}

fn write_lockfile(root: &std::path::Path, env: &str, entries: &[(&str, &str, u64, &str)]) {
    let mut objects: std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>> =
        std::collections::BTreeMap::new();
    for (kind, slug, id, url) in entries {
        objects.entry(kind.to_string()).or_default().insert(
            slug.to_string(),
            serde_json::json!({
                "id": id,
                "url": url,
                "modified_at": null,
                "content_hash": null,
            }),
        );
    }
    let body = serde_json::json!({ "version": 2, "objects": objects });
    std::fs::write(
        root.join(format!(".rdc/state/{env}.lock.json")),
        serde_json::to_string_pretty(&body).unwrap() + "\n",
    ).unwrap();
}

fn write_mapping(root: &std::path::Path, src: &str, tgt: &str, body: &str) {
    std::fs::create_dir_all(root.join(".rdc/map")).unwrap();
    std::fs::write(root.join(format!(".rdc/map/{src}-to-{tgt}.toml")), body).unwrap();
}

#[test]
fn diff_snapshot_vs_snapshot_strips_url_id_noise() {
    // Two envs, identical hook except for env-specific id + url. With or
    // without the mapping, the diff should be silent — the noise strip
    // alone handles top-level id/url.
    let project = TempDir::new().unwrap();
    write_two_env_project(project.path());

    let hook_test = serde_json::json!({
        "id": 42,
        "url": "https://test.rossum.app/api/v1/hooks/42",
        "name": "validator-invoices",
        "type": "function",
        "events": ["annotation_status"],
        "queues": [],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    let hook_prod = serde_json::json!({
        "id": 99,
        "url": "https://prod.rossum.app/api/v1/hooks/99",
        "name": "validator-invoices",
        "type": "function",
        "events": ["annotation_status"],
        "queues": [],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    std::fs::write(
        project.path().join("envs/test/hooks/validator-invoices.json"),
        serde_json::to_string_pretty(&hook_test).unwrap(),
    ).unwrap();
    std::fs::write(
        project.path().join("envs/prod/hooks/validator-invoices.json"),
        serde_json::to_string_pretty(&hook_prod).unwrap(),
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));
}

#[test]
fn diff_snapshot_vs_snapshot_with_mapping_canonicalizes_xref_urls() {
    // Hook references a queue. Different IDs across envs; mapping pairs
    // them. With mapping → diff is empty.
    let project = TempDir::new().unwrap();
    write_two_env_project(project.path());

    write_lockfile(project.path(), "test", &[
        ("queues", "cost-invoices", 600, "https://test.rossum.app/api/v1/queues/600"),
        ("hooks", "validator", 42, "https://test.rossum.app/api/v1/hooks/42"),
    ]);
    write_lockfile(project.path(), "prod", &[
        ("queues", "cost-invoices", 715, "https://prod.rossum.app/api/v1/queues/715"),
        ("hooks", "validator", 99, "https://prod.rossum.app/api/v1/hooks/99"),
    ]);
    write_mapping(project.path(), "test", "prod", r#"version = 1

[hooks]
validator = "validator"

[queues]
cost-invoices = "cost-invoices"
"#);

    let test_hook = serde_json::json!({
        "id": 42,
        "url": "https://test.rossum.app/api/v1/hooks/42",
        "name": "validator",
        "type": "function",
        "events": ["annotation_status"],
        "queues": ["https://test.rossum.app/api/v1/queues/600"],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    let prod_hook = serde_json::json!({
        "id": 99,
        "url": "https://prod.rossum.app/api/v1/hooks/99",
        "name": "validator",
        "type": "function",
        "events": ["annotation_status"],
        "queues": ["https://prod.rossum.app/api/v1/queues/715"],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    std::fs::write(
        project.path().join("envs/test/hooks/validator.json"),
        serde_json::to_string_pretty(&test_hook).unwrap(),
    ).unwrap();
    std::fs::write(
        project.path().join("envs/prod/hooks/validator.json"),
        serde_json::to_string_pretty(&prod_hook).unwrap(),
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));
}

#[test]
fn diff_snapshot_vs_snapshot_without_mapping_strips_top_level_noise_only() {
    // Same setup as the mapping test but *no* mapping file. Top-level
    // id/url stripped; cross-reference queue URL still shows as
    // different.
    let project = TempDir::new().unwrap();
    write_two_env_project(project.path());

    let test_hook = serde_json::json!({
        "id": 42,
        "url": "https://test.rossum.app/api/v1/hooks/42",
        "name": "validator",
        "type": "function",
        "events": ["annotation_status"],
        "queues": ["https://test.rossum.app/api/v1/queues/600"],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    let prod_hook = serde_json::json!({
        "id": 99,
        "url": "https://prod.rossum.app/api/v1/hooks/99",
        "name": "validator",
        "type": "function",
        "events": ["annotation_status"],
        "queues": ["https://prod.rossum.app/api/v1/queues/715"],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    std::fs::write(
        project.path().join("envs/test/hooks/validator.json"),
        serde_json::to_string_pretty(&test_hook).unwrap(),
    ).unwrap();
    std::fs::write(
        project.path().join("envs/prod/hooks/validator.json"),
        serde_json::to_string_pretty(&prod_hook).unwrap(),
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod"])
        .assert().success()
        // Must mention the cross-reference queue URL diff but NOT the
        // top-level id/url noise (those are stripped).
        .stdout(predicate::str::contains("queues/600"))
        .stdout(predicate::str::contains("queues/715"))
        .stdout(predicate::str::contains("hooks/validator.json"))
        .stdout(predicate::str::contains("\"id\"").not())
        .stdout(predicate::str::contains("hooks/42").not())
        .stdout(predicate::str::contains("hooks/99").not());
}

#[test]
fn diff_snapshot_vs_snapshot_with_mapping_shows_real_changes() {
    // Mapping present, queues paired — but `events` differs. The diff
    // should highlight the `events` change without any URL noise.
    let project = TempDir::new().unwrap();
    write_two_env_project(project.path());

    write_lockfile(project.path(), "test", &[
        ("queues", "cost-invoices", 600, "https://test.rossum.app/api/v1/queues/600"),
        ("hooks", "validator", 42, "https://test.rossum.app/api/v1/hooks/42"),
    ]);
    write_lockfile(project.path(), "prod", &[
        ("queues", "cost-invoices", 715, "https://prod.rossum.app/api/v1/queues/715"),
        ("hooks", "validator", 99, "https://prod.rossum.app/api/v1/hooks/99"),
    ]);
    write_mapping(project.path(), "test", "prod", r#"version = 1

[hooks]
validator = "validator"

[queues]
cost-invoices = "cost-invoices"
"#);

    let test_hook = serde_json::json!({
        "id": 42,
        "url": "https://test.rossum.app/api/v1/hooks/42",
        "name": "validator",
        "type": "function",
        "events": ["annotation_status", "annotation_content"],
        "queues": ["https://test.rossum.app/api/v1/queues/600"],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    let prod_hook = serde_json::json!({
        "id": 99,
        "url": "https://prod.rossum.app/api/v1/hooks/99",
        "name": "validator",
        "type": "function",
        "events": ["annotation_status"],
        "queues": ["https://prod.rossum.app/api/v1/queues/715"],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    std::fs::write(
        project.path().join("envs/test/hooks/validator.json"),
        serde_json::to_string_pretty(&test_hook).unwrap(),
    ).unwrap();
    std::fs::write(
        project.path().join("envs/prod/hooks/validator.json"),
        serde_json::to_string_pretty(&prod_hook).unwrap(),
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod"])
        .assert().success()
        // The semantic change shows: annotation_content present on one side.
        .stdout(predicate::str::contains("annotation_content"))
        // ... but URL noise stays silent.
        .stdout(predicate::str::contains("queues/600").not())
        .stdout(predicate::str::contains("queues/715").not())
        .stdout(predicate::str::contains("hooks/42").not())
        .stdout(predicate::str::contains("hooks/99").not());
}
