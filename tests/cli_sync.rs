//! Integration smoke test for `rdc sync` (Task 13).
//!
//! Exercises the clean-env happy path: API returns empty listings, local
//! snapshot has no files, lockfile is empty. The classify adapter and
//! executor are stubs today, so 0 writes occur — the test verifies the
//! pipeline shape (`list_remote` → `scan` → `classify` → `plan` →
//! `confirm` → `execute`) compiles and runs end-to-end without any
//! API writes.
//!
//! Subsequent tasks fill in per-kind hashing (T14–17) and the executor
//! (T14–17); their integration tests will live alongside this one.

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// On a freshly-initialized project where every Rossum listing returns
/// an empty results array and the lockfile is empty, `sync` must:
/// - succeed
/// - issue zero POST / PATCH / DELETE calls
/// - leave a saved (empty / near-empty) lockfile on disk
///
/// This is the canonical clean-env smoke test. It pins the contract that
/// the executor stub is a no-op for the empty case so subsequent tasks
/// can iterate on per-class logic without regressing the happy path.
#[tokio::test]
async fn sync_clean_env_does_no_writes() {
    let server = MockServer::start().await;

    // Organization endpoint — required for bootstrap (the pull pipeline
    // GETs `/api/v1/organizations/<org_id>` to seed the catalog).
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Every listing endpoint the pull pipeline visits returns an empty
    // results array. This mirrors `cli_pull::pull_skips_mdh_when_endpoint_returns_404`
    // (and several others) — clean-env behavior is well-trodden ground.
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks",
        "/api/v1/workspaces",
        "/api/v1/queues",
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
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }
    // No data_storage endpoints mocked — wiremock returns 404 for unknown
    // paths and the MDH driver tolerates that, mirroring
    // `pull_skips_mdh_when_endpoint_returns_404`.

    // Any unexpected POST/PATCH/DELETE will surface via
    // `received_requests()` after the run — we assert the list is empty
    // (excluding the data-storage paths the MDH driver legitimately
    // POSTs to for *reads*).

    let project = TempDir::new().unwrap();

    // Bootstrap the project via the `init` subcommand — same shape as
    // every other integration test in this crate.
    assert_cmd::Command::cargo_bin("rdc")
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

    // `sync::run` reads `std::env::current_dir()`, so the test must
    // hop into the project root for the call. CWD is process-global, so
    // we restore it before returning to avoid bleeding state into any
    // future test that ends up in the same binary.
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("clean-env sync should succeed");

    // No writes hit the API. Data-storage paths use POST for reads by
    // convention (e.g. `/svc/data-storage/api/v1/collections/list`); we
    // exclude them from the assertion, mirroring `cli_deploy::*` tests
    // that gate on the same invariant.
    for req in server.received_requests().await.unwrap_or_default() {
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

    // Lockfile is saved (at least the version header is there). Empty
    // env still produces a parseable file.
    let lf_path = project.path().join(".rdc/state/dev.lock.json");
    assert!(lf_path.exists(), "lockfile should be saved at {}", lf_path.display());
    let lf_raw = std::fs::read_to_string(&lf_path).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw)
        .expect("lockfile must be valid JSON");
    assert!(lf.get("version").is_some(), "lockfile should have a version: {lf_raw}");
}

/// Helper: mock every Rossum listing endpoint with an empty body. The
/// per-test caller can then override specific endpoints with real
/// fixtures.
async fn mock_empty_lists_except(server: &MockServer, override_paths: &[&str]) {
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks",
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        if override_paths.contains(&ep) {
            continue;
        }
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(server)
            .await;
    }
}

/// Pull-side RemoteCreate: env exposes a label that doesn't exist locally
/// and isn't in the lockfile. `sync` must classify it `RemoteCreate` and
/// write the JSON to disk. No API mutations are issued.
#[tokio::test]
async fn sync_remote_create_writes_local_label() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // One label present on the env.
    let labels_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 11,
                "url": format!("{}/api/v1/labels/11", server.uri()),
                "name": "Priority High",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#ff0000",
                "modified_at": "2026-04-15T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(labels_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    let project = TempDir::new().unwrap();

    assert_cmd::Command::cargo_bin("rdc")
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

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new label");

    // No API writes — pull-side only.
    for req in server.received_requests().await.unwrap_or_default() {
        let p = req.url.path();
        if p.contains("/svc/data-storage/") {
            continue;
        }
        assert!(
            !matches!(
                req.method,
                http::Method::POST | http::Method::PATCH | http::Method::DELETE
            ),
            "unexpected mutating request: {} {}",
            req.method,
            p
        );
    }

    // Local file written.
    let label_path = project.path().join("envs/dev/labels/priority-high.json");
    assert!(
        label_path.exists(),
        "label JSON should be written at {}",
        label_path.display()
    );
    let body = std::fs::read_to_string(&label_path).unwrap();
    assert!(body.contains("Priority High"), "label content: {body}");

    // Lockfile records the label.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf_raw.contains("\"labels\""), "lockfile must record label: {lf_raw}");
    assert!(lf_raw.contains("priority-high"), "lockfile must record slug: {lf_raw}");
}

/// Clean-state label: env has the label, local snapshot already mirrors
/// it, lockfile records the matching hash. The classifier must mark it
/// `Clean` and the executor must perform no writes. This pins the
/// "hashes agree" half of the adapter contract — if hashing diverges
/// from how pull writes / push scans, this test goes red.
#[tokio::test]
async fn sync_clean_label_no_writes() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let labels_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 11,
                "url": format!("{}/api/v1/labels/11", server.uri()),
                "name": "Priority High",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#ff0000",
                "modified_at": "2026-04-15T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(labels_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    let project = TempDir::new().unwrap();

    assert_cmd::Command::cargo_bin("rdc")
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

    // First sync: pulls the label and populates the lockfile.
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    rdc::cli::sync::run(
        "dev", false, false, false, false, false, false,
    )
    .await
    .expect("first sync should succeed");

    // Snapshot request counts after the first sync so we can assert the
    // second one is a no-op on writes (same write count == 0).
    let writes_before = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            !r.url.path().contains("/svc/data-storage/")
                && matches!(
                    r.method,
                    http::Method::POST | http::Method::PATCH | http::Method::DELETE
                )
        })
        .count();

    // Second sync: nothing should change. The label is on the env, on
    // disk, and in the lockfile with a matching hash → Clean.
    rdc::cli::sync::run(
        "dev", false, false, false, false, false, false,
    )
    .await
    .expect("clean-state second sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    let writes_after = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            !r.url.path().contains("/svc/data-storage/")
                && matches!(
                    r.method,
                    http::Method::POST | http::Method::PATCH | http::Method::DELETE
                )
        })
        .count();
    assert_eq!(
        writes_before, writes_after,
        "clean-state second sync must not issue any mutating requests"
    );
    assert_eq!(writes_before, 0, "first sync must not issue any mutating requests either");

    // Label file is still present and the lockfile still records it.
    assert!(project.path().join("envs/dev/labels/priority-high.json").exists());
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf_raw.contains("priority-high"));
}

/// Push-side LocalEdit: the label exists on disk with edited content, the
/// lockfile records the pre-edit hash, and the remote still serves the
/// pre-edit body. The classifier must mark it `LocalEdit` and the executor
/// must PATCH the remote exactly once. This pins the push-side branch of
/// the sync executor (Task 15).
#[tokio::test]
async fn sync_local_edit_only_patches_remote_label() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // The label remote serves throughout the test (both the initial pull
    // that seeds the lockfile and the subsequent sync's listing). The
    // sync's push driver also re-lists labels for drift detection — the
    // body it sees here must hash to the base recorded by pull.
    let base_label = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 99,
                "url": format!("{}/api/v1/labels/99", server.uri()),
                "name": "Audit Hold",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#aabbcc",
                "modified_at": "2026-04-15T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&base_label))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    // PATCH /labels/99: server confirms the edit. `.expect(1)` enforces
    // that exactly one PATCH call lands during the second sync.
    let patched_color = "#ff0000";
    let patch_response = serde_json::json!({
        "id": 99,
        "url": format!("{}/api/v1/labels/99", server.uri()),
        "name": "Audit Hold",
        "organization": format!("{}/api/v1/organizations/1", server.uri()),
        "color": patched_color,
        "modified_at": "2026-04-15T09:00:00Z"
    });
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&patch_response))
        .expect(1)
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();

    assert_cmd::Command::cargo_bin("rdc")
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

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();

    // First sync: pulls the label and populates the lockfile with the
    // base content hash. Pull-side branch (Task 14) handles this.
    rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("first sync should succeed");

    // Edit the local label file — this triggers the push-side LocalEdit
    // class on the second sync. The remote still serves the pre-edit
    // body, so `remote_hash == base_hash` and `local_hash != base_hash`.
    let label_path = project.path().join("envs/dev/labels/audit-hold.json");
    assert!(label_path.exists(), "first sync should have written the label");
    let raw = std::fs::read_to_string(&label_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(patched_color.to_string());
    std::fs::write(&label_path, format!("{}\n", serde_json::to_string_pretty(&v).unwrap()))
        .unwrap();

    // Snapshot lockfile hash before second sync so we can assert it changes.
    let lf_before = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json"))
        .unwrap();

    // Second sync: classifier sees LocalEdit; executor must PATCH.
    let result = rdc::cli::sync::run(
        "dev", false, false, false, false, false, false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("second sync should succeed and PATCH the remote label");

    // wiremock's `.expect(1)` on the PATCH mock is verified on Drop, but
    // make the assertion explicit by counting the received requests too.
    let patch_calls = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/labels/99")
        .count();
    assert_eq!(
        patch_calls, 1,
        "exactly one PATCH /labels/99 expected, saw {patch_calls}"
    );

    // The local file is rewritten to the server's post-PATCH canonical
    // form, so the edited color survives.
    let body = std::fs::read_to_string(&label_path).unwrap();
    assert!(
        body.contains(patched_color),
        "local file should retain the edited color after PATCH: {body}"
    );

    // Lockfile hash for labels/audit-hold must have changed: it now
    // records the post-PATCH canonical form, not the pre-edit base.
    let lf_after = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json"))
        .unwrap();
    assert_ne!(
        lf_before, lf_after,
        "lockfile must update after a successful PATCH"
    );
    assert!(lf_after.contains("audit-hold"), "lockfile keeps the slug: {lf_after}");
}
