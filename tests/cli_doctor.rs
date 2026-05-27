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
        "/api/v1/workspaces", "/api/v1/queues",
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

/// Clean env: doctor runs every step, finds nothing to do, and (non-TTY,
/// no flag) skips the destructive rebuild rather than running it.
#[tokio::test]
async fn doctor_clean_env_runs_steps_and_skips_rebuild_lock() {
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
        .stderr(predicate::str::contains("no anomalous"))
        .stderr(predicate::str::contains("rebuild-lock skipped"))
        .stderr(predicate::str::contains("doctor finished for env 'dev'"));
}

/// Pre-flight: doctor warns up front when the local snapshot has edits not
/// yet pushed to the remote (offline scan), so the user knows what the
/// destructive rebuild would discard.
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

/// `--rebuild-lock` authorizes the destructive rebuild directly (no confirm),
/// which backs up the lockfile and re-pulls.
#[tokio::test]
async fn doctor_rebuild_lock_flag_backs_up_and_repulls() {
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

    let lockfile_path = project.path().join(".rdc/state/dev.lock.json");
    assert!(lockfile_path.exists(), "lockfile created by initial sync");

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["doctor", "dev", "--rebuild-lock"])
        .assert().success()
        .stderr(predicate::str::contains("done env 'dev' rebuilt"))
        .stderr(predicate::str::contains("backed up lockfile to"));

    assert!(lockfile_path.exists(), "lockfile re-created by sync");

    let state_dir = project.path().join(".rdc/state");
    let backups: Vec<_> = std::fs::read_dir(&state_dir).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
        .collect();
    assert_eq!(backups.len(), 1, "exactly one backup file");
}

/// `--rebuild-lock` with no lockfile yet: the lockfile-dependent checks are
/// skipped and the rebuild proceeds from a fresh pull.
#[tokio::test]
async fn doctor_rebuild_lock_works_when_lockfile_missing() {
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

    // No lockfile yet (no prior sync).
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["doctor", "dev", "--rebuild-lock"])
        .assert().success()
        .stderr(predicate::str::contains("no lockfile yet"))
        .stderr(predicate::str::contains("done env 'dev' rebuilt"))
        .stderr(predicate::str::contains("no existing lockfile"));

    assert!(project.path().join(".rdc/state/dev.lock.json").exists());
}

/// `--check` lists anomalous store-extension hooks without writing or
/// touching the remote.
#[tokio::test]
async fn doctor_check_lists_anomalous_hooks() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    // Override /hooks with two hooks: one anomalous, one clean.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [
                {
                    "id": 42, "url": format!("{}/api/v1/hooks/42", server.uri()),
                    "name": "Broken Store Hook", "type": "webhook",
                    "queues": [], "events": [], "config": {},
                    "extension_source": "rossum_store", "hook_template": null
                },
                {
                    "id": 43, "url": format!("{}/api/v1/hooks/43", server.uri()),
                    "name": "Healthy Hook", "type": "function",
                    "queues": [], "events": [], "config": {},
                    "extension_source": "custom", "hook_template": null
                }
            ]
        })))
        .with_priority(1)
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
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["doctor", "dev", "--check"])
        .assert().success()
        .stderr(predicate::str::contains("broken-store-hook"))
        .stderr(predicate::str::contains("id 42"))
        .stderr(predicate::str::contains("1 anomalous hook"));
}

/// Store-anomaly cure B (default, non-interactive): PATCH `extension_source`
/// to `custom`. Reached through `doctor` automatically under `--yes`.
#[tokio::test]
async fn doctor_converts_store_anomaly_to_custom() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 42, "url": format!("{}/api/v1/hooks/42", server.uri()),
                "name": "Broken", "type": "webhook",
                "queues": [], "events": [], "config": {},
                "extension_source": "rossum_store", "hook_template": null
            }]
        })))
        .with_priority(1)
        .mount(&server).await;

    let patched = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
    let patched_clone = patched.clone();
    let server_uri = server.uri();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/42"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap();
            *patched_clone.lock().unwrap() = body.clone();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42, "url": format!("{}/api/v1/hooks/42", server_uri),
                "name": "Broken", "type": "webhook",
                "queues": [], "events": [], "config": {},
                "extension_source": "custom", "hook_template": null,
                "modified_at": "2026-05-22T12:00:00.000000Z"
            }))
        })
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Non-interactive (`--yes`): default cure is convert-to-custom; the
    // destructive rebuild is skipped (no --rebuild-lock).
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["--yes", "doctor", "dev"])
        .assert().success()
        .stderr(predicate::str::contains("hooks/broken (id 42) \u{2192} converted to custom"));

    let body = patched.lock().unwrap().clone();
    assert_eq!(body, serde_json::json!({"extension_source": "custom"}));

    let local: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join("envs/dev/hooks/broken.json")).unwrap()
    ).unwrap();
    assert_eq!(local["extension_source"], "custom");
}

/// Store-anomaly cure A (forced via `RDC_DOCTOR_CURE=reinstall`): reinstall
/// as a store extension and rewire dependents. Reached through `doctor`.
#[tokio::test]
async fn doctor_reinstalls_store_anomaly_and_rewires_dependents() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let anomalous_url = format!("{}/api/v1/hooks/42", server.uri());
    let dependent_url = format!("{}/api/v1/hooks/100", server.uri());
    let new_hook_url = format!("{}/api/v1/hooks/999", server.uri());
    let template_url = format!("{}/api/v1/hook_templates/39", server.uri());

    Mock::given(method("GET")).and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [
                {
                    "id": 42, "url": anomalous_url, "name": "Master Data Hub",
                    "type": "webhook", "queues": [], "events": ["annotation_content.initialize"],
                    "config": {"private": true},
                    "extension_source": "rossum_store", "hook_template": null
                },
                {
                    "id": 100, "url": dependent_url, "name": "Downstream",
                    "type": "function", "queues": [],
                    "events": ["annotation_content.initialize"],
                    "config": {"runtime": "python3.12", "code": "def f(p): return {}"},
                    "extension_source": "custom", "hook_template": null,
                    "run_after": [anomalous_url.clone()]
                }
            ]
        }))).with_priority(1).mount(&server).await;

    let template_url_for_listing = template_url.clone();
    Mock::given(method("GET")).and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "url": template_url_for_listing,
                "name": "Master Data Hub", "type": "webhook",
                "extension_source": "rossum_store", "install_action": "copy"
            }]
        }))).with_priority(1).mount(&server).await;

    let install_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let install_clone = install_calls.clone();
    let new_url_inst = new_hook_url.clone();
    let tmpl_url_inst = template_url.clone();
    Mock::given(method("POST")).and(path("/api/v1/hooks/create"))
        .respond_with(move |req: &wiremock::Request| {
            install_clone.lock().unwrap().push(req.body_json().unwrap());
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 999, "url": new_url_inst,
                "name": "Master Data Hub", "type": "webhook",
                "queues": [], "events": ["annotation_content.initialize"],
                "config": {"private": true},
                "extension_source": "rossum_store",
                "hook_template": tmpl_url_inst
            }))
        }).mount(&server).await;

    let new_url_p = new_hook_url.clone();
    let tmpl_url_p = template_url.clone();
    Mock::given(method("PATCH")).and(path("/api/v1/hooks/999"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 999, "url": new_url_p,
                "name": "Master Data Hub", "type": "webhook",
                "queues": [], "events": ["annotation_content.initialize"],
                "config": {"private": true},
                "extension_source": "rossum_store",
                "hook_template": tmpl_url_p
            }))
        }).mount(&server).await;

    let dep_patches = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let dep_clone = dep_patches.clone();
    let new_url_d = new_hook_url.clone();
    let dep_url_d = dependent_url.clone();
    Mock::given(method("PATCH")).and(path("/api/v1/hooks/100"))
        .respond_with(move |req: &wiremock::Request| {
            dep_clone.lock().unwrap().push(req.body_json().unwrap());
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 100, "url": dep_url_d,
                "name": "Downstream", "type": "function", "queues": [],
                "events": ["annotation_content.initialize"],
                "config": {"runtime": "python3.12", "code": "def f(p): return {}"},
                "extension_source": "custom", "hook_template": null,
                "run_after": [new_url_d]
            }))
        }).mount(&server).await;

    let new_url_g100 = new_hook_url.clone();
    let dep_url_g100 = dependent_url.clone();
    Mock::given(method("GET")).and(path("/api/v1/hooks/100"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 100, "url": dep_url_g100,
                "name": "Downstream", "type": "function", "queues": [],
                "events": ["annotation_content.initialize"],
                "config": {"runtime": "python3.12", "code": "def f(p): return {}"},
                "extension_source": "custom", "hook_template": null,
                "run_after": [new_url_g100]
            }))
        }).mount(&server).await;

    let new_url_g999 = new_hook_url.clone();
    let tmpl_url_g999 = template_url.clone();
    Mock::given(method("GET")).and(path("/api/v1/hooks/999"))
        .respond_with(move |_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 999, "url": new_url_g999,
                "name": "Master Data Hub", "type": "webhook",
                "queues": [], "events": ["annotation_content.initialize"],
                "config": {"private": true},
                "extension_source": "rossum_store",
                "hook_template": tmpl_url_g999
            }))
        }).mount(&server).await;

    Mock::given(method("DELETE")).and(path("/api/v1/hooks/42"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Force Cure A via env var; doctor reaches store-anomaly automatically.
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .env("RDC_DOCTOR_CURE", "reinstall")
        .args(["--yes", "doctor", "dev"])
        .assert().success()
        .stderr(predicate::str::contains("hooks/master-data-hub").and(
            predicate::str::contains("reinstalled (new id 999)")));

    let installs = install_calls.lock().unwrap();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["hook_template"], serde_json::json!(template_url));

    let deps = dep_patches.lock().unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0], serde_json::json!({"run_after": [new_hook_url]}));

    let local: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join("envs/dev/hooks/master-data-hub.json")).unwrap()
    ).unwrap();
    assert_eq!(local["extension_source"], "rossum_store");
    assert!(local["hook_template"].as_str().unwrap().contains("/hook_templates/39"));
}
