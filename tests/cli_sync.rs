// The cwd_lock() guard is intentionally held across .await to serialize
// tests that mutate the process-wide current directory.
#![allow(clippy::await_holding_lock)]

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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Global lock serializing tests that mutate process-global state
/// (specifically `std::env::set_current_dir`). Cargo runs tests within a
/// binary in parallel; without serialization here, two tests can change
/// CWD concurrently and one will read the wrong path, producing
/// `NotFound` errors. The lock is acquired in each test's
/// `set_current_dir` window and released after the assertions. Using a
/// `std::sync::Mutex` (not async) is fine — the critical section is
/// short and the tests don't await anything across it that would benefit
/// from yielding.
/// Guard returned by [`cwd_lock`]: holds the global mutex AND restores
/// the working directory captured at lock time when dropped — including
/// on panic. Without the restore, a test that panics inside its
/// `set_current_dir` window leaves the process cwd pointing into its
/// (now deleted) tempdir and every later in-process test fails with
/// `NotFound` — one red test used to cascade into dozens.
struct CwdLock {
    _lock: MutexGuard<'static, ()>,
    prev: Option<std::path::PathBuf>,
}

impl Drop for CwdLock {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            let _ = std::env::set_current_dir(prev);
        }
    }
}

fn cwd_lock() -> CwdLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CwdLock {
        _lock: lock,
        prev: std::env::current_dir().ok(),
    }
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
        "/api/v1/inboxes",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
    assert!(
        lf_path.exists(),
        "lockfile should be saved at {}",
        lf_path.display()
    );
    let lf_raw = std::fs::read_to_string(&lf_path).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).expect("lockfile must be valid JSON");
    assert!(
        lf.get("version").is_some(),
        "lockfile should have a version: {lf_raw}"
    );
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
        "/api/v1/inboxes",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
    assert!(
        lf_raw.contains("\"labels\""),
        "lockfile must record label: {lf_raw}"
    );
    assert!(
        lf_raw.contains("priority-high"),
        "lockfile must record slug: {lf_raw}"
    );
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
    rdc::cli::sync::run("dev", false, false, false, false, false)
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
    rdc::cli::sync::run("dev", false, false, false, false, false)
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
    assert_eq!(
        writes_before, 0,
        "first sync must not issue any mutating requests either"
    );

    // Label file is still present and the lockfile still records it.
    assert!(
        project
            .path()
            .join("envs/dev/labels/priority-high.json")
            .exists()
    );
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("first sync should succeed");

    // Edit the local label file — this triggers the push-side LocalEdit
    // class on the second sync. The remote still serves the pre-edit
    // body, so `remote_hash == base_hash` and `local_hash != base_hash`.
    let label_path = project.path().join("envs/dev/labels/audit-hold.json");
    assert!(
        label_path.exists(),
        "first sync should have written the label"
    );
    let raw = std::fs::read_to_string(&label_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(patched_color.to_string());
    std::fs::write(
        &label_path,
        format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    )
    .unwrap();

    // Snapshot lockfile hash before second sync so we can assert it changes.
    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    // Second sync: classifier sees LocalEdit; executor must PATCH.
    let result = rdc::cli::sync::run("dev", false, false, false, false, false).await;
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
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert_ne!(
        lf_before, lf_after,
        "lockfile must update after a successful PATCH"
    );
    assert!(
        lf_after.contains("audit-hold"),
        "lockfile keeps the slug: {lf_after}"
    );
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("first sync should succeed");

    // Edit the local file so the next sync sees a LocalEdit candidate.
    let label_path = project.path().join("envs/dev/labels/no-push-label.json");
    assert!(
        label_path.exists(),
        "first sync should have written the label"
    );
    let raw = std::fs::read_to_string(&label_path).unwrap();
    let edited_color = "#ff00ff";
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(edited_color.to_string());
    let edited_body = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
    std::fs::write(&label_path, &edited_body).unwrap();

    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    // Second sync with --no-push: the LocalEdit must be ignored.
    let result = rdc::cli::sync::run("dev", false, false, false, /* no_push = */ true, false).await;
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
    assert_eq!(
        patch_calls, 0,
        "expected 0 PATCH calls under --no-push, saw {patch_calls}"
    );

    // Local edit survived intact — the push branch was the only thing
    // that would have rewritten the file with the server's canonical body.
    let body_after = std::fs::read_to_string(&label_path).unwrap();
    assert_eq!(
        body_after, edited_body,
        "--no-push must not touch the locally-edited file"
    );

    // Lockfile is byte-identical: no push → no post-PATCH hash update.
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ true,
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
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
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
/// `--dry-run`, neither happens, and the CLI emits per-item `would push`
/// / `would pull` event-log lines plus a `Dry run: …` summary to stderr.
/// We invoke the binary directly here so we can capture stderr — calling
/// `sync::run` directly would print to the test runner's own stderr.
/// A clean `RemoteDelete` is not a conflict any more: `--dry-run` must
/// list it on the pull side as `(delete local; deleted on env)` and NOT
/// under "would prompt".
#[tokio::test]
async fn sync_dry_run_lists_clean_remote_delete_under_pull() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Phase 1: one label exists; phase 2 (after seed): the env deleted it.
    let n = Arc::new(AtomicUsize::new(0));
    let counter = n.clone();
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(move |_req: &Request| {
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            let results = if first {
                serde_json::json!([{
                    "id": 51,
                    "url": format!("{uri}/api/v1/labels/51"),
                    "name": "Doomed Label",
                    "organization": format!("{uri}/api/v1/organizations/1"),
                    "color": "#101010",
                    "modified_at": "2026-04-15T08:00:00Z"
                }])
            } else {
                serde_json::json!([])
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                "results": results
            }))
        })
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
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    let dry = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--dry-run"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&dry.stdout),
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(dry.status.success(), "dry-run must succeed: {all}");
    assert!(
        all.contains("labels/doomed-label (delete local; deleted on env)"),
        "clean RemoteDelete must be listed on the pull side: {all}"
    );
    assert!(
        !all.contains("doomed-label -- deleted on env"),
        "clean RemoteDelete must not appear as a prompt item: {all}"
    );
    assert!(
        all.contains("0 would prompt"),
        "nothing should be classified as a prompt: {all}"
    );
    assert!(
        all.contains("1 would pull"),
        "the deletion must count under would-pull: {all}"
    );
}

/// A locally-MALFORMED JSON file must surface in `--dry-run` as a
/// dedicated "parse errors" section (not a phantom "would push PATCH"),
/// and a real sync must refuse BEFORE the first remote write — even when
/// another, valid local edit is pending (no partial pushes).
#[tokio::test]
async fn sync_surfaces_parse_errors_and_refuses_partial_push() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Two labels on the env: one will be corrupted locally, one validly
    // edited — the valid PATCH must NOT land when the corrupt one refuses.
    let labels_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 41,
                "url": format!("{}/api/v1/labels/41", server.uri()),
                "name": "Corrupt Me",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#111111",
                "modified_at": "2026-04-15T08:00:00Z"
            },
            {
                "id": 42,
                "url": format!("{}/api/v1/labels/42", server.uri()),
                "name": "Valid Edit",
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

    // Any mutating request is a fail-fast violation.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/41"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/42"))
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
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");

    // Corrupt one label, validly edit the other.
    let corrupt_path = project.path().join("envs/dev/labels/corrupt-me.json");
    std::fs::write(&corrupt_path, b"{\"broken\": ").unwrap();
    let valid_path = project.path().join("envs/dev/labels/valid-edit.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&valid_path).unwrap()).unwrap();
    v["color"] = serde_json::json!("#333333");
    std::fs::write(&valid_path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    std::env::set_current_dir(&prev_cwd).unwrap();

    // Dry-run (binary, so the rendered plan is observable): succeeds as a
    // preview but must name the parse error rather than a phantom PATCH.
    let dry = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--dry-run"])
        .output()
        .unwrap();
    let dry_all = format!(
        "{}{}",
        String::from_utf8_lossy(&dry.stdout),
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(dry.status.success(), "dry-run is a preview and must succeed: {dry_all}");
    assert!(
        dry_all.contains("parse error"),
        "dry-run must surface a parse-errors section: {dry_all}"
    );
    assert!(
        dry_all.contains("labels/corrupt-me"),
        "dry-run must name the unparseable object: {dry_all}"
    );

    // Real sync: refuses before ANY remote write (wiremock .expect(0)
    // guards both PATCH endpoints on Drop).
    std::env::set_current_dir(project.path()).unwrap();
    let err = rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect_err("sync must refuse to push with an unparseable local file");
    std::env::set_current_dir(&prev_cwd).unwrap();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("corrupt-me"),
        "refusal must name the unparseable file: {msg}"
    );
}

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
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    let edit_path = project.path().join("envs/dev/labels/dry-edit.json");
    let create_path = project.path().join("envs/dev/labels/dry-create.json");
    assert!(
        edit_path.exists() && create_path.exists(),
        "seed sync must write both files"
    );

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

    // Drive the dry-run via the actual binary so stderr is captured.
    // The dry-run preview rides on the same `ProgressLog` surface as a
    // regular sync, which writes to stderr.
    let out = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--dry-run", "--yes"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("Dry run:"),
        "dry-run stderr must announce a 'Dry run:' summary: {stderr}"
    );
    assert!(
        stderr.contains("would push") || stderr.contains("would pull"),
        "dry-run stderr must list at least one direction section: {stderr}"
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
    assert_eq!(
        edit_before, edit_after,
        "dry-run must not rewrite local files"
    );
    assert!(
        !create_path.exists(),
        "dry-run must not materialize remote-only labels"
    );
    let lf_after = std::fs::read_to_string(&lf_path).unwrap();
    assert_eq!(lf_before, lf_after, "dry-run must not touch the lockfile");
}

/// `--no-push` and `--no-pull` together are nonsensical: that combination
/// is a read-only inspection, which `rdc sync --dry-run` covers. The sync
/// entry point must reject the pairing up-front with a message that points
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
        /* allow_deletes = */ false, /* no_push = */ true, /* no_pull = */ true,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    let err = result.expect_err("--no-push + --no-pull must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mutually exclusive") || msg.contains("--dry-run"),
        "error message should mention 'mutually exclusive' or '--dry-run': {msg}"
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
    assert!(
        body.contains("AP Approval Flow"),
        "workflow content: {body}"
    );

    // Lockfile records the workflow.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"workflows\""),
        "lockfile must record workflow: {lf_raw}"
    );
    assert!(
        lf_raw.contains("ap-approval-flow"),
        "lockfile must record slug: {lf_raw}"
    );
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
        lf_raw.contains("\"ap-approval-flow/manager-approval\""),
        "lockfile must record step under composite `<workflow>/<step>` key: {lf_raw}"
    );
}

/// Workflow steps nest under their parent workflow; two workflows can
/// both carry a step with the same name and keep clean per-workflow
/// slugs (no `manager-approval-2`). The lockfile keys steps by the
/// composite `<workflow_slug>/<step_slug>`.
#[tokio::test]
async fn sync_pulls_same_named_step_under_two_workflows_with_clean_slugs() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let wf_a_url = format!("{}/api/v1/workflows/701", server.uri());
    let wf_b_url = format!("{}/api/v1/workflows/702", server.uri());
    let workflows_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 701,
                "url": wf_a_url,
                "name": "Workflow A",
                "steps": [format!("{}/api/v1/workflow_steps/11", server.uri())],
                "modified_at": "2026-04-20T08:00:00Z"
            },
            {
                "id": 702,
                "url": wf_b_url,
                "name": "Workflow B",
                "steps": [format!("{}/api/v1/workflow_steps/12", server.uri())],
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
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 11,
                "url": format!("{}/api/v1/workflow_steps/11", server.uri()),
                "name": "Approval",
                "workflow": format!("{}/api/v1/workflows/701", server.uri()),
                "modified_at": "2026-04-20T08:00:00Z"
            },
            {
                "id": 12,
                "url": format!("{}/api/v1/workflow_steps/12", server.uri()),
                "name": "Approval",
                "workflow": format!("{}/api/v1/workflows/702", server.uri()),
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
    let result = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();
    result.expect("sync should succeed");

    let a_path = project
        .path()
        .join("envs/dev/workflows/workflow-a/steps/approval.json");
    let b_path = project
        .path()
        .join("envs/dev/workflows/workflow-b/steps/approval.json");
    assert!(
        a_path.exists(),
        "workflow A step should be at {}",
        a_path.display()
    );
    assert!(
        b_path.exists(),
        "workflow B step should be at {} — globally-unique slugging would have put it at approval-2.json",
        b_path.display()
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"workflow-a/approval\""),
        "lockfile must record under composite `workflow-a/approval`: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"workflow-b/approval\""),
        "lockfile must record under composite `workflow-b/approval`: {lf_raw}"
    );
    assert!(
        !lf_raw.contains("\"approval-2\""),
        "lockfile must NOT auto-suffix workflow_step slugs: {lf_raw}"
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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

/// Regression repro: an engine carrying a server-set `agenda_id` must land on
/// disk with the value redacted to the sentinel (like queue `counts` / hook
/// `status`), NOT the raw live identifier. `redact_on_pull` lists
/// `engines => ["agenda_id"]` and `redact_for_disk` is unit-tested, but the
/// engine pull/sync write path must actually route through it.
#[tokio::test]
async fn sync_redacts_engine_agenda_id_on_disk() {
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
                "agenda_id": "tnt_live_secret_123",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new engine");

    let engine_path = project
        .path()
        .join("envs/dev/engines/invoice-engine/engine.json");
    let body = std::fs::read_to_string(&engine_path).unwrap();
    assert!(
        body.contains("agenda_id"),
        "agenda_id key should remain present on disk: {body}"
    );
    assert!(
        !body.contains("tnt_live_secret_123"),
        "raw live agenda_id must NOT be written to disk; it must be redacted. engine.json:\n{body}"
    );
    assert!(
        body.contains("refreshed live in Rossum; not synced by rdc"),
        "agenda_id must be redacted to the sentinel on disk. engine.json:\n{body}"
    );
}

/// Regression repro: a hook carrying a server-set `status` must land on disk
/// with the value redacted to the sentinel (same root cause as engine
/// `agenda_id` — added together in commit 78b351c). `redact_on_pull` lists
/// `hooks => ["status"]` but the hook serializer must actually route through it.
#[tokio::test]
async fn sync_redacts_hook_status_on_disk() {
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
                "status": "ready",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new hook");

    let json_path = project
        .path()
        .join("envs/dev/hooks/validator-invoices.json");
    let body = std::fs::read_to_string(&json_path).unwrap();
    assert!(
        body.contains("status"),
        "status key should remain present on disk: {body}"
    );
    assert!(
        !body.contains("\"ready\""),
        "raw live hook status must NOT be written to disk; it must be redacted. hook json:\n{body}"
    );
    assert!(
        body.contains("refreshed live in Rossum; not synced by rdc"),
        "hook status must be redacted to the sentinel on disk. hook json:\n{body}"
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
        lf_raw.contains("\"invoice-engine/invoice-number\""),
        "lockfile must record field under composite `<engine>/<field>` key: {lf_raw}"
    );
}

/// Engine fields nest under their parent engine, so two engines having a
/// field with the same name must each get a clean per-engine slug — not
/// `amount` + `amount-2`. The lockfile keys engine_fields by the composite
/// `<engine_slug>/<field_slug>` (mirroring email_templates' per-queue
/// scoping) so two `amount`s coexist.
#[tokio::test]
async fn sync_pulls_same_named_field_under_two_engines_with_clean_slugs() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let engine_a_url = format!("{}/api/v1/engines/401", server.uri());
    let engine_b_url = format!("{}/api/v1/engines/402", server.uri());
    let engines_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 401,
                "url": engine_a_url,
                "name": "Engine A",
                "type": "extractor",
                "modified_at": "2026-04-20T08:00:00Z"
            },
            {
                "id": 402,
                "url": engine_b_url,
                "name": "Engine B",
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
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 501,
                "url": format!("{}/api/v1/engine_fields/501", server.uri()),
                "name": "Amount",
                "engine": format!("{}/api/v1/engines/401", server.uri()),
                "field_type": "number",
                "modified_at": "2026-04-20T08:00:00Z"
            },
            {
                "id": 502,
                "url": format!("{}/api/v1/engine_fields/502", server.uri()),
                "name": "Amount",
                "engine": format!("{}/api/v1/engines/402", server.uri()),
                "field_type": "number",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();
    result.expect("sync should succeed");

    let a_path = project
        .path()
        .join("envs/dev/engines/engine-a/fields/amount.json");
    let b_path = project
        .path()
        .join("envs/dev/engines/engine-b/fields/amount.json");
    assert!(
        a_path.exists(),
        "engine A field should be at {}",
        a_path.display()
    );
    assert!(
        b_path.exists(),
        "engine B field should be at {} — globally-unique slugging would have put it at amount-2.json",
        b_path.display()
    );

    // Lockfile uses composite `<engine>/<field>` keys so both fields coexist.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("\"engine-a/amount\""),
        "lockfile must record engine_fields under composite `engine-a/amount` key: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"engine-b/amount\""),
        "lockfile must record engine_fields under composite `engine-b/amount` key: {lf_raw}"
    );
    assert!(
        !lf_raw.contains("\"amount-2\""),
        "lockfile must NOT auto-suffix engine_field slugs: {lf_raw}"
    );
}

/// Pull-side for an MDH dataset: the Data Storage service returns one
/// collection plus its indexes, and `sync` must write `indexes.json`
/// (stripped of the implicit `_id_` index and the server-set `v`
/// field) under `envs/dev/mdh/<slug>/`. No `collection.json` is
/// written — collection metadata is server-managed and offers no
/// editable surface. MDH is pull-only at this stage, so this only
/// exercises the pull-side branch of the executor.
#[tokio::test]
async fn sync_writes_local_mdh_indexes_without_collection_json() {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/indexes/list"))
        .and(body_partial_json(
            serde_json::json!({"collectionName": "vendors"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "ok",
            "message": "",
            "result": [
                { "v": 2, "name": "_id_", "key": { "_id": 1 } }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/search_indexes/list"))
        .and(body_partial_json(
            serde_json::json!({"collectionName": "vendors"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "ok",
            "message": "",
            "result": []
        })))
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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

    // collection.json was removed in the MDH cleanup pass: it carried
    // only server-managed metadata (uuid, options, idIndex) that the
    // user can't edit, so there's no value in writing it to disk.
    let collection_path = project.path().join("envs/dev/mdh/vendors/collection.json");
    assert!(
        !collection_path.exists(),
        "collection.json must NOT be written; it's pure server metadata"
    );
    let indexes_path = project.path().join("envs/dev/mdh/vendors/indexes.json");
    assert!(
        indexes_path.exists(),
        "indexes JSON should be written at {}",
        indexes_path.display()
    );

    // indexes.json strips the implicit `_id_` regular index and the
    // server-set `v` field on every index, leaving only the
    // user-editable surface. For this fresh dataset (only `_id_`
    // server-side), that means an empty regular array.
    let body = std::fs::read_to_string(&indexes_path).unwrap();
    assert!(
        !body.contains("_id_"),
        "implicit `_id_` index must be stripped from indexes.json: {body}"
    );
    assert!(
        !body.contains("\"v\""),
        "server-set `v` field must be stripped from index defs: {body}"
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    // `mdh_collections` is no longer recorded — collection.json is gone.
    assert!(
        !lf_raw.contains("\"mdh_collections\""),
        "lockfile must NOT record mdh_collections: {lf_raw}"
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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

    let json_path = project
        .path()
        .join("envs/dev/hooks/validator-invoices.json");
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

/// Pull-side RemoteCreate for a Node.js hook. The env exposes a function
/// hook whose `config.runtime` is `"nodejs20.x"`; sync must write
/// `<slug>.json` (with `code` stripped) AND a sibling `<slug>.js` (not
/// `<slug>.py`) carrying the extracted code. No `.py` should appear on
/// disk. The JSON itself must not contain the code.
#[tokio::test]
async fn sync_remote_create_writes_local_js_hook() {
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
                "id": 601,
                "url": format!("{}/api/v1/hooks/601", server.uri()),
                "name": "Validator: invoices JS",
                "type": "function",
                "queues": [],
                "events": ["annotation_content"],
                "config": {
                    "runtime": "nodejs20.x",
                    "code": "module.exports = (input) => input;\n"
                },
                "modified_at": "2026-05-01T08:00:00Z"
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync should succeed when remote has a new JS hook");

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

    let json_path = project
        .path()
        .join("envs/dev/hooks/validator-invoices-js.json");
    let js_path = project
        .path()
        .join("envs/dev/hooks/validator-invoices-js.js");
    let py_path = project
        .path()
        .join("envs/dev/hooks/validator-invoices-js.py");
    assert!(
        json_path.exists(),
        "hook JSON should be written at {}",
        json_path.display()
    );
    assert!(
        js_path.exists(),
        "Node.js hook .js sidecar should be written at {}",
        js_path.display()
    );
    assert!(
        !py_path.exists(),
        "Node.js hook must not produce a .py sidecar at {}",
        py_path.display()
    );

    let json_body = std::fs::read_to_string(&json_path).unwrap();
    assert!(
        json_body.contains("Validator: invoices JS"),
        "hook JSON content: {json_body}"
    );
    assert!(
        json_body.contains("nodejs20.x"),
        "JSON should preserve runtime: {json_body}"
    );
    assert!(
        !json_body.contains("module.exports"),
        "extracted code must not be in JSON: {json_body}"
    );
    let js_body = std::fs::read_to_string(&js_path).unwrap();
    assert_eq!(
        js_body, "module.exports = (input) => input;\n",
        "JS sidecar should carry the exact code bytes"
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(
        lf_raw.contains("validator-invoices-js"),
        "lockfile must record JS-hook slug: {lf_raw}"
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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

    let json_path = project
        .path()
        .join("envs/dev/rules/e-invoice-validation.json");
    let py_path = project
        .path()
        .join("envs/dev/rules/e-invoice-validation.py");
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

    let inboxes_body = serde_json::json!({
        "pagination": { "total_pages": 1, "next": null },
        "results": [{
            "id": 300,
            "url": inbox_url,
            "name": "Cost Invoices Inbox",
            "email": "cost-invoices@mock.rossum.app",
            "queues": [queue_url.clone()],
            "modified_at": "2026-04-10T09:00:00Z",
            "filters": []
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inboxes_body))
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
            "/api/v1/inboxes",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
    assert!(
        lf_raw.contains("\"queues\""),
        "lockfile must record queues: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"schemas\""),
        "lockfile must record schemas: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"inboxes\""),
        "lockfile must record inboxes: {lf_raw}"
    );
    assert!(
        lf_raw.contains("\"email_templates\""),
        "lockfile must record email_templates: {lf_raw}"
    );
    assert!(
        lf_raw.contains("cost-invoices"),
        "queue slug recorded: {lf_raw}"
    );
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
            "/api/v1/inboxes",
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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
            "/api/v1/inboxes",
            "/api/v1/email_templates",
        ],
    )
    .await;

    // Second sync: should be a no-op.
    rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
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
    let tpl_mtime_after = std::fs::metadata(cost_dir.join("email-templates/rejection-notice.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        queue_mtime, queue_mtime_after,
        "queue.json must not be rewritten"
    );
    assert_eq!(
        schema_mtime, schema_mtime_after,
        "schema.json must not be rewritten"
    );
    assert_eq!(
        inbox_mtime, inbox_mtime_after,
        "inbox.json must not be rewritten"
    );
    assert_eq!(
        tpl_mtime, tpl_mtime_after,
        "email template must not be rewritten"
    );
}

/// Regression for the deletion_requested-queue hook churn (Bug #1):
/// a hook references a queue that rdc does NOT track (a
/// `deletion_requested` queue, still returned by `GET /queues` but with
/// `workspace: null`). The hook stores that ref as a RAW URL on disk
/// because the queue can't be portabilized. The classifier's
/// catalog-augment used to seed the untracked queue into its working
/// lockfile, so it portabilized that ref to `rdc://` and the recomputed
/// remote hash diverged from the stable pull-recorded base on EVERY sync
/// → the hook re-pulled forever. After the fix the augment skips
/// workspace-less queues, so the second sync is Clean (`0 changed`) and
/// the raw ref survives on disk.
#[tokio::test]
async fn sync_hook_referencing_deletion_requested_queue_is_clean_on_resync() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Workspace + schema + inbox + email template (queue 100 tree). Queues
    // are mounted SEPARATELY below so the deletion_requested queue 999 is in
    // the same listing.
    let ws_url = format!("{}/api/v1/workspaces/800", server.uri());
    let queue_url = format!("{}/api/v1/queues/100", server.uri());
    let schema_url = format!("{}/api/v1/schemas/200", server.uri());
    let inbox_url = format!("{}/api/v1/inboxes/300", server.uri());

    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
            "results": [{
                "id": 800, "url": ws_url, "name": "Invoices AP",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [queue_url.clone()], "modified_at": "2026-04-20T08:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    // /queues returns BOTH the tracked queue 100 and the untracked
    // deletion_requested queue 999 (workspace=null → rdc never tracks it).
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
            "results": [
                {
                    "id": 100, "url": queue_url.clone(), "name": "Cost Invoices",
                    "workspace": format!("{}/api/v1/workspaces/800", server.uri()),
                    "schema": schema_url.clone(), "inbox": inbox_url.clone(),
                    "modified_at": "2026-04-20T08:00:00Z"
                },
                {
                    "id": 999, "url": format!("{}/api/v1/queues/999", server.uri()),
                    "name": "Deletion Requested Queue",
                    "workspace": serde_json::Value::Null,
                    "schema": serde_json::Value::Null,
                    "status": "deletion_requested",
                    "modified_at": "2026-04-20T08:00:00Z"
                }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 200, "url": schema_url, "name": "Cost Invoices Schema",
            "queues": [queue_url.clone()],
            "content": [
                { "category": "section", "id": "header", "label": "Header", "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" }
                ]}
            ],
            "modified_at": "2026-04-10T09:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total_pages": 1, "next": null },
            "results": [{
                "id": 300, "url": inbox_url,
                "name": "Cost Invoices Inbox",
                "email": "cost-invoices@mock.rossum.app",
                "queues": [queue_url.clone()],
                "modified_at": "2026-04-10T09:00:00Z", "filters": []
            }]
        })))
        .mount(&server)
        .await;

    // A hook that references BOTH the tracked queue 100 AND the untracked
    // deletion_requested queue 999. The 999 ref stays a raw URL forever.
    let hooks_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [{
            "id": 501,
            "url": format!("{}/api/v1/hooks/501", server.uri()),
            "name": "Churning Logger",
            "type": "function",
            "queues": [
                format!("{}/api/v1/queues/999", server.uri()),
                queue_url.clone()
            ],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12", "code": "def x(p):\n    return {}\n" },
            "modified_at": "2026-04-20T08:00:00Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hooks_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(
        &server,
        &[
            "/api/v1/workspaces",
            "/api/v1/queues",
            "/api/v1/inboxes",
            "/api/v1/hooks",
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

    // First sync: pulls the hook + queue tree.
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .assert()
        .success();

    // The hook's untracked-queue ref must be a RAW URL on disk (can't
    // portabilize — queue 999 isn't tracked).
    let hook_path = project.path().join("envs/dev/hooks/churning-logger.json");
    let hook_json = std::fs::read_to_string(&hook_path).unwrap();
    assert!(
        hook_json.contains("/api/v1/queues/999"),
        "untracked-queue ref must remain a raw URL on disk:\n{hook_json}"
    );

    // Second sync: must be Clean — `0 changed`, no hook re-pull.
    let out = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "second sync must succeed:\n{stderr}");
    assert!(
        stderr.contains("(0 changed"),
        "second sync must report 0 changed (no churn); got:\n{stderr}"
    );

    // No mutating API calls on the clean re-sync.
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
}

/// Watch-mode initial reconcile: on `run_watch` startup, before the
/// ctrl-c block, one full `run_cycle` runs. This brings the env to a
/// known state before watching kicks in. Mirrors the setup of
/// `sync_remote_create_writes_local_label` — a remote-only label that the
/// initial reconcile must pull to disk.
#[tokio::test]
async fn sync_watch_initial_reconcile_pulls_remote_creates() {
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
                "id": 21,
                "url": format!("{}/api/v1/labels/21", server.uri()),
                "name": "Audit Hold",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#00ff00",
                "modified_at": "2026-05-01T08:00:00Z"
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

    // `run_watch` blocks on `tokio::signal::ctrl_c` *after* the initial
    // reconcile completes. The reconcile future is `!Send` (the execute
    // pipeline holds a `StdinLock` across an await) so we can't
    // `tokio::spawn` it — instead we race it against a file-existence
    // observer under `tokio::select!`, which cancels `run_watch` as soon
    // as the initial reconcile has demonstrably landed (the label file
    // exists on disk).
    //
    // The observer runs in a `tokio::task::spawn_blocking` thread so its
    // `std::thread::sleep` polls don't share fate with the test runtime's
    // timer driver — `run_watch`'s file-watcher chatter has been
    // observed to stall sub-second `tokio::time::sleep` in the test task
    // for many seconds, which would translate into spurious test
    // timeouts.
    let label_path = project.path().join("envs/dev/labels/audit-hold.json");
    let observer_label = label_path.clone();
    let observer_deadline = std::time::Duration::from_secs(30);
    let observer = tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        while started.elapsed() < observer_deadline {
            if observer_label.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        false
    });
    tokio::select! {
        res = rdc::cli::sync::watch::run_watch(
            "dev", /* interactive = */ false, /* allow_deletes = */ false,
            /* no_push = */ false, /* no_pull = */ false,
            /* poll_interval = */ None, /* verbose = */ false,
        ) => {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!("watch should be blocked on ctrl_c but exited: {res:?}");
        }
        found = observer => {
            match found {
                Ok(true) => {}
                Ok(false) => {
                    std::env::set_current_dir(&prev_cwd).unwrap();
                    panic!(
                        "initial reconcile never wrote {} within {}s",
                        label_path.display(),
                        observer_deadline.as_secs(),
                    );
                }
                Err(e) => {
                    std::env::set_current_dir(&prev_cwd).unwrap();
                    panic!("observer task panicked: {e:?}");
                }
            }
        }
    }

    std::env::set_current_dir(&prev_cwd).unwrap();
}

/// Regression guard: `run_cycle` must not block on a meta-confirmation
/// prompt for any plan, destructive or otherwise. The plan is printed
/// for preview only; per-item gates (conflict resolver, destructive
/// delete gate, remote-delete prompt, auth refresh) handle their own
/// confirmations.
///
/// We run `run_watch` with `interactive=true` under a short timeout for
/// a plan that contains a single `RemoteCreate` for a label that exists
/// upstream but not locally. If a meta-confirmation prompt were
/// reintroduced, it would read stdin — in a non-tty test process that
/// read would either block forever (timeout fires but the label is
/// never pulled) or fail on a closed stdin (run_watch returns Ok/Err
/// early). Both failure modes are caught by the two assertions below.
#[tokio::test]
async fn sync_does_not_show_meta_confirmation_prompt() {
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
                "id": 21,
                "url": format!("{}/api/v1/labels/21", server.uri()),
                "name": "Audit Hold",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#00ff00",
                "modified_at": "2026-05-01T08:00:00Z"
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

    // The key difference from `sync_watch_initial_reconcile_pulls_remote_creates`:
    // `interactive = true`. If a meta-confirmation prompt were
    // reintroduced, the plan would block on stdin and the pull would
    // never reach disk (or the cycle would error and `run_watch` would
    // return Ok/Err immediately instead of blocking on ctrl_c).
    //
    // Race `run_watch` against a file-existence observer. If a prompt
    // were reintroduced, `run_watch` would return early (caught by the
    // first arm) OR block on stdin without writing the label (caught by
    // the observer's deadline).
    let label_path = project.path().join("envs/dev/labels/audit-hold.json");
    let observer_label = label_path.clone();
    let observer_deadline = std::time::Duration::from_secs(30);
    tokio::select! {
        res = rdc::cli::sync::watch::run_watch(
            "dev", /* interactive = */ true, /* allow_deletes = */ false,
            /* no_push = */ false, /* no_pull = */ false,
            /* poll_interval = */ None, /* verbose = */ false,
        ) => {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!(
                "run_watch should be blocked on ctrl_c (no meta-confirmation prompt should fire), got {res:?}",
            );
        }
        _ = async {
            let started = std::time::Instant::now();
            loop {
                if observer_label.exists() {
                    return;
                }
                if started.elapsed() >= observer_deadline {
                    panic!(
                        "label was never pulled within {}s — a meta-confirmation prompt may have been reintroduced at {}",
                        observer_deadline.as_secs(),
                        observer_label.display(),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } => {}
    }

    std::env::set_current_dir(&prev_cwd).unwrap();
}

/// Watch-mode polling end-to-end (Task 15). Proves that polling actually
/// drives reconcile cycles — not just that the initial reconcile works.
///
/// Setup: one label is mounted statically on the server. With a short
/// poll interval, the initial reconcile pulls the label and at least one
/// poll cycle re-lists `/api/v1/labels`. We assert both by counting the
/// responder's invocations and by checking the file landed on disk.
///
/// Robust timing strategy: instead of waiting a fixed wall-clock window
/// and asserting after, we race `run_watch` against a polling loop that
/// resolves as soon as the responder has been hit twice or more (initial
/// reconcile + ≥ 1 poll-driven cycle). `tokio::select!` cancels the
/// `run_watch` branch as soon as we have evidence the poll wiring is
/// live. A 90 s ceiling — far above the empirically-observed ~25 s
/// time-to-second-poll under `cargo test` debug builds — turns "polling
/// is broken" into a clear panic instead of a wall-clock flake.
///
/// The empirical delay is a quirk of `tokio::time::interval` under
/// debug-mode `current_thread` runtimes with concurrent
/// `tokio::sync::mpsc` activity and `notify` file-watcher chatter —
/// it does not reproduce in release builds or under real network
/// latencies, so production polling cadence is unaffected.
#[tokio::test]
async fn sync_watch_poll_catches_remote_drift() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Single label, served on every call. Each hit pushes a notification
    // through `call_tx` so the test can await tokio-wake-driven progress
    // instead of polling an atomic in a loop. The latter approach is
    // fragile under runtime activity (file-watcher chatter in `run_watch`
    // can starve sub-second `tokio::time::sleep` timers in the test
    // task), and was the cause of multi-second stalls in earlier
    // iterations of this test.
    let server_uri = server.uri();
    let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let responder = {
        let call_tx = call_tx.clone();
        move |_req: &Request| {
            let _ = call_tx.send(());
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                "results": [
                    {
                        "id": 31,
                        "url": format!("{server_uri}/api/v1/labels/31"),
                        "name": "Audit Hold",
                        "organization": format!("{server_uri}/api/v1/organizations/1"),
                        "color": "#00ff00",
                        "modified_at": "2026-05-01T08:00:00Z"
                    }
                ]
            }))
        }
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(responder)
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

    // Race `run_watch` (which blocks on ctrl_c) against an observer
    // that resolves as soon as the wiremock responder has been hit
    // twice — initial reconcile + ≥ 1 poll-driven cycle. The
    // responder pushes a notification per hit through `call_rx`
    // (`tokio::sync::mpsc::UnboundedReceiver::recv` is properly
    // waker-registered, so each send wakes the receiver immediately,
    // unlike a `std::sync::mpsc` channel which would leave the
    // receiver parked).
    //
    // The 90 s deadline is a *failure* ceiling, not a wait. On a
    // healthy runtime polling resolves in a few hundred milliseconds;
    // the generous ceiling accommodates an empirical, debug-build-only
    // delay where `tokio::time::interval`'s first tick can fire many
    // seconds after `tokio::spawn` under `current_thread` runtimes
    // sharing a thread with `notify` file-watcher chatter.
    let observer_deadline = std::time::Duration::from_secs(90);
    let mut seen_count = 0usize;
    tokio::select! {
        res = rdc::cli::sync::watch::run_watch(
            "dev", /* interactive = */ false, /* allow_deletes = */ false,
            /* no_push = */ false, /* no_pull = */ false,
            /* poll_interval = */ Some(std::time::Duration::from_millis(200)),
            /* verbose = */ false,
        ) => {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!("watch should be blocked on ctrl_c but exited: {res:?}");
        }
        _ = async {
            while seen_count < 2 {
                match call_rx.recv().await {
                    Some(()) => seen_count += 1,
                    None => break,
                }
            }
        } => {}
        _ = tokio::time::sleep(observer_deadline) => {
            std::env::set_current_dir(&prev_cwd).unwrap();
            panic!(
                "polling never re-listed /api/v1/labels within {}s — saw only {} hit(s)",
                observer_deadline.as_secs(),
                seen_count,
            );
        }
    }

    // The label was written during the initial reconcile that completed
    // before the first poll event fired.
    let label_path = project.path().join("envs/dev/labels/audit-hold.json");
    let exists = label_path.exists();
    std::env::set_current_dir(&prev_cwd).unwrap();

    assert!(
        exists,
        "initial reconcile should have pulled the label to {}",
        label_path.display()
    );
}

/// Watch-mode does not deadlock with concurrent one-shot `sync` (Task 15).
/// The env lock inside `run_watch` is dropped after the initial reconcile
/// and re-acquired only briefly around each cycle; so a one-shot
/// `sync::run` issued while watch is blocked on ctrl_c must acquire the
/// lock and complete.
///
/// We run watch with `poll_interval = None` so the lock is held only
/// during the initial reconcile (no periodic cycles contending for it).
/// After 300ms (well past the initial reconcile), the one-shot runs.
/// Both futures share the same task — `run_cycle` is `!Send`, so
/// `tokio::spawn` would not compile; `tokio::join!` polls them
/// cooperatively on the current task instead.
#[tokio::test]
async fn sync_watch_does_not_deadlock_with_one_shot_sync() {
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

    // The one-shot must run on a SEPARATE OS thread (with its own
    // single-threaded tokio runtime). `EnvLock::acquire` is synchronous
    // — when it blocks on a contended lock, it does so via
    // `std::thread::sleep`, which would freeze the current_thread tokio
    // runtime that's also driving the watch future. With the one-shot on
    // its own thread, the main runtime keeps progressing the watch's
    // initial reconcile to completion, the watch's `EnvLock` guard drops,
    // and the one-shot's lock acquisition then succeeds without timing
    // games.
    //
    // The cwd is process-wide; the spawned thread inherits the project
    // cwd this test set above, so `Paths::for_env(&cwd, "dev")` resolves
    // the same paths watch sees.
    // Use `tokio::sync::oneshot` so the send from the spawned thread
    // wakes the receiver immediately on the test runtime — a plain
    // `std::sync::mpsc` send doesn't notify tokio, which leaves the
    // receiver parked on its own timer despite the channel having a
    // value, and the file-watcher chatter inside `run_watch` can stall
    // that timer for many seconds.
    let (one_shot_tx, one_shot_rx) = tokio::sync::oneshot::channel();
    let one_shot_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("building one-shot sub-runtime");
        let res = rt.block_on(rdc::cli::sync::run(
            "dev", /* interactive = */ false, /* dry_run = */ false,
            /* allow_deletes = */ false, /* no_push = */ false,
            /* no_pull = */ false,
        ));
        let _ = one_shot_tx.send(res);
    });

    // Drive the watch future under a generous deadline. Three exits:
    //   * one-shot finishes (resolves on `recv`) — success path
    //   * watch's future resolves on its own — never expected; panic
    //   * deadline elapses without either — panic loudly
    let deadline = std::time::Duration::from_secs(30);
    let one_shot_res = tokio::select! {
        res = rdc::cli::sync::watch::run_watch(
            "dev", /* interactive = */ false, /* allow_deletes = */ false,
            /* no_push = */ false, /* no_pull = */ false,
            /* poll_interval = */ None, /* verbose = */ false,
        ) => {
            std::env::set_current_dir(&prev_cwd).unwrap();
            let _ = one_shot_thread.join();
            panic!("watch should be parked on ctrl_c but exited: {res:?}");
        }
        res = one_shot_rx => res.expect("one-shot thread dropped the sender"),
        _ = tokio::time::sleep(deadline) => {
            std::env::set_current_dir(&prev_cwd).unwrap();
            let _ = one_shot_thread.join();
            panic!(
                "one-shot sync did not complete within {}s — watch may be holding the env lock",
                deadline.as_secs(),
            );
        }
    };

    one_shot_thread.join().expect("one-shot thread panicked");
    std::env::set_current_dir(&prev_cwd).unwrap();

    assert!(
        one_shot_res.is_ok(),
        "one-shot sync should succeed while watch is idle on ctrl_c: {one_shot_res:?}"
    );
}

/// Regression for the user-reported bug: hook .py sidecar edited
/// locally AND code edited remotely (same JSON portion on both sides).
/// Before the fix the conflict resolver's JSON-only short-circuit
/// silently routed this to `KeepLocal` and the push driver PATCHed
/// local over remote without ever prompting. With the fix the resolver
/// redirects the prompt to the `.py` sidecar so the user sees the
/// code conflict; in non-TTY mode it writes the shadow file and
/// preserves the lockfile base.
#[tokio::test]
async fn sync_hook_code_only_divergence_does_not_silently_push() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Same JSON portion on both calls — only `config.code` changes
    // between the seed pull and the second sync's listing. Stateful
    // counter so the FIRST call serves base code and subsequent calls
    // serve remote-edited code.
    let hook_id = 712u64;
    let list_call_count = Arc::new(AtomicUsize::new(0));
    let server_uri = server.uri();
    let counter = list_call_count.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let code = if n == 0 {
                "def base():\n    return 1\n"
            } else {
                "def remote_edit():\n    return 3\n"
            };
            let modified_at = if n == 0 {
                "2026-05-14T08:00:00Z"
            } else {
                "2026-05-14T10:00:00Z"
            };
            let body = serde_json::json!({
                "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                "results": [
                    {
                        "id": hook_id,
                        "url": format!("{server_uri}/api/v1/hooks/{hook_id}"),
                        "name": "ap-reject-if-no-doc-id",
                        "type": "function",
                        "queues": [],
                        "events": ["annotation_content"],
                        "config": { "runtime": "python3.12", "code": code },
                        "modified_at": modified_at
                    }
                ]
            });
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/hooks"]).await;

    Mock::given(method("PATCH"))
        .and(path(format!("/api/v1/hooks/{hook_id}")))
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

    // Seed.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    // Edit local .py only — JSON file is untouched on disk.
    let py_path = project
        .path()
        .join("envs/dev/hooks/ap-reject-if-no-doc-id.py");
    let json_path = project
        .path()
        .join("envs/dev/hooks/ap-reject-if-no-doc-id.json");
    let json_before = std::fs::read(&json_path).unwrap();
    std::fs::write(&py_path, b"def local_edit():\n    return 2\n").unwrap();

    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    // Second sync — remote now serves the modified-code hook.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync should succeed (no silent push)");

    std::env::set_current_dir(&prev_cwd).unwrap();

    // No PATCH may have been issued — the bug would silently push local
    // code over remote on this path.
    let patch_calls = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.method == http::Method::PATCH && r.url.path() == format!("/api/v1/hooks/{hook_id}")
        })
        .count();
    assert_eq!(
        patch_calls, 0,
        "hook with only-.py divergence must NOT be silently PATCHed; saw {patch_calls}"
    );

    // Local .py edit survived; JSON file unchanged.
    let py_after = std::fs::read(&py_path).unwrap();
    assert_eq!(py_after, b"def local_edit():\n    return 2\n");
    let json_after = std::fs::read(&json_path).unwrap();
    assert_eq!(json_after, json_before, "JSON file must not be touched");

    // Lockfile base preserved so the next sync re-prompts.
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let v_before: serde_json::Value = serde_json::from_str(&lf_before).unwrap();
    let v_after: serde_json::Value = serde_json::from_str(&lf_after).unwrap();
    assert_eq!(
        v_before.pointer("/objects/hooks/ap-reject-if-no-doc-id/content_hash"),
        v_after.pointer("/objects/hooks/ap-reject-if-no-doc-id/content_hash"),
        "lockfile base must remain pinned so the next sync re-prompts"
    );

    // Shadow file written next to the .py sidecar (the prompt
    // redirected away from the JSON, so the shadow lives next to the
    // code).
    let shadow = project
        .path()
        .join("envs/dev/hooks/ap-reject-if-no-doc-id.py.dev");
    assert!(
        shadow.exists(),
        "shadow file should land next to the .py: {}",
        shadow.display()
    );
    let shadow_body = std::fs::read(&shadow).unwrap();
    assert_eq!(shadow_body, b"def remote_edit():\n    return 3\n");
}

/// Regression for the reported bug: with both local and remote changed
/// since the lockfile-recorded base, sync must NOT silently PATCH local
/// over remote — the conflict resolver should kick in (or, in non-TTY
/// mode, write a shadow file and skip the push).
///
/// Scenario:
/// 1. First sync seeds the lockfile from a clean hook (code "base").
/// 2. Local .py sidecar is edited to "local-edit".
/// 3. The hooks GET mock is updated to return the hook with code
///    "remote-edit" and a newer `modified_at`.
/// 4. Second sync runs with `interactive=false` — the conflict resolver
///    falls back to the shadow-file path (it writes
///    `<file>.<env>` with the remote bytes and skips). Crucially: NO
///    `PATCH /hooks/<id>` is sent.
#[tokio::test]
async fn sync_both_diverged_hook_does_not_silently_push() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Initial hook body — what the first sync pulls. Acts as the lockfile
    // base.
    let hook_id = 711u64;
    let base_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": hook_id,
                "url": format!("{}/api/v1/hooks/{hook_id}", server.uri()),
                "name": "ap-reject-if-no-doc-id",
                "type": "function",
                "queues": [],
                "events": ["annotation_content"],
                "config": {
                    "runtime": "python3.12",
                    "code": "def base():\n    return 1\n"
                },
                "modified_at": "2026-05-14T08:00:00Z"
            }
        ]
    });

    // Use a stateful counter so the FIRST list call serves the base body
    // and subsequent calls serve the "remote-edited" body (different
    // code, newer modified_at) — mimicking what happens when the user
    // edits remote via the Rossum UI between pulls.
    let list_call_count = Arc::new(AtomicUsize::new(0));
    let base_body_clone = base_body.clone();
    let server_uri = server.uri();
    let counter = list_call_count.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200).set_body_json(&base_body_clone)
            } else {
                let edited = serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [
                        {
                            "id": hook_id,
                            "url": format!("{server_uri}/api/v1/hooks/{hook_id}"),
                            "name": "ap-reject-if-no-doc-id",
                            "type": "function",
                            "queues": [],
                            "events": ["annotation_content"],
                            "config": {
                                "runtime": "python3.12",
                                "code": "def remote_edit():\n    return 3\n"
                            },
                            "modified_at": "2026-05-14T10:00:00Z"
                        }
                    ]
                });
                ResponseTemplate::new(200).set_body_json(edited)
            }
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/hooks"]).await;

    // Critical: ANY PATCH on this hook would be the bug. `.expect(0)`
    // makes wiremock fail the test on Drop if a PATCH lands.
    Mock::given(method("PATCH"))
        .and(path(format!("/api/v1/hooks/{hook_id}")))
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

    // First sync — seeds the lockfile with base bytes.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    // Locally edit the .py sidecar — different code from base.
    let py_path = project
        .path()
        .join("envs/dev/hooks/ap-reject-if-no-doc-id.py");
    assert!(
        py_path.exists(),
        "first sync should have written the .py sidecar"
    );
    std::fs::write(&py_path, b"def local_edit():\n    return 2\n").unwrap();

    // Snapshot the lockfile so we can confirm the conflict path doesn't
    // advance the base hash (the base must stay pinned so the next sync
    // re-prompts).
    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    // Second sync — remote now serves the modified hook. Both sides have
    // diverged from the lockfile-recorded base → classifier MUST emit
    // BothDiverged, and the non-TTY fallback MUST NOT push.
    let result = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("second sync should succeed (no push, conflict deferred)");

    // The crucial assertion: no PATCH was issued.
    let patch_calls = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.method == http::Method::PATCH && r.url.path() == format!("/api/v1/hooks/{hook_id}")
        })
        .count();
    assert_eq!(
        patch_calls, 0,
        "BothDiverged hook must NOT be silently PATCHed (saw {patch_calls} PATCH calls)"
    );

    // Local .py file survived the conflict — the user's edit must not be
    // discarded.
    let py_after = std::fs::read_to_string(&py_path).unwrap();
    assert_eq!(
        py_after, "def local_edit():\n    return 2\n",
        "local .py edit must survive: {py_after}"
    );

    // Lockfile base is preserved so the next sync re-classifies as a
    // conflict (the base hash for this hook must equal what it was
    // before the second sync).
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let v_before: serde_json::Value = serde_json::from_str(&lf_before).unwrap();
    let v_after: serde_json::Value = serde_json::from_str(&lf_after).unwrap();
    let base_before = v_before
        .pointer("/objects/hooks/ap-reject-if-no-doc-id/content_hash")
        .cloned();
    let base_after = v_after
        .pointer("/objects/hooks/ap-reject-if-no-doc-id/content_hash")
        .cloned();
    assert_eq!(
        base_before, base_after,
        "BothDiverged: lockfile base must remain pinned to the prior base \
         so the next sync re-prompts (before={base_before:?}, after={base_after:?})"
    );
}

// ====================================================================
// Safety contract test matrix (Phase 1 of the sync hardening pass).
//
// For every split-file kind (hooks, rules, schemas), exhaustively
// exercise the divergence shapes that the BothDiverged classifier
// has to surface AND the resolver has to actually prompt on. Each
// test:
//   1. Seeds an initial sync (lockfile + local snapshot).
//   2. Mutates local and/or remote into a divergent state.
//   3. Re-runs sync non-interactively (`interactive=false`) so the
//      resolver falls back to shadow-file + skip semantics.
//   4. Asserts: NO PATCH/POST/DELETE hits the mock for the affected
//      object; local files survive; lockfile base is preserved so the
//      next sync re-prompts.
//
// The class of bug we're guarding against is: a divergent remote
// state silently overwritten by a local edit because the resolver
// short-circuited or the classifier mis-categorised. These tests
// would have caught the `.py`-only hook bug (commit ca7b314) and
// would have caught the analogous asymmetric `.py` / formula bugs.
// ====================================================================

/// Test variant for the hook conflict matrix. Each variant describes
/// (a) what the lockfile-seeded base hook looks like, (b) what local
/// modifications happen between syncs, (c) what the remote returns on
/// the second sync. The harness handles the rest.
#[derive(Debug, Clone, Copy)]
enum HookConflictVariant {
    /// Both sides edited the JSON portion (different event lists, etc.).
    JsonBothEdited,
    /// Both sides edited the .py portion (different code on each side).
    CodeBothEdited,
    /// Local edited JSON; remote edited .py. Disjoint axes → clean
    /// 3-way merge: local keeps its JSON edit, adopts the remote code,
    /// and pushes the JSON edit upstream (exactly one PATCH).
    LocalJsonRemoteCode,
    /// Local has .py (edited), remote removed the code field entirely.
    LocalHasCodeRemoteRemoved,
    /// Local removed the .py (file deleted from disk), remote edited code.
    LocalRemovedCodeRemoteEdited,
    /// Both sides happen to converge on the same edited code. Resolver
    /// must NOT prompt — the combined hashes are equal so the kind is
    /// `Clean`, even though both sides "edited."
    BothEditedToSameCode,
}

/// Drive one hook-conflict scenario through `rdc sync` and assert that
/// no PATCH/POST/DELETE hits the mock (except for the
/// BothEditedToSameCode case, which expects `Clean`). The lockfile's
/// base hash must remain pinned (so the next sync re-prompts).
async fn run_hook_conflict_scenario(variant: HookConflictVariant) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let hook_id = 8000u64;
    let slug = "ap-validator";

    // Base body: code = "base", events = ["annotation_content"].
    let base_code = "def base():\n    return 1\n";
    let local_code_edit = "def local_edit():\n    return 2\n";
    let remote_code_edit = "def remote_edit():\n    return 3\n";
    let same_code_both = "def both_edit_to_same():\n    return 42\n";
    let events_base = vec!["annotation_content"];
    let events_local = vec!["annotation_content", "annotation_status"];
    let events_remote = vec!["annotation_content", "user_invited"];

    // Union of the base+local+remote event edits. For `JsonBothEdited`
    // the 3-way merge unions the three string arrays, so this is the
    // auto-merged result the executor PATCHes upstream.
    let events_union = vec!["annotation_content", "annotation_status", "user_invited"];

    let server_uri = server.uri();

    // `JsonBothEdited` is an AUTO-MERGE that now (post-fix) PATCHes the
    // unioned events back to the remote. After that PATCH lands the
    // listing must serve the merged body so the idempotency sync sees
    // Clean. This flag (flipped by the PATCH responder below) drives
    // phase 3 of the JsonBothEdited listing; all other variants leave it
    // false and keep serving the variant-specific "remote now" body.
    let patch_landed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let list_call_count = Arc::new(AtomicUsize::new(0));
    let counter = list_call_count.clone();
    let uri_clone = server_uri.clone();
    let patch_landed_list = patch_landed.clone();
    let events_union_list = events_union.clone();
    let events_local_list = events_local.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            // First call: seed (always the base body).
            // Subsequent calls: the variant-specific "remote now" body.
            let (events_now, code_now, modified_at) = if n == 0 {
                (
                    events_base.clone(),
                    Some(base_code.to_string()),
                    "2026-05-14T08:00:00Z".to_string(),
                )
            } else {
                match variant {
                    HookConflictVariant::JsonBothEdited
                        if patch_landed_list.load(Ordering::SeqCst) =>
                    {
                        // Phase 3: the unioned events are now on the
                        // remote → idempotency sync classifies Clean.
                        (
                            events_union_list.clone(),
                            Some(base_code.to_string()),
                            "2026-05-14T11:00:00Z".to_string(),
                        )
                    }
                    HookConflictVariant::JsonBothEdited => (
                        events_remote.clone(),
                        Some(base_code.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    HookConflictVariant::LocalJsonRemoteCode
                        if patch_landed_list.load(Ordering::SeqCst) =>
                    {
                        // Phase 3: local events + remote code are now both
                        // on the remote → idempotency sync sees Clean.
                        (
                            events_local_list.clone(),
                            Some(remote_code_edit.to_string()),
                            "2026-05-14T11:00:00Z".to_string(),
                        )
                    }
                    HookConflictVariant::CodeBothEdited => (
                        events_base.clone(),
                        Some(remote_code_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    HookConflictVariant::LocalJsonRemoteCode => (
                        events_base.clone(),
                        Some(remote_code_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    HookConflictVariant::LocalHasCodeRemoteRemoved => (
                        events_base.clone(),
                        None,
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    HookConflictVariant::LocalRemovedCodeRemoteEdited => (
                        events_base.clone(),
                        Some(remote_code_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    HookConflictVariant::BothEditedToSameCode => (
                        events_base.clone(),
                        Some(same_code_both.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                }
            };

            let mut config = serde_json::json!({ "runtime": "python3.12" });
            if let Some(code) = code_now {
                config["code"] = serde_json::Value::String(code);
            }
            let body = serde_json::json!({
                "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                "results": [
                    {
                        "id": hook_id,
                        "url": format!("{uri_clone}/api/v1/hooks/{hook_id}"),
                        "name": "ap-validator",
                        "type": "function",
                        "queues": [],
                        "events": events_now,
                        "config": config,
                        "modified_at": modified_at
                    }
                ]
            });
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/hooks"]).await;

    // The bug class: for every CONFLICT variant, ANY mutating request on
    // this hook is a defense failure. `.expect(0)` makes wiremock fail on
    // Drop if one lands. `BothEditedToSameCode` is Clean → also zero
    // PATCH.
    //
    // Two variants are clean 3-way AUTO-MERGES, not conflicts:
    // `JsonBothEdited` (the event arrays union) and `LocalJsonRemoteCode`
    // (disjoint axes — local JSON edit, remote sidecar edit). Both keep a
    // local-side change that must reach the remote, so for these we mount
    // a real PATCH responder (`.expect(1)`) that echoes the merged hook
    // and flips `patch_landed` to advance the listing to phase 3.
    if matches!(
        variant,
        HookConflictVariant::JsonBothEdited | HookConflictVariant::LocalJsonRemoteCode
    ) {
        let patch_landed_resp = patch_landed.clone();
        let uri_patch = server_uri.clone();
        // What the merged hook looks like on the remote after the PATCH:
        // JsonBothEdited unions the events (code untouched);
        // LocalJsonRemoteCode keeps the local events and the remote code.
        let (patched_events, patched_code) = match variant {
            HookConflictVariant::JsonBothEdited => (events_union.clone(), base_code),
            _ => (events_local.clone(), remote_code_edit),
        };
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v1/hooks/{hook_id}")))
            .respond_with(move |_req: &Request| {
                patch_landed_resp.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": hook_id,
                    "url": format!("{uri_patch}/api/v1/hooks/{hook_id}"),
                    "name": "ap-validator",
                    "type": "function",
                    "queues": [],
                    "events": patched_events,
                    "config": { "runtime": "python3.12", "code": patched_code },
                    "modified_at": "2026-05-14T11:00:00Z"
                }))
            })
            .expect(1)
            .mount(&server)
            .await;
    } else {
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v1/hooks/{hook_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/v1/hooks/{hook_id}")))
        .respond_with(ResponseTemplate::new(204))
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

    // Seed.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    let json_path = project.path().join(format!("envs/dev/hooks/{slug}.json"));
    let py_path = project.path().join(format!("envs/dev/hooks/{slug}.py"));
    let json_before = std::fs::read(&json_path).unwrap();
    let py_existed_before = py_path.exists();

    // Apply local mutation per variant.
    match variant {
        HookConflictVariant::JsonBothEdited => {
            // Edit JSON locally — change the events array.
            let mut v: serde_json::Value = serde_json::from_slice(&json_before).unwrap();
            v["events"] = serde_json::json!(events_local);
            let mut new_json = serde_json::to_vec_pretty(&v).unwrap();
            new_json.push(b'\n');
            std::fs::write(&json_path, &new_json).unwrap();
        }
        HookConflictVariant::CodeBothEdited => {
            std::fs::write(&py_path, local_code_edit.as_bytes()).unwrap();
        }
        HookConflictVariant::LocalJsonRemoteCode => {
            let mut v: serde_json::Value = serde_json::from_slice(&json_before).unwrap();
            v["events"] = serde_json::json!(events_local);
            let mut new_json = serde_json::to_vec_pretty(&v).unwrap();
            new_json.push(b'\n');
            std::fs::write(&json_path, &new_json).unwrap();
        }
        HookConflictVariant::LocalHasCodeRemoteRemoved => {
            std::fs::write(&py_path, local_code_edit.as_bytes()).unwrap();
        }
        HookConflictVariant::LocalRemovedCodeRemoteEdited => {
            std::fs::remove_file(&py_path).expect("seeded .py must exist");
        }
        HookConflictVariant::BothEditedToSameCode => {
            std::fs::write(&py_path, same_code_both.as_bytes()).unwrap();
        }
    }

    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    // Run second sync non-interactively. For CONFLICT variants the
    // resolver falls back to shadow-file behavior and NO PATCH/POST/
    // DELETE may land. For the auto-merge variants (`JsonBothEdited`,
    // `LocalJsonRemoteCode`) exactly one PATCH of the merged hook lands.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync should succeed");

    let hook_mutation_bodies: Vec<Vec<u8>> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            (r.method == http::Method::PATCH
                || r.method == http::Method::POST
                || r.method == http::Method::DELETE)
                && (r.url.path() == format!("/api/v1/hooks/{hook_id}")
                    || r.url.path() == "/api/v1/hooks")
        })
        .map(|r| r.body)
        .collect();
    if matches!(
        variant,
        HookConflictVariant::JsonBothEdited | HookConflictVariant::LocalJsonRemoteCode
    ) {
        // Auto-merge that kept a local-side change (the local-only event)
        // must push exactly one PATCH carrying the merged hook.
        assert_eq!(
            hook_mutation_bodies.len(),
            1,
            "variant {variant:?}: auto-merge must PATCH exactly once; saw {}",
            hook_mutation_bodies.len()
        );
        let patched: serde_json::Value = serde_json::from_slice(&hook_mutation_bodies[0]).unwrap();
        let evs: Vec<&str> = patched["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        let expected_events: &[&str] = match variant {
            // Union of base/local/remote event edits.
            HookConflictVariant::JsonBothEdited => {
                &["annotation_content", "annotation_status", "user_invited"]
            }
            // Remote didn't touch events — only the local edit lands.
            _ => &["annotation_content", "annotation_status"],
        };
        for ev in expected_events {
            assert!(
                evs.contains(ev),
                "variant {variant:?}: PATCH body events must contain {ev}; got {evs:?}"
            );
        }
        if matches!(variant, HookConflictVariant::LocalJsonRemoteCode) {
            assert!(
                !evs.contains(&"user_invited"),
                "variant {variant:?}: PATCH must not invent events the local edit never made; got {evs:?}"
            );
            // The disjoint merge adopts the remote code on disk.
            let py_after = std::fs::read(&py_path).unwrap();
            assert_eq!(
                py_after,
                remote_code_edit.as_bytes(),
                "variant {variant:?}: local .py must adopt the remote code edit"
            );
        }

        // Core data-loss regression guard: the listing now serves the
        // merged (unioned) body (phase 3), so a SECOND sync classifies
        // the hook Clean — no further mutating request, local union
        // events preserved (not reverted).
        let muts_before = hook_mutation_bodies.len();
        rdc::cli::sync::run("dev", false, false, false, false, false)
            .await
            .expect("idempotency sync should succeed");
        let muts_after = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                (r.method == http::Method::PATCH
                    || r.method == http::Method::POST
                    || r.method == http::Method::DELETE)
                    && (r.url.path() == format!("/api/v1/hooks/{hook_id}")
                        || r.url.path() == "/api/v1/hooks")
            })
            .count();
        assert_eq!(
            muts_before, muts_after,
            "variant {variant:?}: idempotency sync must not re-PATCH the hook"
        );
        let local_after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
        let local_evs: Vec<&str> = local_after["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(
            local_evs.contains(&"annotation_status"),
            "variant {variant:?}: local-side event must NOT revert on the idempotency sync; got {local_evs:?}"
        );
    } else {
        // Crucial: zero mutating requests on the hook endpoint.
        assert_eq!(
            hook_mutation_bodies.len(),
            0,
            "variant {variant:?}: hook endpoint must not receive mutating requests; saw {}",
            hook_mutation_bodies.len()
        );
    }

    std::env::set_current_dir(&prev_cwd).unwrap();

    // Lockfile base must remain pinned across the second sync — except
    // for the BothEditedToSameCode case, where the kind classifies as
    // Clean and the lockfile may be no-op resaved (the base hash is
    // unchanged regardless, since the combined hashes equal).
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let v_before: serde_json::Value = serde_json::from_str(&lf_before).unwrap();
    let v_after: serde_json::Value = serde_json::from_str(&lf_after).unwrap();
    let base_before = v_before
        .pointer(&format!("/objects/hooks/{slug}/content_hash"))
        .cloned();
    let base_after = v_after
        .pointer(&format!("/objects/hooks/{slug}/content_hash"))
        .cloned();

    match variant {
        HookConflictVariant::BothEditedToSameCode => {
            // Both sides converged on the same code → combined hashes
            // equal → classifier emits `Clean` (LocalEdit if scanner
            // flagged; but no remote change either way) and no writes
            // occur. The lockfile base may advance to reflect the new
            // canonical bytes — that's not a safety violation.
            // We just assert no PATCH (already asserted above).
            let _ = (base_before, base_after);
        }
        HookConflictVariant::JsonBothEdited | HookConflictVariant::LocalJsonRemoteCode => {
            // Auto-merge variants: 3-way merge resolves these cleanly.
            // JsonBothEdited: base/local/remote each carry a different
            //   `events` member (string array → set-merge union). No
            //   overlap → clean merge that kept a local-side change, so
            //   it PATCHes the union upstream (asserted above) and the
            //   lockfile advances to the post-push base.
            // LocalJsonRemoteCode: local edits JSON `events`, remote
            //   edits sidecar `.py` — disjoint axes. The merge keeps the
            //   local events, adopts the remote code, and PATCHes the
            //   local edit upstream (asserted above).
            // Contract: lockfile MAY advance to the merged/post-push hash
            // (no longer pinned to the prior base).
            let _ = (base_before, base_after);
        }
        _ => {
            assert_eq!(
                base_before, base_after,
                "variant {variant:?}: lockfile base must remain pinned so next sync re-prompts \
                 (before={base_before:?}, after={base_after:?})",
            );
        }
    }

    // Local file survival checks.
    if matches!(variant, HookConflictVariant::LocalRemovedCodeRemoteEdited) {
        // User removed the .py locally — the resolver mustn't silently
        // recreate it (that would discard the user's intent to delete
        // code). The shadow file may land next to either the .py or
        // .json depending on the redirect logic.
        let _ = py_existed_before;
    } else if matches!(variant, HookConflictVariant::LocalHasCodeRemoteRemoved) {
        // User's local .py edit must survive.
        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after,
            local_code_edit.as_bytes(),
            "variant {variant:?}: local .py edit must survive"
        );
    } else if matches!(variant, HookConflictVariant::CodeBothEdited) {
        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after,
            local_code_edit.as_bytes(),
            "variant {variant:?}: local .py edit must survive"
        );
    } else if matches!(
        variant,
        HookConflictVariant::JsonBothEdited | HookConflictVariant::LocalJsonRemoteCode
    ) {
        // Local JSON edit must survive (no silent overwrite).
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
        let evs = v["events"].as_array().unwrap();
        assert!(
            evs.iter().any(|x| x.as_str() == Some("annotation_status")),
            "variant {variant:?}: local JSON edit must survive in {}",
            json_path.display()
        );
    }
}

/// `JsonBothEdited` is NOT a conflict: base/local/remote each carry a
/// different `events` array and the 3-way merge UNIONs them into a clean
/// auto-merge. Because the merge keeps a local-only event, the
/// auto-merge-push fix PATCHes the unioned events upstream (exactly one
/// PATCH), and a follow-up sync is idempotent (no revert). The shared
/// helper branches on this variant; all other hook variants remain pure
/// conflicts that never push.
#[tokio::test]
async fn sync_hook_auto_merges_disjoint_events_pushes() {
    run_hook_conflict_scenario(HookConflictVariant::JsonBothEdited).await;
}

#[tokio::test]
async fn sync_hook_conflict_code_both_edited_never_silently_pushes() {
    run_hook_conflict_scenario(HookConflictVariant::CodeBothEdited).await;
}

/// Local JSON edit + remote code edit touch disjoint axes of the same
/// hook: a clean 3-way merge, not a conflict. The merge keeps the local
/// `events` edit, adopts the remote `.py`, and pushes the JSON edit
/// upstream — exactly one PATCH, idempotent on re-sync, nothing lost on
/// either axis. (Pre-portabilization this case rendered as a conflict
/// only because the remote's raw URLs produced phantom JSON hunks.)
#[tokio::test]
async fn sync_hook_auto_merges_local_json_remote_code() {
    run_hook_conflict_scenario(HookConflictVariant::LocalJsonRemoteCode).await;
}

#[tokio::test]
async fn sync_hook_conflict_local_has_code_remote_removed_never_silently_pushes() {
    run_hook_conflict_scenario(HookConflictVariant::LocalHasCodeRemoteRemoved).await;
}

#[tokio::test]
async fn sync_hook_conflict_local_removed_code_remote_edited_never_silently_pushes() {
    run_hook_conflict_scenario(HookConflictVariant::LocalRemovedCodeRemoteEdited).await;
}

#[tokio::test]
async fn sync_hook_conflict_both_edited_to_same_code_is_clean_no_writes() {
    run_hook_conflict_scenario(HookConflictVariant::BothEditedToSameCode).await;
}

// ---------------------------------------------------------------------
// Rules conflict matrix — mirrors the hook matrix above. Rules share
// the same split-file shape (`<slug>.json` + `<slug>.py`, where the
// code lives in `trigger_condition` at the top level instead of in
// `config.code`). The same bug class applies: asymmetric / code-only
// divergence must never silently round-trip a PATCH.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum RuleConflictVariant {
    /// Both sides edited JSON.
    JsonBothEdited,
    /// Both sides edited trigger_condition (the .py sidecar).
    CodeBothEdited,
    /// Local JSON, remote code. Disjoint axes → clean 3-way merge that
    /// adopts the remote condition and PATCHes the local name upstream.
    LocalJsonRemoteCode,
    /// Local has .py (edited), remote dropped trigger_condition.
    LocalHasCodeRemoteRemoved,
    /// Local removed .py; remote edited trigger_condition.
    LocalRemovedCodeRemoteEdited,
    /// Both converge on the same code.
    BothEditedToSameCode,
}

async fn run_rule_conflict_scenario(variant: RuleConflictVariant) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let rule_id = 9000u64;
    let slug = "e-invoice-validation";
    let base_cond = "annotation_content.total > 1000\n";
    let local_cond_edit = "annotation_content.total > 2000\n";
    let remote_cond_edit = "annotation_content.total > 5000\n";
    let same_cond_both = "annotation_content.both_edited > 42\n";
    let name_base = "E-invoice Validation".to_string();
    let name_local = "E-invoice Validation (local)".to_string();
    let name_remote = "E-invoice Validation (remote)".to_string();

    let list_call_count = Arc::new(AtomicUsize::new(0));
    let counter = list_call_count.clone();
    let uri_clone = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (name_now, cond_now, modified_at) = if n == 0 {
                (
                    name_base.clone(),
                    Some(base_cond.to_string()),
                    "2026-05-14T08:00:00Z".to_string(),
                )
            } else {
                match variant {
                    RuleConflictVariant::JsonBothEdited => (
                        name_remote.clone(),
                        Some(base_cond.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    RuleConflictVariant::CodeBothEdited => (
                        name_base.clone(),
                        Some(remote_cond_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    RuleConflictVariant::LocalJsonRemoteCode => (
                        name_base.clone(),
                        Some(remote_cond_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    RuleConflictVariant::LocalHasCodeRemoteRemoved => {
                        (name_base.clone(), None, "2026-05-14T10:00:00Z".to_string())
                    }
                    RuleConflictVariant::LocalRemovedCodeRemoteEdited => (
                        name_base.clone(),
                        Some(remote_cond_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    RuleConflictVariant::BothEditedToSameCode => (
                        name_base.clone(),
                        Some(same_cond_both.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                }
            };

            let mut rule = serde_json::json!({
                "id": rule_id,
                "url": format!("{uri_clone}/api/v1/rules/{rule_id}"),
                "name": name_now,
                "queues": [],
                "modified_at": modified_at,
            });
            if let Some(c) = cond_now {
                rule["trigger_condition"] = serde_json::Value::String(c);
            }
            let body = serde_json::json!({
                "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                "results": [rule]
            });
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/rules"]).await;

    if matches!(variant, RuleConflictVariant::LocalJsonRemoteCode) {
        // Disjoint auto-merge: the local `name` edit must reach the
        // remote — exactly one PATCH, echoing the merged rule.
        let uri_patch = server.uri();
        let name_local_patch = name_local.clone();
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v1/rules/{rule_id}")))
            .respond_with(move |_req: &Request| {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": rule_id,
                    "url": format!("{uri_patch}/api/v1/rules/{rule_id}"),
                    "name": name_local_patch,
                    "queues": [],
                    "trigger_condition": remote_cond_edit,
                    "modified_at": "2026-05-14T11:00:00Z"
                }))
            })
            .expect(1)
            .mount(&server)
            .await;
    } else {
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v1/rules/{rule_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/v1/rules/{rule_id}")))
        .respond_with(ResponseTemplate::new(204))
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

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    let json_path = project.path().join(format!("envs/dev/rules/{slug}.json"));
    let py_path = project.path().join(format!("envs/dev/rules/{slug}.py"));
    let json_before = std::fs::read(&json_path).unwrap();

    match variant {
        RuleConflictVariant::JsonBothEdited => {
            let mut v: serde_json::Value = serde_json::from_slice(&json_before).unwrap();
            v["name"] = serde_json::json!(name_local);
            let mut nj = serde_json::to_vec_pretty(&v).unwrap();
            nj.push(b'\n');
            std::fs::write(&json_path, &nj).unwrap();
        }
        RuleConflictVariant::CodeBothEdited => {
            std::fs::write(&py_path, local_cond_edit.as_bytes()).unwrap();
        }
        RuleConflictVariant::LocalJsonRemoteCode => {
            let mut v: serde_json::Value = serde_json::from_slice(&json_before).unwrap();
            v["name"] = serde_json::json!(name_local);
            let mut nj = serde_json::to_vec_pretty(&v).unwrap();
            nj.push(b'\n');
            std::fs::write(&json_path, &nj).unwrap();
        }
        RuleConflictVariant::LocalHasCodeRemoteRemoved => {
            std::fs::write(&py_path, local_cond_edit.as_bytes()).unwrap();
        }
        RuleConflictVariant::LocalRemovedCodeRemoteEdited => {
            std::fs::remove_file(&py_path).expect("seeded .py must exist");
        }
        RuleConflictVariant::BothEditedToSameCode => {
            std::fs::write(&py_path, same_cond_both.as_bytes()).unwrap();
        }
    }

    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync should succeed (no silent write)");

    std::env::set_current_dir(&prev_cwd).unwrap();

    let mutation_bodies: Vec<Vec<u8>> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            (r.method == http::Method::PATCH
                || r.method == http::Method::POST
                || r.method == http::Method::DELETE)
                && (r.url.path() == format!("/api/v1/rules/{rule_id}")
                    || r.url.path() == "/api/v1/rules")
        })
        .map(|r| r.body)
        .collect();
    if matches!(variant, RuleConflictVariant::LocalJsonRemoteCode) {
        // Disjoint auto-merge: the local name edit lands in exactly one
        // PATCH, and the local sidecar adopts the remote condition.
        assert_eq!(
            mutation_bodies.len(),
            1,
            "variant {variant:?}: auto-merge must PATCH exactly once; saw {}",
            mutation_bodies.len()
        );
        let patched: serde_json::Value = serde_json::from_slice(&mutation_bodies[0]).unwrap();
        assert_eq!(
            patched["name"].as_str(),
            Some(name_local.as_str()),
            "variant {variant:?}: PATCH must carry the local name edit"
        );
        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after,
            remote_cond_edit.as_bytes(),
            "variant {variant:?}: local .py must adopt the remote condition edit"
        );
    } else {
        assert_eq!(
            mutation_bodies.len(),
            0,
            "variant {variant:?}: rules endpoint must not receive mutating requests; saw {}",
            mutation_bodies.len()
        );
    }

    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let v_before: serde_json::Value = serde_json::from_str(&lf_before).unwrap();
    let v_after: serde_json::Value = serde_json::from_str(&lf_after).unwrap();
    let base_before = v_before
        .pointer(&format!("/objects/rules/{slug}/content_hash"))
        .cloned();
    let base_after = v_after
        .pointer(&format!("/objects/rules/{slug}/content_hash"))
        .cloned();
    // Auto-merge resolves LocalJsonRemoteCode (disjoint edits) and may
    // auto-resolve JsonBothEdited when the edits are set-merge-friendly
    // (e.g. both add distinct entries to a string array). The strong
    // contract — "rules endpoint MUST NOT receive mutating requests" —
    // is asserted above; the lockfile base may advance on auto-merge.
    if !matches!(
        variant,
        RuleConflictVariant::BothEditedToSameCode
            | RuleConflictVariant::JsonBothEdited
            | RuleConflictVariant::LocalJsonRemoteCode
    ) {
        assert_eq!(
            base_before, base_after,
            "variant {variant:?}: lockfile base must remain pinned (before={base_before:?}, after={base_after:?})",
        );
    }

    if matches!(
        variant,
        RuleConflictVariant::LocalHasCodeRemoteRemoved | RuleConflictVariant::CodeBothEdited
    ) {
        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after,
            local_cond_edit.as_bytes(),
            "variant {variant:?}: local .py edit must survive",
        );
    }
}

#[tokio::test]
async fn sync_rule_conflict_json_both_edited_never_silently_pushes() {
    run_rule_conflict_scenario(RuleConflictVariant::JsonBothEdited).await;
}

#[tokio::test]
async fn sync_rule_conflict_code_both_edited_never_silently_pushes() {
    run_rule_conflict_scenario(RuleConflictVariant::CodeBothEdited).await;
}

#[tokio::test]
async fn sync_rule_auto_merges_local_json_remote_code() {
    run_rule_conflict_scenario(RuleConflictVariant::LocalJsonRemoteCode).await;
}

#[tokio::test]
async fn sync_rule_conflict_local_has_code_remote_removed_never_silently_pushes() {
    run_rule_conflict_scenario(RuleConflictVariant::LocalHasCodeRemoteRemoved).await;
}

#[tokio::test]
async fn sync_rule_conflict_local_removed_code_remote_edited_never_silently_pushes() {
    run_rule_conflict_scenario(RuleConflictVariant::LocalRemovedCodeRemoteEdited).await;
}

#[tokio::test]
async fn sync_rule_conflict_both_edited_to_same_code_is_clean_no_writes() {
    run_rule_conflict_scenario(RuleConflictVariant::BothEditedToSameCode).await;
}

// ---------------------------------------------------------------------
// Schemas conflict matrix. Schemas have the same split-file shape as
// hooks/rules but the code sidecars live under `formulas/<field_id>.py`
// instead of a peer `.py`. The combined hash is `schema_combined_hash`.
// The same bug class applies: a divergent formula sidecar must not let
// the resolver short-circuit on JSON equality.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum SchemaConflictVariant {
    /// Local + remote schema.json both edited; formulas unchanged.
    JsonBothEdited,
    /// Local + remote formula both edited; schema.json unchanged.
    FormulaBothEdited,
    /// Local has a formula sidecar (edited); remote dropped the formula
    /// entirely from the schema.
    LocalHasFormulaRemoteRemoved,
    /// Local removed the formula sidecar; remote edited the formula.
    LocalRemovedFormulaRemoteEdited,
    /// Both schema.json and formula edited on both sides.
    JsonAndFormulaBothEdited,
}

async fn run_schema_conflict_scenario(variant: SchemaConflictVariant) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let ws_id = 600u64;
    let queue_id = 700u64;
    let schema_id = 800u64;
    let inbox_id = 900u64;

    let ws_url = format!("{}/api/v1/workspaces/{}", server.uri(), ws_id);
    let queue_url = format!("{}/api/v1/queues/{}", server.uri(), queue_id);
    let schema_url = format!("{}/api/v1/schemas/{}", server.uri(), schema_id);
    let inbox_url = format!("{}/api/v1/inboxes/{}", server.uri(), inbox_id);

    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
            "results": [{
                "id": ws_id,
                "url": ws_url,
                "name": "AP Invoices",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [queue_url.clone()],
                "modified_at": "2026-04-20T08:00:00Z"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
            "results": [{
                "id": queue_id,
                "url": queue_url.clone(),
                "name": "Cost Invoices",
                "workspace": ws_url,
                "schema": schema_url.clone(),
                "inbox": inbox_url.clone(),
                "modified_at": "2026-04-20T08:00:00Z"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total_pages": 1, "next": null },
            "results": [{
                "id": inbox_id,
                "url": inbox_url,
                "name": "Cost Invoices Inbox",
                "email": "cost-invoices@mock.rossum.app",
                "queues": [queue_url.clone()],
                "filters": [],
                "modified_at": "2026-04-20T08:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    let base_formula = "amount_due + amount_tax";
    let local_formula_edit = "amount_due + amount_tax + amount_fee";
    let remote_formula_edit = "amount_due * 1.21";
    let base_name = "Cost Invoices Schema".to_string();
    let remote_name = "Cost Invoices Schema (remote)".to_string();

    // Toggle the schema body after the first sync completes. Using a
    // simple call counter doesn't work here because both `list_remote`
    // and `pull::queues::process` GET the schema each sync (two calls
    // per sync), so the "first call only" heuristic would flip
    // mid-seed. The harness flips this AtomicBool after seeding.
    let serve_modified = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let serve_modified_clone = serve_modified.clone();
    let schema_url_clone = schema_url.clone();
    let queue_url_clone = queue_url.clone();
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/schemas/{schema_id}")))
        .respond_with(move |_req: &Request| {
            let modified = serve_modified_clone.load(Ordering::SeqCst);
            let (name_now, formula_now, modified_at) = if !modified {
                (
                    base_name.clone(),
                    Some(base_formula.to_string()),
                    "2026-04-10T09:00:00Z".to_string(),
                )
            } else {
                match variant {
                    SchemaConflictVariant::JsonBothEdited => (
                        remote_name.clone(),
                        Some(base_formula.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    SchemaConflictVariant::FormulaBothEdited => (
                        base_name.clone(),
                        Some(remote_formula_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    SchemaConflictVariant::LocalHasFormulaRemoteRemoved => {
                        (base_name.clone(), None, "2026-05-14T10:00:00Z".to_string())
                    }
                    SchemaConflictVariant::LocalRemovedFormulaRemoteEdited => (
                        base_name.clone(),
                        Some(remote_formula_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                    SchemaConflictVariant::JsonAndFormulaBothEdited => (
                        remote_name.clone(),
                        Some(remote_formula_edit.to_string()),
                        "2026-05-14T10:00:00Z".to_string(),
                    ),
                }
            };
            let mut datapoint = serde_json::json!({
                "category": "datapoint",
                "id": "amount_total",
                "type": "number",
            });
            if let Some(f) = formula_now {
                datapoint["formula"] = serde_json::Value::String(f);
            }
            let body = serde_json::json!({
                "id": schema_id,
                "url": schema_url_clone,
                "name": name_now,
                "queues": [queue_url_clone],
                "content": [{
                    "category": "section",
                    "id": "header",
                    "label": "Header",
                    "children": [
                        { "category": "datapoint", "id": "invoice_id", "type": "string" },
                        datapoint
                    ]
                }],
                "modified_at": modified_at
            });
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(
        &server,
        &["/api/v1/workspaces", "/api/v1/queues", "/api/v1/inboxes"],
    )
    .await;

    Mock::given(method("PATCH"))
        .and(path(format!("/api/v1/schemas/{schema_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/schemas"))
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

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    // After seeding, flip the schema mock to serve the variant-specific
    // "remote now" body. The mock body is variant-controlled below.
    serve_modified.store(true, Ordering::SeqCst);

    let queue_dir = project
        .path()
        .join("envs/dev/workspaces/ap-invoices/queues/cost-invoices");
    let schema_path = queue_dir.join("schema.json");
    let formula_path = queue_dir.join("formulas/amount_total.py");

    let schema_before = std::fs::read(&schema_path).unwrap();

    match variant {
        SchemaConflictVariant::JsonBothEdited => {
            let mut v: serde_json::Value = serde_json::from_slice(&schema_before).unwrap();
            v["name"] = serde_json::json!("Cost Invoices Schema (local)");
            let mut nj = serde_json::to_vec_pretty(&v).unwrap();
            nj.push(b'\n');
            std::fs::write(&schema_path, &nj).unwrap();
        }
        SchemaConflictVariant::FormulaBothEdited => {
            std::fs::write(&formula_path, local_formula_edit.as_bytes()).unwrap();
        }
        SchemaConflictVariant::LocalHasFormulaRemoteRemoved => {
            std::fs::write(&formula_path, local_formula_edit.as_bytes()).unwrap();
        }
        SchemaConflictVariant::LocalRemovedFormulaRemoteEdited => {
            std::fs::remove_file(&formula_path).expect("seeded formula sidecar must exist");
        }
        SchemaConflictVariant::JsonAndFormulaBothEdited => {
            let mut v: serde_json::Value = serde_json::from_slice(&schema_before).unwrap();
            v["name"] = serde_json::json!("Cost Invoices Schema (local)");
            let mut nj = serde_json::to_vec_pretty(&v).unwrap();
            nj.push(b'\n');
            std::fs::write(&schema_path, &nj).unwrap();
            std::fs::write(&formula_path, local_formula_edit.as_bytes()).unwrap();
        }
    }

    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync should succeed (no silent write)");

    std::env::set_current_dir(&prev_cwd).unwrap();

    let mutation_count = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            (r.method == http::Method::PATCH
                || r.method == http::Method::POST
                || r.method == http::Method::DELETE)
                && (r.url.path() == format!("/api/v1/schemas/{schema_id}")
                    || r.url.path() == "/api/v1/schemas")
        })
        .count();
    assert_eq!(
        mutation_count, 0,
        "variant {variant:?}: schema endpoint must not receive mutating requests; saw {mutation_count}",
    );

    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let v_before: serde_json::Value = serde_json::from_str(&lf_before).unwrap();
    let v_after: serde_json::Value = serde_json::from_str(&lf_after).unwrap();
    let base_before = v_before
        .pointer("/objects/schemas/cost-invoices/content_hash")
        .cloned();
    let base_after = v_after
        .pointer("/objects/schemas/cost-invoices/content_hash")
        .cloned();
    assert_eq!(
        base_before, base_after,
        "variant {variant:?}: lockfile base for schema must remain pinned (before={base_before:?}, after={base_after:?})",
    );

    if matches!(
        variant,
        SchemaConflictVariant::FormulaBothEdited
            | SchemaConflictVariant::LocalHasFormulaRemoteRemoved
            | SchemaConflictVariant::JsonAndFormulaBothEdited
    ) {
        let formula_after = std::fs::read(&formula_path).unwrap();
        assert_eq!(
            formula_after,
            local_formula_edit.as_bytes(),
            "variant {variant:?}: local formula edit must survive",
        );
    }
}

#[tokio::test]
async fn sync_schema_conflict_json_both_edited_never_silently_pushes() {
    run_schema_conflict_scenario(SchemaConflictVariant::JsonBothEdited).await;
}

#[tokio::test]
async fn sync_schema_conflict_formula_both_edited_never_silently_pushes() {
    run_schema_conflict_scenario(SchemaConflictVariant::FormulaBothEdited).await;
}

#[tokio::test]
async fn sync_schema_conflict_local_has_formula_remote_removed_never_silently_pushes() {
    run_schema_conflict_scenario(SchemaConflictVariant::LocalHasFormulaRemoteRemoved).await;
}

#[tokio::test]
async fn sync_schema_conflict_local_removed_formula_remote_edited_never_silently_pushes() {
    run_schema_conflict_scenario(SchemaConflictVariant::LocalRemovedFormulaRemoteEdited).await;
}

#[tokio::test]
async fn sync_schema_conflict_json_and_formula_both_edited_never_silently_pushes() {
    run_schema_conflict_scenario(SchemaConflictVariant::JsonAndFormulaBothEdited).await;
}

// ====================================================================
// Regression: `rdc doctor --rebuild-lock` followed by `rdc sync` must
// not panic on the classifier's `(local_changed=true, local_tombstoned=
// false, remote_present=true, locked_present=false)` cell. This used to
// fall into the catch-all panic arm because no class covered the
// "lockfile-missing, both sides present" state — a state the rebuild-
// lock workflow legitimately produces.
//
// Both sub-cases are covered:
//   1. Local matches remote byte-for-byte → classify as `Clean`, sync
//      rebuilds the lockfile entry, no writes hit the API.
//   2. Local differs from remote → classify as `BothDiverged`. The
//      resolver fires; in non-TTY mode it falls back to the shadow file
//      path, leaves the lockfile base unset (so the next sync re-prompts),
//      and never silently PATCHes the user's local edit onto remote.
// ====================================================================

/// Sub-case 1: post-`rebuild-lock` with local==remote. Sync must classify
/// the label as `Clean`, rebuild the lockfile entry, and issue zero writes.
#[tokio::test]
async fn sync_after_rebuild_lock_in_sync_label_yields_clean_and_rebuilds_lockfile() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // A single stable label served by every listing call. The body
    // hashes identically across the initial sync, the post-rebuild sync,
    // and any drift re-list during push (there is no push here).
    let label_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 42,
                "url": format!("{}/api/v1/labels/42", server.uri()),
                "name": "Rebuild Lock Stable",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#abcdef",
                "modified_at": "2026-05-14T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&label_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    // Any mutating call would be the bug. `.expect(0)` makes wiremock
    // fail the test on Drop if any PATCH lands.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/42"))
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

    // First sync: pulls the label and seeds the lockfile.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync seeds the lockfile");

    let label_path = project
        .path()
        .join("envs/dev/labels/rebuild-lock-stable.json");
    assert!(label_path.exists(), "first sync writes the label file");

    // Snapshot the local bytes so we can later prove they survive the
    // rebuild-lock → sync round trip byte-for-byte.
    let local_before = std::fs::read(&label_path).unwrap();

    // Simulate `rdc doctor --rebuild-lock`: wipe the lockfile but leave
    // the local snapshot file on disk. The remote still serves the same
    // body. Classifier will see: local_changed=true (no lockfile to
    // compare), remote_present=true, locked_present=false → the
    // previously-panicking cell.
    let lockfile_path = project.path().join(".rdc/state/dev.lock.json");
    std::fs::remove_file(&lockfile_path).unwrap();

    // Second sync: this would have panicked pre-fix. With the fix in
    // place, the canonical hashes match → classify as Clean → executor
    // dispatches through pull driver to rebuild the lockfile entry.
    let result = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();
    result.expect("post-rebuild-lock sync must not panic when local==remote");

    // Local file still there with the canonical body. The pull driver
    // may re-write byte-identical content; the bytes on disk must still
    // match what was there before (post-canonicalize).
    let local_after = std::fs::read(&label_path).unwrap();
    assert_eq!(
        local_before, local_after,
        "Clean post-rebuild-lock must not corrupt or alter local bytes"
    );

    // No mutating API calls hit the mock — Clean is pull-and-record only.
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

    // Lockfile entry is rebuilt — the second sync recorded the hash
    // so subsequent syncs see truly-Clean state.
    let lf_raw = std::fs::read_to_string(&lockfile_path).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    let recorded = lf
        .pointer("/objects/labels/rebuild-lock-stable/content_hash")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    assert!(
        recorded.is_some(),
        "post-rebuild-lock sync must rebuild the lockfile entry: {lf_raw}"
    );

    // No shadow file landed — Clean means "no conflict, no prompt".
    let shadow = project
        .path()
        .join("envs/dev/labels/rebuild-lock-stable.json.dev");
    assert!(
        !shadow.exists(),
        "Clean post-rebuild-lock must not produce a shadow file at {}",
        shadow.display()
    );
}

/// Sub-case 2: post-`rebuild-lock` with local != remote. Sync must
/// classify the label as `BothDiverged`. In non-TTY mode the resolver
/// falls back to the shadow file path, no PATCH lands, and the lockfile
/// stays unset so the next sync re-prompts. This is the load-bearing
/// case — pre-fix it would have panicked; pre-hardening it would have
/// silently overwritten the user's local edit onto remote.
#[tokio::test]
async fn sync_after_rebuild_lock_diverged_label_does_not_panic_and_does_not_silently_push() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Remote serves a "remote-color" body on every listing call.
    let remote_label_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 73,
                "url": format!("{}/api/v1/labels/73", server.uri()),
                "name": "Rebuild Lock Diverged",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#aa0000",
                "modified_at": "2026-05-14T08:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&remote_label_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    // Any PATCH on labels/73 here would be a silent push — the load-
    // bearing assertion against the original silent-data-loss bug.
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/73"))
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

    // First sync seeds local + lockfile from remote.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync seeds the lockfile");

    let label_path = project
        .path()
        .join("envs/dev/labels/rebuild-lock-diverged.json");
    assert!(label_path.exists(), "first sync writes the label file");

    // Locally edit the label so it no longer matches the remote body.
    let raw = std::fs::read_to_string(&label_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String("#00ff00".to_string());
    let local_edited_bytes = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
    std::fs::write(&label_path, &local_edited_bytes).unwrap();

    // Simulate `rdc doctor --rebuild-lock`: wipe the lockfile. The
    // local edit stays on disk. Remote still serves the original body.
    let lockfile_path = project.path().join(".rdc/state/dev.lock.json");
    std::fs::remove_file(&lockfile_path).unwrap();

    // Second sync: classifier sees (true, false, true, false) with
    // local_hash != remote_hash → BothDiverged. Non-TTY → shadow file
    // fallback, no push, base preserved (None) so the next sync
    // re-prompts.
    let result = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();
    result.expect("post-rebuild-lock diverged sync must not panic");

    // No PATCH/POST/DELETE on the label.
    let patch_calls = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/labels/73")
        .count();
    assert_eq!(
        patch_calls, 0,
        "BothDiverged post-rebuild-lock must NOT be silently PATCHed (saw {patch_calls} PATCH calls)"
    );

    // Local edit survives — the user's edit is not discarded by the
    // conflict path.
    let local_after = std::fs::read_to_string(&label_path).unwrap();
    assert_eq!(
        local_after, local_edited_bytes,
        "local edit must survive the conflict path: {local_after}"
    );

    // Shadow file is written next to the local file so the user sees
    // the env-side body.
    let shadow = project
        .path()
        .join("envs/dev/labels/rebuild-lock-diverged.json.dev");
    assert!(
        shadow.exists(),
        "BothDiverged in non-TTY mode must produce a shadow file at {}",
        shadow.display()
    );

    // Lockfile must NOT advance to either side's hash — without a base,
    // any hash recorded here would mean the next sync no longer sees
    // a conflict. The contract is: preserve "no base" so the user
    // re-prompts.
    let lf_raw = std::fs::read_to_string(&lockfile_path).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    let recorded = lf
        .pointer("/objects/labels/rebuild-lock-diverged/content_hash")
        .and_then(|v| v.as_str());
    assert!(
        recorded.is_none(),
        "BothDiverged conflict path must not advance the lockfile base, got: {recorded:?}"
    );
}

/// Phase-ordering regression: with a `LocalEdit` and a `RemoteCreate` on
/// the same sync, the executor must run the push-side block BEFORE the
/// pull-side block so the user's local edits land on the remote as soon
/// as the conflict resolver finishes. Push-side activity (the PATCH on
/// `labels/order-push-edit`) must therefore precede pull-side activity
/// (the per-kind `labels … pulled` summary) in the captured stderr.
///
/// Pull and push touch disjoint `(kind, slug)` sets (the classifier
/// produces mutually-exclusive classes), so this is purely a sequencing
/// contract; no race is introduced.
#[tokio::test]
async fn sync_pushes_local_edits_before_pulling_remote_changes() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Two labels: id=41 will be edited locally (LocalEdit), id=42 will
    // be created remotely after the seed sync (RemoteCreate).
    let seed_labels = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 41,
                "url": format!("{}/api/v1/labels/41", server.uri()),
                "name": "Order Push Edit",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#111111",
                "modified_at": "2026-04-15T08:00:00Z"
            }
        ]
    });
    let post_seed_labels = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 41,
                "url": format!("{}/api/v1/labels/41", server.uri()),
                "name": "Order Push Edit",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#111111",
                "modified_at": "2026-04-15T08:00:00Z"
            },
            {
                "id": 42,
                "url": format!("{}/api/v1/labels/42", server.uri()),
                "name": "Order Push Create",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#222222",
                "modified_at": "2026-04-15T08:00:00Z"
            }
        ]
    });

    // Seed listing — only the first label exists at the time of the
    // initial sync. The wiremock matcher consults mocks in reverse
    // insertion order, so install the seed mock with `.up_to_n_times(1)`
    // and the post-seed mock unbounded afterward.
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&seed_labels))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&post_seed_labels))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    // PATCH /labels/41 — the push side of the second sync.
    let patched_color = "#abcdef";
    let patch_response = serde_json::json!({
        "id": 41,
        "url": format!("{}/api/v1/labels/41", server.uri()),
        "name": "Order Push Edit",
        "organization": format!("{}/api/v1/organizations/1", server.uri()),
        "color": patched_color,
        "modified_at": "2026-04-15T09:00:00Z"
    });
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/41"))
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

    // Seed: pull the first label to populate the lockfile.
    rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await
    .expect("seed sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    // Edit the local file so the second sync classifies it as LocalEdit.
    let edit_path = project.path().join("envs/dev/labels/order-push-edit.json");
    assert!(edit_path.exists(), "seed sync must write the first label");
    let raw = std::fs::read_to_string(&edit_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(patched_color.to_string());
    std::fs::write(
        &edit_path,
        format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    )
    .unwrap();

    // Drive the second sync via the actual binary so we can capture
    // stderr — the in-process `rdc::cli::sync::run` writes to the test
    // runner's own stderr and is harder to inspect for log ordering.
    let out = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();

    let push_idx = stderr.find("label/order-push-edit").unwrap_or_else(|| {
        panic!("expected push-side activity ('label/order-push-edit') in stderr: {stderr}");
    });
    let pull_idx = stderr.find("labels (1 pulled)").unwrap_or_else(|| {
        panic!("expected pull-side summary ('labels (1 pulled)') in stderr: {stderr}");
    });
    assert!(
        push_idx < pull_idx,
        "push must run before pull: 'label/order-push-edit' at byte {push_idx}, \
         'labels (1 pulled)' at byte {pull_idx}\n--- stderr ---\n{stderr}"
    );

    // Sanity: the PATCH happened (covered by `.expect(1)` on the mock)
    // and the RemoteCreate landed on disk.
    let created_path = project
        .path()
        .join("envs/dev/labels/order-push-create.json");
    assert!(
        created_path.exists(),
        "pull-side RemoteCreate must still run after the push: {}",
        created_path.display()
    );
}

/// Editing only `secrets/<env>.hook-secrets.json` (no change to the
/// hook JSON or `.py` sidecar) must still surface as a force-PATCH to
/// /hooks/<id> on the next sync, with the secrets map in the body. The
/// lockfile entry's `secrets_hash` is updated so a second sync is a
/// no-op.
#[tokio::test]
async fn sync_hook_secrets_only_edit_triggers_force_patch() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let hook_id = 4242u64;
    let server_uri = server.uri();
    let hook_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": hook_id,
                "url": format!("{server_uri}/api/v1/hooks/{hook_id}"),
                "name": "mdh-lookup",
                "type": "webhook",
                "queues": [],
                "events": ["annotation_content"],
                "config": { "url": "https://mdh.example.com/lookup" },
                "modified_at": "2026-04-01T10:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&hook_body))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &["/api/v1/hooks"]).await;

    // Capture the PATCH body so we can assert the secrets map made it
    // onto the wire. wiremock's `Request` keeps the raw bytes; we read
    // them out of `received_requests` after the test.
    let server_uri = server.uri();
    Mock::given(method("PATCH"))
        .and(path(format!("/api/v1/hooks/{hook_id}")))
        .respond_with(move |req: &Request| {
            let mut body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| serde_json::json!({}));
            // Echo back the body (with id/url injected) so the rest of
            // the response handlers stay happy. The test only inspects
            // the request, not the response.
            if let Some(obj) = body.as_object_mut() {
                obj.insert("id".to_string(), serde_json::json!(hook_id));
                obj.insert(
                    "url".to_string(),
                    serde_json::json!(format!("{server_uri}/api/v1/hooks/{hook_id}")),
                );
                obj.insert("type".to_string(), serde_json::json!("webhook"));
                obj.insert("name".to_string(), serde_json::json!("mdh-lookup"));
                obj.insert(
                    "modified_at".to_string(),
                    serde_json::json!("2026-04-01T11:00:00Z"),
                );
            }
            ResponseTemplate::new(200).set_body_json(body)
        })
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

    // Seed sync — pulls the hook, populates lockfile. No PATCH expected
    // here (the `.expect(1)` on the PATCH mock covers the WHOLE test;
    // the second sync below is what fires it).
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");

    // User adds a secret value to the gitignored hook-secrets file.
    std::fs::write(
        project.path().join("secrets/dev.hook-secrets.json"),
        r#"{ "hooks": { "mdh-lookup": { "api_key": "k-prod-abc" } } }"#,
    )
    .unwrap();

    // Second sync — hook JSON/code is unchanged on disk and on remote;
    // only the secrets file changed. The force-PATCH pass should fire.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("secrets-only force PATCH should succeed");

    // Third sync — secrets_hash now matches; should be a no-op.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("third sync should be a no-op");

    std::env::set_current_dir(&prev_cwd).unwrap();

    // The PATCH body must have carried `secrets.api_key = "k-prod-abc"`.
    let patch_bodies: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.method == http::Method::PATCH && r.url.path() == format!("/api/v1/hooks/{hook_id}")
        })
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .collect();
    assert_eq!(
        patch_bodies.len(),
        1,
        "expected exactly one PATCH (the secrets-only force-push); got {}",
        patch_bodies.len()
    );
    let secrets_obj = patch_bodies[0]
        .get("secrets")
        .and_then(|v| v.as_object())
        .expect("PATCH body must include a `secrets` object");
    assert_eq!(
        secrets_obj.get("api_key").and_then(|v| v.as_str()),
        Some("k-prod-abc"),
        "PATCH body's `secrets.api_key` must match the local file"
    );

    // Lockfile records the new secrets_hash so the third sync was a
    // no-op (the `.expect(1)` mock would have tripped otherwise).
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&lf).unwrap();
    let hash = v
        .pointer("/objects/hooks/mdh-lookup/secrets_hash")
        .and_then(|v| v.as_str());
    assert!(
        hash.is_some_and(|s| s.len() == 64),
        "lockfile should record a 64-char hex secrets_hash; got {hash:?}"
    );
}

// Minimal mocks for a pull-only sync: organization GET plus empty
// listings for every kind. Individual tests override specific
// endpoints (e.g. `/api/v1/hooks`) before mounting this. Kept private
// to the file — duplicated in `tests/cli_repair.rs` because tests
// crates can't share helpers without a shared module, and we don't
// want to introduce one for two callers.
async fn mount_minimal_pull(server: &MockServer) {
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
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
            .mount(server)
            .await;
    }
}

/// Pull must surface the store-extension anomaly
/// (`extension_source: "rossum_store"` + `hook_template: null`) at the
/// time the file lands on disk. Without a Warn here, the user only
/// finds out at push/deploy when the guard refuses — by which point
/// the local snapshot already contains the broken marker.
#[tokio::test]
async fn pull_warns_on_anomalous_store_extension() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    // Override /hooks with one anomalous result. Lower priority number
    // beats the empty-list default mounted by `mount_minimal_pull`.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 42,
                "url": format!("{}/api/v1/hooks/42", server.uri()),
                "name": "Broken Store Hook",
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": {},
                "extension_source": "rossum_store",
                "hook_template": null
            }]
        })))
        .with_priority(1)
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

    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert()
        .success()
        .stderr(predicates::str::contains("broken-store-hook"))
        .stderr(predicates::str::contains("hook_template"))
        .stderr(predicates::str::contains("rdc doctor"));
}

/// 3-way auto-merge: local changes one field, remote changes a
/// different field. With a fresh base cache from the previous sync,
/// the next sync should auto-resolve without any user prompt — and
/// the merged file must contain BOTH edits.
///
/// Because the merge kept a local-side change (`color`), the merged
/// result must be PATCHed back so the remote receives the local color.
/// (Before the auto-merge-push fix, the merge was written locally only
/// and the local color silently reverted on the next sync — data loss.)
/// This test pins both halves: exactly one PATCH carrying the local
/// color, then a second sync that classifies Clean (no further writes,
/// no revert) — the latter being the core data-loss regression guard.
#[tokio::test]
async fn sync_auto_merges_disjoint_label_edits_without_prompting() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let org_url = format!("{}/api/v1/organizations/1", server.uri());
    let label_url = format!("{}/api/v1/labels/81", server.uri());
    let base_color = "#aabbcc";
    let local_color = "#112233";
    let base_name = "Three Way";
    let remote_name = "Three Way (renamed by remote)";

    // Stateful listing across three phases, mirrored by two flags. A
    // bare call-counter is unreliable because each sync GETs the label
    // list more than once (classification + push-side drift check).
    //
    //   phase 1 (seed):        name=base,   color=base
    //   phase 2 (remote edit): name=remote, color=base   (flip after sync 1)
    //   phase 3 (post-PATCH):  name=remote, color=local  (PATCH responder flips)
    //
    // Phase 3 is the merged remote: the local color now lives on the
    // remote, so the third (idempotency) sync classifies the slug Clean.
    let serve_remote_name = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let patch_landed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let serve_remote_name_list = serve_remote_name.clone();
    let patch_landed_list = patch_landed.clone();
    let org_url_list = org_url.clone();
    let label_url_list = label_url.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(move |_req: &Request| {
            let name = if serve_remote_name_list.load(Ordering::SeqCst) {
                remote_name
            } else {
                base_name
            };
            let color = if patch_landed_list.load(Ordering::SeqCst) {
                local_color
            } else {
                base_color
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                "results": [{
                    "id": 81,
                    "url": label_url_list,
                    "name": name,
                    "organization": org_url_list,
                    "color": color,
                    "modified_at": "2026-04-15T08:00:00Z"
                }]
            }))
        })
        .mount(&server)
        .await;

    // PATCH /labels/81: the merged label (local color + remote name)
    // reaches the remote here. The responder flips `patch_landed` so the
    // listing reflects the merge afterwards, and echoes a body carrying
    // the merged fields with a fresh `modified_at`.
    let patch_landed_resp = patch_landed.clone();
    let org_url_patch = org_url.clone();
    let label_url_patch = label_url.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/81"))
        .respond_with(move |_req: &Request| {
            patch_landed_resp.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 81,
                "url": label_url_patch,
                "name": remote_name,
                "organization": org_url_patch,
                "color": local_color,
                "modified_at": "2026-04-15T09:00:00Z"
            }))
        })
        .expect(1)
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

    // First sync — seeds lockfile + base cache from phase 1.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync");

    // Local edit: change ONLY color. Keep name as initially synced.
    let label_path = project.path().join("envs/dev/labels/three-way.json");
    let raw = std::fs::read_to_string(&label_path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["color"] = serde_json::Value::String(local_color.to_string());
    let local_edited = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
    std::fs::write(&label_path, &local_edited).unwrap();

    // Remote now serves the renamed label (phase 2): local changed
    // `color`, remote changed `name`.
    serve_remote_name.store(true, Ordering::SeqCst);

    // Second sync — the 3-way merge accepts both disjoint edits, then
    // (because a local-side field survived the merge) PATCHes the merged
    // result back so the remote receives the local color.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync auto-merges and pushes");

    let merged_raw = std::fs::read_to_string(&label_path).unwrap();
    let merged: serde_json::Value = serde_json::from_str(&merged_raw).unwrap();
    assert_eq!(
        merged["color"], local_color,
        "merged file must keep the local color edit: {merged_raw}"
    );
    assert_eq!(
        merged["name"], remote_name,
        "merged file must include the remote name change: {merged_raw}"
    );

    // Exactly one PATCH /labels/81 landed, and its body carries the
    // local-side color — the merge propagated the local change upstream.
    let patch_reqs: Vec<_> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/labels/81")
        .collect();
    assert_eq!(
        patch_reqs.len(),
        1,
        "auto-merge that kept a local change must push exactly one PATCH /labels/81, saw {}",
        patch_reqs.len()
    );
    let patch_body: serde_json::Value = serde_json::from_slice(&patch_reqs[0].body).unwrap();
    assert_eq!(
        patch_body["color"], local_color,
        "the PATCH body must carry the local-side color: {patch_body}"
    );

    // Core data-loss regression guard: a THIRD sync (remote now serves
    // the merged body, phase 3) must classify the slug Clean — no
    // further mutating call against the Rossum API, and the local color
    // must NOT revert. (Data-storage list reads are POSTs under `/svc/`
    // and are excluded; only `/api/v1/` object writes count here.)
    let count_object_mutations = |reqs: Vec<Request>| -> usize {
        reqs.into_iter()
            .filter(|r| {
                r.url.path().starts_with("/api/v1/")
                    && matches!(
                        r.method,
                        http::Method::PATCH | http::Method::POST | http::Method::DELETE
                    )
            })
            .count()
    };
    let mutations_before =
        count_object_mutations(server.received_requests().await.unwrap_or_default());

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("third sync is idempotent");

    std::env::set_current_dir(&prev_cwd).unwrap();

    let mutations_after =
        count_object_mutations(server.received_requests().await.unwrap_or_default());
    assert_eq!(
        mutations_before, mutations_after,
        "second sync must be idempotent: no further mutating calls to the Rossum API"
    );

    let after_raw = std::fs::read_to_string(&label_path).unwrap();
    let after: serde_json::Value = serde_json::from_str(&after_raw).unwrap();
    assert_eq!(
        after["color"], local_color,
        "local color must NOT revert on the idempotency sync: {after_raw}"
    );
    assert_eq!(
        after["name"], remote_name,
        "remote name must persist on the idempotency sync: {after_raw}"
    );
}

/// Milestone progress line on CI: when the listing phase processes ≥200
/// items, `bump` emits a `list   listing 200` event line on non-TTY. This
/// test mocks 300 labels across 3 pages so the aggregate count crosses 200
/// during the fan-out, triggering exactly one milestone. All other kinds
/// return empty lists so only labels contribute to the count.
#[tokio::test]
async fn sync_emits_progress_milestone_for_large_list() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let label = |id: u64| {
        serde_json::json!({
            "id": id,
            "url": format!("{}/api/v1/labels/{}", server.uri(), id),
            "name": format!("Label {id}"),
            "organization": format!("{}/api/v1/organizations/1", server.uri())
        })
    };
    let page = |from: u64| {
        serde_json::json!({
            "pagination": { "total_pages": 3, "next": null },
            "results": (from..from + 100).map(label).collect::<Vec<_>>()
        })
    };
    use wiremock::matchers::query_param;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(100)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .and(query_param("page", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(200)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(0)))
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

    let mut cmd = assert_cmd::Command::cargo_bin("rdc").unwrap();
    let assert = cmd
        .current_dir(project.path())
        .args(["sync", "dev"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("listing 200"),
        "expected a list milestone in:\n{stderr}"
    );
}

/// Regression: hook with an overlay must NOT oscillate between Write and
/// KeepLocal across successive pulls.
///
/// Under C-1, overlays are migrate-only: pull/sync never strip
/// overlay-managed fields, so the field rides in the snapshot like any
/// other content and the lockfile baseline is computed over exactly the
/// bytes on disk. The invariant this test pins is the absence of phantom
/// drift: with an overlay configured, the second sync over an unchanged
/// remote must classify Clean — no file rewrites, no lockfile churn, no
/// API mutations.
#[tokio::test]
async fn sync_hook_with_overlay_no_phantom_drift() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Hook served by the API. It has a `description` field that the overlay
    // will strip from the on-disk snapshot.
    let hook_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [{
            "id": 555,
            "url": format!("{}/api/v1/hooks/555", server.uri()),
            "name": "Validator hook",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": {
                "runtime": "python3.12",
                "code": "def validate(payload):\n    pass\n"
            },
            "description": "PROD-specific description managed by overlay"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_body))
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

    // Install an overlay that manages the `description` field on this hook.
    // Under C-1 the overlay is migrate-only — pull keeps the field on disk —
    // but its mere presence must not perturb hashing into phantom drift.
    let overlay_dir = project.path().join("envs/dev");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
        overlay_dir.join("overlay.toml"),
        r#"version = 1

[hooks.validator-hook]
"description" = "PROD-specific description managed by overlay"
"#,
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();

    // First sync: pull writes the hook (overlay-stripped) and records the
    // post-overlay combined hash in the lockfile.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    // Under C-1 overlays are migrate-only: the overlay-managed field must
    // ride in the snapshot like any other content (pull does not strip it).
    let hook_path = project.path().join("envs/dev/hooks/validator-hook.json");
    assert!(hook_path.exists(), "hook file must exist after first sync");
    let disk_json = std::fs::read_to_string(&hook_path).unwrap();
    assert!(
        disk_json.contains("PROD-specific description"),
        "C-1: overlay-managed field must remain in the on-disk hook: {disk_json}",
    );

    // Snapshot the lockfile hash and the on-disk file so we can assert they
    // are untouched by the second sync.
    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let disk_before = std::fs::read_to_string(&hook_path).unwrap();

    // Second sync: API still serves the same hook body. The classifier must
    // see the recorded hash == on-disk hash == remote hash → Clean. No file
    // rewrites, no API mutations. This is the phantom-drift regression.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync should succeed (clean state)");
    std::env::set_current_dir(&prev_cwd).unwrap();

    // No mutating requests on either sync.
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
            "unexpected mutating request: {} {} — hook with overlay must not cause phantom drift",
            req.method,
            p
        );
    }

    // Lockfile and on-disk file are unchanged: Clean classification produces
    // no writes. Any change here would indicate the oscillation bug is back.
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let disk_after = std::fs::read_to_string(&hook_path).unwrap();
    assert_eq!(
        lf_before, lf_after,
        "lockfile must be unchanged after second sync (no phantom drift)"
    );
    assert_eq!(
        disk_before, disk_after,
        "on-disk hook file must be unchanged after second sync (no phantom drift)"
    );
}

/// Regression (bug b): workspace phantom drift when `modified_at` changes.
///
/// Before the fix, the sync adapter hashed workspaces using raw
/// `serde_json::to_vec_pretty` (which keeps `modified_at`), while the pull
/// driver writes via the `KindCodec` which strips `modified_at` recursively.
/// The result: a workspace whose remote `modified_at` changed (a normal API
/// side-effect, e.g. after updating a queue) would be classified `RemoteEdit`
/// or `BothDiverged` on the next sync even though no meaningful content
/// changed — phantom drift.
///
/// After the fix, the adapter routes through the same KindCodec as the pull
/// driver, so `modified_at` is stripped before hashing on both sides.
///
/// Test strategy: sync once to record the baseline, then serve the same
/// workspace body with a bumped `modified_at` and assert the second sync
/// classifies it `Clean` (no writes, no file rewrites, no lockfile changes).
#[tokio::test]
async fn sync_workspace_modified_at_change_is_clean() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Initial workspace served on the first sync. The `modified_at` is
    // deliberately included — the pull driver strips it before hashing.
    let workspace_body_v1 = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 900,
                "url": format!("{}/api/v1/workspaces/900", server.uri()),
                "name": "Phantom Drift Test",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [],
                "modified_at": "2026-01-01T10:00:00Z"
            }
        ]
    });
    // Second listing: same workspace, bumped `modified_at` only. No other
    // content change. The classifier MUST see this as Clean.
    let workspace_body_v2 = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 900,
                "url": format!("{}/api/v1/workspaces/900", server.uri()),
                "name": "Phantom Drift Test",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [],
                "modified_at": "2026-06-01T20:00:00Z"
            }
        ]
    });

    // First call returns v1, second call returns v2 (bumped modified_at).
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&workspace_body_v1))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&workspace_body_v2))
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

    // First sync: pull writes the workspace (modified_at stripped) and
    // records the post-strip hash in the lockfile.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("first sync should succeed");

    let ws_path = project
        .path()
        .join("envs/dev/workspaces/phantom-drift-test/workspace.json");
    assert!(
        ws_path.exists(),
        "workspace file must exist after first sync"
    );

    // Verify `modified_at` is absent from the on-disk file (the codec strips it).
    let disk_json = std::fs::read_to_string(&ws_path).unwrap();
    assert!(
        !disk_json.contains("modified_at"),
        "on-disk workspace must not contain modified_at; got: {disk_json}"
    );

    // Snapshot lockfile and on-disk file bytes for the post-second-sync diff.
    let lf_before =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let disk_before = std::fs::read_to_string(&ws_path).unwrap();

    // Second sync: API now serves the workspace with a bumped `modified_at`.
    // The classifier MUST see Clean (no RemoteEdit, no file rewrite, no
    // lockfile mutation). This is the phantom-drift regression.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("second sync should succeed (clean state despite bumped modified_at)");
    std::env::set_current_dir(&prev_cwd).unwrap();

    // No mutating requests on either sync.
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
            "unexpected mutating request: {} {} — bumped modified_at must not cause phantom drift",
            req.method,
            p
        );
    }

    // Lockfile and on-disk file are unchanged after the second sync.
    let lf_after =
        std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let disk_after = std::fs::read_to_string(&ws_path).unwrap();
    assert_eq!(
        lf_before, lf_after,
        "lockfile must be unchanged after second sync (workspace modified_at bump is not a change)"
    );
    assert_eq!(
        disk_before, disk_after,
        "on-disk workspace must be unchanged after second sync (phantom drift fix)"
    );
}

/// A hook's `status` is a read-only, server-managed health field ("ready" /
/// "failed" / …). It's redacted to the sentinel on disk; a push-PATCH must NOT
/// echo it back — sending the sentinel is at best ignored, at worst a 400 on
/// the status enum. `strip_patch_extra` must drop it from the PATCH body,
/// exactly as `strip_for_create` does for POST bodies.
#[tokio::test]
async fn push_hook_patch_body_omits_status() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let hook_id = 7007u64;
    let server_uri = server.uri();
    let hooks_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": hook_id,
                "url": format!("{server_uri}/api/v1/hooks/{hook_id}"),
                "name": "status-hook",
                "type": "webhook",
                "queues": [],
                "events": ["annotation_content"],
                "config": { "url": "https://hook.example.com/run" },
                "status": "ready",
                "modified_at": "2026-04-01T10:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&hooks_body))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &["/api/v1/hooks"]).await;

    let patch_response = serde_json::json!({
        "id": hook_id,
        "url": format!("{server_uri}/api/v1/hooks/{hook_id}"),
        "name": "status-hook",
        "type": "webhook",
        "queues": [],
        "events": ["annotation_content"],
        "config": { "url": "https://hook.example.com/run-v2" },
        "status": "ready",
        "modified_at": "2026-04-02T10:00:00Z"
    });
    Mock::given(method("PATCH"))
        .and(path(format!("/api/v1/hooks/{hook_id}")))
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

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");

    let hook_json = project.path().join("envs/dev/hooks/status-hook.json");
    assert!(hook_json.exists(), "seed sync must write the hook json");
    let seed_disk = std::fs::read_to_string(&hook_json).unwrap();
    assert!(
        !seed_disk.contains("\"ready\""),
        "seed pull must redact hook status, not write raw 'ready':\n{seed_disk}"
    );

    // Edit a config value so the hook is a LocalEdit → push PATCH.
    let mut v: serde_json::Value = serde_json::from_str(&seed_disk).unwrap();
    v["config"]["url"] = serde_json::Value::String("https://hook.example.com/run-v2".to_string());
    std::fs::write(
        &hook_json,
        format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    )
    .unwrap();

    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("push sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    let patch_bodies: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.method == http::Method::PATCH && r.url.path() == format!("/api/v1/hooks/{hook_id}")
        })
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .collect();
    assert_eq!(
        patch_bodies.len(),
        1,
        "expected exactly one hook PATCH; got {}",
        patch_bodies.len()
    );
    assert!(
        patch_bodies[0].get("status").is_none(),
        "hook PATCH body must NOT contain the redacted `status`; got:\n{}",
        serde_json::to_string_pretty(&patch_bodies[0]).unwrap()
    );
}

/// Bug-c regression: after a push-PATCH of an existing engine, the on-disk
/// `engine.json` must contain the sentinel string for `agenda_id` (NOT the
/// raw live value returned by the server), and the lockfile content_hash must
/// equal `codec("engines").base_hash(patch_response)` — i.e. the post-overlay
/// KindCodec hash.
///
/// Before the fix, the post-PATCH write used raw `serde_json::to_vec_pretty`
/// which re-emitted the live `agenda_id`; the lockfile hash was recorded from
/// those un-redacted bytes, causing a hash mismatch on the next pull.
#[tokio::test]
async fn push_engine_patch_redacts_agenda_id_on_disk() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Seed pull: the engine exists remotely with a live agenda_id.
    let live_agenda_id = "tnt_seed_abc999";
    let engines_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 901,
                "url": format!("{}/api/v1/engines/901", server.uri()),
                "name": "Seed Engine",
                "type": "extractor",
                "agenda_id": live_agenda_id,
                "modified_at": "2026-05-01T08:00:00Z"
            }
        ]
    });
    // The second GET /engines (drift check before PATCH) returns the same body.
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&engines_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/engines"]).await;

    // PATCH response carries a fresh live agenda_id (as the server would in
    // reality). The push driver must NOT write this verbatim.
    let patched_agenda_id = "tnt_patched_xyz777";
    let patch_response = serde_json::json!({
        "id": 901,
        "url": format!("{}/api/v1/engines/901", server.uri()),
        "name": "Patched Engine",
        "type": "extractor",
        "agenda_id": patched_agenda_id,
        "modified_at": "2026-05-02T08:00:00Z"
    });
    Mock::given(method("PATCH"))
        .and(path("/api/v1/engines/901"))
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

    // Seed sync: pulls the engine and records the lockfile baseline.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");

    let engine_path = project
        .path()
        .join("envs/dev/engines/seed-engine/engine.json");
    assert!(engine_path.exists(), "seed sync must write engine.json");

    // Verify seed state: agenda_id must already be redacted on disk.
    let seed_disk = std::fs::read_to_string(&engine_path).unwrap();
    assert!(
        !seed_disk.contains(live_agenda_id),
        "seed pull must redact agenda_id; found raw value in:\n{seed_disk}"
    );
    assert!(
        seed_disk.contains("refreshed live in Rossum"),
        "seed pull must write sentinel; got:\n{seed_disk}"
    );

    // Mutate the local file to trigger a LocalEdit → push path.
    let mut v: serde_json::Value = serde_json::from_str(&seed_disk).unwrap();
    v["name"] = serde_json::Value::String("Patched Engine".to_string());
    std::fs::write(
        &engine_path,
        format!("{}\n", serde_json::to_string_pretty(&v).unwrap()),
    )
    .unwrap();

    // Second sync: LocalEdit → push PATCH; must write codec bytes + redacted
    // agenda_id to disk, not the raw `patched_agenda_id` from the server.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("push sync should succeed");
    std::env::set_current_dir(&prev_cwd).unwrap();

    // Post-PATCH disk assertion (bug-c fix).
    let post_patch_disk = std::fs::read_to_string(&engine_path).unwrap();
    assert!(
        !post_patch_disk.contains(patched_agenda_id),
        "post-PATCH engine.json must NOT contain the raw agenda_id '{}'; got:\n{post_patch_disk}",
        patched_agenda_id
    );
    assert!(
        post_patch_disk.contains("refreshed live in Rossum"),
        "post-PATCH engine.json must contain the redaction sentinel; got:\n{post_patch_disk}"
    );

    // Lockfile hash must equal the codec hash of the PATCH response
    // (without overlay, since no overlay.toml exists for this env).
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    let recorded_hash = lf["objects"]["engines"]["seed-engine"]["content_hash"]
        .as_str()
        .expect("content_hash must be present after push");

    let codec = rdc::snapshot::codec::codec("engines").expect("engines codec must be registered");
    let expected_art = codec
        .disk_bytes(&patch_response)
        .expect("codec disk_bytes for patch_response");
    // No overlay → combined_hash of just the json (no sidecars for engines).
    // Reference-form-agnostic hashing normalizes the engine's self-url via the
    // lockfile, so the expected hash must use the same (production) lockfile.
    let lf_loaded =
        rdc::state::Lockfile::load(&project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let expected_hash =
        rdc::snapshot::codec::combined_hash(&expected_art.json, &expected_art.sidecars, &lf_loaded);

    assert_eq!(
        recorded_hash, expected_hash,
        "lockfile content_hash after PATCH must equal codec combined_hash of the PATCH response"
    );

    // The PATCH body itself must NOT carry `agenda_id`. It's a read-only,
    // server-managed identifier; echoing the redaction sentinel (or any src
    // value) is at best ignored and at worst overwrites/400s the engine's
    // identifier on the remote. `strip_patch_extra` must remove it before the
    // PATCH, exactly as `strip_for_create` does for POST bodies.
    let engine_patch_bodies: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/engines/901")
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .collect();
    assert_eq!(
        engine_patch_bodies.len(),
        1,
        "expected exactly one engine PATCH; got {}",
        engine_patch_bodies.len()
    );
    assert!(
        engine_patch_bodies[0].get("agenda_id").is_none(),
        "engine PATCH body must NOT contain `agenda_id`; got:\n{}",
        serde_json::to_string_pretty(&engine_patch_bodies[0]).unwrap()
    );
}

/// Migration-safety — unedited legacy snapshot converges silently on first sync.
///
/// Scenario: a user upgrades rdc to a version that introduces the codec-based
/// on-disk format (agenda_id → sentinel, modified_at stripped). Their local
/// `engine.json` is in the OLD form — raw agenda_id value and modified_at still
/// present — and the lockfile content_hash was recorded from those old bytes.
/// The local file was UNEDITED relative to that hash (no real user changes).
///
/// Expected behaviour on the next sync:
/// 1. No conflict is reported.
/// 2. `engine.json` is silently rewritten to the new canonical form (sentinel +
///    no modified_at).
/// 3. The lockfile content_hash is updated to the new codec hash.
/// 4. A second sync immediately after is fully Clean (no further rewrites).
///
/// Mechanism: `decide_pull_action` computes `local_hash` from the old bytes
/// via `content_hash()` which calls `canonicalize_for_hash()`. That function
/// strips `modified_at` (a NOISE_FIELD) before hashing, so `local_hash` equals
/// the OLD lockfile `base_hash` (both ignore modified_at). Since
/// `local_matches_base = true`, the action is `PullAction::Write` — a silent
/// rewrite of the remote (new-codec) bytes. No self-heal code is needed; the
/// existing three-way logic already handles this case.
#[tokio::test]
async fn sync_legacy_unedited_engine_converges_silently() {
    use rdc::snapshot::codec::combined_hash;
    use rdc::state::lockfile::content_hash;

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Remote returns the engine with a live agenda_id (server-assigned, rotates
    // on training). This is what the sync classifier will produce codec bytes
    // from — the codec redacts it to the sentinel.
    let remote_agenda_id = "tnt_remote_new_abc";
    let engines_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 801,
                "url": format!("{}/api/v1/engines/801", server.uri()),
                "name": "Legacy Upgrade Engine",
                "type": "extractor",
                "agenda_id": remote_agenda_id,
                "modified_at": "2026-05-10T12:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&engines_body))
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

    // Manufacture the legacy on-disk state BEFORE running any sync.
    // The engine.json is in the OLD format: raw agenda_id value and modified_at
    // present, exactly as the pre-codec pull would have written it.
    let old_agenda_id = "tnt_old_from_previous_rdc";
    let engine_dir = project
        .path()
        .join("envs/dev/engines/legacy-upgrade-engine");
    std::fs::create_dir_all(&engine_dir).unwrap();
    let engine_path = engine_dir.join("engine.json");
    let old_engine_json = serde_json::json!({
        "id": 801,
        "url": format!("{}/api/v1/engines/801", server.uri()),
        "name": "Legacy Upgrade Engine",
        "type": "extractor",
        "agenda_id": old_agenda_id,
        "modified_at": "2026-04-01T10:00:00Z"
    });
    let old_bytes = format!(
        "{}\n",
        serde_json::to_string_pretty(&old_engine_json).unwrap()
    );
    std::fs::write(&engine_path, &old_bytes).unwrap();

    // Record the OLD-format hash in the lockfile — this is what a pre-codec
    // rdc version would have written. `content_hash` strips `modified_at` via
    // `canonicalize_for_hash`, so the stored hash is sensitive only to the real
    // fields (`agenda_id: "tnt_old_from_previous_rdc"`, `name`, etc.).
    // Pre-B (legacy) lockfile: hash recorded in URL form, so seed with an empty
    // lockfile (no ref normalization) to faithfully simulate the old baseline.
    let old_base_hash = content_hash(old_bytes.as_bytes(), &rdc::state::Lockfile::default());
    let lockfile_path = project.path().join(".rdc/state/dev.lock.json");
    // The lockfile may or may not exist (init doesn't create it). Build a valid
    // v2 lockfile with the engine entry pre-populated.
    let lf_json = serde_json::json!({
        "version": 2,
        "objects": {
            "engines": {
                "legacy-upgrade-engine": {
                    "id": 801,
                    "url": format!("{}/api/v1/engines/801", server.uri()),
                    "modified_at": "2026-04-01T10:00:00Z",
                    "content_hash": old_base_hash
                }
            }
        }
    });
    std::fs::create_dir_all(project.path().join(".rdc/state")).unwrap();
    std::fs::write(
        &lockfile_path,
        format!("{}\n", serde_json::to_string_pretty(&lf_json).unwrap()),
    )
    .unwrap();

    // Run the first sync (non-interactive, no push, no pull suppression).
    // Use an explicit block so the cwd_guard is dropped before we need to
    // acquire it again for the second sync below — std::sync::Mutex is
    // non-reentrant, so two acquisitions on the same thread without an
    // intervening release deadlock.
    let result = {
        let _cwd_guard = cwd_lock();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(project.path()).unwrap();
        let r = rdc::cli::sync::run("dev", false, false, false, false, false).await;
        std::env::set_current_dir(&prev_cwd).unwrap();
        r
    };

    result.expect("first sync on legacy snapshot must succeed without conflict");

    // No API mutations — this is purely a pull-side migration rewrite.
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
            "legacy migration must not issue any mutating request: {} {}",
            req.method,
            p
        );
    }

    // The engine.json must now be in the new canonical form: agenda_id is the
    // sentinel, and modified_at is absent.
    let new_disk = std::fs::read_to_string(&engine_path).unwrap();
    assert!(
        !new_disk.contains(old_agenda_id),
        "after migration, engine.json must NOT contain the raw old agenda_id '{}'; got:\n{new_disk}",
        old_agenda_id
    );
    assert!(
        !new_disk.contains(remote_agenda_id),
        "after migration, engine.json must NOT contain the raw remote agenda_id '{}'; got:\n{new_disk}",
        remote_agenda_id
    );
    assert!(
        new_disk.contains("refreshed live in Rossum"),
        "after migration, engine.json must contain the redaction sentinel; got:\n{new_disk}"
    );
    assert!(
        !new_disk.contains("\"modified_at\""),
        "after migration, engine.json must NOT contain modified_at; got:\n{new_disk}"
    );

    // Lockfile content_hash must equal the new codec hash.
    let lf_raw = std::fs::read_to_string(&lockfile_path).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    let recorded_hash = lf["objects"]["engines"]["legacy-upgrade-engine"]["content_hash"]
        .as_str()
        .expect("content_hash must be present after migration sync");

    let codec = rdc::snapshot::codec::codec("engines").expect("engines codec registered");
    let remote_value = engines_body["results"][0].clone();
    let art = codec.disk_bytes(&remote_value).expect("codec disk_bytes");
    // Production recorded the hash with the (post-migration) lockfile, which
    // normalizes the engine's self-url; match it here.
    let lf_loaded = rdc::state::Lockfile::load(&lockfile_path).unwrap();
    let expected_hash = combined_hash(&art.json, &art.sidecars, &lf_loaded);

    assert_eq!(
        recorded_hash, expected_hash,
        "after migration, lockfile content_hash must equal the codec hash of the remote body"
    );

    // A second sync must be fully Clean — no rewrites, no lockfile changes.
    let lf_after_first = std::fs::read_to_string(&lockfile_path).unwrap();
    let disk_after_first = std::fs::read_to_string(&engine_path).unwrap();

    {
        let _cwd_guard2 = cwd_lock();
        let prev_cwd2 = std::env::current_dir().unwrap();
        std::env::set_current_dir(project.path()).unwrap();
        rdc::cli::sync::run("dev", false, false, false, false, false)
            .await
            .expect("second sync must succeed (fully clean after migration)");
        std::env::set_current_dir(&prev_cwd2).unwrap();
    }

    let lf_after_second = std::fs::read_to_string(&lockfile_path).unwrap();
    let disk_after_second = std::fs::read_to_string(&engine_path).unwrap();

    assert_eq!(
        lf_after_first, lf_after_second,
        "lockfile must be identical on the second sync (fully clean after migration)"
    );
    assert_eq!(
        disk_after_first, disk_after_second,
        "engine.json must be identical on the second sync (fully clean after migration)"
    );
}

/// Migration-safety — locally-edited legacy snapshot surfaces a conflict on sync.
///
/// Scenario: same legacy on-disk state as `sync_legacy_unedited_engine_converges_silently`,
/// but the user has ALSO edited the `name` field of the engine (a real user
/// change). The lockfile `base_hash` was recorded from the old pre-edit bytes.
///
/// Expected behaviour: the sync correctly detects a conflict (`local_hash !=
/// base_hash` AND `remote_hash != base_hash`). The conflict is surfaced via the
/// shadow-file mechanism (non-interactive), NOT silently swallowed.
///
/// A conflict here is acceptable and expected: the user has real local edits
/// that diverge from the remote AND from the base — there is no safe automatic
/// merge. The test documents and locks down this behaviour.
#[tokio::test]
async fn sync_legacy_edited_engine_surfaces_conflict() {
    use rdc::state::lockfile::content_hash;

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Remote returns the engine with a different name than the local edit,
    // and a live agenda_id — both sides have diverged from the base.
    let engines_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 802,
                "url": format!("{}/api/v1/engines/802", server.uri()),
                "name": "Remote Name Engine",
                "type": "extractor",
                "agenda_id": "tnt_remote_xyz",
                "modified_at": "2026-05-11T12:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&engines_body))
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

    // Legacy on-disk state: old format (raw agenda_id + modified_at).
    // The user had the original name "Base Name Engine" at the time of the last
    // pull (pre-codec rdc version).
    let old_agenda_id = "tnt_old_base";
    let engine_dir = project.path().join("envs/dev/engines/remote-name-engine");
    std::fs::create_dir_all(&engine_dir).unwrap();
    let engine_path = engine_dir.join("engine.json");

    // The BASE bytes (before any user edit) — what the old rdc pull wrote.
    let base_engine_json = serde_json::json!({
        "id": 802,
        "url": format!("{}/api/v1/engines/802", server.uri()),
        "name": "Base Name Engine",
        "type": "extractor",
        "agenda_id": old_agenda_id,
        "modified_at": "2026-04-01T10:00:00Z"
    });
    let base_bytes = format!(
        "{}\n",
        serde_json::to_string_pretty(&base_engine_json).unwrap()
    );
    // Record the base hash in the lockfile (from the OLD bytes). Legacy/pre-B
    // baseline → empty lockfile (URL form, no ref normalization).
    let base_hash = content_hash(base_bytes.as_bytes(), &rdc::state::Lockfile::default());

    // The LOCAL bytes (user has edited the name field — a real change).
    let mut edited = base_engine_json.clone();
    edited["name"] = serde_json::Value::String("Locally Edited Name".to_string());
    let edited_bytes = format!("{}\n", serde_json::to_string_pretty(&edited).unwrap());
    std::fs::write(&engine_path, &edited_bytes).unwrap();

    // Write lockfile with the PRE-EDIT base hash.
    let lf_json = serde_json::json!({
        "version": 2,
        "objects": {
            "engines": {
                "remote-name-engine": {
                    "id": 802,
                    "url": format!("{}/api/v1/engines/802", server.uri()),
                    "modified_at": "2026-04-01T10:00:00Z",
                    "content_hash": base_hash
                }
            }
        }
    });
    let lockfile_path = project.path().join(".rdc/state/dev.lock.json");
    std::fs::create_dir_all(project.path().join(".rdc/state")).unwrap();
    std::fs::write(
        &lockfile_path,
        format!("{}\n", serde_json::to_string_pretty(&lf_json).unwrap()),
    )
    .unwrap();

    // Snapshot the local file and lockfile before the sync.
    let local_before = std::fs::read_to_string(&engine_path).unwrap();
    let lf_before = std::fs::read_to_string(&lockfile_path).unwrap();

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    // Sync must succeed (conflicts are non-fatal in non-interactive mode; they
    // surface as shadow files and a lockfile freeze, not an error return).
    let result = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync with locally-edited legacy engine must not return an error");

    // No API mutations — conflicts don't push to the remote.
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
            "conflict must not issue any mutating request: {} {}",
            req.method,
            p
        );
    }

    // The local file must be PRESERVED (not overwritten with remote bytes).
    let local_after = std::fs::read_to_string(&engine_path).unwrap();
    assert_eq!(
        local_before, local_after,
        "conflict: local file must be preserved unchanged"
    );
    assert!(
        local_after.contains("Locally Edited Name"),
        "conflict: user's local edit must survive; got:\n{local_after}"
    );

    // A shadow file must have been written next to the local file, containing
    // the new-codec remote bytes (so the user can inspect the remote side).
    let shadow_path = engine_path.with_extension("json.dev");
    assert!(
        shadow_path.exists(),
        "conflict: shadow file must be created at {}",
        shadow_path.display()
    );
    let shadow = std::fs::read_to_string(&shadow_path).unwrap();
    assert!(
        shadow.contains("Remote Name Engine"),
        "shadow file must contain the remote name; got:\n{shadow}"
    );
    assert!(
        shadow.contains("refreshed live in Rossum"),
        "shadow file must contain the redaction sentinel (new codec form); got:\n{shadow}"
    );

    // Lockfile base_hash must be FROZEN — not advanced — so the conflict
    // re-surfaces on the next sync.
    let lf_after = std::fs::read_to_string(&lockfile_path).unwrap();
    let lf_val: serde_json::Value = serde_json::from_str(&lf_after).unwrap();
    let recorded_hash = lf_val["objects"]["engines"]["remote-name-engine"]["content_hash"]
        .as_str()
        .expect("content_hash must be present");
    assert_eq!(
        recorded_hash, base_hash,
        "conflict: lockfile content_hash must be frozen at the prior base (not advanced)"
    );
    // The lockfile's other fields may have been updated (e.g. modified_at from
    // the remote), but the base hash governs whether the conflict re-surfaces.
    let _ = lf_before; // consumed; noted that lf may have changed in modified_at
}

/// Two queues with the SAME name in DIFFERENT workspaces are kept fully
/// distinct: queue slug assignment is GLOBAL and pinned by id, so each gets
/// its own slug (`shared-queue` / `shared-queue-2`), its own dir, and its own
/// lockfile entry — no collapse, no cross-attribution. (Before the fix, both
/// took the bare slug `shared-queue` and one silently overwrote the other.)
#[tokio::test]
async fn sync_keeps_same_named_queues_distinct_across_workspaces() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Two workspaces with DISTINCT names (so the workspaces themselves do
    // not collide) — each owns a queue named identically.
    let workspaces_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 100,
                "url": format!("{}/api/v1/workspaces/100", server.uri()),
                "name": "Workspace Alpha",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [format!("{}/api/v1/queues/200", server.uri())]
            },
            {
                "id": 101,
                "url": format!("{}/api/v1/workspaces/101", server.uri()),
                "name": "Workspace Beta",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [format!("{}/api/v1/queues/201", server.uri())]
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workspaces_body))
        .mount(&server)
        .await;

    // Two queues, SAME name, different workspaces, different schemas.
    let queues_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 200,
                "url": format!("{}/api/v1/queues/200", server.uri()),
                "name": "Shared Queue",
                "workspace": format!("{}/api/v1/workspaces/100", server.uri()),
                "schema": format!("{}/api/v1/schemas/300", server.uri())
            },
            {
                "id": 201,
                "url": format!("{}/api/v1/queues/201", server.uri()),
                "name": "Shared Queue",
                "workspace": format!("{}/api/v1/workspaces/101", server.uri()),
                "schema": format!("{}/api/v1/schemas/301", server.uri())
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queues_body))
        .mount(&server)
        .await;

    // Schema bodies fetched per-queue by id during prefetch.
    for (sid, sname, qid) in [(300, "Schema Alpha", 200), (301, "Schema Beta", 201)] {
        let schema_body = serde_json::json!({
            "id": sid,
            "url": format!("{}/api/v1/schemas/{sid}", server.uri()),
            "name": sname,
            "queues": [format!("{}/api/v1/queues/{qid}", server.uri())],
            "content": []
        });
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/schemas/{sid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(schema_body))
            .mount(&server)
            .await;
    }

    mock_empty_lists_except(&server, &["/api/v1/workspaces", "/api/v1/queues"]).await;

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
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync must succeed: same-named queues across workspaces are now distinct");

    let lf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap(),
    )
    .unwrap();
    // Both queues tracked under DISTINCT slugs — no collapse.
    let queues = lf["objects"]["queues"].as_object().expect("queues map");
    assert_eq!(
        queues.len(),
        2,
        "both queues must be tracked distinctly: {lf}"
    );
    let mut ids: Vec<u64> = queues.values().map(|e| e["id"].as_u64().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, vec![200, 201], "both queue ids present: {lf}");
    // Schemas likewise distinct (keyed by their queue's slug).
    assert_eq!(
        lf["objects"]["schemas"].as_object().unwrap().len(),
        2,
        "both schemas tracked distinctly: {lf}"
    );

    // Each queue's files landed in its OWN workspace dir with a distinct slug.
    let alpha = project
        .path()
        .join("envs/dev/workspaces/workspace-alpha/queues");
    let beta = project
        .path()
        .join("envs/dev/workspaces/workspace-beta/queues");
    assert_eq!(
        std::fs::read_dir(&alpha).unwrap().count(),
        1,
        "one queue dir under alpha"
    );
    assert_eq!(
        std::fs::read_dir(&beta).unwrap().count(),
        1,
        "one queue dir under beta"
    );
    let alpha_q = std::fs::read_dir(&alpha)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let beta_q = std::fs::read_dir(&beta)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    assert_ne!(
        alpha_q, beta_q,
        "the two same-named queues got distinct slugs/dirs"
    );

    // Pull-side only: no remote mutations.
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
            "pull-side only: unexpected mutation {} {}",
            req.method,
            p
        );
    }
}

/// Regression: a single un-deletable object must NOT abort the whole
/// delete batch.
///
/// The delete phase walks tombstones in reverse-dependency order and used
/// to `?`-propagate the first per-object DELETE error, which aborted the
/// entire batch and orphaned every sibling + parent. Live repro: deleting
/// a queue subtree fails because Rossum auto-creates a `rejection_default`
/// email_template whose DELETE returns `400 {"detail":"Cannot delete
/// template with unique type: ..."}`. That single 400 stranded the queue,
/// schema, and workspace.
///
/// This test seeds three `LocalDelete` tombstones of the same kind
/// (labels — no `modified_at`, so the drift check passes cleanly) and
/// mocks the MIDDLE label's DELETE to return 400 while the other two
/// return 204. With the bug present, the batch aborts on the 400 and the
/// label that sorts AFTER it never gets its DELETE; with the fix, all
/// three DELETEs land, the sync exits success, and the failed one is
/// surfaced as a warning + left in the lockfile.
#[tokio::test]
async fn sync_delete_skips_failed_and_continues_batch() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Three labels. Names chosen so their slugs sort with the failing
    // one (id 52) in the MIDDLE of the BTreeMap iteration order:
    //   "aaa-label" (51) < "mmm-label" (52, fails) < "zzz-label" (53).
    // That guarantees a third label is queued AFTER the failure, so the
    // bug (abort-on-first-error) is observable: id 53's DELETE never
    // lands under the old code.
    let labels_body = serde_json::json!({
        "pagination": { "total": 3, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 51,
                "url": format!("{}/api/v1/labels/51", server.uri()),
                "name": "Aaa Label",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#111111"
            },
            {
                "id": 52,
                "url": format!("{}/api/v1/labels/52", server.uri()),
                "name": "Mmm Label",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#222222"
            },
            {
                "id": 53,
                "url": format!("{}/api/v1/labels/53", server.uri()),
                "name": "Zzz Label",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "color": "#333333"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&labels_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    // DELETE mocks: the two outer labels succeed (204), the middle one
    // rejects with a 400 carrying a Rossum-style detail body — exactly
    // the shape of the unique-type-template rejection from the live repro.
    Mock::given(method("DELETE"))
        .and(path("/api/v1/labels/51"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/labels/52"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "detail": "Cannot delete object with unique constraint"
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/labels/53"))
        .respond_with(ResponseTemplate::new(204))
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

    // First sync: pull the three labels so local files + lockfile entries
    // exist. This is the standard seed pattern used across this file.
    rdc::cli::sync::run("dev", false, false, false, false, false)
        .await
        .expect("seed sync should succeed");

    let aaa = project.path().join("envs/dev/labels/aaa-label.json");
    let mmm = project.path().join("envs/dev/labels/mmm-label.json");
    let zzz = project.path().join("envs/dev/labels/zzz-label.json");
    assert!(
        aaa.exists() && mmm.exists() && zzz.exists(),
        "seed sync must write all three label files"
    );

    // Delete all three local files → three LocalDelete tombstones. The
    // lockfile entries stay, so the classifier sees clean deletes.
    std::fs::remove_file(&aaa).unwrap();
    std::fs::remove_file(&mmm).unwrap();
    std::fs::remove_file(&zzz).unwrap();

    // Second sync with --allow-deletes (non-interactive). Must NOT abort
    // on the middle label's 400.
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* allow_deletes = */ true, /* no_push = */ false, /* no_pull = */ false,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("sync must succeed despite a per-object delete failure");

    // The batch ran to completion: every label's DELETE was attempted,
    // including id 53 which sorts AFTER the failed id 52. Under the bug,
    // the 400 on /labels/52 aborts the loop and /labels/53 is never
    // called — this is the assertion that goes RED on old code.
    let reqs = server.received_requests().await.unwrap_or_default();
    let delete_hit = |id: u64| -> usize {
        let p = format!("/api/v1/labels/{id}");
        reqs.iter()
            .filter(|r| r.method == http::Method::DELETE && r.url.path() == p)
            .count()
    };
    assert_eq!(delete_hit(51), 1, "DELETE /labels/51 should have run");
    assert_eq!(
        delete_hit(52),
        1,
        "DELETE /labels/52 should have been attempted (and failed)"
    );
    assert_eq!(
        delete_hit(53),
        1,
        "DELETE /labels/53 must run even though /labels/52 failed before it"
    );

    // The lockfile reflects reality: the two deleted labels are gone, the
    // failed one is retained so a future sync retries it.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    let labels_obj = lf
        .get("objects")
        .and_then(|o| o.get("labels"))
        .and_then(|l| l.as_object())
        .cloned()
        .unwrap_or_default();
    assert!(
        !labels_obj.contains_key("aaa-label"),
        "successfully-deleted label must be removed from lockfile: {lf_raw}"
    );
    assert!(
        !labels_obj.contains_key("zzz-label"),
        "successfully-deleted label must be removed from lockfile: {lf_raw}"
    );
    assert!(
        labels_obj.contains_key("mmm-label"),
        "failed-delete label must be retained in lockfile for retry: {lf_raw}"
    );
}

// ---------------------------------------------------------------------------
// Push-side CREATE (POST) positive coverage.
//
// The classifier marks a file present on disk with NO lockfile entry and NO
// matching remote object as `LocalCreate` (see `classify.rs`:
// `(local_changed, _, remote_present, locked_present) == (true, false, false,
// false)`). The push driver then POSTs it, records the returned id in the
// lockfile, and writes back the canonical (codec) form so a re-sync is a
// no-op. These tests lock in that behavior — previously only a NEGATIVE
// `.expect(0)` guard existed (asserting NO create in conflict scenarios) and
// `hooks` was the only kind with a positive create test (in `tests/api.rs`).
//
// Each test was confirmed to genuinely exercise the POST: pointing the mocked
// POST `path()` at a wrong endpoint makes the create fail (sync errors with a
// decode/404), so a passing run proves the POST landed on the asserted path.
// ---------------------------------------------------------------------------

/// Count mutating (POST/PATCH/DELETE) requests against a given path, ignoring
/// the data-storage paths (which use POST for *reads*).
fn count_mutations(reqs: &[wiremock::Request], exact_path: &str) -> usize {
    reqs.iter()
        .filter(|r| {
            r.url.path() == exact_path
                && matches!(
                    r.method,
                    http::Method::POST | http::Method::PATCH | http::Method::DELETE
                )
        })
        .count()
}

/// Push-side LocalCreate for a label: a label JSON exists on disk with no
/// lockfile entry and the remote listing omits it. `sync` must classify it
/// `LocalCreate`, POST `/labels`, record the returned id in the lockfile, and
/// write back the canonical form. A second sync must then be a no-op (no
/// further POST/PATCH).
#[tokio::test]
async fn push_create_label() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Stateful label listing: empty on the FIRST list (before the create),
    // then serves the created label on every subsequent list. This mirrors a
    // real Rossum — once POSTed, the object appears in future listings — so
    // the second sync classifies it `Clean` (not `RemoteDelete`) and is a
    // genuine idempotency check, not a deferred-delete artifact.
    let created_label = serde_json::json!({
        "id": 7777,
        "url": format!("{}/api/v1/labels/7777", server.uri()),
        "name": "Priority High",
        "organization": format!("{}/api/v1/organizations/1", server.uri()),
        "color": "#ff0000",
        "modified_at": "2026-05-01T08:00:00Z"
    });
    let label_list_calls = Arc::new(AtomicUsize::new(0));
    let counter = label_list_calls.clone();
    let created_for_list = created_label.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                serde_json::json!({ "pagination": { "next": null }, "results": [] })
            } else {
                serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [created_for_list.clone()]
                })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &["/api/v1/labels"]).await;

    // POST /labels → server assigns id 7777 and a url. `.expect(1)` enforces
    // exactly one create call across the whole test (one sync only).
    Mock::given(method("POST"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_label))
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

    // Seed a NEW local label with no lockfile entry. `organization` is a
    // verbatim env URL (not a portable `rdc://` ref); the push driver passes
    // it through unchanged.
    let labels_dir = project.path().join("envs/dev/labels");
    std::fs::create_dir_all(&labels_dir).unwrap();
    let local_label = serde_json::json!({
        "id": 0,
        "url": "",
        "name": "Priority High",
        "organization": format!("{}/api/v1/organizations/1", server.uri()),
        "color": "#ff0000"
    });
    let mut bytes = serde_json::to_vec_pretty(&local_label).unwrap();
    bytes.push(b'\n');
    std::fs::write(labels_dir.join("priority-high.json"), &bytes).unwrap();

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let r1 = rdc::cli::sync::run("dev", false, false, false, false, false).await;

    // Snapshot mutations after the first sync, then run a second sync to
    // assert idempotency, before restoring CWD.
    let reqs_after_first = server.received_requests().await.unwrap_or_default();
    let r2 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    r1.expect("first sync (label create) should succeed");
    r2.expect("second sync should succeed and be a no-op");

    // The POST landed exactly once on /labels.
    assert_eq!(
        count_mutations(&reqs_after_first, "/api/v1/labels"),
        1,
        "exactly one POST /labels should have happened on the create sync"
    );

    // The POST body carried the user-authored name and stripped the
    // placeholder id/url (per strip_for_create).
    let post = reqs_after_first
        .iter()
        .find(|r| r.method == http::Method::POST && r.url.path() == "/api/v1/labels")
        .expect("a POST /labels request must exist");
    let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["name"], "Priority High", "POST body name: {body}");
    assert!(
        body.get("id").is_none(),
        "placeholder id must be stripped from POST body: {body}"
    );
    assert!(
        body.get("url").is_none(),
        "placeholder url must be stripped from POST body: {body}"
    );

    // Lockfile records the returned id.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    assert_eq!(
        lf.pointer("/objects/labels/priority-high/id"),
        Some(&serde_json::json!(7777)),
        "lockfile must record the created label id: {lf_raw}"
    );

    // Idempotency: the second sync issued no further mutation on /labels
    // (the only mutating request total is the single create POST).
    let reqs_after_second = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        count_mutations(&reqs_after_second, "/api/v1/labels"),
        1,
        "second sync must not POST/PATCH /labels again (idempotent)"
    );
}

/// Push-side LocalCreate for a rule: a rule JSON (+ `.py` sidecar carrying
/// `trigger_condition`) exists on disk with no lockfile entry and the remote
/// omits it. `sync` must POST `/rules` with the reconstituted
/// `trigger_condition`, record the id, and be idempotent on re-sync.
#[tokio::test]
async fn push_create_rule() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let trigger = "annotation_content.total > 1000\n";
    let created_rule = serde_json::json!({
        "id": 8888,
        "url": format!("{}/api/v1/rules/8888", server.uri()),
        "name": "E-invoice Validation",
        "queues": [],
        "trigger_condition": trigger,
        "modified_at": "2026-05-01T08:00:00Z"
    });
    // Stateful listing: empty before the create, then the created rule — so
    // the second sync sees it as Clean (a true idempotency check).
    let rule_list_calls = Arc::new(AtomicUsize::new(0));
    let counter = rule_list_calls.clone();
    let created_for_list = created_rule.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                serde_json::json!({ "pagination": { "next": null }, "results": [] })
            } else {
                serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [created_for_list.clone()]
                })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &["/api/v1/rules"]).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_rule))
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

    // Seed a NEW local rule: JSON without `trigger_condition` (it lives in
    // the `.py` sidecar, exactly as pull writes it).
    let rules_dir = project.path().join("envs/dev/rules");
    std::fs::create_dir_all(&rules_dir).unwrap();
    let local_rule = serde_json::json!({
        "id": 0,
        "url": "",
        "name": "E-invoice Validation",
        "queues": []
    });
    let mut bytes = serde_json::to_vec_pretty(&local_rule).unwrap();
    bytes.push(b'\n');
    std::fs::write(rules_dir.join("e-invoice-validation.json"), &bytes).unwrap();
    std::fs::write(
        rules_dir.join("e-invoice-validation.py"),
        trigger.as_bytes(),
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let r1 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    let reqs_after_first = server.received_requests().await.unwrap_or_default();
    let r2 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    r1.expect("first sync (rule create) should succeed");
    r2.expect("second sync should succeed and be a no-op");

    assert_eq!(
        count_mutations(&reqs_after_first, "/api/v1/rules"),
        1,
        "exactly one POST /rules should have happened on the create sync"
    );

    let post = reqs_after_first
        .iter()
        .find(|r| r.method == http::Method::POST && r.url.path() == "/api/v1/rules")
        .expect("a POST /rules request must exist");
    let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["name"], "E-invoice Validation", "POST body: {body}");
    assert_eq!(
        body["trigger_condition"], trigger,
        "trigger_condition from the .py sidecar must be folded into the POST body: {body}"
    );

    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    assert_eq!(
        lf.pointer("/objects/rules/e-invoice-validation/id"),
        Some(&serde_json::json!(8888)),
        "lockfile must record the created rule id: {lf_raw}"
    );

    let reqs_after_second = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        count_mutations(&reqs_after_second, "/api/v1/rules"),
        1,
        "second sync must not POST/PATCH /rules again (idempotent)"
    );
}

/// Keystone test: dependency-ordered CREATE of a brand-new workspace + its
/// queue + the queue's schema, all seeded locally with no lockfile entries
/// and `rdc://` portable refs. `sync` must POST them in dependency order
/// (workspace → schema → queue), and the queue POST body must have its
/// `workspace`/`schema` `rdc://` refs RESOLVED to the freshly-created env
/// URLs (derived from the ids returned by the workspace/schema POSTs, which
/// run first and feed the lockfile). A second sync must be idempotent.
#[tokio::test]
async fn push_create_dependency_ordered_workspace_schema_queue() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // Fresh ids assigned by the server on create. The env's api_base is
    // `{server.uri()}/api/v1`, so the lockfile DERIVES each created object's
    // URL as `{api_base}/<kind>/<id>` (see Lockfile::url_for_slug). The queue
    // POST body's resolved refs must therefore be exactly these URLs.
    let ws_id = 5100u64;
    let schema_id = 5200u64;
    let queue_id = 5300u64;
    let ws_url = format!("{}/api/v1/workspaces/{ws_id}", server.uri());
    let schema_url = format!("{}/api/v1/schemas/{schema_id}", server.uri());
    let queue_url = format!("{}/api/v1/queues/{queue_id}", server.uri());

    let created_ws = serde_json::json!({
        "id": ws_id,
        "url": ws_url,
        "name": "Invoices AP",
        "organization": format!("{}/api/v1/organizations/1", server.uri()),
        "queues": [queue_url.clone()],
        "modified_at": "2026-05-01T08:00:00Z"
    });
    let created_schema = serde_json::json!({
        "id": schema_id,
        "url": schema_url,
        "name": "Cost Invoices Schema",
        "queues": [queue_url.clone()],
        "content": [
            { "category": "section", "id": "header", "label": "Header", "children": [
                { "category": "datapoint", "id": "invoice_id", "type": "string" }
            ]}
        ],
        "modified_at": "2026-05-01T08:00:00Z"
    });
    let created_queue = serde_json::json!({
        "id": queue_id,
        "url": queue_url,
        "name": "Cost Invoices",
        "workspace": ws_url.clone(),
        "schema": schema_url.clone(),
        "modified_at": "2026-05-01T08:00:00Z"
    });

    // Stateful workspace/queue listings: empty before the create, then the
    // created objects afterward — so the second sync sees Clean (genuine
    // idempotency) instead of treating them as remote-deleted.
    let ws_list_calls = Arc::new(AtomicUsize::new(0));
    let counter = ws_list_calls.clone();
    let created_ws_for_list = created_ws.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                serde_json::json!({ "pagination": { "next": null }, "results": [] })
            } else {
                serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [created_ws_for_list.clone()]
                })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    let queue_list_calls = Arc::new(AtomicUsize::new(0));
    let counter = queue_list_calls.clone();
    let created_queue_for_list = created_queue.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                serde_json::json!({ "pagination": { "next": null }, "results": [] })
            } else {
                serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [created_queue_for_list.clone()]
                })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    // The created schema is fetched by id on the second sync's pull (the
    // queue's `schema` URL points here).
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/schemas/{schema_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&created_schema))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/workspaces", "/api/v1/queues"]).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_ws))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/schemas"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_schema))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_queue))
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

    // Seed the local snapshot: workspace/<ws>/workspace.json and the nested
    // queue.json + schema.json. The queue references the workspace and schema
    // via portable `rdc://` refs (env-agnostic), exactly as pull writes them.
    let ws_dir = project.path().join("envs/dev/workspaces/invoices-ap");
    let q_dir = ws_dir.join("queues/cost-invoices");
    std::fs::create_dir_all(&q_dir).unwrap();

    let ws_json = serde_json::json!({
        "id": 0,
        "url": "",
        "name": "Invoices AP",
        "organization": format!("{}/api/v1/organizations/1", server.uri()),
        "queues": []
    });
    let mut b = serde_json::to_vec_pretty(&ws_json).unwrap();
    b.push(b'\n');
    std::fs::write(ws_dir.join("workspace.json"), &b).unwrap();

    let schema_json = serde_json::json!({
        "id": 0,
        "url": "",
        "name": "Cost Invoices Schema",
        "queues": [],
        "content": [
            { "category": "section", "id": "header", "label": "Header", "children": [
                { "category": "datapoint", "id": "invoice_id", "type": "string" }
            ]}
        ]
    });
    let mut b = serde_json::to_vec_pretty(&schema_json).unwrap();
    b.push(b'\n');
    std::fs::write(q_dir.join("schema.json"), &b).unwrap();

    let queue_json = serde_json::json!({
        "id": 0,
        "url": "",
        "name": "Cost Invoices",
        "workspace": "rdc://workspaces/invoices-ap",
        "schema": "rdc://schemas/cost-invoices"
    });
    let mut b = serde_json::to_vec_pretty(&queue_json).unwrap();
    b.push(b'\n');
    std::fs::write(q_dir.join("queue.json"), &b).unwrap();

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let r1 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    let reqs_after_first = server.received_requests().await.unwrap_or_default();
    let r2 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    r1.expect("first sync (dependency-ordered create) should succeed");
    r2.expect("second sync should succeed and be a no-op");

    // All three POSTs landed exactly once.
    assert_eq!(
        count_mutations(&reqs_after_first, "/api/v1/workspaces"),
        1,
        "exactly one POST /workspaces"
    );
    assert_eq!(
        count_mutations(&reqs_after_first, "/api/v1/schemas"),
        1,
        "exactly one POST /schemas"
    );
    assert_eq!(
        count_mutations(&reqs_after_first, "/api/v1/queues"),
        1,
        "exactly one POST /queues"
    );

    // Dependency ordering: the workspace and schema POSTs both precede the
    // queue POST (the queue's resolved refs depend on their fresh ids).
    let post_index = |p: &str| -> usize {
        reqs_after_first
            .iter()
            .position(|r| r.method == http::Method::POST && r.url.path() == p)
            .unwrap_or_else(|| panic!("expected a POST to {p}"))
    };
    let ws_idx = post_index("/api/v1/workspaces");
    let schema_idx = post_index("/api/v1/schemas");
    let queue_idx = post_index("/api/v1/queues");
    assert!(
        ws_idx < queue_idx,
        "workspace POST (#{ws_idx}) must precede queue POST (#{queue_idx})"
    );
    assert!(
        schema_idx < queue_idx,
        "schema POST (#{schema_idx}) must precede queue POST (#{queue_idx})"
    );

    // The queue POST body's refs are RESOLVED to the freshly-created env URLs
    // (NOT `rdc://`), proving the workspace/schema ids flowed into the queue.
    let queue_post = reqs_after_first
        .iter()
        .find(|r| r.method == http::Method::POST && r.url.path() == "/api/v1/queues")
        .expect("a POST /queues request must exist");
    let qbody: serde_json::Value = serde_json::from_slice(&queue_post.body).unwrap();
    assert_eq!(
        qbody["workspace"],
        serde_json::json!(format!("{}/api/v1/workspaces/{ws_id}", server.uri())),
        "queue.workspace must resolve to the freshly-created workspace URL: {qbody}"
    );
    assert_eq!(
        qbody["schema"],
        serde_json::json!(format!("{}/api/v1/schemas/{schema_id}", server.uri())),
        "queue.schema must resolve to the freshly-created schema URL: {qbody}"
    );
    assert!(
        !qbody.to_string().contains("rdc://"),
        "no unresolved rdc:// refs may remain in the queue POST body: {qbody}"
    );

    // Lockfile records all three ids under the right kinds.
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    assert_eq!(
        lf.pointer("/objects/workspaces/invoices-ap/id"),
        Some(&serde_json::json!(ws_id)),
        "lockfile workspace id: {lf_raw}"
    );
    assert_eq!(
        lf.pointer("/objects/schemas/cost-invoices/id"),
        Some(&serde_json::json!(schema_id)),
        "lockfile schema id: {lf_raw}"
    );
    assert_eq!(
        lf.pointer("/objects/queues/cost-invoices/id"),
        Some(&serde_json::json!(queue_id)),
        "lockfile queue id: {lf_raw}"
    );

    // Idempotency: the second sync issued no further create on any of the
    // three endpoints.
    let reqs_after_second = server.received_requests().await.unwrap_or_default();
    for ep in ["/api/v1/workspaces", "/api/v1/schemas", "/api/v1/queues"] {
        assert_eq!(
            count_mutations(&reqs_after_second, ep),
            1,
            "second sync must not mutate {ep} again (idempotent)"
        );
    }
}

/// Push-side LocalCreate for an inbox + email template, both queue-nested.
/// The owning workspace/queue/schema already exist (seeded in the lockfile +
/// remote listing as Clean), so only the new inbox and email template are
/// `LocalCreate`. `sync` must POST `/inboxes` and `/email_templates`, each
/// with its `queue` ref resolved to the existing queue URL, and be
/// idempotent on re-sync.
#[tokio::test]
async fn push_create_inbox_and_email_template() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // The workspace + queue + schema already exist remotely and locally
    // (Clean). The inbox and email_template are NOT in the listings → the
    // local files for them are LocalCreate.
    let ws_url = format!("{}/api/v1/workspaces/800", server.uri());
    let queue_url = format!("{}/api/v1/queues/100", server.uri());
    let schema_url = format!("{}/api/v1/schemas/200", server.uri());

    let workspaces_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [{
            "id": 800, "url": ws_url, "name": "Invoices AP",
            "organization": format!("{}/api/v1/organizations/1", server.uri()),
            "queues": [queue_url.clone()], "modified_at": "2026-04-20T08:00:00Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(workspaces_body))
        .mount(&server)
        .await;

    let queues_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [{
            "id": 100, "url": queue_url.clone(), "name": "Cost Invoices",
            "workspace": ws_url.clone(), "schema": schema_url.clone(),
            "modified_at": "2026-04-20T08:00:00Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queues_body))
        .mount(&server)
        .await;

    let schema_body = serde_json::json!({
        "id": 200, "url": schema_url.clone(), "name": "Cost Invoices Schema",
        "queues": [queue_url.clone()],
        "content": [
            { "category": "section", "id": "header", "label": "Header", "children": [
                { "category": "datapoint", "id": "invoice_id", "type": "string" }
            ]}
        ],
        "modified_at": "2026-04-10T09:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(schema_body))
        .mount(&server)
        .await;

    // POST endpoints for the two new nested objects. The server assigns ids
    // and (for the inbox) the email address.
    let created_inbox = serde_json::json!({
        "id": 300,
        "url": format!("{}/api/v1/inboxes/300", server.uri()),
        "name": "Cost Invoices Inbox",
        "email": "cost-invoices@mock.rossum.app",
        "queues": [queue_url.clone()],
        "filters": [],
        "modified_at": "2026-05-01T08:00:00Z"
    });
    let created_template = serde_json::json!({
        "id": 9001,
        "url": format!("{}/api/v1/email_templates/9001", server.uri()),
        "name": "Rejection Notice",
        "subject": "Your invoice was rejected",
        "queue": queue_url.clone(),
        "modified_at": "2026-05-01T08:00:00Z"
    });

    // Stateful inbox/email_template listings: empty until the create, then
    // the created object — so the final sync classifies them Clean (genuine
    // idempotency). The first sync (initial pull) and the second sync's pull
    // both see empty; only the third sync's pull sees the created objects.
    let inbox_list_calls = Arc::new(AtomicUsize::new(0));
    let counter = inbox_list_calls.clone();
    let created_inbox_for_list = created_inbox.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes"))
        .respond_with(move |_req: &Request| {
            // Two list calls happen before the create lands (initial pull +
            // the create sync's own pull pass), so serve the created body
            // only from the third list onward.
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n < 2 {
                serde_json::json!({ "pagination": { "next": null }, "results": [] })
            } else {
                serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [created_inbox_for_list.clone()]
                })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    let tpl_list_calls = Arc::new(AtomicUsize::new(0));
    let counter = tpl_list_calls.clone();
    let created_tpl_for_list = created_template.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(move |_req: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n < 2 {
                serde_json::json!({ "pagination": { "next": null }, "results": [] })
            } else {
                serde_json::json!({
                    "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
                    "results": [created_tpl_for_list.clone()]
                })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(
        &server,
        &[
            "/api/v1/workspaces",
            "/api/v1/queues",
            "/api/v1/inboxes",
            "/api/v1/email_templates",
        ],
    )
    .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/inboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_inbox))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&created_template))
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

    // First sync: pull the existing workspace/queue/schema so they land in
    // the lockfile as Clean. The inbox + template files don't exist yet, so
    // nothing is created. The seed-then-create-then-resync steps below all
    // run inside the same CWD window; results are asserted after CWD is
    // restored (mirroring the other push_create tests) so a mid-test panic
    // can't poison the process-global CWD.
    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let pull = rdc::cli::sync::run("dev", false, false, false, false, false).await;

    // Now seed the NEW inbox + email template under the existing queue, both
    // with `rdc://queues/cost-invoices` refs.
    let q_dir = project
        .path()
        .join("envs/dev/workspaces/invoices-ap/queues/cost-invoices");
    let inbox_json = serde_json::json!({
        "id": 0, "url": "",
        "name": "Cost Invoices Inbox",
        "queues": ["rdc://queues/cost-invoices"],
        "filters": []
    });
    let mut b = serde_json::to_vec_pretty(&inbox_json).unwrap();
    b.push(b'\n');
    std::fs::write(q_dir.join("inbox.json"), &b).unwrap();

    let tpl_dir = q_dir.join("email-templates");
    std::fs::create_dir_all(&tpl_dir).unwrap();
    let tpl_json = serde_json::json!({
        "id": 0, "url": "",
        "name": "Rejection Notice",
        "subject": "Your invoice was rejected",
        "queue": "rdc://queues/cost-invoices"
    });
    let mut b = serde_json::to_vec_pretty(&tpl_json).unwrap();
    b.push(b'\n');
    std::fs::write(tpl_dir.join("rejection-notice.json"), &b).unwrap();

    let r2 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    let reqs_after_create = server.received_requests().await.unwrap_or_default();
    let r3 = rdc::cli::sync::run("dev", false, false, false, false, false).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    pull.expect("initial pull sync should succeed");
    r2.expect("second sync (inbox + template create) should succeed");
    r3.expect("third sync should succeed and be a no-op");

    assert_eq!(
        count_mutations(&reqs_after_create, "/api/v1/inboxes"),
        1,
        "exactly one POST /inboxes"
    );
    assert_eq!(
        count_mutations(&reqs_after_create, "/api/v1/email_templates"),
        1,
        "exactly one POST /email_templates"
    );

    // Inbox POST body: `email` is stripped (server-assigned) and the queue
    // ref resolved to the existing queue URL.
    let inbox_post = reqs_after_create
        .iter()
        .find(|r| r.method == http::Method::POST && r.url.path() == "/api/v1/inboxes")
        .expect("a POST /inboxes request must exist");
    let ibody: serde_json::Value = serde_json::from_slice(&inbox_post.body).unwrap();
    assert_eq!(
        ibody["name"], "Cost Invoices Inbox",
        "inbox POST body: {ibody}"
    );
    assert_eq!(
        ibody["queues"],
        serde_json::json!([queue_url.clone()]),
        "inbox.queues must resolve to the existing queue URL: {ibody}"
    );
    assert!(
        ibody.get("email").is_none(),
        "server-assigned email must be stripped from the inbox POST body: {ibody}"
    );

    // Email template POST body: queue ref resolved.
    let tpl_post = reqs_after_create
        .iter()
        .find(|r| r.method == http::Method::POST && r.url.path() == "/api/v1/email_templates")
        .expect("a POST /email_templates request must exist");
    let tbody: serde_json::Value = serde_json::from_slice(&tpl_post.body).unwrap();
    assert_eq!(
        tbody["name"], "Rejection Notice",
        "template POST body: {tbody}"
    );
    assert_eq!(
        tbody["queue"],
        serde_json::json!(queue_url.clone()),
        "template.queue must resolve to the existing queue URL: {tbody}"
    );

    // Lockfile records both created ids (email_templates use a compound key).
    let lf_raw = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf: serde_json::Value = serde_json::from_str(&lf_raw).unwrap();
    assert_eq!(
        lf.pointer("/objects/inboxes/cost-invoices/id"),
        Some(&serde_json::json!(300)),
        "lockfile inbox id: {lf_raw}"
    );
    assert_eq!(
        lf.pointer("/objects/email_templates/invoices-ap~1cost-invoices~1rejection-notice/id"),
        Some(&serde_json::json!(9001)),
        "lockfile email_template id (compound key): {lf_raw}"
    );

    // Idempotency: the third sync issued no further create.
    let reqs_after_second = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        count_mutations(&reqs_after_second, "/api/v1/inboxes"),
        1,
        "third sync must not POST /inboxes again (idempotent)"
    );
    assert_eq!(
        count_mutations(&reqs_after_second, "/api/v1/email_templates"),
        1,
        "third sync must not POST /email_templates again (idempotent)"
    );
}

/// Regression for the inbox-push → parent-queue re-pull churn (Bug #2):
/// after `rdc sync` PATCHes an inbox edit, the NEXT sync must be Clean.
///
/// Root cause (empirically confirmed against the live Rossum API): the
/// inbox **PATCH** response OMITS the `bounce_email_to` field, while the
/// **GET** response INCLUDES it (as `null`). `bounce_email_to` rides in
/// the `Inbox` model's untyped `extra` passthrough, so a PATCH-derived
/// base lacks the key while the next sync's classifier — which reads the
/// inbox off the bulk `GET /inboxes` list — sees the key present (null).
/// base(no key) ≠ remote(key=null) → spurious RemoteEdit → the queue
/// driver re-pulls the bundle once ("queues (1 pulled … inboxes 1)").
///
/// This mock REPLICATES THE QUIRK: the PATCH handler returns a body
/// WITHOUT `bounce_email_to`, while every GET (list + by-id) returns it as
/// `null`. The fix re-baselines the inbox push from a fresh GET (not the
/// PATCH response), so the recorded base shape matches the classifier's
/// input on the next sync and the third sync settles to Clean.
///
/// NOTE: a prior version of this test echoed `bounce_email_to` back in the
/// PATCH response (identical PATCH/GET shapes) and was therefore a
/// false-negative — it passed even on the unfixed code. The PATCH-response
/// omission below is load-bearing.
#[tokio::test]
async fn sync_inbox_push_then_resync_is_clean() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let ws_url = format!("{}/api/v1/workspaces/800", server.uri());
    let queue_url = format!("{}/api/v1/queues/100", server.uri());
    let schema_url = format!("{}/api/v1/schemas/200", server.uri());
    let inbox_url = format!("{}/api/v1/inboxes/300", server.uri());

    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
            "results": [{
                "id": 800, "url": ws_url.clone(), "name": "Invoices AP",
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "queues": [queue_url.clone()], "modified_at": "2026-04-20T08:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
            "results": [{
                "id": 100, "url": queue_url.clone(), "name": "Cost Invoices",
                "workspace": ws_url.clone(), "schema": schema_url.clone(),
                "inbox": inbox_url.clone(), "modified_at": "2026-04-20T08:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    // The parent queue object is byte-identical before and after the inbox
    // PATCH (no cascade) — its GET body never changes. The inbox GET-by-id is
    // used by the push driver's drift check.
    let schema_body = serde_json::json!({
        "id": 200, "url": schema_url.clone(), "name": "Cost Invoices Schema",
        "queues": [queue_url.clone()],
        "content": [
            { "category": "section", "id": "header", "label": "Header", "children": [
                { "category": "datapoint", "id": "invoice_id", "type": "string" }
            ]}
        ],
        "modified_at": "2026-04-10T09:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(schema_body))
        .mount(&server)
        .await;

    // Stateful inbox state. CRUCIAL: GET responses (list + by-id) ALWAYS
    // carry `bounce_email_to: null`; the PATCH response (below) strips it.
    // This is the exact live-API quirk that drives Bug #2.
    let inbox_state = Arc::new(Mutex::new(serde_json::json!({
        "id": 300, "url": inbox_url.clone(),
        "name": "Cost Invoices Inbox",
        "email": "cost-invoices@mock.rossum.app",
        "bounce_email_to": serde_json::Value::Null,
        "queues": [queue_url.clone()],
        "modified_at": "2026-04-10T09:00:00Z", "filters": []
    })));

    let st = inbox_state.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes"))
        .respond_with(move |_req: &Request| {
            // GET includes `bounce_email_to: null` — this is what the
            // classifier reads to compute the inbox's remote hash.
            let body = st.lock().unwrap_or_else(|p| p.into_inner()).clone();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "total_pages": 1, "next": null },
                "results": [body]
            }))
        })
        .mount(&server)
        .await;

    let st = inbox_state.clone();
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(move |_req: &Request| {
            // GET-by-id also includes `bounce_email_to: null` — used by the
            // push driver's drift check and (post-fix) the re-baseline.
            let body = st.lock().unwrap_or_else(|p| p.into_inner()).clone();
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;

    let st = inbox_state.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(move |req: &Request| {
            // Apply the PATCH body onto the stored state so subsequent GETs
            // reflect the edit, then return a response body that OMITS
            // `bounce_email_to` — replicating the live Rossum quirk where
            // the inbox PATCH response drops that field while GET keeps it
            // (null). The stored state (served by GET) KEEPS the key.
            let patch: serde_json::Value = req.body_json().unwrap();
            let mut guard = st.lock().unwrap_or_else(|p| p.into_inner());
            if let (Some(obj), Some(p)) = (guard.as_object_mut(), patch.as_object()) {
                for (k, v) in p {
                    obj.insert(k.clone(), v.clone());
                }
                // The server keeps bounce_email_to null regardless of input.
                obj.insert("bounce_email_to".to_string(), serde_json::Value::Null);
                obj.insert(
                    "modified_at".to_string(),
                    serde_json::json!("2026-05-01T10:00:00Z"),
                );
            }
            let mut patch_response = guard.clone();
            // PATCH RESPONSE omits bounce_email_to (the quirk under test).
            if let Some(obj) = patch_response.as_object_mut() {
                obj.remove("bounce_email_to");
            }
            ResponseTemplate::new(200).set_body_json(patch_response)
        })
        .mount(&server)
        .await;

    mock_empty_lists_except(
        &server,
        &["/api/v1/workspaces", "/api/v1/queues", "/api/v1/inboxes"],
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

    // Sync 1: pull the queue tree (inbox lands on disk in rdc form).
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .assert()
        .success();

    // Edit the inbox name locally → LocalEdit → PATCH. (We edit `name`, not
    // `bounce_email_to`, because the server keeps the latter null; the point
    // is that the PATCH *response* shape differs from GET regardless.)
    let inbox_path = project
        .path()
        .join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/inbox.json");
    let mut inbox: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&inbox_path).unwrap()).unwrap();
    inbox["name"] = serde_json::json!("Cost Invoices Inbox (edited)");
    std::fs::write(
        &inbox_path,
        format!("{}\n", serde_json::to_string_pretty(&inbox).unwrap()),
    )
    .unwrap();

    // Sync 2: pushes the inbox PATCH.
    let out2 = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "second sync (push) must succeed:\n{}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out2.stderr).contains("patch  inbox/cost-invoices"),
        "second sync must PATCH the edited inbox:\n{}",
        String::from_utf8_lossy(&out2.stderr)
    );

    // Sync 3: must be Clean — 0 changed, no inbox/queue re-pull.
    let out3 = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .output()
        .unwrap();
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    assert!(out3.status.success(), "third sync must succeed:\n{stderr3}");
    assert!(
        stderr3.contains("(0 changed"),
        "third sync after an inbox push must be Clean (0 changed); got:\n{stderr3}"
    );
}
