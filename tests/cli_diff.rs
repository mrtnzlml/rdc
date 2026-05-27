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

/// Pins the perf invariant established by the O(N²) → O(N) fix in
/// `diff_local_vs_remote`: every `list_*` endpoint that backs a per-slug
/// diff lookup must be called AT MOST ONCE per `rdc diff` run, regardless
/// of how many local objects exist. Previously the queue tree issued one
/// `list_queues` per local queue.json, and similar repetition existed
/// for labels / engine_fields / email_templates.
///
/// This test boots a small project with N >= 3 queues mirrored to the
/// mock server and asserts each list endpoint sees exactly one hit.
#[tokio::test]
async fn diff_lists_each_remote_endpoint_exactly_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
        .mount(&server).await;

    // Workspace tree: 1 workspace containing 3 queues. Each queue has a
    // schema and inbox so the diff hits get_schema/get_inbox in parallel
    // and uses the queue list to look up queue bodies.
    let ws_url = format!("{}/api/v1/workspaces/800", server.uri());
    let queue_ids = [100u64, 101, 102];
    let queue_urls: Vec<String> = queue_ids
        .iter()
        .map(|id| format!("{}/api/v1/queues/{id}", server.uri()))
        .collect();
    let schema_urls: Vec<String> = queue_ids
        .iter()
        .map(|id| format!("{}/api/v1/schemas/2{id:02}", server.uri()))
        .collect();
    let inbox_urls: Vec<String> = queue_ids
        .iter()
        .map(|id| format!("{}/api/v1/inboxes/3{id:02}", server.uri()))
        .collect();

    let workspaces_body = serde_json::json!({
        "pagination": { "total": 1, "next": null },
        "results": [{
            "id": 800, "url": ws_url, "name": "ap",
            "organization": format!("{}/api/v1/organizations/1", server.uri()),
            "queues": queue_urls.clone(),
            "modified_at": "2026-04-20T08:00:00Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workspaces_body))
        .mount(&server).await;

    let queues_body = serde_json::json!({
        "pagination": { "total": 3, "next": null },
        "results": queue_ids.iter().enumerate().map(|(i, id)| serde_json::json!({
            "id": id,
            "url": queue_urls[i],
            "name": format!("Q{id}"),
            "workspace": format!("{}/api/v1/workspaces/800", server.uri()),
            "schema": schema_urls[i],
            "inbox": inbox_urls[i],
            "modified_at": "2026-04-10T09:00:00Z"
        })).collect::<Vec<_>>()
    });
    // Mount once: this single mock serves both `sync` (one list call)
    // and `diff` (post-fix: one list call). Total expected = 2.
    //
    // Pre-fix, the queue-tree loop called `list_queues` ONCE PER
    // LOCAL QUEUE — with 3 queues, total would be 1 (sync) + 3 (diff) =
    // 4, failing this expectation. Post-fix it's exactly 2.
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queues_body))
        .expect(2)
        .mount(&server).await;

    for (i, id) in queue_ids.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/schemas/2{id:02}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": format!("2{id:02}").parse::<u64>().unwrap(),
                "url": schema_urls[i],
                "name": format!("S{id}"),
                "queues": [queue_urls[i]],
                "content": [],
                "modified_at": "2026-04-10T09:00:00Z"
            })))
            .mount(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/inboxes/3{id:02}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": format!("3{id:02}").parse::<u64>().unwrap(),
                "url": inbox_urls[i],
                "name": format!("I{id}"),
                "email": format!("q{id}@mock.rossum.app"),
                "queues": [queue_urls[i]],
                "filters": [],
                "modified_at": "2026-04-10T09:00:00Z"
            })))
            .mount(&server).await;
    }

    // email_templates: empty (no list call expected from queue tree if
    // no local template files exist; we still mount the endpoint for
    // pull).
    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
        .mount(&server).await;

    // Flat-kind lists — each should be called AT MOST ONCE during diff.
    // (Pull calls them once too, so total expected = pull(1) + diff(0 or
    // 1) depending on whether the local snapshot has any files of the
    // kind. Local snapshot is freshly synced from the same server, so
    // it has none of these kinds populated locally → no diff list call.)
    for ep in [
        "/api/v1/rules", "/api/v1/labels",
        "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
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
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    // diff with 3 local queues. Pre-fix: 3 list_queues calls. Post-fix:
    // 1 list_queues call. wiremock's `.expect(1)` will assert on drop
    // of the server.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));
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
        .stdout(predicate::str::contains("Update(hooks/validator-invoices.py)"))
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
        .stdout(predicate::str::contains("Update(hooks/validator-invoices.py)"))
        .stdout(predicate::str::contains("- in a   + in b"))
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

#[test]
fn diff_snapshot_vs_snapshot_raw_reveals_id_and_url() {
    // Two hooks differing ONLY in id+url. Normal diff strips them and is
    // silent; --raw must reveal both.
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

    // Sanity: normal diff is silent.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));

    // --raw reveals id + both urls.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod", "--raw"])
        .assert().success()
        .stdout(predicate::str::contains("\"id\""))
        .stdout(predicate::str::contains("hooks/42"))
        .stdout(predicate::str::contains("hooks/99"))
        .stdout(predicate::str::contains("no diffs").not());
}
