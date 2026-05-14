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

use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Global lock serializing tests that mutate process-global state
/// (specifically `std::env::set_current_dir`). Cargo runs tests within a
/// binary in parallel; without serialization here, two tests can change
/// CWD concurrently and one will read the wrong path, producing
/// `NotFound` errors. The lock is acquired in each test's
/// `set_current_dir` window and released after the assertions. Using a
/// `std::sync::Mutex` (not async) is fine — the critical section is
/// short and the tests don't await anything across it that would benefit
/// from yielding.
fn cwd_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture(name: &str) -> serde_json::Value {
    // Resolve via `CARGO_MANIFEST_DIR` rather than the current working
    // directory: several tests `set_current_dir` into a tempdir, and
    // because cargo runs integration tests in parallel within one binary
    // a relative path here is racy.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let raw = std::fs::read_to_string(format!("{manifest_dir}/testdata/fixtures/{name}")).unwrap();
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
    let _cwd_guard = cwd_lock();
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

    let _cwd_guard = cwd_lock();
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
    let _cwd_guard = cwd_lock();
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

    let _cwd_guard = cwd_lock();
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

/// `--no-push` must suppress the push side of sync entirely. Setup mirrors
/// `sync_local_edit_only_patches_remote_label`: first sync seeds the
/// lockfile from a clean label, then the local file is edited so the
/// second sync would normally classify it `LocalEdit` and PATCH. With
/// `--no-push` set on the second sync, the executor's push branch is
/// skipped (`if !no_push`), so:
/// - zero PATCHes hit the API
/// - the edited local file stays edited (no one overwrites it)
/// - the lockfile is unchanged (no push → no recorded post-PATCH hash)
#[tokio::test]
async fn sync_no_push_skips_local_edit() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Single label, served identically on every listing call. The PATCH
    // mock below uses `.expect(0)` so any push attempt would fail the
    // wiremock verification on Drop.
    let base_label = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 77,
                "url": format!("{}/api/v1/labels/77", server.uri()),
                "name": "No Push Label",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#000000",
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

    // `.expect(0)` — wiremock validates no PATCH lands during this test.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();

    // First sync seeds the lockfile and writes the label file. This is
    // the "clean" baseline `--no-push` will be compared against.
    rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("first sync should succeed");

    // Edit the local file so the next sync sees a LocalEdit candidate.
    let label_path = project.path().join("envs/dev/labels/no-push-label.json");
    assert!(label_path.exists(), "first sync should have written the label");
    let raw = std::fs::read_to_string(&label_path).unwrap();
    let edited_color = "#ff00ff";
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(edited_color.to_string());
    let edited_body = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
    std::fs::write(&label_path, &edited_body).unwrap();

    let lf_before = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json"))
        .unwrap();

    // Second sync with --no-push: the LocalEdit must be ignored.
    let result = rdc::cli::sync::run(
        "dev", false, false, false, false, /* no_push = */ true, false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync --no-push should succeed");

    // Zero PATCHes — corroborates the `.expect(0)` wiremock assertion.
    let patch_calls = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/labels/77")
        .count();
    assert_eq!(patch_calls, 0, "expected 0 PATCH calls under --no-push, saw {patch_calls}");

    // Local edit survived intact — the push branch was the only thing
    // that would have rewritten the file with the server's canonical body.
    let body_after = std::fs::read_to_string(&label_path).unwrap();
    assert_eq!(
        body_after, edited_body,
        "--no-push must not touch the locally-edited file"
    );

    // Lockfile is byte-identical: no push → no post-PATCH hash update.
    let lf_after = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json"))
        .unwrap();
    assert_eq!(
        lf_before, lf_after,
        "lockfile must not change when --no-push suppresses the only write"
    );
}

/// `--no-pull` must suppress the pull side of sync entirely. The remote
/// exposes a label that doesn't exist locally — a vanilla sync would
/// classify it `RemoteCreate` and write the file. With `--no-pull`, the
/// executor's pull branch is skipped (`if !no_pull`), so no local file
/// is created and the lockfile records no entry for it.
#[tokio::test]
async fn sync_no_pull_skips_remote_change() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // One label on the env, nothing local. Pull would normally write it.
    let labels_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 55,
                "url": format!("{}/api/v1/labels/55", server.uri()),
                "name": "No Pull Label",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#123456",
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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ true,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync --no-pull should succeed");

    // No local label file was created — the pull branch was the only
    // path that would have written one.
    let label_path = project.path().join("envs/dev/labels/no-pull-label.json");
    assert!(
        !label_path.exists(),
        "--no-pull must not write a local file; found one at {}",
        label_path.display()
    );

    // Lockfile must not record the remote-only label. Reading the JSON
    // and asserting absence is more robust than a substring scan because
    // the labels key may exist with an empty map.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json"))
        .unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    let labels = lf
        .get("objects")
        .and_then(|o| o.get("labels"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let recorded = match &labels {
        serde_json::Value::Object(m) => m.contains_key("no-pull-label"),
        _ => false,
    };
    assert!(
        !recorded,
        "--no-pull must not record the remote-only label in the lockfile: {lf_raw}"
    );
}

/// `--dry-run` must short-circuit before any executor branch runs. With
/// both a local edit AND a remote-only label on the env, a vanilla sync
/// would PATCH the former and create a local file for the latter. Under
/// `--dry-run`, neither happens, and the CLI emits a "Plan: sync …"
/// header to stdout. We invoke the binary directly here so we can assert
/// on the captured stdout — calling `sync::run` directly would print to
/// the test runner's own stdout.
#[tokio::test]
async fn sync_dry_run_makes_zero_writes() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Two labels on the env: one we'll set up as LocalEdit candidate,
    // one untouched locally so it stays RemoteCreate.
    let labels_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 31,
                "url": format!("{}/api/v1/labels/31", server.uri()),
                "name": "Dry Edit",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#111111",
                "modified_at": "2026-04-15T08:00:00Z"
            },
            {
                "id": 32,
                "url": format!("{}/api/v1/labels/32", server.uri()),
                "name": "Dry Create",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#222222",
                "modified_at": "2026-04-15T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&labels_body))
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

    // Seed the lockfile + local files via a first (real) sync, then edit
    // one of the labels and delete the other so the dry-run sees a
    // LocalEdit + a RemoteCreate at the same time.
    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    rdc::cli::sync::run(
        "dev", false, false, false, false, false, false,
    )
    .await
    .expect("seed sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    let edit_path = project.path().join("envs/dev/labels/dry-edit.json");
    let create_path = project.path().join("envs/dev/labels/dry-create.json");
    assert!(edit_path.exists() && create_path.exists(), "seed sync must write both files");

    // Edit one label.
    let raw = std::fs::read_to_string(&edit_path).unwrap();
    let edited_color = "#9999ee";
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(edited_color.to_string());
    let edited_body = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
    std::fs::write(&edit_path, &edited_body).unwrap();

    // Delete the other so it shows up as RemoteCreate on the next sync.
    // Also strip its lockfile entry so the classifier doesn't see it as
    // a LocalDelete tombstone instead.
    std::fs::remove_file(&create_path).unwrap();
    let lf_path = project.path().join(".rdc/state/dev.lock.json");
    let lf_raw = std::fs::read_to_string(&lf_path).unwrap();
    let mut lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    if let Some(labels) = lf
        .get_mut("objects")
        .and_then(|o| o.get_mut("labels"))
        .and_then(|l| l.as_object_mut())
    {
        labels.remove("dry-create");
    }
    std::fs::write(&lf_path, serde_json::to_string_pretty(&lf).unwrap()).unwrap();

    // Snapshot disk state so we can assert nothing changed.
    let edit_before = std::fs::read_to_string(&edit_path).unwrap();
    let lf_before = std::fs::read_to_string(&lf_path).unwrap();

    // Drive the dry-run via the actual binary so stdout is captured.
    let out = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--dry-run", "--yes"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Plan: sync"),
        "dry-run stdout must contain 'Plan: sync': {stdout}"
    );

    // Zero writes hit the API across the dry-run invocation. We can't
    // diff request counts the same way as `sync_clean_label_no_writes`
    // because the seed sync above already issued non-mutating GETs;
    // instead we assert the absolute count of POST/PATCH/DELETE is zero
    // (excluding data-storage convention) over the *whole* test lifetime
    // — neither sync above ever issued one either, so the invariant is
    // strictly "no mutating call ever lands in this test".
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
            "dry-run sync must not issue any mutating request: {} {}",
            req.method,
            p
        );
    }

    // Local files and lockfile are byte-identical.
    let edit_after = std::fs::read_to_string(&edit_path).unwrap();
    assert_eq!(edit_before, edit_after, "dry-run must not rewrite local files");
    assert!(!create_path.exists(), "dry-run must not materialize remote-only labels");
    let lf_after = std::fs::read_to_string(&lf_path).unwrap();
    assert_eq!(lf_before, lf_after, "dry-run must not touch the lockfile");
}

/// `--no-push` and `--no-pull` together are nonsensical: that combination
/// is a read-only inspection, which is `rdc status`'s job. The sync entry
/// point must reject the pairing up-front with a message that points
/// users at the right tool.
#[tokio::test]
async fn sync_no_push_and_no_pull_together_errors() {
    // No project setup is needed — the flag check fires before any
    // filesystem or API access. We still create a tempdir + cd into it
    // so `current_dir()` doesn't surprise the test runner.
    let project = TempDir::new().unwrap();
    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ true, /* no_pull = */ true,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    let err = result.expect_err("--no-push + --no-pull must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mutually exclusive") || msg.contains("rdc status"),
        "error message should mention 'mutually exclusive' or 'rdc status': {msg}"
    );
}

/// Pull-side RemoteCreate for a workflow: env exposes a workflow that
/// doesn't exist locally and isn't in the lockfile. `sync` must classify
/// it `RemoteCreate` and write `envs/dev/workflows/<slug>/workflow.json`.
/// Workflows are read-only at the Rossum API, so no mutations are issued.
#[tokio::test]
async fn sync_remote_create_writes_local_workflow() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let workflows_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 700,
                "url": format!("{}/api/v1/workflows/700", server.uri()),
                "name": "AP Approval Flow",
                "steps": [],
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workflows_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/workflows"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new workflow");

    // No API writes — pull-side only (workflows are read-only).
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

    let workflow_path = project
        .path()
        .join("envs/dev/workflows/ap-approval-flow/workflow.json");
    assert!(
        workflow_path.exists(),
        "workflow JSON should be written at {}",
        workflow_path.display()
    );
    let body = std::fs::read_to_string(&workflow_path).unwrap();
    assert!(body.contains("AP Approval Flow"), "workflow content: {body}");

    // Lockfile records the workflow.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf_raw.contains("\"workflows\""), "lockfile must record workflow: {lf_raw}");
    assert!(lf_raw.contains("ap-approval-flow"), "lockfile must record slug: {lf_raw}");
}

/// Pull-side RemoteCreate for a workflow step. Requires the parent
/// workflow to be present too (the driver skips orphan steps), so this
/// mocks both endpoints. Asserts the nested file at
/// `envs/dev/workflows/<workflow_slug>/steps/<step_slug>.json` exists.
#[tokio::test]
async fn sync_remote_create_writes_local_workflow_step() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let workflow_url = format!("{}/api/v1/workflows/700", server.uri());
    let workflows_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 700,
                "url": workflow_url,
                "name": "AP Approval Flow",
                "steps": [
                    format!("{}/api/v1/workflow_steps/1", server.uri())
                ],
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workflows_body))
        .mount(&server)
        .await;

    let steps_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 1,
                "url": format!("{}/api/v1/workflow_steps/1", server.uri()),
                "name": "Manager Approval",
                "workflow": format!("{}/api/v1/workflows/700", server.uri()),
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workflow_steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(steps_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/workflows", "/api/v1/workflow_steps"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new workflow step");

    // No API mutations — both kinds are read-only.
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

    let step_path = project
        .path()
        .join("envs/dev/workflows/ap-approval-flow/steps/manager-approval.json");
    assert!(
        step_path.exists(),
        "workflow step JSON should be written at {}",
        step_path.display()
    );
    let body = std::fs::read_to_string(&step_path).unwrap();
    assert!(body.contains("Manager Approval"), "step content: {body}");

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"workflow_steps\""),
        "lockfile must record workflow_steps: {lf_raw}"
    );
    assert!(
        lf_raw.contains("manager-approval"),
        "lockfile must record step slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for the organization singleton: the org JSON
/// from `/api/v1/organizations/<id>` lands at `envs/dev/organization.json`
/// and the lockfile records it under the `"self"` slug. The org is
/// read-only at the Rossum API so no mutations should land.
#[tokio::test]
async fn sync_remote_create_writes_local_organization() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &[]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote serves an organization");

    // No mutating calls — the org is pull-only.
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

    let org_path = project.path().join("envs/dev/organization.json");
    assert!(
        org_path.exists(),
        "organization JSON should be written at {}",
        org_path.display()
    );
    let body = std::fs::read_to_string(&org_path).unwrap();
    assert!(
        body.contains("Acme Test Org"),
        "organization content: {body}"
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"organization\""),
        "lockfile must record organization: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"self\""),
        "lockfile must record the 'self' slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for a workspace: env exposes a workspace that
/// doesn't exist locally and isn't in the lockfile. `sync` must classify
/// it `RemoteCreate` and write `envs/dev/workspaces/<slug>/workspace.json`.
/// Workspaces are push-capable so the pull-side branch in the executor
/// must dispatch to `pull::workspaces::process`; this test pins that wire-up.
#[tokio::test]
async fn sync_remote_create_writes_local_workspace() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let workspaces_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 800,
                "url": format!("{}/api/v1/workspaces/800", server.uri()),
                "name": "Invoices AP",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [],
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workspaces_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/workspaces"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new workspace");

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

    let workspace_path = project
        .path()
        .join("envs/dev/workspaces/invoices-ap/workspace.json");
    assert!(
        workspace_path.exists(),
        "workspace JSON should be written at {}",
        workspace_path.display()
    );
    let body = std::fs::read_to_string(&workspace_path).unwrap();
    assert!(body.contains("Invoices AP"), "workspace content: {body}");

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"workspaces\""),
        "lockfile must record workspace: {lf_raw}"
    );
    assert!(
        lf_raw.contains("invoices-ap"),
        "lockfile must record slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for an engine: env exposes an engine that
/// doesn't exist locally. `sync` must classify it `RemoteCreate` and
/// write `envs/dev/engines/<slug>/engine.json`.
#[tokio::test]
async fn sync_remote_create_writes_local_engine() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let engines_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 401,
                "url": format!("{}/api/v1/engines/401", server.uri()),
                "name": "Invoice Engine",
                "type": "extractor",
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(engines_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/engines"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new engine");

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

    let engine_path = project
        .path()
        .join("envs/dev/engines/invoice-engine/engine.json");
    assert!(
        engine_path.exists(),
        "engine JSON should be written at {}",
        engine_path.display()
    );
    let body = std::fs::read_to_string(&engine_path).unwrap();
    assert!(body.contains("Invoice Engine"), "engine content: {body}");

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"engines\""),
        "lockfile must record engine: {lf_raw}"
    );
    assert!(
        lf_raw.contains("invoice-engine"),
        "lockfile must record slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for an engine field. Requires the parent
/// engine to be present too (the driver skips orphan fields), so this
/// mocks both endpoints. Asserts the nested file at
/// `envs/dev/engines/<engine_slug>/fields/<field_slug>.json` exists.
#[tokio::test]
async fn sync_remote_create_writes_local_engine_field() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let engine_url = format!("{}/api/v1/engines/401", server.uri());
    let engines_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 401,
                "url": engine_url,
                "name": "Invoice Engine",
                "type": "extractor",
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(engines_body))
        .mount(&server)
        .await;

    let fields_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 501,
                "url": format!("{}/api/v1/engine_fields/501", server.uri()),
                "name": "Invoice Number",
                "engine": format!("{}/api/v1/engines/401", server.uri()),
                "field_type": "string",
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/engine_fields"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fields_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/engines", "/api/v1/engine_fields"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new engine field");

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

    let field_path = project
        .path()
        .join("envs/dev/engines/invoice-engine/fields/invoice-number.json");
    assert!(
        field_path.exists(),
        "engine field JSON should be written at {}",
        field_path.display()
    );
    let body = std::fs::read_to_string(&field_path).unwrap();
    assert!(body.contains("Invoice Number"), "field content: {body}");

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"engine_fields\""),
        "lockfile must record engine_fields: {lf_raw}"
    );
    assert!(
        lf_raw.contains("invoice-number"),
        "lockfile must record field slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for an MDH dataset: the Data Storage service
/// returns one collection with an index set, and `sync` must write both
/// `collection.json` and `indexes.json` under `envs/dev/mdh/<slug>/`.
/// MDH is pull-only (no push pipeline), so this exercises only the
/// pull-side branch of the executor.
#[tokio::test]
async fn sync_remote_create_writes_local_mdh_dataset() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &[]).await;

    // Data Storage endpoints use POST with a JSON envelope `{code, message, result}`.
    use wiremock::matchers::body_partial_json;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/collections/list"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "message": "",
                "result": [
                    {
                        "name": "vendors",
                        "type": "collection",
                        "options": {},
                        "idIndex": { "v": 2, "key": { "_id": 1 }, "name": "_id_" }
                    }
                ]
            })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/indexes/list"))
        .and(body_partial_json(
            serde_json::json!({"collectionName": "vendors"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "message": "",
                "result": [
                    { "v": 2, "name": "_id_", "key": { "_id": 1 } }
                ]
            })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/search_indexes/list"))
        .and(body_partial_json(
            serde_json::json!({"collectionName": "vendors"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "message": "",
                "result": []
            })),
        )
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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when MDH has a new dataset");

    // No mutating API calls on the Rossum API side — MDH is pull-only.
    // Data Storage uses POSTs for *reads* (RPC-style), so those are
    // excluded from the assertion (mirroring the existing cli_sync
    // tests' convention).
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

    let collection_path = project.path().join("envs/dev/mdh/vendors/collection.json");
    let indexes_path = project.path().join("envs/dev/mdh/vendors/indexes.json");
    assert!(
        collection_path.exists(),
        "collection JSON should be written at {}",
        collection_path.display()
    );
    assert!(
        indexes_path.exists(),
        "indexes JSON should be written at {}",
        indexes_path.display()
    );
    let body = std::fs::read_to_string(&collection_path).unwrap();
    assert!(body.contains("vendors"), "collection content: {body}");

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"mdh_collections\""),
        "lockfile must record mdh_collections: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"mdh_indexes\""),
        "lockfile must record mdh_indexes: {lf_raw}"
    );
    assert!(
        lf_raw.contains("vendors"),
        "lockfile must record dataset slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for a hook. The env exposes a function hook
/// with `config.code` populated; sync must write `<slug>.json` (with
/// `code` stripped) AND a sibling `<slug>.py` carrying the extracted
/// code. The combined hash recorded in the lockfile must match what
/// `pull::hooks::process` would compute, so re-running sync sees Clean
/// state and emits no further writes.
#[tokio::test]
async fn sync_remote_create_writes_local_hook() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let hooks_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 501,
                "url": format!("{}/api/v1/hooks/501", server.uri()),
                "name": "Validator: invoices",
                "type": "function",
                "queues": [],
                "events": ["annotation_content"],
                "config": {
                    "runtime": "python3.12",
                    "code": "def x(payload):\n    return {}\n"
                },
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hooks_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/hooks"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new hook");

    // No mutating API calls — pull-side only.
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

    let json_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    assert!(
        json_path.exists(),
        "hook JSON should be written at {}",
        json_path.display()
    );
    assert!(
        py_path.exists(),
        "hook .py sidecar should be written at {}",
        py_path.display()
    );
    let json_body = std::fs::read_to_string(&json_path).unwrap();
    assert!(
        json_body.contains("Validator: invoices"),
        "hook JSON content: {json_body}"
    );
    assert!(
        !json_body.contains("def x"),
        "extracted code must not be in JSON: {json_body}"
    );
    let py_body = std::fs::read_to_string(&py_path).unwrap();
    assert!(
        py_body.contains("def x"),
        "extracted code must land in .py sidecar: {py_body}"
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"hooks\""),
        "lockfile must record hooks: {lf_raw}"
    );
    assert!(
        lf_raw.contains("validator-invoices"),
        "lockfile must record slug: {lf_raw}"
    );
}

/// Pull-side RemoteCreate for a rule. The env exposes a rule with
/// `trigger_condition` set; sync must write `<slug>.json` (with the
/// condition stripped) AND a sibling `<slug>.py` carrying the extracted
/// condition. The combined hash recorded in the lockfile must match
/// what `pull::rules::process` would compute.
#[tokio::test]
async fn sync_remote_create_writes_local_rule() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let rules_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 2597,
                "url": format!("{}/api/v1/rules/2597", server.uri()),
                "name": "E-invoice Validation",
                "queues": [],
                "trigger_condition": "annotation_content.total > 1000\n",
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rules_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/rules"]).await;

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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new rule");

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

    let json_path = project.path().join("envs/dev/rules/e-invoice-validation.json");
    let py_path = project.path().join("envs/dev/rules/e-invoice-validation.py");
    assert!(
        json_path.exists(),
        "rule JSON should be written at {}",
        json_path.display()
    );
    assert!(
        py_path.exists(),
        "rule .py sidecar should be written at {}",
        py_path.display()
    );
    let json_body = std::fs::read_to_string(&json_path).unwrap();
    assert!(
        json_body.contains("E-invoice Validation"),
        "rule JSON content: {json_body}"
    );
    assert!(
        !json_body.contains("annotation_content.total"),
        "trigger_condition must not be in JSON: {json_body}"
    );
    let py_body = std::fs::read_to_string(&py_path).unwrap();
    assert!(
        py_body.contains("annotation_content.total"),
        "trigger_condition must land in .py sidecar: {py_body}"
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"rules\""),
        "lockfile must record rules: {lf_raw}"
    );
    assert!(
        lf_raw.contains("e-invoice-validation"),
        "lockfile must record slug: {lf_raw}"
    );
}

/// Helper: wire up a queue-tree fixture (workspace + queue + schema +
/// inbox + email template) on the mock server. Mirrors the
/// `pull_writes_full_workspace_tree` setup. Returns the mock-side URLs
/// the test can assert against later. The same URLs (with `server.uri()`)
/// are referenced by every nested object so the adapter resolves
/// queue → workspace and template → queue cleanly.
async fn mount_queue_tree_fixture(server: &MockServer) {
    let ws_url = format!("{}/api/v1/workspaces/800", server.uri());
    let queue_url = format!("{}/api/v1/queues/100", server.uri());
    let schema_url = format!("{}/api/v1/schemas/200", server.uri());
    let inbox_url = format!("{}/api/v1/inboxes/300", server.uri());

    let workspaces_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 800,
                "url": ws_url,
                "name": "Invoices AP",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [queue_url.clone()],
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workspaces_body))
        .mount(server)
        .await;

    let queues_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 100,
                "url": queue_url.clone(),
                "name": "Cost Invoices",
                "workspace": format!("{}/api/v1/workspaces/800", server.uri()),
                "schema": schema_url.clone(),
                "inbox": inbox_url.clone(),
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queues_body))
        .mount(server)
        .await;

    let schema_body = serde_json::json!({
        "id": 200,
        "url": schema_url,
        "name": "Cost Invoices Schema",
        "queues": [queue_url.clone()],
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
                        "formula": "amount_due + amount_tax"
                    }
                ]
            }
        ],
        "modified_at": "2026-04-10T09:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(schema_body))
        .mount(server)
        .await;

    let inbox_body = serde_json::json!({
        "id": 300,
        "url": inbox_url,
        "name": "Cost Invoices Inbox",
        "email": "cost-invoices@mock.rossum.app",
        "queues": [queue_url.clone()],
        "modified_at": "2026-04-10T09:00:00Z",
        "filters": []
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inbox_body))
        .mount(server)
        .await;

    let email_templates_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 9001,
                "url": format!("{}/api/v1/email_templates/9001", server.uri()),
                "name": "Rejection Notice",
                "subject": "Your invoice was rejected",
                "queue": queue_url,
                "modified_at": "2026-04-20T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(email_templates_body))
        .mount(server)
        .await;
}

/// Pull-side RemoteCreate for the full queue tree. The env exposes
/// a workspace, a queue (with linked schema + inbox), and an email
/// template scoped to that queue. None of these exist locally. `sync`
/// must classify the queue tree as RemoteCreate and dispatch through
/// `pull::queues::process` (which writes all 4 file types) plus
/// `pull::email_templates::process`.
#[tokio::test]
async fn sync_remote_create_writes_local_queue_tree() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    mount_queue_tree_fixture(&server).await;

    mock_empty_lists_except(
        &server,
        &[
            "/api/v1/workspaces",
            "/api/v1/queues",
            "/api/v1/email_templates",
        ],
    )
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

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new queue tree");

    // No mutating API calls.
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

    let cost_dir = project
        .path()
        .join("envs/dev/workspaces/invoices-ap/queues/cost-invoices");
    assert!(
        cost_dir.join("queue.json").exists(),
        "queue.json should be written at {}",
        cost_dir.join("queue.json").display()
    );
    assert!(
        cost_dir.join("schema.json").exists(),
        "schema.json should be written at {}",
        cost_dir.join("schema.json").display()
    );
    assert!(
        cost_dir.join("inbox.json").exists(),
        "inbox.json should be written at {}",
        cost_dir.join("inbox.json").display()
    );
    assert!(
        cost_dir.join("formulas/amount_total.py").exists(),
        "formula sidecar should be written at {}",
        cost_dir.join("formulas/amount_total.py").display()
    );
    let schema_json = std::fs::read_to_string(cost_dir.join("schema.json")).unwrap();
    assert!(
        !schema_json.contains("amount_due + amount_tax"),
        "formula must not be in schema.json: {schema_json}"
    );

    // Email template nests under the queue.
    let tpl_path = cost_dir.join("email-templates/rejection-notice.json");
    assert!(
        tpl_path.exists(),
        "email template should be written at {}",
        tpl_path.display()
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf_raw.contains("\"queues\""), "lockfile must record queues: {lf_raw}");
    assert!(lf_raw.contains("\"schemas\""), "lockfile must record schemas: {lf_raw}");
    assert!(lf_raw.contains("\"inboxes\""), "lockfile must record inboxes: {lf_raw}");
    assert!(
        lf_raw.contains("\"email_templates\""),
        "lockfile must record email_templates: {lf_raw}"
    );
    assert!(lf_raw.contains("cost-invoices"), "queue slug recorded: {lf_raw}");
    assert!(
        lf_raw.contains("invoices-ap/cost-invoices/rejection-notice"),
        "email template compound key recorded: {lf_raw}"
    );
}

/// Idempotency for the queue tree: after an initial sync, a second
/// sync run with no remote or local changes should be a no-op
/// (no API mutations, no file rewrites). Pins the Clean classification
/// for the four nested kinds.
#[tokio::test]
async fn sync_clean_queue_tree_no_writes() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    mount_queue_tree_fixture(&server).await;

    mock_empty_lists_except(
        &server,
        &[
            "/api/v1/workspaces",
            "/api/v1/queues",
            "/api/v1/email_templates",
        ],
    )
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

    // First sync: writes the queue tree.
    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("first sync should succeed");

    // Snapshot the on-disk file mtimes so we can verify the second run
    // doesn't rewrite them. mtime is a coarser signal than byte
    // comparison but it's what we have without rebuilding the whole
    // file tree; `write_atomic` skips writes when bytes match, so a
    // no-op sync should leave the mtimes alone.
    let cost_dir = project
        .path()
        .join("envs/dev/workspaces/invoices-ap/queues/cost-invoices");
    let queue_mtime = std::fs::metadata(cost_dir.join("queue.json"))
        .unwrap()
        .modified()
        .unwrap();
    let schema_mtime = std::fs::metadata(cost_dir.join("schema.json"))
        .unwrap()
        .modified()
        .unwrap();
    let inbox_mtime = std::fs::metadata(cost_dir.join("inbox.json"))
        .unwrap()
        .modified()
        .unwrap();
    let tpl_mtime = std::fs::metadata(cost_dir.join("email-templates/rejection-notice.json"))
        .unwrap()
        .modified()
        .unwrap();

    // Clear the request log so the second-run assertion only sees the
    // second-run traffic.
    server.reset().await;

    // Re-mount the same fixture (reset clears mocks too).
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    mount_queue_tree_fixture(&server).await;
    mock_empty_lists_except(
        &server,
        &[
            "/api/v1/workspaces",
            "/api/v1/queues",
            "/api/v1/email_templates",
        ],
    )
    .await;

    // Second sync: should be a no-op.
    rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* diff = */ false, /* allow_deletes = */ false,
        /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("second sync should succeed (clean state)");
    std::env::set_current_dir(&prev_cwd).unwrap();

    // No mutating API calls in the second run.
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
            "unexpected mutating request on clean re-sync: {} {}",
            req.method,
            p
        );
    }

    // Files unchanged byte-for-byte and (best-effort) mtime-stable.
    let queue_mtime_after = std::fs::metadata(cost_dir.join("queue.json"))
        .unwrap()
        .modified()
        .unwrap();
    let schema_mtime_after = std::fs::metadata(cost_dir.join("schema.json"))
        .unwrap()
        .modified()
        .unwrap();
    let inbox_mtime_after = std::fs::metadata(cost_dir.join("inbox.json"))
        .unwrap()
        .modified()
        .unwrap();
    let tpl_mtime_after =
        std::fs::metadata(cost_dir.join("email-templates/rejection-notice.json"))
            .unwrap()
            .modified()
            .unwrap();
    assert_eq!(queue_mtime, queue_mtime_after, "queue.json must not be rewritten");
    assert_eq!(schema_mtime, schema_mtime_after, "schema.json must not be rewritten");
    assert_eq!(inbox_mtime, inbox_mtime_after, "inbox.json must not be rewritten");
    assert_eq!(tpl_mtime, tpl_mtime_after, "email template must not be rewritten");
}
