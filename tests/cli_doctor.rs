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
        "/api/v1/saved_views",
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

/// The chain the guard exists for, end to end and entirely offline: a queue is
/// renamed in `dev`, `rdc doctor dev` realigns the slug AND records the
/// divergence, and the next `rdc migrate dev prod --mirror` therefore renames
/// prod's queue instead of pruning it and creating a new one.
///
/// Step 4 is a second assertion in disguise: with no recorded row migrate now
/// REFUSES this promotion outright, so a regression in the recording turns
/// this test red at the migrate, not only at the mapping-file check.
#[test]
fn doctor_records_a_rename_so_the_next_promotion_renames_instead_of_recreating() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args([
            "init",
            "--env",
            "dev=https://dev.example/api/v1:1",
            "--env",
            "prod=https://prod.example/api/v1:2",
        ])
        .assert()
        .success();

    let write = |path: std::path::PathBuf, body: serde_json::Value| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    };
    for env in ["dev", "prod"] {
        let e = root.join(format!("envs/{env}"));
        // A pulled snapshot carries its own `id` and a portable own-`url`;
        // migrate rewrites the id to the TARGET's when it has one, which is
        // what step 4 asserts.
        let base = if env == "dev" { 1 } else { 500 };
        write(
            e.join("workspaces/main/workspace.json"),
            serde_json::json!({ "id": base, "url": "rdc://workspaces/main", "name": "Main" }),
        );
        write(
            e.join("workspaces/main/queues/invoices/queue.json"),
            serde_json::json!({
                "id": base + 1,
                "url": "rdc://queues/invoices",
                "name": "Invoices",
                "workspace": "rdc://workspaces/main",
                "schema": "rdc://schemas/invoices",
            }),
        );
        write(
            e.join("workspaces/main/queues/invoices/schema.json"),
            serde_json::json!({
                "id": base + 2,
                "url": "rdc://schemas/invoices",
                "name": "Invoices",
                "content": [],
            }),
        );
        // Both envs hold the object remotely; prod's ids are what a mistaken
        // promotion would DELETE.
        write(
            root.join(format!(".rdc/state/{env}.lock.json")),
            serde_json::json!({
                "version": 3,
                "api_base": format!("https://{env}.example/api/v1"),
                "objects": {
                    "workspaces": { "main": { "id": base, "modified_at": null } },
                    "queues": { "invoices": { "id": base + 1, "modified_at": null } },
                    "schemas": { "invoices": { "id": base + 2, "modified_at": null } },
                },
            }),
        );
    }

    // 1. Rename the queue in dev — the local `name` changes, the slug lags.
    let qpath = root.join("envs/dev/workspaces/main/queues/invoices/queue.json");
    let mut q: serde_json::Value = serde_json::from_slice(&std::fs::read(&qpath).unwrap()).unwrap();
    q["name"] = serde_json::json!("Vendor Invoices");
    std::fs::write(&qpath, serde_json::to_vec_pretty(&q).unwrap()).unwrap();

    // 2. doctor realigns the slug...
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["doctor", "dev", "--yes"])
        .assert()
        .success();
    assert!(
        root.join("envs/dev/workspaces/main/queues/vendor-invoices/queue.json")
            .exists(),
        "the dev tree moved to the new slug"
    );

    // 3. ...and records the divergence, one row per queue-keyed kind.
    let mapping = std::fs::read_to_string(root.join(".rdc/mapping.toml"))
        .expect("doctor must have written .rdc/mapping.toml");
    for kind in ["queues", "schemas", "inboxes"] {
        // `inboxes` has no lockfile entry here, so only queues/schemas appear.
        if kind == "inboxes" {
            continue;
        }
        assert!(
            mapping.contains(&format!("[[{kind}]]")),
            "no {kind} row recorded:\n{mapping}"
        );
    }
    assert!(mapping.contains("dev = \"vendor-invoices\""), "{mapping}");
    assert!(mapping.contains("prod = \"invoices\""), "{mapping}");

    // 4. The promotion is now a rename: prod keeps its slug (and so its ids),
    //    and only the name travels.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "dev", "prod", "--mirror", "--yes"])
        .assert()
        .success()
        .stderr(predicate::str::contains("0 pruned"));

    let prod_q: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("envs/prod/workspaces/main/queues/invoices/queue.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(prod_q["name"], "Vendor Invoices", "the new name promoted");
    assert_eq!(prod_q["id"], 501, "onto prod's own object");
    assert!(
        !root
            .join("envs/prod/workspaces/main/queues/vendor-invoices")
            .exists(),
        "no second queue under the source's slug"
    );
}

/// `doctor --dry-run` previews the rename and writes no mapping row.
#[test]
fn doctor_dry_run_records_no_mapping_row() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args([
            "init",
            "--env",
            "dev=https://dev.example/api/v1:1",
            "--env",
            "prod=https://prod.example/api/v1:2",
        ])
        .assert()
        .success();
    std::fs::create_dir_all(root.join("envs/dev/hooks")).unwrap();
    std::fs::write(
        root.join("envs/dev/hooks/old-hook.json"),
        br#"{"id":1,"name":"New Hook","queues":[]}"#,
    )
    .unwrap();
    for (env, id) in [("dev", 1), ("prod", 501)] {
        std::fs::create_dir_all(root.join(".rdc/state")).unwrap();
        std::fs::write(
            root.join(format!(".rdc/state/{env}.lock.json")),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 3,
                "api_base": format!("https://{env}.example/api/v1"),
                "objects": { "hooks": { "old-hook": { "id": id, "modified_at": null } } },
            }))
            .unwrap(),
        )
        .unwrap();
    }

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["doctor", "dev", "--dry-run"])
        .assert()
        .success();

    assert!(
        !root.join(".rdc/mapping.toml").exists(),
        "a dry run must write nothing"
    );
    assert!(
        root.join("envs/dev/hooks/old-hook.json").exists(),
        "and must not move the file either"
    );
}
