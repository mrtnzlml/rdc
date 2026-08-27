//! Live round-trip for the `saved_views` kind.
//!
//! Creates one SHARED and one PRIVATE view directly on the API, syncs, and
//! asserts the shared one is snapshotted while the private one is not. Deletes
//! both before returning.

use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::teardown::Teardown;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_saved_views_round_trip() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };

    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up both views —
    // `teardown_by_prefix` lists saved views WITHOUT the shared-only filter
    // `rdc` applies, so it reaches the private one too even though `rdc`
    // itself never learns that one exists.
    let teardown = Teardown::new(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
    );

    // --- create one shared + one private view directly on the API ---
    // `organization` is server-populated and read-only (an override is
    // silently ignored per the design doc's OPTIONS probe), so it is
    // deliberately omitted here.
    let shared_name = run_id.prefix("shared-view");
    let private_name = run_id.prefix("private-view");
    // `{"$and": []}` is NOT a valid "match everything" query -- the server
    // rejects an empty `$and` with 400 `{"query":{"$and":["This list may not
    // be empty."]}}` (verified live 2026-08-27). Use a real, non-empty
    // condition instead.
    let query = serde_json::json!({ "$and": [ { "status": { "$in": ["to_review"] } } ] });

    let (shared_id, _) = client
        .create(
            "saved_view",
            &serde_json::json!({
                "name": shared_name,
                "shared": true,
                "query": query,
            }),
        )
        .await
        .expect("create shared saved view");
    let (private_id, _) = client
        .create(
            "saved_view",
            &serde_json::json!({
                "name": private_name,
                "shared": false,
                "query": query,
            }),
        )
        .await
        .expect("create private saved view");

    // --- pull into a fresh local project ---
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init project");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "initial sync --no-push failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    // --- the shared view landed on disk; the private one did not ---
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();
    let slug = lockfile_keys(&lf, "saved_views")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("shared saved view slug not found in lockfile");
    let rel = format!("envs/test/saved-views/{slug}.json");
    assert!(project.exists(&rel), "shared saved view must be pulled to {rel}");

    let raw = project.read_to_string(&rel).expect("read pulled shared saved view file");
    for gone in ["created_by", "created_at", "modified_at", "modified_by"] {
        assert!(!raw.contains(gone), "{gone} must be stripped from disk; got:\n{raw}");
    }
    let on_disk: serde_json::Value = serde_json::from_str(&raw).expect("parse pulled saved view");
    assert_eq!(on_disk["name"], serde_json::Value::String(shared_name.clone()));

    // The private view must never reach the lockfile...
    assert!(
        lf.slug_for_id("saved_views", private_id).is_none(),
        "private view must not be recorded in the lockfile"
    );
    // ...nor any file under saved-views/ -- the one property no mock can
    // really prove, since the filter exists precisely because the server
    // ignores `?shared=true`.
    let saved_views_dir = project.path().join("envs/test/saved-views");
    let on_disk_ids: Vec<u64> = std::fs::read_dir(&saved_views_dir)
        .expect("read saved-views dir")
        .flatten()
        .map(|entry| {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap_or_default())
                    .unwrap_or(serde_json::Value::Null);
            v.get("id").and_then(|i| i.as_u64()).unwrap_or(0)
        })
        .collect();
    assert!(
        !on_disk_ids.contains(&private_id),
        "private view (id {private_id}) must not appear as a file under saved-views/; found ids {on_disk_ids:?}"
    );

    // --- edit the local name and push ---
    let mut edited = on_disk.clone();
    let new_name = format!("{shared_name}-edited");
    edited["name"] = serde_json::Value::String(new_name.clone());
    std::fs::write(
        project.path().join(&rel),
        serde_json::to_vec_pretty(&edited).unwrap(),
    )
    .unwrap();

    let push = project.run_rdc(&["sync", "test"]);
    assert!(
        push.status.success(),
        "push sync failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    let remote_after_push = client
        .find_listed_value("saved_view", shared_id)
        .await
        .expect("list saved views after push")
        .expect("shared saved view must still exist remotely after push");
    assert_eq!(
        remote_after_push.get("name"),
        Some(&serde_json::Value::String(new_name)),
        "pushed saved view name mismatch: {remote_after_push:?}"
    );

    // --- delete the local file, sync --allow-deletes, remote is gone ---
    std::fs::remove_file(project.path().join(&rel)).expect("removing local saved view file");
    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(
        del.status.success(),
        "delete sync failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );

    // No GET-by-id exists for this kind (same as labels/rules/queues); list +
    // find-by-id absence IS the "GET -> 404" check here.
    let after_delete = client
        .find_listed_value("saved_view", shared_id)
        .await
        .expect("list saved views after delete");
    assert!(
        after_delete.is_none(),
        "shared saved view must be deleted on the remote after --allow-deletes"
    );

    drop(teardown); // explicit: delete everything now (also runs on panic)
}
