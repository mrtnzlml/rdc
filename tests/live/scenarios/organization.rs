use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;

/// The org's `settings` is a permanent, live singleton — never a throwaway
/// object a run creates and deletes — so restoring its original value is not
/// optional cleanup, it's the difference between this test and corrupting a
/// shared test org's document-list columns. This guard restores it in `Drop`,
/// the same way `support::teardown::Teardown` restores torn-down objects:
/// `Drop` can fire mid-unwind (a failed `assert!` above), and `block_on` on
/// the current thread panics there ("Cannot start a runtime from within a
/// runtime") — a second panic while already unwinding aborts the process. So
/// the async restore runs on a dedicated OS thread with its own current-thread
/// runtime, joined by `thread::scope` before `drop` returns.
struct RestoreSettings {
    client: LiveClient,
    org_id: u64,
    original: serde_json::Value,
}

impl Drop for RestoreSettings {
    fn drop(&mut self) {
        let client = &self.client;
        let org_id = self.org_id;
        let original = &self.original;
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("restore organization settings: could not build runtime: {e}");
                        return;
                    }
                };
                if let Err(e) = rt.block_on(client.patch_organization_settings(org_id, original)) {
                    eprintln!("restore organization settings failed (continuing): {e:#}");
                }
            });
        });
    }
}

/// Edit `settings.annotation_list_table.columns` locally to a single `meta`
/// column, push it through `rdc sync`, and confirm the remote org actually
/// persisted it — then restore the org's original `settings` verbatim and
/// confirm THAT too. A `meta` column (as opposed to a `column_type: "schema"`
/// one) needs no `schema_id` naming a real field, so this is valid against
/// any env, including a shared sandbox org this harness does not own the
/// schema of.
///
/// Deliberately narrow: this is the one live check that `rdc sync` actually
/// round-trips a `settings` edit end-to-end against a real org. The push
/// driver's other branches (unmanaged-field no-op, absent-`settings` skip,
/// the "only settings pushed" notice) are covered by fast unit tests in
/// `src/cli/push/organization.rs` and don't need network access to verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_organization_settings_push() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let client = LiveClient::connect(&cfg).expect("connect");

    // Baseline, captured straight from the API before anything is touched.
    let original_settings = client
        .get_organization_settings(cfg.org_id)
        .await
        .expect("GET organization for baseline settings");

    // Constructed BEFORE any mutation below, so a panic anywhere in this test
    // still restores the org on the way out.
    let restore = RestoreSettings {
        client: LiveClient::connect(&cfg).expect("connect (restore)"),
        org_id: cfg.org_id,
        original: original_settings.clone(),
    };

    // Pull the org into a fresh local project.
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init project");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "initial sync --no-push failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    assert!(
        project.exists("envs/test/organization.json"),
        "rdc sync did not pull envs/test/organization.json"
    );

    // Edit the managed subtree to one `meta` column and push.
    let mut org_json = project.read_json("envs/test/organization.json");
    let pushed_columns = serde_json::json!([
        { "visible": true, "column_type": "meta", "width": 120.0, "meta_name": "status" }
    ]);
    // Preserve any sibling keys under `settings` / `annotation_list_table`
    // (e.g. an existing `request_dashboard_table`) — only `columns` changes.
    if !org_json["settings"].is_object() {
        org_json["settings"] = serde_json::json!({});
    }
    if !org_json["settings"]["annotation_list_table"].is_object() {
        org_json["settings"]["annotation_list_table"] = serde_json::json!({});
    }
    org_json["settings"]["annotation_list_table"]["columns"] = pushed_columns.clone();
    std::fs::write(
        project.path().join("envs/test/organization.json"),
        serde_json::to_vec_pretty(&org_json).expect("serialize edited organization.json"),
    )
    .expect("write edited organization.json");

    let push = project.run_rdc(&["sync", "test"]);
    assert!(
        push.status.success(),
        "push sync failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    // Assert on `settings` only — never on the org's `name` or any other
    // identifier — and confirm against the REMOTE, not the local file `rdc`
    // just wrote back (which would only prove rdc believes it, not that the
    // server does).
    let after_push = client
        .get_organization_settings(cfg.org_id)
        .await
        .expect("GET organization after push");
    assert_eq!(
        after_push.get("annotation_list_table").and_then(|t| t.get("columns")),
        Some(&pushed_columns),
        "pushed column did not persist remotely: {after_push}"
    );

    // Restore now (join the background thread synchronously) rather than
    // waiting for the guard to fall out of scope at the end of the function,
    // so the assertion below observes a completed restore.
    drop(restore);
    let restored = client
        .get_organization_settings(cfg.org_id)
        .await
        .expect("GET organization after restore");
    assert_eq!(
        restored, original_settings,
        "organization settings were not restored to their original value"
    );
}
