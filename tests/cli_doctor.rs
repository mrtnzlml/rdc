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
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/inboxes",
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

/// Clean env: doctor runs every (offline) step and finds nothing to do.
#[tokio::test]
async fn doctor_clean_env_runs_offline_steps() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["doctor", "dev"])
        .assert().success()
        .stderr(predicate::str::contains("no unpushed local changes"))
        .stderr(predicate::str::contains("base cache: no orphans"))
        .stderr(predicate::str::contains("doctor finished for env 'dev'"));
}

/// `--dry-run` previews the offline steps without writing (base-cache prune is
/// reported, not applied).
#[tokio::test]
async fn doctor_dry_run_previews_without_writing() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["doctor", "dev", "--dry-run"])
        .assert().success()
        .stderr(predicate::str::contains("would prune orphan base cache entries"))
        .stderr(predicate::str::contains("doctor finished for env 'dev'"));
}

/// Pre-flight: doctor warns up front when the local snapshot has edits not
/// yet pushed to the remote (offline scan), so the user knows what's on disk
/// but not yet on the server.
#[tokio::test]
async fn doctor_warns_about_unpushed_local_changes() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;
    // One label so there's a tracked object to edit locally.
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 7, "url": format!("{}/api/v1/labels/7", server.uri()),
                "name": "Priority", "color": "#ff0000",
                "organization": format!("{}/api/v1/organizations/1", server.uri())
            }]
        })))
        .with_priority(1)
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())]).assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Edit the local label so it diverges from the lockfile base (color only,
    // so the slug still matches the name and the realign step stays a no-op).
    let label_path = project.path().join("envs/dev/labels/priority.json");
    let mut label: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&label_path).unwrap()).unwrap();
    label["color"] = serde_json::json!("#00ff00");
    std::fs::write(&label_path, format!("{}\n", serde_json::to_string_pretty(&label).unwrap())).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["doctor", "dev"])
        .assert().success()
        .stderr(predicate::str::contains("1 local change"))
        .stderr(predicate::str::contains("not yet pushed"));
}

/// `rdc doctor` is the offline pre-flight, so it must catch a field that
/// exceeds the API's `max_length` with no network calls at all — before
/// the user ever starts a sync and waits out the remote listing.
#[tokio::test]
async fn doctor_reports_field_exceeding_api_length_limit() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 9, "url": format!("{}/api/v1/hooks/9", server.uri()),
                "name": "example-hook", "type": "webhook", "queues": [],
                "events": ["annotation_content"],
                "config": { "url": "https://hook.example.com/run" },
                "description": "short"
            }]
        })))
        .with_priority(1)
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())]).assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Grow `description` past the API's 2000-character cap.
    let hook_path = project.path().join("envs/dev/hooks/example-hook.json");
    let mut hook: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hook_path).unwrap()).unwrap();
    hook["description"] = serde_json::json!("x".repeat(2406));
    std::fs::write(&hook_path, format!("{}\n", serde_json::to_string_pretty(&hook).unwrap())).unwrap();

    // Point the env at a dead port: proves the check is genuinely offline.
    let rdc_toml = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&rdc_toml).unwrap()
        .replace(&server.uri(), "http://127.0.0.1:1");
    std::fs::write(&rdc_toml, cfg).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["doctor", "dev"])
        .assert().success()
        .stderr(predicate::str::contains("hooks/example-hook"))
        .stderr(predicate::str::contains("description"))
        .stderr(predicate::str::contains("2406"))
        .stderr(predicate::str::contains("2000"));
}
